//! xdg-desktop-portal backends, served on the compositor's private session bus
//! so packaged agent drivers work on distributions whose Xwayland lacks libei
//! (Debian 13, Ubuntu 24.04).
//!
//! zbus serves the backends on the private bus as
//! `org.freedesktop.impl.portal.desktop.waywire`, implementing `RemoteDesktop`
//! (with `ConnectToEIS`) and Screenshot; method calls that need the seat or the
//! rendered frame travel over a channel into the calloop loop and are answered
//! when the loop gets to them.
//!
//! Discovery: the frontend finds backends through `*.portal` files in
//! `xdg-desktop-portal/portals` data directories and selects them through
//! `<desktop>-portals.conf` matched against `XDG_CURRENT_DESKTOP`. At startup
//! this module writes both files under the runtime directory, prepends that
//! directory to `XDG_DATA_DIRS`, and sets `XDG_DESKTOP_PORTAL_DIR` to it:
//! frontends through 1.18 look up `.portal` files only in that single
//! variable's directory, so it also carries symlinks to the portals already
//! installed on the system. Those variables and the private bus address are
//! published with `org.freedesktop.DBus.UpdateActivationEnvironment` and
//! exported to the session program.

mod remote_desktop;
mod screenshot;

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::Result;
use calloop::LoopHandle;
use calloop::channel::Event;
use calloop::channel::Sender;
use smithay::reexports::pixman::Image;
use tokio::sync::oneshot;
use zbus::blocking::Connection;
use zbus::zvariant::ObjectPath;

use crate::State;
use crate::portal::remote_desktop::RemoteDesktop;
use crate::portal::screenshot::Access;
use crate::portal::screenshot::Screenshot;

/// Bus name this backend owns, the convention for desktop-specific backends.
pub(crate) const PORTAL_NAME: &str = "org.freedesktop.impl.portal.desktop.waywire";

/// Path the backend interfaces are served on, matching other backends.
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

/// Interfaces waywire implements, in the `.portal` file's semicolon list form.
const PORTAL_INTERFACES: &str = "org.freedesktop.impl.portal.RemoteDesktop;org.freedesktop.impl.portal.Screenshot;org.freedesktop.impl.portal.Access;";

/// Requests the D-Bus thread routes into the calloop loop, which owns the
/// seat, the keyboard layout, and the rendered frame.
pub(crate) enum PortalRequest {
    /// Hand out a new EI connection for a `ConnectToEIS` call.
    ConnectToEis {
        reply: oneshot::Sender<std::io::Result<(OwnedFd, std::os::unix::net::UnixStream)>>,
    },
    /// Copy the rendered frame into a PNG file; the reply is its path.
    Screenshot {
        reply: oneshot::Sender<anyhow::Result<PathBuf>>,
    },
}

/// Directories the compositor publishes for portal discovery.
#[derive(Clone)]
pub(crate) struct PortalDirs {
    /// Prepended to `XDG_DATA_DIRS`.
    pub data_dir: PathBuf,
    /// Set as `XDG_DESKTOP_PORTAL_DIR`.
    pub portals_dir: PathBuf,
}

/// Contents of `waywire.portal`, which describes this backend to the frontend.
fn portal_file_contents() -> String {
    format!(
        "[portal]\n\
         DBusName={PORTAL_NAME}\n\
         Interfaces={PORTAL_INTERFACES}\n\
         UseIn=waywire;\n"
    )
}

/// Contents of `waywire-portals.conf`. Interfaces waywire implements route here
/// unconditionally; everything else keeps the desktop's packaged backend, if
/// any, or none.
fn portals_conf_contents(default_portal: Option<&str>) -> String {
    let default_portal = default_portal.unwrap_or("none");
    format!(
        "[preferred]\n\
         default={default_portal}\n\
         org.freedesktop.impl.portal.RemoteDesktop=waywire\n\
         org.freedesktop.impl.portal.Screenshot=waywire\n\
         org.freedesktop.impl.portal.Access=waywire\n"
    )
}

/// Standard system data directories per the XDG base directory specification.
fn data_dirs() -> Vec<PathBuf> {
    data_dirs_from(std::env::var_os("XDG_DATA_DIRS").as_deref())
}

/// Data directories from an `XDG_DATA_DIRS` value; empty or absent means the
/// default pair.
fn data_dirs_from(dirs: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let Some(dirs) = dirs else {
        return ["/usr/local/share".into(), "/usr/share".into()].into();
    };
    let dirs = dirs.to_string_lossy();
    if dirs.is_empty() {
        return ["/usr/local/share".into(), "/usr/share".into()].into();
    }
    dirs.split(':')
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Name of the packaged backend to keep for interfaces waywire does not
/// implement, looked up in the usual portal directories.
fn default_portal() -> Option<&'static str> {
    for dir in data_dirs() {
        if dir.join("xdg-desktop-portal/portals/gtk.portal").exists() {
            return Some("gtk");
        }
    }
    None
}

