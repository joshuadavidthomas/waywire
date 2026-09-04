# Sprite Desktop v1: self-contained desktop runtime

**Status:** implementation underway; M0 platform gate passed ([record](./probes/M0.md)); M1 in progress  
**Baseline:** v0 spike at change `xrkyqoky` / commit `c37e7760`  
**Prior spec:** [SPEC.v0.md](./SPEC.v0.md)

## 1. Goal

Turn an existing Fly Sprite into a persistent browser desktop by running one versioned installer against it.

After installation, the Sprite's own private HTTPS URL serves a small noVNC application. A single Go binary inside the Sprite serves that page, checks the browser's Origin, bridges the WebSocket to Xvnc over a Unix socket, and keeps the Sprite awake while a viewer is attached. Fly authenticates the URL. No Cloudflare Worker, separate gateway, Sprites API token in the browser, or new control tool is required.

The installed Sprite must:

- wake when its private URL receives a request;
- serve the desktop page from that URL;
- recover the desktop and bridge services after suspension or machine restart;
- reconnect the browser while those services become ready;
- preserve files and desktop settings on the Sprite filesystem;
- keep raw RFB reachable only from processes inside the Sprite;
- reject cross-origin browser WebSocket upgrades;
- keep an idle but attached session alive without application-level traffic; and
- report the installed runtime, dependency versions, and live readiness.

v1 provides one shared XFCE session to every organization admin admitted by the Fly URL policy. Those admins are trusted co-owners. v1 does not enforce a one-person identity boundary.

## 2. User experience

Fly's `sprite` CLI remains the control tool. This project ships no separate control tool. It ships one installer with two entry points that run the same in-Sprite steps.

### 2.1 Outside entry point

The documented path is a repository script that drives Fly's TypeScript SDK from the operator's machine:

```sh
sprite url update --auth sprite -s my-sprite
pnpm install-desktop --sprite my-sprite --release v1.0.0
```

The script:

1. reads the Sprite through the SDK and asserts `urlSettings.auth` is `sprite` and `urlSettings.privateAccess` is `admins`, setting `privateAccess` only when `--set-private-access` is passed;
2. reads the network policy and reports any required package source the policy would refuse;
3. uploads the release archive and in-Sprite installer through the SDK filesystem API;
4. runs the in-Sprite installer through the SDK, passing `--archive <uploaded path>` as installer arguments, and streams its output;
5. validates from outside: managed services match their recorded definitions, `check()` reports healthy, an org-token request to `/healthz` succeeds, and unauthenticated HTTP and WebSocket requests are redirected to Fly's login.

The org token stays on the operator's machine. The browser never receives it.

### 2.2 Inside entry point

The in-Sprite installer is a standalone Bash script for operators who do not want Node locally. URL authentication must be verified before the installer runs, because the installer cannot inspect it and a bridge behind a public URL is a passwordless desktop for anyone who forges the Origin header:

```sh
sprite api /v1/sprites/my-sprite -- -fSs -X PUT \
  -H 'Content-Type: application/json' \
  -d '{"url_settings":{"auth":"sprite","private_access":"admins"}}'
sprite api /v1/sprites/my-sprite -- -fSs
# Confirm url_settings.auth is "sprite" and private_access is "admins".
sprite exec --no-port-forward -s my-sprite -- \
  bash -lc "curl -fsSLo /tmp/install-desktop 'https://github.com/OWNER/REPOSITORY/releases/download/v1.0.0/install.sh' && bash /tmp/install-desktop"
```

The placeholders are not literal values. M0 verified the PUT and GET commands above against `sprite-desktop-v1-probe`; both returned the required URL settings. This path performs every in-Sprite step and prints the outside validation commands it cannot run itself.

`install.sh` also accepts `--archive <path>` for a release archive already present in the Sprite. The outside entry point uses this after uploading, so both paths verify and install the same bytes through the same steps.

During development, `sprite exec --file local:remote` may upload the same script instead of downloading a release.

### 2.3 Canonical origin

The installer derives the canonical origin from `sprite-env info`, which prints `sprite_url` from inside the Sprite. The operator does not supply it. `DESKTOP_ORIGIN` may override it only for tests, and the installer prints a warning when the override differs from `sprite-env info`.

### 2.4 Using the desktop

When installation finishes, the operator opens the Sprite URL. Fly redirects an unauthenticated browser through its login. The authenticated page connects to `/vnc` on the same origin and renders XFCE.

Opening the page starts the connection automatically. The page exposes Disconnect, Reconnect, Send Ctrl+Alt+Del, and Paste controls. Disconnect stops every retry and lets the Sprite become idle.

The first navigation to a Sprite that is fully stopped, as after a machine restart, may fail before the page loads. Fly's router waits about ten seconds for a stopped Sprite, and v0 measured about 45 seconds for a machine restart. In that case the browser shows its own error page and the operator reloads. No code in this project can run before the page exists. M3 measures how often this happens; it is not an acceptance failure.

## 3. Architecture

```text
Authenticated browser
    |
    | HTTPS and WebSocket
    v
Fly private Sprite URL router
    |
    | HTTP service on port 8080
    v
sprite-desktop-bridge  (one Go binary)
    |-- embedded noVNC application
    |-- exact Origin check on /vnc
    |-- WebSocket ping frames
    |-- /healthz and /version
    |-- Tasks API hold while a viewer is attached
    `-- RFB relay over a Unix socket
                              |
                              v
                    /tmp/sprite-desktop/rfb.sock
                              |
                              v
                         Xvnc + XFCE
