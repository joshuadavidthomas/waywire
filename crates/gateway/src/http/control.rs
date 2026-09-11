use std::sync::Arc;

use anyhow::anyhow;
use axum::extract::ws::Message;
use axum::extract::ws::WebSocket;
use futures_util::StreamExt;
use nix::time::ClockId;
use nix::time::clock_gettime;
use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::info;
use tracing::warn;
use waywire_protocol::browser::ClientEvent;
use waywire_protocol::browser::ClientMessage;
use waywire_protocol::browser::ControlState;
use waywire_protocol::browser::QualityLevels;
use waywire_protocol::browser::parse_browser_record;
use waywire_protocol::pipe::Command;
use waywire_protocol::pipe::ResetVideo;
use waywire_protocol::pipe::Text as TextCommand;

use super::AppState;
use super::admission::SocketAdmission;
use super::outbound::OUTBOUND_BYTE_LIMIT;
use super::outbound::OUTBOUND_MESSAGE_LIMIT;
use super::outbound::Outbound;
use super::outbound::OutboundError;
use super::outbound::WriterFailure;
use super::outbound::control_writer;
use super::socket::SocketEnd;
use super::socket::WRITE_LIMIT;
use super::socket::log_socket_close;
use crate::session::FeedbackOutcome;
use crate::session::InputOutcome;
use crate::session::LeaseState;
use crate::session::SocketId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Audience {
    Everyone,
    LeaseOwner,
}

trait ClientEventAudience {
    fn audience(&self) -> Audience;
}

impl ClientEventAudience for ClientEvent {
    fn audience(&self) -> Audience {
        match self {
            ClientEvent::Clipboard { .. } | ClientEvent::ResetVideoRefused { .. } => {
                Audience::LeaseOwner
            }
            ClientEvent::Cursor(_)
            | ClientEvent::ResizeApplied(_)
            | ClientEvent::VideoConfig { .. }
            | ClientEvent::ControlState { .. }
            | ClientEvent::Pong { .. }
            | ClientEvent::Quality(_) => Audience::Everyone,
        }
    }
}

pub(super) async fn control_socket(
    socket: WebSocket,
    state: AppState,
    socket_id: SocketId,
    _admission: SocketAdmission,
) {
    info!(socket_id = socket_id.get(), "control socket opened");
    let reason = run_control_socket(socket, &state, socket_id).await;
    log_socket_close("control", socket_id, &reason);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lease {
    Held,
    NotHeld,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriterStatus {
    Running,
    Finished,
}

#[expect(
    clippy::too_many_lines,
    reason = "the exhaustive socket select keeps event ordering and cancellation in one loop"
)]
async fn run_control_socket(socket: WebSocket, state: &AppState, socket_id: SocketId) -> SocketEnd {
    let (snapshot, mut events) = state.events.subscribe();
    let cancellation = state.connections.cancellation.child_token();
    let (sink, mut incoming) = socket.split();
    let (sender, receiver) = mpsc::channel(OUTBOUND_MESSAGE_LIMIT);
    let outbound = Outbound {
        tx: sender,
        bytes: Arc::new(Semaphore::new(OUTBOUND_BYTE_LIMIT)),
    };
    let mut writer = tokio::spawn(control_writer(sink, receiver, cancellation.clone()));
    let mut writer_status = WriterStatus::Running;
    let mut lease = Lease::NotHeld;

    let reason = 'connected: {
        if let Err(error) = enqueue_snapshot(
            &outbound,
            snapshot,
            state.sessions.quality(),
            Lease::NotHeld,
        ) {
            break 'connected error.into();
        }

        loop {
            tokio::select! {
                biased;
                () = state.connections.cancellation.cancelled() => break 'connected SocketEnd::Shutdown,
                writer_result = &mut writer => {
                    writer_status = WriterStatus::Finished;
                    break 'connected match writer_result {
                        Ok(Ok(())) => SocketEnd::Shutdown,
                        Ok(Err(error)) => SocketEnd::failed(error),
                        Err(error) => SocketEnd::failed(
                            anyhow::Error::new(error).context("control writer task failed"),
                        ),
                    };
                }
                incoming_result = incoming.next() => {
                    let message = match incoming_result {
                        Some(Ok(message)) => message,
                        Some(Err(error)) => break 'connected SocketEnd::failed(
                            anyhow::Error::new(error).context("WebSocket read failed"),
                        ),
                        None => break 'connected SocketEnd::Disconnected,
                    };
                    let result = match message {
                        Message::Text(text) => {
                            handle_text(&outbound, state, socket_id, &mut lease, text.as_str()).await
                        }
                        Message::Binary(bytes) => {
                            let command = parse_browser_record(&bytes).map_err(|error| {
                                SocketEnd::failed(
                                    anyhow::Error::new(error)
                                        .context("browser control parse failed"),
                                )
                            });
                            match command {
                                Ok(command) => match lease {
                                    Lease::NotHeld => Ok(()),
                                    Lease::Held => match state.sessions.input(socket_id, command).await {
                                        Ok(InputOutcome::Sent) => Ok(()),
                                        Ok(InputOutcome::NotLeaseOwner) => {
                                            lease = Lease::NotHeld;
                                            warn!(
                                                socket_id = socket_id.get(),
                                                "control socket input lease disagreed with session owner"
                                            );
                                            Ok(())
                                        }
                                        Err(error) => Err(SocketEnd::failed(
                                            error.context("send binary input"),
                                        )),
                                    },
                                },
                                Err(error) => Err(error),
                            }
                        }
                        Message::Close(frame) => break 'connected SocketEnd::ClientClosed(frame),
                        Message::Ping(_) | Message::Pong(_) => Ok(()),
                    };
                    if let Err(error) = result {
                        break 'connected error;
                    }
                }
                event_result = events.recv() => {
                    if let Err(error) = dispatch_control_event(
                        &outbound,
                        state,
                        lease,
                        event_result,
                    ) {
                        break 'connected error;
                    }
                }
            }
        }
    };

    finish_control_socket(
        reason,
        state,
        socket_id,
        cancellation,
        writer_status,
        &mut writer,
    )
    .await
}

