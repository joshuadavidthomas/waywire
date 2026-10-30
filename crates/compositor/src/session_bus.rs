//! Private D-Bus session bus owned by the compositor.
//!
//! Portal backends and the xdg-desktop-portal frontend must share one bus, and
//! the compositor must not rewrite an inherited desktop session's activation
//! environment. Each compositor therefore starts its own `dbus-daemon` with a
//! socket in its runtime directory and points its portal clients at it.

use std::io::BufRead;
use std::io::BufReader;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;

use nix::sys::signal::SigSet;
use nix::sys::signal::SigmaskHow;
use nix::sys::signal::pthread_sigmask;

/// A `dbus-daemon` child and the address it published.
pub(crate) struct SessionBus {
    child: Option<Child>,
    address: String,
}

impl SessionBus {
    /// Start `dbus-daemon` with its socket under `runtime_path`, returning the
    /// address it published. Portals are optional, so a daemon that cannot
    /// start only disables them.
    pub(crate) fn start(runtime_path: &Path) -> Option<Self> {
        let socket = runtime_path.join("bus");
        let expected_address = format!("unix:path={}", socket.display());
        let mut command = Command::new("dbus-daemon");
        command
            .args(["--session", "--nofork", "--print-address"])
            .arg(format!("--address={expected_address}"))
            // Services the bus activates inherit this environment. Keep them
            // off a host bus even before the portal thread updates it.
            .env("DBUS_SESSION_BUS_ADDRESS", &expected_address)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // SAFETY: pthread_sigmask is async-signal-safe, and the closure only
        // clears the mask inherited from the calloop signal source.
        unsafe {
            command.pre_exec(|| {
                pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)
                    .map_err(std::io::Error::from)
            });
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(%error, "could not start dbus-daemon; portal backends disabled");
                return None;
            }
        };
        let Some(stdout) = child.stdout.take() else {
            tracing::warn!("dbus-daemon printed no address; portal backends disabled");
            reap(&mut child);
            return None;
        };
        let mut address = String::new();
        if let Err(error) = BufReader::new(stdout).read_line(&mut address) {
            tracing::warn!(%error, "could not read dbus-daemon address; portal backends disabled");
            reap(&mut child);
            return None;
        }
        let address = address.trim();
        if address.is_empty() {
            tracing::warn!("dbus-daemon published no address; portal backends disabled");
            reap(&mut child);
            return None;
        }
        tracing::info!(address, "private session bus ready");
        Some(Self {
            child: Some(child),
            address: address.to_owned(),
        })
    }

    /// The daemon's PID while it is running.
    pub(crate) fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Address portal clients and activated services must connect to.
    pub(crate) fn address(&self) -> &str {
        &self.address
    }

    /// Log a daemon that exited on its own; the desktop keeps running.
    pub(crate) fn poll(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::warn!(%status, "dbus-daemon exited; portal backends unavailable");
                self.child = None;
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "could not inspect dbus-daemon"),
        }
    }
}

impl Drop for SessionBus {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Err(error) = child.kill() {
            tracing::warn!(pid = child.id(), %error, "could not stop dbus-daemon");
        }
        if let Err(error) = child.wait() {
            tracing::warn!(pid = child.id(), %error, "could not reap dbus-daemon");
        }
    }
}

fn reap(child: &mut Child) {
    if let Err(error) = child.kill() {
        tracing::warn!(pid = child.id(), %error, "could not stop dbus-daemon");
    }
    if let Err(error) = child.wait() {
        tracing::warn!(pid = child.id(), %error, "could not reap dbus-daemon");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_bus_serves_the_address_it_publishes() {
        let runtime = tempfile::tempdir().expect("temporary runtime directory");
        let Some(bus) = SessionBus::start(runtime.path()) else {
            // The runtime handles a missing daemon; so does the test.
            return;
        };
        let connection = zbus::blocking::connection::Builder::address(bus.address())
            .expect("published address parses")
            .build()
            .expect("connect to the private bus");
        assert!(connection.unique_name().is_some());
    }
}