```

The browser data path ends inside the Sprite. No central service handles framebuffer, keyboard, mouse, resize, or clipboard traffic.

### 3.1 Process topology

The runtime owns two Sprite services. Both run as the `sprite` user, which is how the Sprite service manager already runs the v0 service.

`sprite-desktop` runs the X11 desktop stack:

- Xvnc listens on the Unix socket `/tmp/sprite-desktop/rfb.sock` with mode 0600 and RFB security type `None`.
- Xvnc also listens on `127.0.0.1:5900` so `sprite proxy 5900` remains a debugging path for an owner. M2 confirms both listeners coexist.
- Xvnc provides display `:1`, 24-bit color, shared access, and RandR resizing.
- X screen blanking and DPMS are disabled. No screensaver or screen locker package is installed. The `sprite` user has no password, so a locked screen would be unrecoverable from the browser.
- A D-Bus session starts one full `xfce4-session` with Firefox configured as its browser.
- The service script treats exit of either Xvnc or XFCE as failure, terminates the other, removes stale sockets and X locks, and exits so the service manager restarts the whole desktop.

`sprite-desktop-bridge` owns the Sprite HTTP URL:

- it is the only service registered with `--http-port 8080`, binds all interfaces on 8080, and is the only non-loopback listener;
- it serves the embedded browser application, `/healthz`, and `/version`;
- it accepts a `/vnc` upgrade only after the exact Origin check in §4.2;
- it relays bytes between the WebSocket and the Xvnc Unix socket with no framing;
- it sends WebSocket ping frames on an interval so an idle session never goes silent at the transport layer;
- it holds a Sprite task through `/.sprite/api.sock` while at least one viewer is attached, when that feature is enabled by configuration (§3.4);
- it starts independently of the desktop service so it answers the page load while Xvnc is still starting; and
- it depends on nothing under `/.sprite/bin` and on no apt package beyond libc.

The bridge is a static Go binary built for `linux/amd64`. It uses one maintained WebSocket library; M1 records the choice. It has no plugin system, no configuration language, and no request routing beyond the fixed paths below.

### 3.2 Port and path contract

| Address or path                | Owner  | Exposure                 | Meaning                                   |
| ------------------------------ | ------ | ------------------------ | ----------------------------------------- |
| `/tmp/sprite-desktop/rfb.sock` | Xvnc   | `sprite` user processes  | raw RFB                                   |
| `127.0.0.1:5900`               | Xvnc   | loopback only            | raw RFB for `sprite proxy` debugging      |
| `:8080`                        | bridge | Sprite URL router        | sole HTTP listener                        |
| `/`                            | bridge | authenticated Sprite URL | uncacheable desktop HTML shell            |
| `/assets/<content-hash>.*`     | bridge | authenticated Sprite URL | immutable embedded browser assets         |
| `/vnc`                         | bridge | authenticated Sprite URL | exact-origin WebSocket upgrade only       |
| `/healthz`                     | bridge | authenticated Sprite URL | live readiness document                   |
| `/version`                     | bridge | authenticated Sprite URL | release and install-time package metadata |

The HTTP route contract is exact:

| Request                                      | Result                                                               |
| -------------------------------------------- | -------------------------------------------------------------------- |
| `GET` or `HEAD /`                            | HTML shell, `Cache-Control: no-store`                                |
| `GET` or `HEAD /assets/<manifest-entry>`     | matching asset, `Cache-Control: public, max-age=31536000, immutable` |
| `GET` or `HEAD /healthz`                     | JSON, `Cache-Control: no-store`                                      |
| `GET` or `HEAD /version`                     | JSON, `Cache-Control: no-store`                                      |
| `GET /vnc` with Upgrade and a valid Origin   | 101 and RFB relay                                                    |
| `GET /vnc` with Upgrade and any other Origin | 403 before upgrade                                                   |
| plain `GET /vnc`                             | 426                                                                  |
| unsupported method on a known path           | 405                                                                  |
| every other path                             | 404                                                                  |

There is no SPA fallback and no filesystem serving. Assets come from the binary's embedded filesystem, so path traversal is not possible.

`/healthz` reports:

```json
{
  "release": "v1.0.0",
  "bridge": "ok",
  "rfb": "listening",
  "attached": 1,
  "task_held": true,
  "checked_at": "2026-09-04T20:00:00Z"
}
```

`rfb` is `listening` when the bridge's most recent readiness probe succeeded, otherwise `starting`. The probe completes a full RFB handshake against the Xvnc socket, through `ServerInit`, and then closes cleanly. It runs at most once every 10 seconds regardless of how often `/healthz` is requested, and never while a viewer is attached, since an attached viewer is proof enough. v0 recorded that TigerVNC treats a connection closed mid-handshake as a failed login and can blacklist the peer; M1 proves that a sustained stream of `/healthz` requests neither blacklists nor delays a real viewer. If it does, the probe becomes non-connecting and reports only whether the socket path exists.

A control plane can poll this document through the Sprite URL with an org token; the review confirmed that a Bearer token authenticates the URL.

### 3.3 Session milestones

The browser tracks two milestones:

- `connected`: noVNC emitted its `connect` event after RFB negotiation;
- `first-frame`: the canvas has nonzero dimensions and its sampled pixels differ from the initial black framebuffer.

Only `connected` drives the state machine in §5.1. `first-frame` is recorded for timing and lifecycle measurements and never causes a retry. A black screen after `connected` is either XFCE still starting, which the desktop service's own supervision resolves, or a legitimately dark desktop.

### 3.4 Keepalive

Fly pauses a Sprite after about 30 idle seconds. Two mechanisms prevent an attached viewer from experiencing that pause:

1. **Transport pings.** The bridge sends a WebSocket ping every 20 seconds on every attached `/vnc` connection. Browsers answer pings automatically. This keeps bytes flowing through the router on a static screen and is always on.
2. **Task hold.** When `keepalive.task` is enabled in `/etc/sprite-desktop/config.json`, the bridge upserts the task `sprite-desktop-viewer` with a 90 second expiry every 30 seconds while `attached > 0`, and deletes it when the last viewer disconnects. The socket at `/.sprite/api.sock` is world-writable, so the bridge needs no privilege. If the bridge dies with the task held, the task expires on its own.

M0 decides the default for `keepalive.task`. If an attached WebSocket with ping traffic and no exec session holds the Sprite active by itself, the default is off, and the pings alone carry the session. If it does not, the default is on. Either way the setting is recorded in the M0 test record and the installer writes it. The 2026-09-04 pings-only trials passed for five and thirty minutes without exec sessions or a task; the selected default is off. Josh also verified cookie-authenticated browser echoes at 22:46 UTC; SameSite metadata remains unrecorded.

Disconnect must let the Sprite idle. With the task hold enabled, idle begins at most 90 seconds after the last viewer leaves. The acceptance test records the observed delay.

### 3.5 Connection lifetime

Pings, two relay directions, the viewer count, and task renewal are the bridge's own code, so their rules are written down:

- A viewer is counted from the moment the upgrade succeeds until its cleanup runs. Cleanup runs exactly once per connection, whichever side fails first, and closes both the WebSocket and the Xvnc socket.
- Either relay direction failing, or the Xvnc socket refusing the dial, closes both endpoints and triggers cleanup.
- A ping that receives no pong within 15 seconds counts as a missed pong. Two consecutive missed pongs close the connection. Sending pings alone does not detect a vanished viewer; the pong deadline does.
- The task is acquired immediately when the viewer count goes from zero to one, not on the next renewal tick. Renewal continues every 30 seconds while the count is above zero. Deletion happens when the count reaches zero.
- All task operations run on one goroutine fed by a channel. A delete queued for a departing viewer cannot cancel a hold that a newer viewer already needs; the goroutine reads the current count before each operation.
- Binary WebSocket frames carry RFB bytes in both directions. Ping and data writes go through the WebSocket library's documented single-writer path; the bridge never writes to one connection from two goroutines.
- Cleanup and task operations are covered by tests that kill the browser side, kill the Xvnc side, and stop the pong stream. Pong-loss detection takes at most 55 seconds under the test clock (20 seconds until the next ping, followed by two 15-second deadlines on the 20-second schedule). After cleanup, the viewer count is zero and the task is released within one 30-second renewal interval.

Without these rules a dead connection can hold the Sprite awake for as long as its task keeps renewing.

## 4. Authentication and browser isolation

### 4.1 Fly owns authentication

The Sprite URL remains in `sprite` authentication mode with `privateAccess: admins`. The SDK types both fields on `URLSettings`, readable through `getSprite` and writable through `updateURLSettings`. M0 confirms the server honors `privateAccess` with one round trip.

The in-Sprite installer cannot read or change the URL's external auth setting. The outside entry point asserts both fields before install and validates from outside afterward. The inside entry point's documented sequence puts the same verification before the installer runs (§2.2), and the installer prints the outside validation afterward.

If the `admins` scope cannot be set or read, stop for a scope decision. Do not silently widen access to all organization users.

Every organization admin admitted by Fly receives full control of the same desktop and is a trusted co-owner. Xvnc remains passwordless because Fly authentication and the same-origin bridge guard the only external path.

v1 has no custom passwords, access keys, HMAC tickets, public URL mode, or Cloudflare Access layer.

### 4.2 The bridge enforces one exact Origin

Fly authentication answers who may reach the URL. The router forwards the browser's `Origin` unchanged, adds no identity header, and does not check Origin itself. The bridge's Origin check is what stops a hostile page from using the admin's authenticated browser session to open the desktop WebSocket.

The bridge compares `Origin` byte-for-byte against the canonical origin in `/etc/sprite-desktop/config.json`. It accepts exactly one value and rejects the request with 403 before upgrade when Origin is:

- absent;
- `null`;
- malformed;
- an HTTP origin;
- a different host;
- the correct host with another port; or
- a list or otherwise ambiguous value.

The expected origin is never derived from `Host`, `Forwarded`, or `X-Forwarded-Host`. The release binary contains no Sprite-specific value.

A page running inside the desktop's own Firefox is on the Sprite itself. It cannot reach RFB because Xvnc's bridge path is a Unix socket, which browsers cannot open, and because a browser cannot complete an RFB handshake against `127.0.0.1:5900` through a fetch or WebSocket. It can reach the bridge on `127.0.0.1:8080`, where the same Origin check rejects it. There is no second bridge process and no loopback WebSocket port to defend.

Origin is browser request isolation, not authorization. A non-browser client that owns a valid Fly credential can forge it. That client is already authorized to control the Sprite.

M0 records the `SameSite` attribute of Fly's session cookie. The Origin check is mandatory regardless; the record says whether it is the only line.

### 4.3 In-Sprite trust boundary

`/.sprite/api.sock` is mode 0666. Any native process inside the Sprite can create tasks, restart services, or read service definitions. That is already code execution inside the Sprite. v1 does not design around it and documents it here so nobody mistakes the socket for a privileged interface.

### 4.4 Response policy

The bridge sets and tests at least:

```text
Content-Security-Policy: default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'
X-Content-Type-Options: nosniff
Referrer-Policy: no-referrer
```

The final CSP may add only directives required by the built noVNC application and proven by browser tests. Do not add wildcard sources or `unsafe-eval`. No service worker in v1, so stale assets cannot outlive a runtime upgrade.

## 5. Browser application

The current Svelte/noVNC interface becomes a static application embedded in the bridge binary. The Sprite runs no Node or SvelteKit server.

The application must preserve the proven behavior from v0:

- the repository's noVNC 1.7 dependency, built into the application rather than taken from Ubuntu's noVNC 1.6 package;
- responsive canvas sizing;
- `resizeSession` and viewport scaling;
- mouse and keyboard input;
- Ctrl+Alt+Del;
- local-to-remote clipboard paste;
- manual disconnect; and
- cancellation of stale connection attempts.

It must remove every Sprites TCP-proxy concern:

- no ticket request;
- no gateway URL;
- no target initialization frame;
- no proxy acknowledgement filter; and
- no Sprite health polling.

The WebSocket URL is the page's own origin plus `/vnc`. Re-authenticate navigates the top-level page to the page's own origin. There is no runtime configuration document.

### 5.1 Connection and recovery states

```text
booting
connecting
connected
retry-wait
offline
stopped-by-user
failed
```

`connecting` begins when a fresh noVNC `RFB` object is constructed and ends when noVNC emits `connect`. Each state has a bounded exit; an open but stalled socket cannot remain `connecting` forever.

| Event                        | Current state      | Action and next state                                                                    |
| ---------------------------- | ------------------ | ---------------------------------------------------------------------------------------- |
| page mount                   | `booting`          | create a 60 s online-time budget and enter `connecting`                                  |
| noVNC `connect`              | `connecting`       | enter `connected` and clear the retry budget                                             |
| 10 s negotiation timeout     | `connecting`       | invalidate and close the attempt; enter `retry-wait`                                     |
| unexpected disconnect        | `connecting`       | invalidate and close the attempt; enter `retry-wait`                                     |
| unexpected disconnect        | `connected`        | create a fresh 60 s budget; enter `retry-wait`                                           |
| retry timer fires            | `retry-wait`       | create one fresh `RFB` object and enter `connecting`                                     |
| browser becomes offline      | `connected`        | invalidate the attempt and timers; create a fresh budget in paused form; enter `offline` |
| browser becomes offline      | retrying state     | invalidate the attempt and timers; pause the remaining budget; enter `offline`           |
| browser becomes online       | `offline`          | resume the stored budget and enter `connecting` immediately                              |
| user selects Disconnect      | any state          | invalidate the attempt and timers; enter `stopped-by-user`                               |
| user selects Reconnect       | `stopped-by-user`  | create a fresh budget and enter `connecting`                                             |
| online-time budget expires   | any retrying state | invalidate the attempt and timers; enter `failed`                                        |
| user selects Retry           | `failed`           | create a fresh budget and enter `connecting`                                             |
| user selects Re-authenticate | `failed`           | navigate the top-level page to its own origin                                            |
| page visibility changes      | any state          | update presentation only; never create another attempt                                   |

Every callback and timer captures the current generation and becomes a no-op after invalidation. At most one `RFB` object and one retry timer may exist. Entering `offline`, `stopped-by-user`, or `failed` closes the current object. Offline time does not consume the budget.

Retry delay starts at 250 ms, doubles to a 5 s cap, and adds uniformly distributed jitter from zero through 20 percent of the computed delay. The 60-second budget includes connection and retry-wait time. Only `connected` resets it.

The client cannot read Fly's HTTP redirect from a failed WebSocket upgrade. After the budget expires it offers both Retry and Re-authenticate rather than guessing whether the cause was authentication, wake, or process failure.

The browser sends no application-level heartbeat. Every data frame belongs to RFB. Keepalive is the bridge's job (§3.4).

### 5.2 Concurrent tabs

v1 keeps Xvnc shared mode. Two authenticated administrators may view and control the same desktop at once. The UI does not claim ownership or isolate input.

## 6. Installer and release contract

### 6.1 Supported target

The first release supports the Sprite image tested by v0:

- Ubuntu 26.04 (`resolute`);
- amd64; and
- the current Sprite service manager and URL router.

The installer refuses another OS codename or architecture before mutation.

### 6.2 Release contents and trust

A release contains:

```text
install.sh
sprite-desktop-<version>-linux-amd64.tar.zst
SHA256SUMS
```

`install.sh` embeds its release identifier, archive URL, expected byte size, and archive SHA-256, and verifies the archive against those values whether it downloaded it or received it through `--archive`. A checksum file beside the archive is for inspection, not a separate trust anchor. The release process publishes the source revision through the repository's release channel.

The archive contains the bridge binary, the desktop service script, `manifest.json`, and `sources.lock`. It never contains a Sprite hostname or generated configuration.

The installed layout is:

```text
/opt/sprite-desktop/
  current -> releases/<version>
  releases/<version>/
    bin/sprite-desktop-bridge
    bin/desktop.sh
    manifest.json
    sources.lock
