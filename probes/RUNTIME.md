# v1 runtime acceptance

The runtime is installed on `sprite-desktop-v1` at
<https://sprite-desktop-v1-6ra.sprites.app>. `josh-desktop` and the v0 source remain
unchanged apart from a TypeScript fix in the old tunnel test.

Current installed bundle: `v1.0.0-dev.13`, on `sprite-desktop-v1` and
`sprite-desktop-v1-conflicts`. Earlier browser and idle measurements below used
`dev.9`; installer recovery passed again on `dev.13`. These are local build
artifacts, not published GitHub releases. M5 cutover has not happened.

## Passed on live Sprites

- Fresh full installation on `sprite-desktop-v1` and `sprite-desktop-v1-idle`.
  Both use private Sprite authentication restricted to org administrators.
- The native `sprite exec --file` entry point installed `dev.10` on
  `sprite-desktop-v1-conflicts`, without a Node command inside the Sprite.
  The SDK entry point subsequently upgraded it to `dev.13`.
- A native VNC client (`vncdotool`) captured the fresh XFCE desktop through
  `sprite proxy 15900:5900`. Wallpaper and icons rendered, rechecking the Glycin
  workaround on the newly installed image. The local proxy stopped afterward.
- Recognized v0 adoption passed on `sprite-desktop-v1-probe` with `dev.12`, after
  removing our M0 echo service. Only the two v1 services remain. The old launcher
  and XFCE helpers retained their exact pre-install hashes.
- Live mode inspection found runtime directory 0700 and RFB socket 0600, both
  owned by `sprite`; binaries 0555 and config/version/ownership records 0644,
  owned by root. RFB listeners were only 127.0.0.1 and ::1.
- Interrupted upgrade and service creation: recovery after the pending record,
  each service operation, and before commit. Recovery restarted every surviving
  managed process. See `results/runtime-installer.json` and `install-recovery.ts`.
- Same-version rerun preserved service PIDs, package observation bytes, and config
  and version file inodes. It repaired a deliberately changed script owner/mode.
- Changing the canonical Origin restarted only the bridge. The test restored the
  Sprite's actual Origin afterward.
- Foreign HTTP service, both reserved service names, and occupied 5900/8080 ports
  were refused before apt or runtime mutation. Unsupported OS, a simulated arm64
  dpkg response, and a denied package source were also refused. See
  `results/runtime-preflight.json` and `install-preflight.ts`.
- Eight invalid Origin forms returned 403 through the private URL. Static assets,
  methods, HEAD responses, cache policy, security headers, and absence of the API
  token in response bodies passed. See `results/runtime-transport.json`.
- 1,000 health requests in a minute with a viewer, then 1,000 without a viewer,
  left Xvnc accepting full new handshakes. Health latency p95 was 74/77 ms; new
  handshakes took 346/330 ms. Neither phase used a stub RFB server.
- XFCE, Firefox, mouse input, keyboard input, clipboard transfer and paste,
  Ctrl+Alt+Del, and remote resize worked. Xrandr confirmed 1406×753 after resize.
  Clipboard rejection also produced visible error feedback.
- Desktop service restart recovered in 5.5 s; bridge restart recovered in 11.3 s.
  Offline/online left exactly one attached viewer.
- Five suspended navigations and three machine restarts recovered. A desktop file
  and an XFCE helper preference survived. Disconnect left no new resource requests
  and permitted the Sprite to become warm. See
  `results/runtime-browser-lifecycle.json`.
- A separate fresh Sprite stayed attached for 30 minutes with 90 control pings,
  no task hold, and no exec, HTTP polling, or RFB traffic during the wait. Actual
  X11 saver state was disabled (3), with 34 minutes of X input idleness. The Sprite
  became warm 5.1 s after disconnect. See `results/runtime-idle.json`.
- Native Firefox loaded `http://127.0.0.1:8080`. Its noncanonical Origin could not
  connect to VNC; the UI stopped retrying after its 60-second budget and showed
  Retry and Re-authenticate. This verifies bounded connection failure, not an
  expired Fly cookie.
- `runtime-children.sh` passed on `dev.13`: TERM to Xvnc, XFCE, then the bridge
  produced new service PIDs and listening RFB. Every old process in the affected
  service cgroup was gone. The probe waits out the ten-second health cache before
  checking recovery; its 12-second results are upper bounds, not latency samples.

