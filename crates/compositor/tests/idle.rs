use std::fs;
use std::io::Read;
use std::io::Write;
use std::io::{
    self,
};
use std::process::Child;
use std::process::Command as ProcessCommand;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use nix::fcntl::FcntlArg;
use nix::fcntl::OFlag;
use nix::fcntl::fcntl;
use waywire_protocol::Decoder;
use waywire_protocol::Record;
use waywire_protocol::pipe::Command;
use waywire_protocol::pipe::Event;
use waywire_protocol::pipe::KeyframeReadiness;
use waywire_protocol::pipe::KeyframeState;

struct Compositor(Child);

impl Drop for Compositor {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

#[expect(
    clippy::expect_used,
    reason = "test helper requires a live compositor pipe"
)]
fn events(child: &mut Child, decoder: &mut Decoder<Event>) -> Vec<Event> {
    let mut bytes = [0; 4096];
    let mut result = Vec::new();
    loop {
        match child.stdout.as_mut().expect("event pipe").read(&mut bytes) {
            Ok(0) => panic!("compositor event pipe closed"),
            Ok(n) => result.extend(decoder.push(&bytes[..n]).expect("decode events")),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return result,
            Err(error) => panic!("read events: {error}"),
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "test helper requires Linux process status"
)]
fn switches(pid: u32) -> u64 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).expect("compositor status");
    status
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name == "voluntary_ctxt_switches" || name == "nonvoluntary_ctxt_switches")
                .then(|| value.trim().parse::<u64>().expect("context switches"))
        })
        .sum()
}

#[test]
#[ignore = "requires FFmpeg and a working Xwayland on PATH"]
fn idle_compositor_stops_rendering_and_emitting_frames() {
    let runtime = tempfile::tempdir().expect("runtime directory");
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("RTP socket");
    let port = socket.local_addr().expect("RTP address").port().to_string();
    let mut compositor = Compositor(
        ProcessCommand::new(env!("CARGO_BIN_EXE_waywire-compositor"))
            .args([
                "--rtp-port",
                &port,
                "--resolution",
                "1280x720",
                "--frame-rate",
                "30",
            ])
            .env("XDG_RUNTIME_DIR", runtime.path())
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start compositor"),
    );
    let stdout = compositor.0.stdout.as_ref().expect("event pipe");
    let flags = OFlag::from_bits_truncate(fcntl(stdout, FcntlArg::F_GETFL).expect("get flags"));
    fcntl(stdout, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).expect("nonblocking events");
    let mut decoder = Decoder::<Event>::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(generation) = events(&mut compositor.0, &mut decoder)
            .into_iter()
            .find_map(|event| {
                if let Event::Frame(frame) = event {
                    Some(frame.generation)
                } else {
                    None
                }
            })
        {
            let ack = Command::KeyframeReadiness(KeyframeReadiness {
                generation,
                state: KeyframeState::Cached,
            });
            compositor
                .0
                .stdin
                .as_mut()
                .expect("command pipe")
                .write_all(&ack.encode())
                .expect("acknowledge keyframe");
            break;
        }
        assert!(Instant::now() < deadline, "first frame deadline");
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_secs(2));
    events(&mut compositor.0, &mut decoder);
    let before = switches(compositor.0.id());
    thread::sleep(Duration::from_secs(2));
    let wakeups = switches(compositor.0.id()) - before;
    assert!(
        wakeups < 25,
        "idle compositor woke {wakeups} times in two seconds"
    );
    assert!(
        events(&mut compositor.0, &mut decoder)
            .iter()
            .all(|event| !matches!(event, Event::Frame(_))),
        "idle compositor emitted a frame"
    );
}