/etc/sprite-desktop/
  config.json
  version.json
/var/lib/sprite-desktop/
  install.json
```

`manifest.json` records the release version, source revision, supported OS, noVNC version, bridge library versions, required ports and socket paths, and the exact service command and argument tuples. `config.json` holds the canonical origin, socket path, ping interval, and keepalive settings. `version.json` records the package versions observed by `dpkg` at install time; `/version` serves it and labels those versions as install-time observations.

### 6.3 Package policy

`sources.lock` records each apt source and signing-key fingerprint the installer relies on, and the package names it installs from each. It does not pin exact versions. Ubuntu removes superseded versions from its pockets when security updates land, so an exact pin would fail on Canonical's schedule and take the installer down with it.

The installer fails before mutation when a locked source or key is unreachable or its fingerprint differs. It records the versions apt selected in `version.json`. It does not fail on version drift.

One exception: the v0 desktop required Glycin 2.1.5 from `resolute-proposed` and a `SNAP` environment workaround. v1 either pins that exact package set and reverifies it in M4, or replaces it with a tested fix. It may not install an unrecorded package from a proposed pocket because it is newest that day.

Firefox comes from Mozilla's apt repository, as in v0. M4 verifies that repository and every Ubuntu source are reachable under the default Sprite network policy.

### 6.4 Users and permissions

The in-Sprite installer runs as the `sprite` user with noninteractive `sudo`.

| Files or process           | Owner or runtime user | Rule                                                 |
| -------------------------- | --------------------- | ---------------------------------------------------- |
| `/opt/sprite-desktop`      | `root:root`           | directories 0755; files read-only after verification |
| `/etc/sprite-desktop`      | `root:root`           | files 0644; rewritten atomically                     |
| `/var/lib/sprite-desktop`  | `root:root`           | `install.json` 0644                                  |
| Xvnc, D-Bus, XFCE, Firefox | `sprite`              | `HOME=/home/sprite`; never root                      |
| bridge                     | `sprite`              | unprivileged port 8080; Sprite service manager only  |

The installer seeds a Firefox or XFCE default only when that user setting does not exist. Reruns and upgrades never rewrite files under `/home/sprite/.config`, the Desktop directory, browser profiles, or other user-owned state.

### 6.5 Preflight and ownership

The installer takes an exclusive `flock` on `/var/lock/sprite-desktop-install.lock`. Preflight runs under that lock before any apt, file, or service mutation. It refuses installation when:

- any foreign Sprite service has an HTTP port set;
- an unmanaged process listens on 5900 or 8080;
- either intended service name exists without a matching `install.json` record, whether `pending` or `committed`;
- the canonical origin cannot be derived or fails validation;
- a newer runtime owns the installation directory; or
- the OS or architecture is unsupported.

`install.json` is the ownership marker. It records schema version, owner ID `dev.sprite-desktop.runtime`, a `state` of `pending` or `committed`, the target release, the config hash, and the exact definition of every managed service. A pending record also retains the previous committed service definitions. The installer writes it in `pending` state before touching any service and rewrites it as `committed` after live validation. A service is owned when its live definition, read from `/.sprite/api.sock` at `/v1/services`, matches the committed definition or either the previous or target definition in a pending record. A `pending` record on rerun means the previous run was interrupted; the rerun repairs forward from it rather than refusing.

A recognized v0 install is the sole migration exception. Its service must be named `desktop`, use command `/home/sprite/.local/bin/desktop.sh`, have no arguments or HTTP port, and its command file must have SHA-256 `4d643f592c2f6d403db63907a1bfb2b5ec11f41ff88ff943c10a3962c480967c`. The installer removes that service after recording the migration and leaves the old home-directory scripts and every user configuration file in place. Any mismatch is a conflict.

The in-Sprite installer never changes URL authentication.

### 6.6 Install sequence and recovery

The in-Sprite installer runs these steps in order:

1. preflight, under the lock;
2. obtain the release archive, from `--archive` or by download, verify its size and SHA-256 against the values embedded in `install.sh`, and unpack it into a new directory under `releases/`;
3. apt install from the sources recorded in that release's `sources.lock`;
4. render `config.json` and `version.json` to temporary files;
5. validate: `sprite-desktop-bridge --check-config` against the rendered config, `bash -n` on the desktop script;
6. write `install.json` in `pending` state with the target release, config hash, and both previous and intended service definitions;
7. move the rendered files into place and switch the `current` symlink atomically;
8. converge services: remove a recognized v0 service, create or replace only owned services, restart only services whose definition, release, or rendered config changed;
9. validate live: both services running, `ss` shows only the expected listeners, `/healthz` on `127.0.0.1:8080` answers, a local RFB handshake against the Unix socket succeeds;
10. rewrite `install.json` as `committed`.

Recovery is rerunning the installer. Every step is idempotent: existing packages are skipped, an existing verified release directory is reused, unchanged config is not rewritten, and unchanged services are not restarted. A rerun that finds a `pending` record accepts only services matching its previous or target definitions, recreates and validates the target configuration if needed, and continues from step 7. Recovery restarts every managed service after convergence, even if the files already match the target: an interrupted switch may have left old processes running. Live validation must confirm that `/healthz` reports the target release. Committed same-version reruns still avoid needless restarts. Apt in step 3 can change shared packages; release selection and managed service changes begin at step 7. There is no transaction log and no rollback command in v1. Forward repair by rerun is the recovery strategy; the Sprite holds the files and settings this product promises to preserve, so recreating it is never the documented advice.

A successful same-version rerun must:

- leave one exact definition for each managed service;
- repair changed managed files;
- preserve user files and XFCE settings;
- avoid restarting a healthy service when release, config, and service definitions agree; and
- report the same release and checksums.

A newer installer unpacks and validates the new release before switching. An upgrade may end an active desktop session; the installer says so before mutation. Older release directories remain until a later cleanup policy exists.

## 7. Sprite URL ownership

Fly permits one HTTP service per Sprite. v1 therefore defines a desktop-enabled Sprite as owning that Sprite's URL.

The runtime does not reverse proxy a user's development server, choose another application port, or replace an existing HTTP service. Preflight reports the conflicting service and exits before mutation.

`sprite proxy 8080` cannot be used to view the page, because the browser's Origin would be `http://localhost:8080`. `sprite proxy 5900` with a native VNC client remains the debug path.