fn dispatch_control_event(
    outbound: &Outbound,
    state: &AppState,
    lease: Lease,
    result: Result<ClientEvent, broadcast::error::RecvError>,
) -> Result<(), SocketEnd> {
    match result {
        Ok(event) => match (event.audience(), lease) {
            (Audience::Everyone, Lease::Held | Lease::NotHeld)
            | (Audience::LeaseOwner, Lease::Held) => {
                outbound.enqueue_event(&event).map_err(SocketEnd::from)
            }
            (Audience::LeaseOwner, Lease::NotHeld) => Ok(()),
        },
        Err(broadcast::error::RecvError::Lagged(count)) => {
            debug!(lagged_events = count, "control event receiver lagged");
            enqueue_snapshot(
                outbound,
                state.events.snapshot(),
                state.sessions.quality(),
                lease,
            )
            .map_err(SocketEnd::from)
        }
        Err(broadcast::error::RecvError::Closed) => Err(SocketEnd::failed(anyhow!(
            "control event broadcaster closed"
        ))),
    }
}

fn enqueue_snapshot(
    outbound: &Outbound,
    snapshot: Vec<ClientEvent>,
    quality: QualityLevels,
    lease: Lease,
) -> Result<(), OutboundError> {
    for event in snapshot {
        match (event.audience(), lease) {
            (Audience::Everyone, Lease::Held | Lease::NotHeld)
            | (Audience::LeaseOwner, Lease::Held) => outbound.enqueue_event(&event)?,
            (Audience::LeaseOwner, Lease::NotHeld) => {}
        }
    }
    outbound.enqueue_event(&ClientEvent::Quality(quality))
}

