use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use nix::errno::Errno;
use nix::sys::signal::Signal;
use nix::sys::signal::kill;
use nix::sys::wait::Id;
use nix::sys::wait::WaitPidFlag;
use nix::sys::wait::WaitStatus;
use nix::sys::wait::waitid;
use nix::unistd::Pid;
use tokio::process::Child;
use tokio::process::Command as ProcessCommand;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;
use tracing::info;

use super::Config;
use super::SpawnedDaemon;

const GROUP_EXIT_DEADLINE: Duration = Duration::from_secs(2);

pub(super) fn spawn_daemon(config: &Config, rtp_port: u16) -> Result<SpawnedDaemon> {
    let mut command = ProcessCommand::new(&config.path);
    command
        .args(daemon_args(config, rtp_port))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn private daemon {}", config.path.display()))?;
    let raw_pid = i32::try_from(
        child
            .id()
            .ok_or_else(|| anyhow!("daemon has no process ID"))?,
    )
    .context("daemon process ID exceeds i32")?;
    let pid = Pid::from_raw(raw_pid);
    let stdin = child.stdin.take().context("open daemon stdin")?;
    let stdout = child.stdout.take().context("open daemon stdout")?;
    info!(%pid, path = %config.path.display(), "daemon spawned");
    Ok(SpawnedDaemon {
        child,
        pid,
        stdin,
        stdout,
    })
}

fn daemon_args(config: &Config, rtp_port: u16) -> Vec<String> {
    vec![
        "--frame-rate".into(),
        config.frame_rate.get().to_string(),
        "--bitrate".into(),
        config.bitrate.get().to_string(),
        "--rtp-port".into(),
        rtp_port.to_string(),
        "--xkb-layout".into(),
        config.xkb_layout.as_str().into(),
        "--resolution".into(),
        format!(
            "{}x{}",
            config.resolution.width(),
            config.resolution.height()
        ),
    ]
}

pub(super) async fn wait_for_leader_exit(pid: Pid) -> Result<WaitStatus> {
    loop {
        match waitid(
            Id::Pid(pid),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOWAIT | WaitPidFlag::WNOHANG,
        )? {
            WaitStatus::StillAlive => sleep(Duration::from_millis(25)).await,
            status @ (WaitStatus::Exited(..)
            | WaitStatus::Signaled(..)
            | WaitStatus::Stopped(..)
            | WaitStatus::PtraceEvent(..)
            | WaitStatus::PtraceSyscall(_)
            | WaitStatus::Continued(_)) => return Ok(status),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcessGroupState {
    Alive,
    Gone,
}

fn process_group_state(result: std::result::Result<(), Errno>) -> Result<ProcessGroupState> {
    match result {
        Ok(()) => Ok(ProcessGroupState::Alive),
        Err(Errno::ESRCH) => Ok(ProcessGroupState::Gone),
        Err(error) => Err(error.into()),
    }
}

pub(super) async fn cleanup_group(child: &mut Child, pid: Pid) -> Result<()> {
    let group = Pid::from_raw(-pid.as_raw());
    let state = process_group_state(kill(group, Signal::SIGTERM))
        .context("send SIGTERM to daemon process group")?;
    if state == ProcessGroupState::Alive {
        let deadline = Instant::now() + GROUP_EXIT_DEADLINE;
        loop {
            match process_group_state(kill(group, None))
                .context("probe daemon process group after SIGTERM")?
            {
                ProcessGroupState::Gone => break,
                ProcessGroupState::Alive if Instant::now() < deadline => {
                    sleep(Duration::from_millis(25)).await;
                }
                ProcessGroupState::Alive => {
                    let _ = process_group_state(kill(group, Signal::SIGKILL))
                        .context("send SIGKILL to daemon process group")?;
                    break;
                }
            }
        }
    }
    timeout(GROUP_EXIT_DEADLINE, child.wait())
        .await
        .context("daemon leader could not be reaped")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use nix::errno::Errno;
    use waywire_protocol::pipe::Fps;
    use waywire_protocol::pipe::FrameSize;
    use waywire_protocol::pipe::Kbps;

    use super::ProcessGroupState;
    use super::daemon_args;
    use super::process_group_state;
    use crate::daemon::Config;
    use crate::daemon::XkbLayout;

    #[test]
    fn streamd_arguments_include_the_configured_resolution() {
        let config = Config {
            path: PathBuf::from("streamd"),
            frame_rate: Fps::new(60).expect("valid frame rate"),
            bitrate: Kbps::new(16_000).expect("valid bitrate"),
            xkb_layout: XkbLayout::parse("us").expect("valid layout"),
            resolution: FrameSize::new(2560, 1440).expect("valid resolution"),
        };

        assert_eq!(
            daemon_args(&config, 5000),
            [
                "--frame-rate",
                "60",
                "--bitrate",
                "16000",
                "--rtp-port",
                "5000",
                "--xkb-layout",
                "us",
                "--resolution",
                "2560x1440",
            ]
        );
    }

    #[test]
    fn only_esrch_means_a_process_group_is_gone() {
        assert_eq!(
            process_group_state(Err(Errno::ESRCH)).expect("ESRCH should mean the group is gone"),
            ProcessGroupState::Gone
        );
        assert_eq!(
            process_group_state(Ok(())).expect("a successful probe should mean the group is alive"),
            ProcessGroupState::Alive
        );
        let error = process_group_state(Err(Errno::EPERM))
            .expect_err("EPERM must remain a process-group cleanup error");
        assert_eq!(error.downcast_ref::<Errno>(), Some(&Errno::EPERM));
    }
}