A future release may design explicit routing for user applications. That work must account for arbitrary application protocols, authentication, WebSocket paths, cookies, and port discovery. It is not an installer convenience.

## 8. Evidence from v0 and the architecture review

The repository has already proved:

- Xvnc, XFCE, and Firefox run together on the current Ubuntu 26.04 Sprite image;
- noVNC renders the full desktop and handles input, resize, paste, and reconnect;
- the desktop service returns after Sprite suspension and machine restart;
- files survive both lifecycle events; and
- a complete RFB 3.8 handshake succeeds through the Sprites TCP proxy.

The 2026-09-04 architecture review tested `josh-desktop` directly, with a temporary port-8080 echo service removed afterward:

- an authenticated WebSocket upgrade through the private Sprite URL returned 101;
- text and 1 MB binary frames survived the route;
- a WebSocket held idle for four minutes still worked, with an exec session open;
- unauthenticated HTTP and WebSocket requests redirected to Fly's Sprite login;
- the router stripped `Authorization`, forwarded `Origin` unchanged, and added no identity header;
- a forged `Origin` reached the test service;
- an org Bearer token authenticated a plain request to the URL;
- Sprite services run as the `sprite` user;
- `sprite-env info` prints the canonical `sprite_url` from inside the Sprite;
- `/.sprite/api.sock` is mode 0666, serves `/v1/tasks` and `/v1/services`, and returns 404 for `/v1/info`;
- task upsert and delete worked from the `sprite` user with plain curl;
- Xvnc accepts `-rfbunixpath` and `-rfbunixmode`;
- the installed `@fly/sprites` 0.2.2 types `privateAccess`, `httpPort` on `ServiceRequest`, and the full services, filesystem, and policy surface; and
- a second HTTP-port service registration fails with 409.