The browser lifecycle measurements used a localhost acceptance proxy that kept
credentials outside the browser. They prove the installed UI and transport, not
cookie-authenticated browser access. M0 separately proved Fly browser-cookie
access to the same-origin echo service. Josh subsequently opened the real private
runtime URL in his browser, supplied a screenshot showing the outer viewer
connected to XFCE, and confirmed it worked. Full-runtime browser access passed;
expired-cookie behavior remains untested.

| Navigation class          | Connected p50 / p95 | Visible pixels p50 / p95 |
| ------------------------- | ------------------- | ------------------------ |
| Warm, 5 trials            | 517 / 604 ms        | 726 / 837 ms             |
| Suspended, 5 trials       | 597 / 669 ms        | 829 / 888 ms             |
| Machine restart, 3 trials | 22.4 / 23.1 s       | 24.3 / 24.7 s            |

These are navigation-relative browser performance marks. No first-navigation
reload was required in these three runtime restart trials. M0's shorter HTTP
client timeout did require subsequent requests; that earlier result still stands.

## GUI testing change

On Josh's direction, use cua-driver inside the Sprite for remaining desktop
checks. The native agent-browser keyboard helper emitted no keydown events on
the canvas, and its clipboard permission needed separate setup. The ad hoc CDP
input script was removed instead of retaining another automation path.

`cua-driver` 0.23.2 runs as the `sprite` user in the temporary
`desktop-acceptance-cua` service, with `DISPLAY=:1`. `at-spi2-core` was installed
for this test tool, not added to the runtime's package requirements. Native
Firefox snapshots expose its controls. Use `element_token` from the latest
snapshot; this driver release rejects bare element indices. Clipboard plus
foreground hotkeys works for Firefox's address bar when accessibility text
insertion reports failed delivery. Read the screenshot after every action.

The saver probe similarly installed `libxss1` only on the disposable idle Sprite.

## Live installation discoveries

- Fresh images have `/var/lock` pointing at a missing `/run/lock`. The installer
  creates the lock directory after checking the supported OS and architecture.
- zstd is absent. Releases use gzip so extraction requires no apt bootstrap.
- Sprite restricts `/proc` fd inspection. `ss -p` cannot reliably name Xvnc's
  PID, even as root. Ownership checks instead compare every socket's kernel
  cgroup with the service's `/svc.<name>` group, after validating its full service
  definition against the ownership record.
- Apt reads only the release's source and preference files for installer
  operations; unrelated repositories and pins cannot supply its candidates.
- IPv4 HTTP requests to three Ubuntu archive addresses timed out, while IPv6
  HTTP and IPv4 HTTPS succeeded. The locked Ubuntu source now uses HTTPS.
  Source verification and apt operations passed on `dev.12` and `dev.13`.
- Killing Xvnc on `dev.10` left SSH/GPG agents in separate process groups. Sprite
  still reported the service running despite its dead launcher. The launcher now
  checks its exact service cgroup and cleans all processes in that cgroup, with
  bounded TERM/KILL cleanup. Children no longer inherit its lock descriptor.
  Signal traps exit through cleanup and ignore further signals while cleaning.
- GPG's key inspection created a keybox in the user's home directory. Verification
  now ignores user options and uses the installer's temporary directory instead.
- One immediate post-create attempt found invalid canonical URL metadata. It
  refused before package changes; rerunning after `sprite-env info` returned the
  canonical URL succeeded. Do not guess a hostname when metadata is absent.

## Still required before M5

- Bounded auth-failure behavior with an expired Fly cookie. Normal access through
  the private URL has now been confirmed by Josh.
- Complete browser evidence for hidden-tab return, two shared tabs, and an active
  upgrade. The old hidden-tab experiment was started but not verified on return;
  later upgrade/recovery tests restarted that desktop repeatedly.
- Final validation, v0 removal, README rewrite, and clean-checkout/fresh-Sprite
  validation. Do not mark v1 complete on the strength of the local tests alone.

## Repeat the child-exit regression check

This ends desktop sessions. Use a disposable Sprite, with browser viewers
disconnected:

```sh
sprite exec -s sprite-desktop-v1-conflicts \
  --file probes/runtime-children.sh:/tmp/runtime-children.sh \
  -- bash /tmp/runtime-children.sh
```

The test captures PIDs from the service's kernel cgroup, signals a captured PID,
waits for automatic service replacement, then requires every old PID to be gone.
It fails on the orphaned-agent bug; killing only the launcher is not an adequate
substitute for the Xvnc and XFCE cases.
