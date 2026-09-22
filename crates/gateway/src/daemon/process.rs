use std::ffi::OsString;
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

fn daemon_args(config: &Config, rtp_port: u16) -> Vec<OsString> {
    let mut args = vec![
        "--frame-rate".into(),
        config.frame_rate.get().to_string().into(),
        "--bitrate".into(),
        config.bitrate.get().to_string().into(),
        "--rtp-port".into(),
        rtp_port.to_string().into(),
        "--xkb-layout".into(),
        config.xkb_layout.as_str().into(),
        "--resolution".into(),
        format!(
            "{}x{}",
            config.resolution.width(),
            config.resolution.height()
        )
        .into(),
    ];
    if !config.session.is_empty() {
        args.push("--".into());
        args.extend(config.session.iter().cloned());
    }
    args
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
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::time::Duration;

    use nix::errno::Errno;
    use nix::sys::wait::WaitStatus;
    use nix::unistd::Pid;
    use tokio::io::AsyncBufReadExt;
    use tokio::io::BufReader;
    use tokio::process::Command;
    use tokio::time::Instant;
    use tokio::time::sleep;
    use tokio::time::timeout;
    use waywire_protocol::pipe::Fps;
    use waywire_protocol::pipe::FrameSize;
    use waywire_protocol::pipe::Kbps;

    use super::ProcessGroupState;
    use super::cleanup_group;
    use super::daemon_args;
    use super::process_group_state;
    use super::wait_for_leader_exit;
    use crate::daemon::Config;
    use crate::daemon::XkbLayout;

    #[test]
    fn compositor_arguments_preserve_resolution_and_session_argv() {
        let mut config = Config {
            path: PathBuf::from("waywire-compositor"),
            frame_rate: Fps::new(60).expect("valid frame rate"),
            bitrate: Kbps::new(16_000).expect("valid bitrate"),
            xkb_layout: XkbLayout::parse("us").expect("valid layout"),
            resolution: FrameSize::new(2560, 1440).expect("valid resolution"),
            session: Vec::new(),
        };

        let mut expected = vec![
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
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
        assert_eq!(daemon_args(&config, 5000), expected);

        config.session = vec![
            "foot".into(),
            "--title=A desktop terminal".into(),
            OsString::from_vec(b"non-UTF8-\xff".to_vec()),
        ];
        expected.extend([
            OsString::from("--"),
            OsString::from("foot"),
            OsString::from("--title=A desktop terminal"),
            OsString::from_vec(b"non-UTF8-\xff".to_vec()),
        ]);
        assert_eq!(daemon_args(&config, 5000), expected);
    }

    #[tokio::test]
    async fn exited_compositor_keeps_descendants_owned_until_cleanup() {
        // The child inherits ignored SIGTERM and survives its leader. Reaping
        // the leader early or signalling only its PID would leave this running.
        let mut leader = Command::new("sh")
            .args(["-c", "trap '' TERM; sleep 60 & echo $!; exit 0"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn process group");
        let pid = Pid::from_raw(i32::try_from(leader.id().expect("leader PID")).expect("PID fits"));
        let mut output = BufReader::new(leader.stdout.take().expect("leader stdout"));
        let mut line = String::new();
        timeout(Duration::from_secs(5), output.read_line(&mut line))
            .await
            .expect("child PID deadline")
            .expect("read child PID");
        let child: u32 = line.trim().parse().expect("child PID");
        assert_eq!(
            timeout(Duration::from_secs(5), wait_for_leader_exit(pid))
                .await
                .expect("leader exit deadline")
                .expect("observe leader"),
            WaitStatus::Exited(pid, 0)
        );
        cleanup_group(&mut leader, pid)
            .await
            .expect("clean up exited leader's group");
        assert_eq!(
            leader
                .try_wait()
                .expect("leader wait")
                .expect("leader reaped")
                .code(),
            Some(0)
        );

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match std::fs::read_to_string(format!("/proc/{child}/stat")) {
                Ok(stat) => {
                    let state = stat.rsplit_once(')').expect("process stat").1.trim_start();
                    if state.starts_with('Z') {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "application survived group cleanup"
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => panic!("read child process state: {error}"),
            }
            sleep(Duration::from_millis(10)).await;
        }
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