The review's idle-socket test had an exec session open, so it did not prove that a URL WebSocket alone prevents suspension. M0 settles that.

## 9. Milestones and hard stops

### M0: Platform probe

Use a disposable Go HTTP/WebSocket service on a test Sprite, registered as a service with `--http-port 8080`. Record commands and results in a test record.

Prove:

- `getSprite` reads `urlSettings.auth` and `urlSettings.privateAccess`, `updateURLSettings` sets `privateAccess: admins`, and the server returns the new value;
- the same two fields can be read and set without Node, through `sprite api` or `sprite config`, and the exact commands are recorded for §2.2;
- unauthenticated GET and WebSocket upgrade do not reach the service;
- a browser completes Fly login, loads the page, and opens a same-origin WebSocket;
- the `SameSite` attribute of Fly's session cookie, recorded from the browser;
- the router preserves the exact Origin and canonical Host;
- authenticated text and at least 1 MB binary frames pass;
- the router reaches port 8080 after warm and cold wake, and when loopback listeners open before 8080;
- an attached WebSocket with 20 s pings, no exec or console session open, and no task held either keeps the Sprite active across five minutes or drops in a measured, repeatable way, which decides the `keepalive.task` default;
- with the task hold enabled, the same test stays connected, and the Sprite idles within 90 s of disconnect;
- a WebSocket held for 30 minutes with pings and no RFB traffic stays open through the router;
- the first navigation after `restartSprite` fails or succeeds, with timing, across three trials;
- an idle Xvnc desktop left alone for 15 minutes does or does not blank, with and without `-s 0` and `xset s off`;
- `sprite-env info` reports the same URL as `getSprite`, and where it reads it from;
- only one service can own the HTTP route.

