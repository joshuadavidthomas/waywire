# Waymote on a Sprite

Small trial of the stock [Waymote](https://github.com/rockorager/waymote) demo.
The original XFCE desktops remain in place. The gateway demo now has
[opt-in comparison recording](../compare/README.md) and
[local cursor shapes](../compare/CURSORS.md). The comparison gateway and stream
daemon are patched builds of v0.1.2; the original binaries remain in place.

- Sprite: `sprite-desktop-waymote`
- URL: <https://sprite-desktop-waymote-6ra.sprites.app>
- Fly URL policy: `auth: sprite`, `private_access: admins`
- Upstream binaries: `v0.1.2`, Linux x86-64; archive checked against upstream `SHA256SUMS`
- Ubuntu packages: LXQt 2.3, labwc 0.9.3, wlroots 0.19.2, FFmpeg 8.0.1, foot 1.25.0
- Headless output: 1280×720 at startup, then follows the controlling viewer's canvas; pixman rendering, 60 FPS target, 8 Mbps H.264 target
- Audio: disabled for this first trial

`run.sh` starts labwc with `lxqt-session` and the Waymote gateway. It sets the
LXQt environment directly instead of using `startlxqtwayland`, whose VM detection
forces the cursor into captured desktop pixels. `WLR_NO_HARDWARE_CURSORS=0` keeps
this headless output's cursor separate. The comparison SDK also stops locking
the pointer on double-click; ordinary use keeps the browser's local arrow.
The patched stream daemon sends remote cursor shapes separately. Install
`breeze-cursor-theme`; the launcher selects `breeze_cursors` at size 24.
See the [local-cursor experiment](../compare/CURSORS.md).
`WAYMOTE_GATEWAY` and `WAYMOTE_STREAMD` select the comparison binaries when set;
otherwise the launcher uses the original release binaries. The comparison frontend uses Waymote's `observe` resize
policy; the unpatched upstream frontend still requests its fixed demo size.
LXQt provides the panel, application menu and PCManFM-Qt file manager. `service.json` runs it under a session D-Bus and assigns the Sprite's
HTTP port. The gateway launches its own stream daemon and FFmpeg.

The server started capture/encoding, `/healthz` returned `ok`, and an
unauthenticated external request redirected to Fly login. Josh's browser screenshot
confirmed the initial terminal stream rendered. The trial now runs LXQt instead.
Native cua-driver screenshots confirmed the desktop, icons and panel; clicks
opened the application menu and home folder. This checks the desktop locally,
not Waymote's browser input. Browser input and feel still need Josh's feedback.
CPU and latency have not been benchmarked.

The minimal apt install needs explicit `lxqt-menu-data`, `breeze-icon-theme` and
`qt6-svg-plugins`; without them the menu data or icons are missing.
`breeze-cursor-theme` supplies the cursor images needed by the current trial. A test-only
`waymote-cua` service was used for native screenshots and clicks, then stopped.
Sprite does not provide a normal login/system power service, so LXQt power and
policy-agent warnings are expected; shutdown, suspend and graphical privilege
prompts are not part of this trial.

Click the stream to take control. Other viewers can watch, but Waymote permits
only one input/clipboard controller at a time.

## Reproduce on a disposable Sprite

1. Create a Sprite and set private authentication with administrator-only access.
2. Upload `ubuntu.sources` to `/tmp/waymote.sources`. Install `labwc foot wlr-randr
wayland-utils ffmpeg dbus-x11 fonts-dejavu-core lxqt-core lxqt-wayland-session
qt6-wayland featherpad lxqt-menu-data breeze-icon-theme qt6-svg-plugins
breeze-cursor-theme` using apt with
   `-o Dir::Etc::sourcelist=/tmp/waymote.sources -o Dir::Etc::sourceparts=-`.
3. Reconstruct the exact comparison source from the immutable commit archive. Run these commands from this repository's root; the recorder files are local probe inputs copied after the source patch, not part of the upstream archive or patch:

   ```sh
   repo=$PWD
   commit=90564cfb02030c494939c6fdf29cae9c4d689c67
   archive=/tmp/waymote-$commit.tar.gz
   source=/tmp/waymote-$commit
   curl -fL --retry 3 -o "$archive" \
     "https://github.com/rockorager/waymote/archive/$commit.tar.gz"
   printf '%s  %s\n' \
     8cd53b5222b57e0f04448a231963ae98fbe5dbaf3fc2611340b6e89f04eb19d1 \
     "$archive" | sha256sum --check --strict
   rm -rf "$source"
   mkdir -p "$source"
   tar -xzf "$archive" -C "$source" --strip-components=1
   (
     cd "$source"
     patch --dry-run -p1 < "$repo/probes/compare/waymote.patch"
     patch -p1 < "$repo/probes/compare/waymote.patch"
   )
   cp "$repo/probes/compare/recorder.mjs" \
     "$repo/probes/compare/metrics.mjs" \
     "$source/gateway/examples/web/"
   ```

   Build this reconstructed tree, then unpack the verified release server archive in `/home/sprite/waymote` before uploading the comparison binaries.

4. Change the public URL in `run.sh` to the new Sprite's canonical HTTPS URL.
   Upload the script as `/home/sprite/waymote/run.sh` and `service.json` as
   `/tmp/waymote-service.json`. Upload `session.conf` and `lxqt.conf` to
   `/home/sprite/.config/lxqt/` to select labwc and the icon/panel themes.
5. Register the service from inside the Sprite:

   ```sh
   curl -fsS --unix-socket /.sprite/api.sock -X PUT \
     -H 'Content-Type: application/json' \
     --data-binary @/tmp/waymote-service.json http://sprite/v1/services/waymote
   ```

This launcher is a disposable experiment, not the versioned desktop installer.