/// Write `waywire.portal` and `waywire-portals.conf` under the runtime
/// directory and link the system's packaged portal files in, returning the
/// directories to publish in the environment.
pub(crate) fn write_portal_files(runtime_path: &Path) -> Result<PortalDirs> {
    let data_dir = runtime_path.join("share");
    let portals_dir = data_dir.join("xdg-desktop-portal/portals");
    std::fs::create_dir_all(&portals_dir)?;
    std::fs::write(portals_dir.join("waywire.portal"), portal_file_contents())?;
    std::fs::write(
        portals_dir.join("waywire-portals.conf"),
        portals_conf_contents(default_portal()),
    )?;
    for dir in data_dirs() {
        let Ok(entries) = std::fs::read_dir(dir.join("xdg-desktop-portal/portals")) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".portal") || name == "waywire.portal" {
                continue;
            }
            let link = portals_dir.join(&*name);
            if !link.exists()
                && let Err(error) = std::os::unix::fs::symlink(entry.path(), &link)
            {
                tracing::debug!(%error, link = %link.display(), "link packaged portal file");
            }
        }
    }
    Ok(PortalDirs {
        data_dir,
        portals_dir,
    })
}

/// Environment a portal-using program needs on this desktop, in activation
/// environment form.
pub(crate) fn portal_environment(dirs: &PortalDirs, bus_address: &str) -> Vec<(String, String)> {
    portal_environment_from(
        dirs,
        std::env::var_os("XDG_DATA_DIRS").as_deref(),
        bus_address,
    )
}

/// The environment from an `XDG_DATA_DIRS` value the compositor runs with.
#[expect(
    clippy::expect_used,
    reason = "runtime directory paths never contain path-list separators"
)]
fn portal_environment_from(
    dirs: &PortalDirs,
    value: Option<&std::ffi::OsStr>,
    bus_address: &str,
) -> Vec<(String, String)> {
    let mut data_dirs = data_dirs_from(value);
    data_dirs.insert(0, dirs.data_dir.clone());
    let joined = std::env::join_paths(data_dirs.iter())
        .expect("data directory paths never contain separators")
        .to_string_lossy()
        .into_owned();
    vec![
        (
            "DBUS_SESSION_BUS_ADDRESS".to_owned(),
            bus_address.to_owned(),
        ),
        ("XDG_DATA_DIRS".to_owned(), joined),
        ("XDG_CURRENT_DESKTOP".to_owned(), "waywire".to_owned()),
        (
            "XDG_DESKTOP_PORTAL_DIR".to_owned(),
            dirs.portals_dir.to_string_lossy().into_owned(),
        ),
    ]
}

/// Serve the portal backends on `bus_address` for the lifetime of the
/// compositor. Returns the published directories, or `None` when the backends
/// could not be started; the desktop itself still runs.
pub(crate) fn start(
    handle: &LoopHandle<'static, State>,
    runtime_path: &Path,
    bus_address: &str,
    image: Rc<RefCell<Image<'static, 'static>>>,
    layout: String,
) -> Result<Option<PortalDirs>> {
    let dirs = write_portal_files(runtime_path)?;
    let screenshots_dir = runtime_path.join("screenshots");
    let (requests, incoming) = calloop::channel::channel::<PortalRequest>();
    let loop_handle = handle.clone();
    if let Err(error) = handle.insert_source(incoming, move |request, (), _state| match request {
        Event::Msg(request) => match request {
            PortalRequest::ConnectToEis { reply } => {
                if let Err(error) =
                    reply.send(remote_desktop::connect_to_eis(&loop_handle, &layout))
                {
                    tracing::debug!(?error, "portal requester hung up");
                }
            }
            PortalRequest::Screenshot { reply } => {
                if let Err(error) = reply.send(screenshot::write_screenshot(
                    &image.borrow(),
                    &screenshots_dir,
                )) {
                    tracing::debug!(?error, "portal requester hung up");
                }
            }
        },
        Event::Closed => {
            // The portal thread is gone; nothing else can send on the channel.
        }
    }) {
        // Without the request channel the backends cannot answer the calls
        // that need the seat or the frame; the desktop itself still starts.
        tracing::error!(?error, "portal request channel failed to install");
        return Ok(None);
    }
    // Connect and publish before the session program starts: its first portal
    // call may activate xdg-desktop-portal, which reads discovery variables
    // from the activation environment.
    let connection = match build_connection(requests, bus_address) {
        Ok(connection) => connection,
        Err(error) => {
            tracing::error!(%error, "portal D-Bus service failed to start");
            return Ok(None);
        }
    };
    if let Err(error) = publish_environment(&connection, &dirs, bus_address) {
        tracing::error!(%error, "portal activation environment");
        return Ok(None);
    }
    std::thread::Builder::new()
        .name("waywire-portal".to_owned())
        .spawn(move || {
            // The connection serves requests on zbus's own executor; this
            // thread just keeps it alive for the compositor's lifetime.
            let _connection = connection;
            loop {
                std::thread::park();
            }
        })?;
    Ok(Some(dirs))
}