**STOP:** Do not implement the runtime if browser authentication cannot cover the same-origin WebSocket, the router rewrites Origin, or the router cannot reach the service within the browser's retry budget after a warm or cold wake.

### M1: Bridge binary, installer skeleton, static page

Build the bridge with the embedded page, `/healthz`, `/version`, the Origin check, the route contract, and the response policy, but with `/vnc` relaying to a stub. Build the in-Sprite installer with preflight, the install layout, ownership records, and service convergence. Build the outside script with URL assertion, upload, run, and validation.

Prove on a fresh Sprite:

- both entry points install successfully;
- a second run has the same managed checksums and service count and restarts nothing;
- interrupting the installer after each service create, replace, and restart, and before commit, leaves a `pending` record, and the rerun converges to a `committed` state with one definition per service;
- changing only `config.json` restarts the bridge and nothing else;
- foreign HTTP-port service and service-name conflicts cause pre-mutation refusal;
- unsupported OS and architecture fail before mutation;
- release, config, and user files have the specified owners and modes;
- the bridge is the only non-loopback listener;
- the authenticated URL serves every route with the required method, cache, content type, and security headers; and
- missing, null, malformed, HTTP, foreign, alternate-port, and multi-value Origins get 403 before upgrade.

**STOP:** Do not add desktop transport until install reruns are deterministic and the Origin matrix passes.