#[derive(Debug, Error)]
enum CleanupFailure {
    #[error("release input lease")]
    Release(#[source] anyhow::Error),
    #[error("control writer cleanup")]
    Writer(#[source] anyhow::Error),
    #[error("release input lease; control writer cleanup also failed: {writer:#}")]
    Both {
        #[source]
        release: anyhow::Error,
        writer: anyhow::Error,
    },
}

async fn finish_control_socket(
    mut reason: SocketEnd,
    state: &AppState,
    socket_id: SocketId,
    cancellation: CancellationToken,
    writer_status: WriterStatus,
    writer: &mut JoinHandle<Result<(), WriterFailure>>,
) -> SocketEnd {
    cancellation.cancel();
    let release_error = state.sessions.release(socket_id).await.err();

    let writer_error = match writer_status {
        WriterStatus::Finished => None,
        WriterStatus::Running => match timeout(WRITE_LIMIT, &mut *writer).await {
            Ok(Ok(Ok(()))) => None,
            Ok(Ok(Err(error))) => Some(anyhow::Error::new(error)),
            Ok(Err(error)) => Some(anyhow::Error::new(error).context("control writer task failed")),
            Err(_elapsed) => {
                writer.abort();
                let _ = writer.await;
                Some(anyhow!(
                    "control writer did not stop within {WRITE_LIMIT:?}"
                ))
            }
        },
    };
    let cleanup_error = match (release_error, writer_error) {
        (Some(release), Some(writer)) => {
            Some(anyhow::Error::new(CleanupFailure::Both { release, writer }))
        }
        (Some(error), None) => Some(anyhow::Error::new(CleanupFailure::Release(error))),
        (None, Some(error)) => Some(anyhow::Error::new(CleanupFailure::Writer(error))),
        (None, None) => None,
    };

    let Some(cleanup_error) = cleanup_error else {
        return reason;
    };
    match &mut reason {
        SocketEnd::Failed(failure) => failure.cleanup = Some(cleanup_error),
        SocketEnd::Shutdown | SocketEnd::ClientClosed(_) | SocketEnd::Disconnected => warn!(
            socket_id = socket_id.get(),
            reason = %format_args!("{cleanup_error:#}"),
            "socket cleanup failed"
        ),
    }
    reason
}

#[expect(
    clippy::too_many_lines,
    reason = "one exhaustive match keeps the complete browser message protocol visible"
)]
async fn handle_text(
    outbound: &Outbound,
    state: &AppState,
    socket_id: SocketId,
    lease: &mut Lease,
    text: &str,
) -> Result<(), SocketEnd> {
    let message = ClientMessage::parse_json(text.as_bytes()).map_err(|error| {
        SocketEnd::failed(anyhow::Error::new(error).context("browser control parse failed"))
    })?;
    match message {
        ClientMessage::AcquireControl => {
            let control_state = match state
                .sessions
                .acquire(socket_id)
                .await
                .map_err(|error| SocketEnd::failed(error.context("acquire input lease")))?
            {
                LeaseState::Active => {
                    *lease = Lease::Held;
                    ControlState::Active
                }
                LeaseState::Busy => {
                    *lease = Lease::NotHeld;
                    ControlState::Busy
                }
            };
            outbound
                .enqueue_event(&ClientEvent::ControlState {
                    state: control_state,
                })
                .map_err(SocketEnd::from)?;
            if control_state == ControlState::Active
                && let Some(text) = state.events.latest_clipboard()
            {
                outbound
                    .enqueue_event(&ClientEvent::Clipboard { text })
                    .map_err(SocketEnd::from)?;
            }
            Ok(())
        }
        ClientMessage::ReleaseControl => {
            state
                .sessions
                .release(socket_id)
                .await
                .map_err(|error| SocketEnd::failed(error.context("release input lease")))?;
            *lease = Lease::NotHeld;
            outbound
                .enqueue_event(&ClientEvent::ControlState {
                    state: ControlState::Ready,
                })
                .map_err(SocketEnd::from)
        }
        ClientMessage::Ping { id: request_id } => {
            let time = clock_gettime(ClockId::CLOCK_MONOTONIC).map_err(|error| {
                SocketEnd::failed(anyhow::Error::new(error).context("read monotonic clock"))
            })?;
            let nanos = i128::from(time.tv_sec()) * 1_000_000_000 + i128::from(time.tv_nsec());
            outbound
                .enqueue_event(&ClientEvent::Pong {
                    id: request_id,
                    server_nanos: nanos.to_string(),
                })
                .map_err(SocketEnd::from)
        }
        ClientMessage::Feedback(feedback) => {
            let outcome = state
                .sessions
                .feedback(socket_id, feedback)
                .await
                .map_err(|error| SocketEnd::failed(error.context("apply quality feedback")))?;
            match outcome {
                FeedbackOutcome::Applied(levels) => outbound
                    .enqueue_event(&ClientEvent::Quality(levels))
                    .map_err(SocketEnd::from),
                FeedbackOutcome::NotLeaseOwner => Ok(()),
            }
        }
        ClientMessage::Text {
            action,
            text,
            sequence,
        } => match *lease {
            Lease::NotHeld => Ok(()),
            Lease::Held => {
                let outcome = state
                    .sessions
                    .input(
                        socket_id,
                        Command::Text(TextCommand {
                            action,
                            sequence,
                            text,
                        }),
                    )
                    .await
                    .map_err(|error| SocketEnd::failed(error.context("send text input")))?;
                match outcome {
                    InputOutcome::Sent => Ok(()),
                    InputOutcome::NotLeaseOwner => {
                        *lease = Lease::NotHeld;
                        warn!(
                            socket_id = socket_id.get(),
                            "control socket input lease disagreed with session owner"
                        );
                        Ok(())
                    }
                }
            }
        },
        ClientMessage::ClipboardWrite { text } => match *lease {
            Lease::NotHeld => Ok(()),
            Lease::Held => {
                let outcome = state
                    .sessions
                    .input(socket_id, Command::Clipboard(text))
                    .await
                    .map_err(|error| SocketEnd::failed(error.context("send clipboard input")))?;
                match outcome {
                    InputOutcome::Sent => Ok(()),
                    InputOutcome::NotLeaseOwner => {
                        *lease = Lease::NotHeld;
                        warn!(
                            socket_id = socket_id.get(),
                            "control socket input lease disagreed with session owner"
                        );
                        Ok(())
                    }
                }
            }
        },
        ClientMessage::ResetVideo => match *lease {
            Lease::NotHeld => Ok(()),
            Lease::Held => {
                let outcome = state
                    .sessions
                    .input(socket_id, Command::ResetVideo(ResetVideo))
                    .await
                    .map_err(|error| SocketEnd::failed(error.context("reset video")))?;
                match outcome {
                    InputOutcome::Sent => Ok(()),
                    InputOutcome::NotLeaseOwner => {
                        *lease = Lease::NotHeld;
                        warn!(
                            socket_id = socket_id.get(),
                            "control socket input lease disagreed with session owner"
                        );
                        Ok(())
                    }
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    use tokio::time::timeout;
    use waywire_protocol::browser::TextAction;
    use waywire_protocol::pipe::ClipboardText;
    use waywire_protocol::pipe::Fps;
    use waywire_protocol::pipe::FrameSize;
    use waywire_protocol::pipe::Generation;
    use waywire_protocol::pipe::InputSequence;
    use waywire_protocol::pipe::InputText;
    use waywire_protocol::pipe::Kbps;
    use waywire_protocol::pipe::RequestId;
    use waywire_protocol::pipe::ResizeApplied;
    use waywire_protocol::pipe::ScaleV120;

    use super::*;
    use crate::daemon::AppEvents;
    use crate::daemon::TestCommandReceiver;
    use crate::daemon::test_command_sink;
    use crate::http::AppState;
    use crate::http::Origin;
    use crate::http::SocketConnections;
    use crate::http::outbound::OutboundItem;
    use crate::session::Sessions;
    use crate::video::Readiness;
    use crate::video::VideoHub;
    fn test_state() -> (AppState, TestCommandReceiver) {
        let (commands, receiver) = test_command_sink();
        let sessions = Sessions::new(
            commands,
            Kbps::new(8_000).expect("test bitrate should be valid"),
            Fps::new(60).expect("test frame rate should be valid"),
        );
        (
            AppState {
                readiness: Readiness::new(),
                events: AppEvents::new(),
                sessions,
                hub: VideoHub::new(Fps::new(60).expect("test frame rate should be valid")),
                origin: Origin::parse("https://x.test").expect("test origin should parse"),
                connections: SocketConnections::new(),
                next_socket_id: Arc::new(AtomicU64::new(1)),
            },
            receiver,
        )
    }
    fn outbound() -> (Outbound, mpsc::Receiver<OutboundItem>) {
        let (tx, rx) = mpsc::channel(16);
        (
            Outbound {
                tx,
                bytes: Arc::new(Semaphore::new(OUTBOUND_BYTE_LIMIT)),
            },
            rx,
        )
    }
    async fn next_text(receiver: &mut mpsc::Receiver<OutboundItem>) -> String {
        let item = receiver
            .recv()
            .await
            .expect("client event should be enqueued");
        match item.message {
            Message::Text(text) => text.to_string(),
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) | Message::Close(_) => {
                panic!("client event should be text")
            }
        }
    }
    async fn acquire(state: &AppState, commands: &mut TestCommandReceiver, socket: SocketId) {
        assert_eq!(
            state
                .sessions
                .acquire(socket)
                .await
                .expect("lease should acquire"),
            LeaseState::Active
        );
        assert!(matches!(
            commands.recv().await,
            Some(Command::ReleaseAll(_))
        ));
    }
    #[tokio::test]
    async fn acquire_message_enqueues_active_state_and_reset_command() {
        let (state, mut commands) = test_state();
        let (outbound, mut events) = outbound();
        let mut lease = Lease::NotHeld;

        handle_text(
            &outbound,
            &state,
            SocketId::new(1),
            &mut lease,
            r#"{"type":"acquire"}"#,
        )
        .await
        .expect("acquire message should succeed");

        assert_eq!(lease, Lease::Held);
        assert_eq!(
            next_text(&mut events).await,
            serde_json::to_string(&ClientEvent::ControlState {
                state: ControlState::Active,
            })
            .expect("expected event should serialize")
        );
        assert!(matches!(
            commands.recv().await,
            Some(Command::ReleaseAll(_))
        ));
    }
    #[tokio::test]
    async fn release_message_enqueues_ready_state_and_reset_command() {
        let (state, mut commands) = test_state();
        let socket = SocketId::new(1);
        acquire(&state, &mut commands, socket).await;
        let (outbound, mut events) = outbound();
        let mut lease = Lease::Held;

        handle_text(
            &outbound,
            &state,
            socket,
            &mut lease,
            r#"{"type":"release"}"#,
        )
        .await
        .expect("release message should succeed");

        assert_eq!(lease, Lease::NotHeld);
        assert_eq!(
            next_text(&mut events).await,
            serde_json::to_string(&ClientEvent::ControlState {
                state: ControlState::Ready,
            })
            .expect("expected event should serialize")
        );
        assert!(matches!(
            commands.recv().await,
            Some(Command::ReleaseAll(_))
        ));
    }
    #[tokio::test]
    async fn ping_message_enqueues_pong_without_a_command() {
        let (state, mut commands) = test_state();
        let (outbound, mut events) = outbound();
        let mut lease = Lease::NotHeld;

        handle_text(
            &outbound,
            &state,
            SocketId::new(1),
            &mut lease,
            r#"{"type":"ping","id":7}"#,
        )
        .await
        .expect("ping message should succeed");

        assert!(next_text(&mut events).await.contains(r#""type":"pong""#));
        assert!(
            timeout(Duration::from_millis(20), commands.recv())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn feedback_message_enqueues_quality_and_changed_quality_command() {
        let (state, mut commands) = test_state();
        let socket = SocketId::new(1);
        acquire(&state, &mut commands, socket).await;
        let (outbound, mut events) = outbound();
        let mut lease = Lease::Held;
        let message = r#"{"type":"feedback","received":50,"presented":50,"queuePeak":5,"queueBusyMs":200.0,"sampleMs":1000.0,"dropped":0,"rtt":20.0}"#;

        handle_text(&outbound, &state, socket, &mut lease, message)
            .await
            .expect("first feedback should succeed");
        let _ = next_text(&mut events).await;
        handle_text(&outbound, &state, socket, &mut lease, message)
            .await
            .expect("second feedback should succeed");

        assert!(next_text(&mut events).await.contains(r#""type":"quality""#));
        assert!(matches!(commands.recv().await, Some(Command::Quality(_))));
    }
    #[tokio::test]
    async fn text_message_sends_text_command_without_an_event() {
        let (state, mut commands) = test_state();
        let socket = SocketId::new(1);
        acquire(&state, &mut commands, socket).await;
        let (outbound, mut events) = outbound();
        let mut lease = Lease::Held;

        handle_text(
            &outbound,
            &state,
            socket,
            &mut lease,
            r#"{"type":"text","action":"commit","text":"hi","sequence":3}"#,
        )
        .await
        .expect("text message should succeed");

        assert_eq!(
            commands.recv().await,
            Some(Command::Text(TextCommand {
                action: TextAction::Commit,
                text: InputText::new("hi".to_owned()).expect("test text should be valid"),
                sequence: InputSequence::new(3).expect("test sequence should be valid"),
            }))
        );
        assert!(
            timeout(Duration::from_millis(20), events.recv())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn clipboard_message_sends_clipboard_command_without_an_event() {
        let (state, mut commands) = test_state();
        let socket = SocketId::new(1);
        acquire(&state, &mut commands, socket).await;
        let (outbound, mut events) = outbound();
        let mut lease = Lease::Held;

        handle_text(
            &outbound,
            &state,
            socket,
            &mut lease,
            r#"{"type":"clipboard-write","text":"copy"}"#,
        )
        .await
        .expect("clipboard message should succeed");

        assert_eq!(
            commands.recv().await,
            Some(Command::Clipboard(
                ClipboardText::new("copy".to_owned()).expect("test clipboard should be valid")
            ))
        );
        assert!(
            timeout(Duration::from_millis(20), events.recv())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn reset_video_message_uses_the_input_lease() {
        let (state, mut commands) = test_state();
        let socket = SocketId::new(1);
        acquire(&state, &mut commands, socket).await;
        let (outbound, _events) = outbound();
        let mut lease = Lease::Held;

        handle_text(
            &outbound,
            &state,
            socket,
            &mut lease,
            r#"{"type":"reset-video"}"#,
        )
        .await
        .expect("reset video message should succeed");

        assert_eq!(commands.recv().await, Some(Command::ResetVideo(ResetVideo)));
    }

    #[tokio::test]
    async fn nonowner_reset_video_message_sends_no_command() {
        let (state, mut commands) = test_state();
        acquire(&state, &mut commands, SocketId::new(1)).await;
        let (outbound, _events) = outbound();
        let mut lease = Lease::NotHeld;

        handle_text(
            &outbound,
            &state,
            SocketId::new(2),
            &mut lease,
            r#"{"type":"reset-video"}"#,
        )
        .await
        .expect("nonowner reset video should be ignored");

        assert!(
            timeout(Duration::from_millis(20), commands.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn lagged_events_enqueue_a_snapshot_and_deliver_the_retained_event() {
        let (state, _commands) = test_state();
        let (outbound, mut events) = outbound();
        let (_snapshot, mut receiver) = state.events.subscribe();
        let retained = ClientEvent::ResizeApplied(ResizeApplied {
            request_id: RequestId::new(7).expect("test request ID should be valid"),
            size: FrameSize::new(1280, 720).expect("test frame size should be valid"),
            scale_v120: ScaleV120::new(120).expect("test scale should be valid"),
            generation: Generation::new(2).expect("test generation should be valid"),
        });

        state.events.publish_for_test(ClientEvent::ControlState {
            state: ControlState::Busy,
        });
        state.events.publish_for_test(retained.clone());
        for id in 0..63 {
            state.events.publish_for_test(ClientEvent::Pong {
                id,
                server_nanos: "0".to_owned(),
            });
        }

        let lagged = receiver.recv().await;
        assert!(matches!(
            lagged,
            Err(broadcast::error::RecvError::Lagged(1))
        ));
        dispatch_control_event(&outbound, &state, Lease::NotHeld, lagged)
            .expect("lagged event should resynchronise");
        assert!(next_text(&mut events).await.contains(r#""type":"cursor""#));
        assert!(next_text(&mut events).await.contains(r#""type":"quality""#));

        let next = receiver.recv().await;
        assert_eq!(next, Ok(retained));
        dispatch_control_event(&outbound, &state, Lease::NotHeld, next)
            .expect("event loop should accept the retained event");
        assert!(
            next_text(&mut events)
                .await
                .contains(r#""type":"resize-applied""#)
        );
    }
    #[test]
    fn combined_cleanup_error_renders_release_and_writer_failures() {
        let error = anyhow::Error::new(CleanupFailure::Both {
            release: anyhow!("release failed"),
            writer: anyhow!("writer failed"),
        });
        let rendered = format!("{error:#}");

        assert!(rendered.contains("release failed"));
        assert!(rendered.contains("writer failed"));
    }
}