fn build_connection(requests: Sender<PortalRequest>, bus_address: &str) -> Result<Connection> {
    let remote_desktop = RemoteDesktop::new(requests.clone());
    let connection = zbus::blocking::connection::Builder::address(bus_address)?
        .name(zbus::names::WellKnownName::try_from(PORTAL_NAME)?)?
        .serve_at(ObjectPath::try_from(OBJECT_PATH)?, remote_desktop)?
        .serve_at(
            ObjectPath::try_from(OBJECT_PATH)?,
            Screenshot::new(requests),
        )?
        .serve_at(ObjectPath::try_from(OBJECT_PATH)?, Access)?
        .build()?;
    Ok(connection)
}

/// Make the frontend and portal clients inherit this desktop's discovery
/// environment, both for services the bus activates and for programs the
/// compositor spawns.
fn publish_environment(
    connection: &Connection,
    dirs: &PortalDirs,
    bus_address: &str,
) -> Result<()> {
    let environment = portal_environment(dirs, bus_address);
    let settings: std::collections::HashMap<String, String> = environment.iter().cloned().collect();
    zbus::blocking::Proxy::new(
        connection,
        "org.freedesktop.DBus",
        ObjectPath::try_from("/org/freedesktop/DBus")?,
        "org.freedesktop.DBus",
    )?
    .call::<_, _, ()>("UpdateActivationEnvironment", &(settings,))?;
    Ok(())
}

/// The generated `.portal` and portals.conf contents match what packaged
/// backends ship.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portal_file_declares_waywire_interfaces() {
        assert_eq!(
            portal_file_contents(),
            "[portal]\n\
             DBusName=org.freedesktop.impl.portal.desktop.waywire\n\
             Interfaces=org.freedesktop.impl.portal.RemoteDesktop;org.freedesktop.impl.portal.Screenshot;org.freedesktop.impl.portal.Access;\n\
             UseIn=waywire;\n"
        );
    }

    #[test]
    fn portals_conf_routes_waywire_interfaces_and_keeps_the_packaged_default() {
        assert_eq!(
            portals_conf_contents(Some("gtk")),
            "[preferred]\n\
             default=gtk\n\
             org.freedesktop.impl.portal.RemoteDesktop=waywire\n\
             org.freedesktop.impl.portal.Screenshot=waywire\n\
             org.freedesktop.impl.portal.Access=waywire\n"
        );
        assert_eq!(
            portals_conf_contents(None),
            "[preferred]\n\
             default=none\n\
             org.freedesktop.impl.portal.RemoteDesktop=waywire\n\
             org.freedesktop.impl.portal.Screenshot=waywire\n\
             org.freedesktop.impl.portal.Access=waywire\n"
        );
    }

    #[test]
    fn portal_environment_prepends_the_data_dir_and_names_the_desktop() {
        let dirs = PortalDirs {
            data_dir: PathBuf::from("/run/waywire/share"),
            portals_dir: PathBuf::from("/run/waywire/share/xdg-desktop-portal/portals"),
        };
        let environment = portal_environment_from(
            &dirs,
            Some(std::ffi::OsStr::new("/usr/local/share:/usr/share")),
            "unix:path=/run/waywire/bus,guid=0123456789abcdef",
        );
        assert_eq!(
            environment,
            vec![
                (
                    "DBUS_SESSION_BUS_ADDRESS".to_owned(),
                    "unix:path=/run/waywire/bus,guid=0123456789abcdef".to_owned()
                ),
                (
                    "XDG_DATA_DIRS".to_owned(),
                    "/run/waywire/share:/usr/local/share:/usr/share".to_owned()
                ),
                ("XDG_CURRENT_DESKTOP".to_owned(), "waywire".to_owned()),
                (
                    "XDG_DESKTOP_PORTAL_DIR".to_owned(),
                    "/run/waywire/share/xdg-desktop-portal/portals".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn data_dirs_fall_back_to_the_default_pair() {
        assert_eq!(
            data_dirs_from(None),
            vec![
                PathBuf::from("/usr/local/share"),
                PathBuf::from("/usr/share")
            ]
        );
        assert_eq!(
            data_dirs_from(Some(std::ffi::OsStr::new(""))),
            data_dirs_from(None)
        );
        assert_eq!(
            data_dirs_from(Some(std::ffi::OsStr::new("/opt/share:/usr/share"))),
            vec![PathBuf::from("/opt/share"), PathBuf::from("/usr/share"),]
        );
    }

    #[test]
    fn write_portal_files_publishes_discovery_and_conf() {
        let runtime = tempfile::tempdir().expect("temporary runtime directory");
        let dirs = write_portal_files(runtime.path()).expect("portal files written");
        assert_eq!(dirs.data_dir, runtime.path().join("share"));
        assert_eq!(
            std::fs::read_to_string(
                dirs.data_dir
                    .join("xdg-desktop-portal/portals/waywire.portal")
            )
            .expect("waywire.portal readable"),
            portal_file_contents()
        );
        let conf = std::fs::read_to_string(
            dirs.data_dir
                .join("xdg-desktop-portal/portals/waywire-portals.conf"),
        )
        .expect("waywire-portals.conf readable");
        assert!(conf.starts_with("[preferred]\ndefault="));
        assert!(conf.contains("org.freedesktop.impl.portal.RemoteDesktop=waywire\n"));
    }
}