### M2: Direct desktop transport

Install the desktop service. Relay `/vnc` to the Xvnc Unix socket.

Prove:

- `ss` shows Xvnc on the Unix socket and `127.0.0.1:5900` only, and the bridge on 8080 only;
- a valid same-origin browser reaches the RFB server without a Sprites proxy initialization frame;
- a page loaded inside the desktop's Firefox cannot open `/vnc` on `127.0.0.1:8080`;
- XFCE renders and accepts mouse and keyboard input;
- Firefox launches;
- Ctrl+Alt+Del and paste work;
- resizing the browser changes the remote Xvnc resolution; and
- `sprite proxy 5900` with a native client still works; and
- one thousand `/healthz` requests in a minute, with a viewer attached and then without, neither disconnect nor delay a real viewer and leave Xvnc accepting new connections. M1 covers readiness with the stub; this M2 check uses real Xvnc.

**STOP:** Do not build recovery around a transport that has not passed the security matrix.

### M3: Wake and recovery client

Replace the current connect flow with the v1 state machine, generation cancellation, bounded backoff, offline handling, and explicit stopped state.

Prove:

- navigation from a suspended Sprite reaches `connected` within the 60 s budget;
- a WebSocket that upgrades but never completes RFB negotiation reaches retry rather than hanging;
- restarting Xvnc through the service manager produces bounded retries and recovery;
- restarting the bridge produces bounded retries and recovery;
- browser offline/online and laptop suspend/resume produce no duplicate RFB clients or stale callbacks;
- Reconnect from `stopped-by-user` and Retry from `failed` each create exactly one new attempt;
- a tab hidden for 20 minutes is still connected or recovers within the budget on return;
- Disconnect leaves the page open with no further HTTP requests, sockets, or timers, and the Sprite is observed entering its idle state within the documented delay; and
- an auth failure ends in a Re-authenticate action rather than an endless wake loop.

Record p50 and p95 navigation-to-`connected` and navigation-to-`first-frame` timings for at least five warm, five suspended, and three machine-restart trials. Report machine-restart first-navigation failures as their own count, not as budget failures.

**STOP:** Do not call the runtime self-contained until a suspended Sprite and an interrupted session both recover without a terminal command.

### M4: Fresh install, lifecycle, and upgrade gate

Run both installer entry points against a new supported Sprite rather than the existing hand-tuned one.

Prove:

- the default network policy permits every locked package source;
- the full install and rerun pass;
- a desktop file and a changed XFCE preference survive same-version rerun, upgrade, suspension, and machine restart;
- the desktop and bridge services recover when each child process exits;
- three consecutive idle-window cycles remain connected;
- an idle desktop does not blank or lock after 30 minutes;
- two tabs exhibit the documented shared-session behavior;
- an in-place runtime upgrade reports and ends the active session, then reconnects on the new version;
- `/version` matches the committed release and install-time `dpkg` observations; and
- the Glycin fix from §6.3 is reverified on the fresh image.

**STOP:** An unreachable locked source, an unowned service mutation, or a non-loopback RFB listener blocks release.

### M5: Clean cutover

Once M0 through M4 pass, remove the v0 path rather than retaining a fallback:

- delete `apps/gateway`;
- remove ticket signing and `packages/shared`;
- remove `/api/desktop/status`, `/api/desktop/wake`, and `/api/desktop/ticket`;
- delete the Sprites proxy initialization and acknowledgement code;
- turn `packages/sprite-provision` into the outside installer and acceptance harness rather than deleting it;
- replace gateway and ticket tunnel checks with private-URL and same-origin bridge checks;
- remove Cloudflare bindings, Wrangler config, secrets, setup scripts, and root commands used only by v0; and
- rewrite the README around the two installer entry points and the private Sprite URL.

Keep [SPEC.v0.md](./SPEC.v0.md) as the record of the spike.

Run the final validation from a clean checkout and a fresh Sprite. No Cloudflare account, Worker deployment, custom gateway, or browser-held Sprites token may be required.

## 10. Acceptance criteria

### Installation

- Either entry point turns a fresh supported Sprite into a desktop Sprite.
- Same-version reruns converge without needless service restarts.
- Foreign services and occupied ports cause pre-mutation refusal.
- Release bytes are immutable; Sprite-specific configuration lives under `/etc` and `/var/lib`.
- `/version` identifies the exact release and install-time package observations.
- The outside entry point validates URL settings and external rejection without operator hand-work.

### Security

- The Sprite URL uses private `sprite` authentication with `privateAccess: admins`.
- Every admitted admin is documented as a trusted co-owner of the shared desktop.
- Unauthenticated HTTP and WebSocket requests do not reach the bridge.
- The bridge accepts only the canonical HTTPS Origin for `/vnc`.
- Xvnc listens only on a Unix socket and loopback.
- No browser receives a Sprites API token or custom desktop secret.
- No public URL mode or raw public VNC port is required.
- Responses carry the tested CSP and security headers.

### Desktop

- A logged-in admin opens the Sprite URL and reaches `connected` without running a local proxy.
- Mouse, keyboard, resize, Ctrl+Alt+Del, and paste work.
- Firefox opens from the desktop.
- Files and settings survive suspension and machine restart.
- An attached idle session stays connected across idle windows.
- Desktop or bridge failure recovers through Sprite services and browser retry.
- Disconnect stops retry and permits idle suspension within the documented delay.

