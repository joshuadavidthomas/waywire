//! `Screenshot` portal backend: PNG of the already-rendered frame, plus the
//! access dialog the portal frontend consults before the first screenshot.
//!
//! xdg-desktop-portal checks the permission store on every
//! `org.freedesktop.portal.Screenshot.Screenshot` call and, for an unknown
//! application, asks its Access backend for a decision before forwarding to
//! this Screenshot backend. Without an Access implementation the first call
//! would never be forwarded, so this module also serves
//! `org.freedesktop.impl.portal.Access` with an auto-approving dialog: the
//! desktop is headless and its callers are on the box. Ownership arbitration
//! (ROADMAP phase 2) will gate what this grants.
#![expect(
    clippy::used_underscore_binding,
    clippy::unused_self,
    reason = "portal method signatures are fixed by the specification; zbus's generated dispatchers use the parameters"
)]

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use calloop::channel::Sender;
use zbus::interface;
use zbus::zvariant::OwnedObjectPath;
use zbus::zvariant::OwnedValue;
use zbus::zvariant::Value;

use crate::PortalRequest;

/// The frontend's permission check passes only when this interface reports
/// version 2, matching the packaged portal interface.
const IMPL_VERSION: u32 = 2;

/// Cancelled: no pixel picker exists on a headless compositor without a user
/// interface, and the Screenshot specification allows a refused request.
const RESPONSE_CANCELLED: u32 = 2;

/// Approved.
const RESPONSE_OK: u32 = 0;

/// Serves `org.freedesktop.impl.portal.Screenshot` on the session bus.
pub(crate) struct Screenshot {
    requests: Sender<PortalRequest>,
}

impl Screenshot {
    pub(crate) fn new(requests: Sender<PortalRequest>) -> Self {
        Self { requests }
    }
}

/// Serves `org.freedesktop.impl.portal.Access` on the session bus.
pub(crate) struct Access;

#[interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl Screenshot {
    async fn screenshot(
        &self,
        _handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        tracing::debug!("portal Screenshot requested");
        let (reply, wait) = tokio::sync::oneshot::channel();
        if self
            .requests
            .send(PortalRequest::Screenshot { reply })
            .is_err()
        {
            tracing::debug!("portal requester gone; refusing screenshot");
            return (RESPONSE_CANCELLED, HashMap::new());
        }
        match wait.await {
            Ok(Ok(path)) => {
                tracing::debug!(path = %path.display(), "portal Screenshot written");
                // Unencoded file URIs are correct for the runtime directory
                // paths this compositor uses; they contain no reserved bytes.
                let uri = format!("file://{}", path.display());
                let value = OwnedValue::try_from(Value::from(uri));
                let mut results = HashMap::new();
                if let Ok(value) = value {
                    results.insert("uri".to_owned(), value);
                }
                (RESPONSE_OK, results)
            }
            _ => (RESPONSE_CANCELLED, HashMap::new()),
        }
    }

    fn pick_color(
        &self,
        _handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        (RESPONSE_CANCELLED, HashMap::new())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        IMPL_VERSION
    }
}

#[interface(name = "org.freedesktop.impl.portal.Access")]
#[expect(
    clippy::too_many_arguments,
    reason = "the AccessDialog method takes every argument the specification gives it"
)]
impl Access {
    fn access_dialog(
        &self,
        _handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _title: String,
        _subtitle: String,
        _body: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        // See the module comment: approve without a dialog for now.
        tracing::debug!("portal AccessDialog approved");
        (RESPONSE_OK, HashMap::new())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        IMPL_VERSION
    }
}

/// Write the rendered frame into a PNG file under `directory`. The caller
/// passes the most recently rendered frame between loop iterations, so taking
/// a screenshot never triggers a render and the cursor is never in it. The
/// frame is `height` rows of `stride` bytes of little-endian ARGB32 and opaque,
/// because every render clears it with an opaque color, so the file holds its
/// RGB bytes, as screencopy's XRGB buffers do.
pub fn write_screenshot(
    pixels: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    directory: &Path,
) -> Result<PathBuf> {
    std::fs::create_dir_all(directory)?;
    let mut rgb = Vec::with_capacity(width * height * 3);
    for row in pixels.chunks_exact(stride) {
        for pixel in row[..width * 4].chunks_exact(4) {
            rgb.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let path = directory.join(format!("waywire-{nanos:020}.png"));
    let mut file = File::create(&path).with_context(|| format!("create {}", path.display()))?;
    let mut encoder = png::Encoder::new(
        &mut file,
        u32::try_from(width).context("screenshot width exceeds PNG's limit")?,
        u32::try_from(height).context("screenshot height exceeds PNG's limit")?,
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    // Agents screenshot in a loop. At 1920x1080, fast deflate encodes a desktop
    // in 4-7 ms where zlib's default level, which Cairo uses, takes 50-70 ms;
    // the file is larger, by about 40% for a web page.
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&rgb)?;
    writer.finish()?;
    if let Err(error) = file.sync_all() {
        tracing::warn!(%error, path = %path.display(), "screenshot fsync");
    }
    prune_screenshots(directory);
    Ok(path)
}

/// Screenshots kept in the runtime directory. Agents take screenshots in a
/// loop and the runtime directory is usually memory-backed, so older files
/// are removed; callers read the returned file right away.
const KEPT_SCREENSHOTS: usize = 8;

/// Remove all but the newest [`KEPT_SCREENSHOTS`] screenshots. Names carry a
/// zero-padded timestamp, so they sort by age.
fn prune_screenshots(directory: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut names: Vec<_> = entries
        .flatten()
        .map(|entry| entry.file_name())
        .filter(|name| {
            let name = name.to_string_lossy();
            name.starts_with("waywire-") && name.ends_with(".png")
        })
        .collect();
    names.sort_unstable();
    let excess = names.len().saturating_sub(KEPT_SCREENSHOTS);
    for name in &names[..excess] {
        let path = directory.join(name);
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::debug!(%error, path = %path.display(), "remove old screenshot");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pruning_keeps_only_the_newest_screenshots() {
        let directory = tempfile::tempdir().expect("temporary screenshot directory");
        // Out-of-order creation, and one unrelated file that must survive.
        for nanos in [5_u128, 12, 1, 9, 3, 11, 7, 2, 10, 4, 8, 6] {
            std::fs::write(
                directory.path().join(format!("waywire-{nanos:020}.png")),
                b"",
            )
            .expect("write fake screenshot");
        }
        std::fs::write(directory.path().join("notes.txt"), b"").expect("write unrelated file");

        prune_screenshots(directory.path());

        let mut left: Vec<String> = std::fs::read_dir(directory.path())
            .expect("read screenshot directory")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let mut expected: Vec<String> = (5_u128..=12)
            .map(|nanos| format!("waywire-{nanos:020}.png"))
            .collect();
        expected.insert(0, "notes.txt".to_owned());
        assert_eq!(left, expected);
    }
}