### Independence

- The installed runtime serves its own browser application and bridge from one binary.
- Desktop traffic never passes through Cloudflare or a shared gateway.
- The bridge depends on no apt package beyond libc and nothing under `/.sprite/bin`.
- A future control plane can treat the Sprite URL, `/healthz`, and `/version` as its boundary without entering the desktop data path.

## 11. Verification gates

Repository validation during implementation uses the project's real commands:

```sh
pnpm check
pnpm test
pnpm build
pnpm format:check
go vet ./...
go test ./...
bash -n sprite/*.sh
shellcheck sprite/*.sh
```

The cutover may change the workspace shape, but it must preserve equivalent type, test, build, format, Go vet, Bash syntax, and ShellCheck gates. Remove `pnpm typegen` when no Wrangler bindings remain.

Every milestone also runs its Sprite acceptance checks. A local green build cannot establish URL auth, Origin forwarding, wake behavior, socket binding, keepalive, or persistence.

Before each implementation milestone, compare its in-scope files with baseline change `xrkyqoky`. If runtime behavior, Sprite image, package availability, SDK surface, or URL-router semantics have drifted, rerun M0 and update this spec before continuing.

## 12. Deferred work

v1 does not include:

- a standalone control tool in any language;
- public Sprite URLs;
- custom passwords, access keys, or signed session tickets;
- Cloudflare Workers or Cloudflare Access;
- a fleet control plane;
- embedding the private Sprite page in a cross-origin iframe;
- public raw VNC or a native-client transport beyond `sprite proxy 5900`;
- reverse proxying another application through the Sprite's one HTTP URL;
- multi-user identity, session ownership, or isolated displays;
- audio;
- remote-to-local clipboard sync;
- agents or computer-use automation;
- checkpoints in the browser UI;
- service workers;
- a rollback command or release cleanup; or
- KasmVNC, xpra, or Guacamole.

A later fleet manager may list, create, install, upgrade, and open desktop Sprites through the SDK and the private URL. It must not proxy noVNC assets, WebSocket messages, or RFB traffic. If a second operator needs a downloadable tool, a Go outside tool built on `superfly/sprites-go` can share a module with the bridge; that is a v2 decision.

## 13. Alternatives considered

### Keep the Cloudflare gateway

Rejected. It keeps an organization credential in a shared data-plane service, adds another long-lived hop, and leaves each desktop dependent on infrastructure outside its Sprite. v0 proved the transport; v1 removes it.

### nginx plus websockify as the bridge

Rejected. The earlier draft of this spec chose them to avoid application code. In practice they are three configuration surfaces with known traps: nginx closes idle WebSocket upstreams after 60 seconds by default, running nginx unprivileged needs explicit prefix and temp paths, Debian package hooks try to start a root daemon, websockify on a loopback port needs its own Origin check to defend against a page inside the desktop's Firefox, and neither can send pings, hold a task, or report Xvnc readiness. A bridge of a few hundred lines of Go removes all of that and is smaller than the test matrix it replaces.

### Add custom tickets on a public Sprite URL

Rejected for v1. Fly's private URL already authenticates organization members and tokens. Public mode would require a credential lifecycle, rate limiting, login UI, recovery, and session storage.

### Build a standalone control tool

Rejected. Fly's CLI and SDKs own authentication, Sprite selection, exec, URL configuration, service inspection, and port forwarding. The outside installer is a repository script over Fly's SDK, not a tool of its own. The in-Sprite installer is Bash so it needs nothing beyond `sprite exec`.

### Use the Sprites TCP proxy for normal access

Rejected as a product path. It remains available to an owner for debugging, but the browser desktop must work from the private Sprite URL alone.

### Exact apt version pins

Rejected. Ubuntu removes superseded package versions from its pockets, so exact pins fail on the distribution's schedule. Sources and keys are locked; versions are observed and recorded.

### Usable-canvas predicate as a retry trigger

Rejected. A blank or dark framebuffer after RFB negotiation is not a transport failure. Using it to tear down a session would loop on a blanked or legitimately dark desktop. The predicate survives as a measurement only.

### KasmVNC

Deferred. It could replace Xvnc, the bridge, and noVNC with a web-focused server, but it adds an unverified package on Ubuntu 26.04, a different server model, and another stack to learn. Reconsider only if TigerVNC's encoding proves unacceptable in M3 or M4 measurements.

### Reverse proxy user applications beside the desktop

Deferred. A generic application proxy would turn this installer into a routing platform and create unresolved path, cookie, auth, and WebSocket conflicts. v1 reserves the Sprite URL for the desktop.

## 14. References

- [v0 spike specification](./SPEC.v0.md)
- [v0 implementation record](./README.md)
- [Working with Sprites](https://docs.sprites.dev/working-with-sprites/)
- [Keeping a Sprite Running](https://docs.sprites.dev/keeping-sprites-running/)
- [Services](https://docs.sprites.dev/concepts/services/)
- [Networking](https://docs.sprites.dev/concepts/networking/)
- [Lifecycle and Persistence](https://docs.sprites.dev/concepts/lifecycle/)
- [Sprites JavaScript SDK](https://github.com/superfly/sprites-js)
- [Sprites Go SDK](https://github.com/superfly/sprites-go)
- [noVNC API](https://novnc.com/noVNC/docs/API.html)
- [TigerVNC](https://tigervnc.org/)
