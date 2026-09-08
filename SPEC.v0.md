# Spike: Agent Desktop on Sprites, rendered through Cloudflare

> **HISTORICAL:** This document records the discarded v0 Cloudflare/VNC design. Its commands and requirements are not instructions for the current Socket implementation. See [README.md](./README.md) for the current project.

**Status:** spec for a throwaway-quality prototype. Optimize for "does this work end to end," not for polish.
**Owner:** Josh (single user for the spike; no multi-tenancy).

## 1. Goal

A browser tab, served from a Cloudflare Worker, that shows a live, interactive Linux desktop (xfce) running inside a Fly Sprite. The user can click, type, and open a browser inside that desktop. The Sprite sleeps when the tab closes and comes back with its filesystem intact.

Out of scope for the spike: any agent. No LLM, no computer-use loop, no xdotool automation, no multi-user, no billing, no Connectors. The point is to prove the rendering/transport path so an agent can be dropped onto the same desktop later.

## 2. Verified platform facts (do not re-research; do verify in code)

These were checked against current docs on 2026-09-04. Anything not listed here is an unknown — see §9.

### Sprites (Fly)
- REST base: `https://api.sprites.dev`, auth is `Authorization: Bearer $SPRITES_TOKEN` on every request.
- Create: `POST /v1/sprites` `{ "name": "...", "url_settings": { "auth": "sprite" } }`. Get: `GET /v1/sprites/:name`. Health: `GET /v1/sprites/:name/check` → `status` ∈ `healthy | hibernated | unhealthy | error`.
- TCP proxy: `WSS /v1/sprites/:name/proxy`. After the WebSocket upgrade, the client sends **one JSON text frame** `{"host":"localhost","port":5900}`. From then on the socket is a **raw binary TCP relay** to that port — no framing, no prefixes. This is the whole reason the design below works: the Worker (or browser) can speak RFB over this socket directly.
- HTTP proxy: requests to `https://api.sprites.dev/v1/sprites/:name/*` are forwarded to port 8080 in the sprite. Fallback path only (§9).
- Exec: `sprite exec -- <cmd>` (CLI) or the JS SDK `sprite.exec(cmd)`. Sprites run Ubuntu 25.10 with `sudo` available; `apt install` works.
- Services: `sprite-env services create <name> --cmd <bin> --args <...>` (run *inside* the sprite). Services auto-restart on every wake. Anything started via `sprite exec`/`console` dies on sleep.
- Lifecycle: running → warm within seconds of no activity; warm → cold later. Filesystem persists across both. RAM/processes do not. Warm wake ≈ 100–500 ms, cold ≈ 1–2 s. Warm and cold are not billed.
- Activity that keeps a sprite awake includes an open TCP connection — so an open proxy tunnel keeps it running while the desktop is being viewed.
- 100 GB persistent storage per sprite. Checkpoints = filesystem snapshots (`sprite checkpoint create`, `sprite restore <id>`).
- JS SDK: `@fly/sprites`, `new SpritesClient(token)`, `client.sprite(name)`, `client.createSprite(name)`, `sprite.exec(...)`.

### noVNC
- `@novnc/novnc` on npm. `new RFB(targetEl, urlOrChannel, options)`. The second argument may be a WebSocket URL **or an existing `WebSocket` / `RTCDataChannel` object** (since 1.3). We rely on the object form.

### Cloudflare Workers
- A Worker can `fetch()` an upstream with `Upgrade: websocket` and either (a) return that Response directly so Cloudflare pipes the socket without the Worker touching bytes, or (b) call `response.webSocket.accept()` and pump messages itself.
- `@sveltejs/adapter-cloudflare` owns the Worker `fetch` export. Class-based exports (Durable Objects) need extra wiring; this spike avoids needing them by putting the WebSocket bridge in a separate plain Worker.

## 3. Architecture

```
Browser (SvelteKit page + noVNC)
   │  1. GET /api/desktop/status, POST /api/desktop/wake   (HTTP → web app)
   │  2. GET /api/desktop/ticket → short-lived signed ticket
   │  3. wss://gateway/vnc?ticket=...                        (WebSocket → gateway)
   ▼
apps/web  (SvelteKit on Workers)          apps/gateway  (plain Worker)
   - UI, shadcn-svelte                        - verifies ticket
   - status/wake via Sprites REST             - opens wss://api.sprites.dev/v1/sprites/<name>/proxy
   - mints tickets (HMAC, 60 s TTL)             with Bearer token
                                              - returns the upstream 101 to the browser (passthrough)
                                                          │
                                                          ▼
                                            Sprite (Ubuntu 25.10)
                                              Service "desktop":
                                                Xvnc :1  (TigerVNC, localhost:5900, no auth)
                                                xfce4-session on DISPLAY=:1
                                                firefox available in the session
```

Why two Workers: keeps the SvelteKit adapter question out of the critical path. The gateway is ~100 lines and has no framework. Both deploy from one pnpm workspace. (Collapsing into one Worker later is a known option via `@joshthomas/sveltekit-adapter-cloudflare`, not for this spike.)

Why passthrough: the browser sends the proxy init frame itself (`{"host":"localhost","port":5900}`) as the first message, then hands the same `WebSocket` object to noVNC's `RFB`. The Worker only injects the `Authorization` header and never sees a VNC frame, so Worker CPU time is ~zero regardless of session length. If passthrough fails (§9 U1), the fallback is the Worker-in-the-loop bridge in §6.3.

Why TigerVNC `Xvnc` instead of Xvfb + x11vnc: one process is both the X server and the VNC server. Fewer moving parts, and it supports `-SecurityTypes None -localhost yes` so the only way in is the Sprites proxy, which is already authenticated.

Deployment for the spike: **none until the last milestone.** Both Workers run locally under `wrangler dev` (workerd); the sprite is the only remote component. Everything that's uncertain — the proxy relay, wake behavior, service restart, persistence, noVNC over the tunnel — is exercised identically from local workerd, so there's nothing to learn from deploying early. Cloudflare Access is the closing stretch goal (M5), configured in the dashboard, no code. The ticket scheme exists regardless so the gateway can't be hit with a bare WebSocket and so the browser never holds the Sprites token.

## 4. Repository layout

```
agent-desktop/
  package.json            # pnpm workspace root, scripts fan out
  pnpm-workspace.yaml
  tsconfig.base.json
  apps/
    web/                  # SvelteKit + shadcn-svelte, adapter-cloudflare
    gateway/              # plain Worker: WebSocket bridge + ticket verify
  packages/
    shared/               # zod schemas + types shared by web/gateway (ticket payload, status shape)
    sprite-provision/     # TS CLI: create sprite, upload + run provision script, register service
  sprite/
    provision.sh          # idempotent; runs inside the sprite via exec
    desktop.sh            # the Service command: starts Xvnc + xfce
```

Tooling: pnpm 9+, TypeScript strict, Vite, Wrangler 4. Biome or ESLint+Prettier — pick one, don't bikeshed. No test framework beyond `vitest` for the ticket signing.

## 5. Sprite side

### 5.1 `sprite/provision.sh` (idempotent, run as the `sprite` user with sudo)

```bash
#!/usr/bin/env bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  tigervnc-standalone-server tigervnc-common \
  xfce4 xfce4-terminal dbus-x11 \
  fonts-dejavu-core xdg-utils

# Firefox from Mozilla's apt repo (Ubuntu's own firefox is a snap; snapd is not assumed to exist here).
if ! command -v firefox >/dev/null; then
  sudo install -d -m 0755 /etc/apt/keyrings
  curl -fsSL https://packages.mozilla.org/apt/repo-signing-key.gpg \
    | sudo tee /etc/apt/keyrings/packages.mozilla.org.asc >/dev/null
  echo "deb [signed-by=/etc/apt/keyrings/packages.mozilla.org.asc] https://packages.mozilla.org/apt mozilla main" \
    | sudo tee /etc/apt/sources.list.d/mozilla.list >/dev/null
  printf 'Package: *\nPin: origin packages.mozilla.org\nPin-Priority: 1000\n' \
    | sudo tee /etc/apt/preferences.d/mozilla >/dev/null
  sudo apt-get update && sudo apt-get install -y firefox
fi

mkdir -p "$HOME/.vnc" "$HOME/.local/bin"
install -m 0755 /dev/stdin "$HOME/.local/bin/desktop.sh" <<'EOF'
#!/usr/bin/env bash
# Service entrypoint. Must stay in the foreground.
set -euo pipefail
export DISPLAY=:1
export HOME=/home/sprite
# Clean up a stale lock from a previous (killed) session.
rm -f /tmp/.X1-lock /tmp/.X11-unix/X1
Xvnc :1 -geometry 1440x900 -depth 24 \
  -rfbport 5900 -localhost yes -SecurityTypes None \
  -AlwaysShared -desktop "sprite" &
XVNC_PID=$!
# Wait for the X socket.
for _ in $(seq 1 50); do [ -S /tmp/.X11-unix/X1 ] && break; sleep 0.1; done
dbus-launch --exit-with-session startxfce4 &
wait $XVNC_PID
EOF
```

Notes for the implementer:
- Do **not** set a VNC password. Access control is the Sprites token + Cloudflare Access. Document this in the README so nobody flips `url_settings.auth` to `public` and assumes the desktop is safe (the HTTP URL is unrelated to the proxy, but the point stands).
- Resolution is fixed at 1440×900 for the spike. noVNC `resizeSession` + Xvnc `RandR` is a follow-up.
- If `xfce4` pulls in something that wants systemd and fails, fall back to `xfce4-session` + `xfwm4` + `xfce4-panel` explicitly rather than the metapackage.

### 5.2 Register the Service (run inside the sprite, once)

```bash
sprite-env services create desktop --cmd /home/sprite/.local/bin/desktop.sh
```

Verify with `sprite-env services list` and `ss -ltnp | grep 5900`.

### 5.3 `packages/sprite-provision` (TS CLI)

Thin wrapper over `@fly/sprites` so the whole sprite setup is one command from the repo:

```
pnpm provision --name josh-desktop
```

Steps: `createSprite` if missing → write `provision.sh` into `/home/sprite/provision.sh` (SDK filesystem API or `exec` with heredoc) → `exec bash provision.sh` streaming output → `exec sprite-env services create ...` → `exec ss -ltnp` and assert `:5900` is listening → print the sprite name and `/check` status. Idempotent on rerun. Read `SPRITES_TOKEN` from env only.

If the SDK's file-write API turns out awkward, `sprite exec -- bash -c "cat > ~/provision.sh <<'EOF' ... EOF"` is fine for a spike.

## 6. Cloudflare side

### 6.1 `packages/shared`

```ts
// ticket.ts
export const TicketPayload = z.object({
  sub: z.string(),          // user id; for the spike a constant "josh"
  sprite: z.string(),       // sprite name
  port: z.number().int(),   // 5900
  exp: z.number().int(),    // unix seconds, now + 60
  nonce: z.string(),
});
export type TicketPayload = z.infer<typeof TicketPayload>;
// sign(payload, secret) → base64url(JSON) + "." + base64url(HMAC-SHA256)   (WebCrypto only; no Node deps)
// verify(ticket, secret) → TicketPayload | null   (constant-time compare, checks exp)

// status.ts
export const DesktopStatus = z.object({
  sprite: z.string(),
  health: z.enum(["healthy", "hibernated", "unhealthy", "error", "unknown"]),
  checkedAt: z.string(),
});
```

### 6.2 `apps/web` (SvelteKit)

Bindings / env (via `platform.env`):
- `SPRITES_TOKEN` (secret)
- `SPRITE_NAME` (var, e.g. `josh-desktop`)
- `TICKET_SECRET` (secret; shared with gateway)
- `GATEWAY_WS_URL` (var, e.g. `wss://desktop-gw.<domain>/vnc`)

Routes:
- `GET /api/desktop/status` → calls `GET https://api.sprites.dev/v1/sprites/${SPRITE_NAME}/check` server-side, returns `DesktopStatus`. Never proxies the token.
- `POST /api/desktop/wake` → for the spike, wake by opening and immediately closing a proxy tunnel is overkill; use `exec` of `true` via the REST/WS exec API **only if** U2 in §9 shows the proxy alone doesn't wake it. Otherwise this route is a no-op that returns `status`.
- `GET /api/desktop/ticket` → mints a 60-second ticket for `{ sub: "josh", sprite: SPRITE_NAME, port: 5900 }`. Returns `{ ticket, wsUrl: GATEWAY_WS_URL + "?ticket=" + ticket }`.

Page `/` (single page, shadcn-svelte components):
- Header: sprite name, health badge (polls `/api/desktop/status` every 10 s while disconnected, stops polling while connected).
- Main: a `Card` containing a `div` that noVNC attaches to. Fill available height; `scaleViewport: true`, `clipViewport: false`.
- Toolbar (`Button`s): **Connect**, **Disconnect**, **Send Ctrl+Alt+Del**, **Paste** (reads `navigator.clipboard.readText()` → `rfb.clipboardPasteFrom`).
- State machine in a Svelte 5 rune store: `idle → fetching-ticket → opening-socket → sending-init → attaching-rfb → connected → disconnected(reason)`. Show the state and any error string; this is the debugging surface for the spike.

Connect flow (client, `src/lib/vnc/connect.ts`):

```ts
import RFB from "@novnc/novnc/lib/rfb.js";

export async function connect(target: HTMLElement) {
  const { wsUrl } = await fetch("/api/desktop/ticket").then(r => r.json());
  const ws = new WebSocket(wsUrl);
  ws.binaryType = "arraybuffer";
  await new Promise<void>((res, rej) => { ws.onopen = () => res(); ws.onerror = () => rej(new Error("ws open failed")); });
  ws.send(JSON.stringify({ host: "localhost", port: 5900 }));   // Sprites proxy init frame
  // Hand the live socket to noVNC. It must not have received any RFB bytes yet;
  // the proxy doesn't send anything until the relay is established, so this is safe.
  const rfb = new RFB(target, ws, { shared: true });
  rfb.scaleViewport = true;
  rfb.addEventListener("connect", ...); rfb.addEventListener("disconnect", e => ... e.detail.clean ...);
  return rfb;
}
```

If the proxy ever emits an ack frame after init (U1), buffer: attach a temporary `onmessage` that discards exactly one text frame, then construct `RFB`. Confirm from the first spike run and delete the dead path.

### 6.3 `apps/gateway` (plain Worker)

Bindings: `SPRITES_TOKEN`, `TICKET_SECRET`.

```ts
export default {
  async fetch(req: Request, env: Env): Promise<Response> {
    const url = new URL(req.url);
    if (url.pathname !== "/vnc") return new Response("not found", { status: 404 });
    if (req.headers.get("Upgrade") !== "websocket") return new Response("expected websocket", { status: 426 });

    const payload = await verify(url.searchParams.get("ticket") ?? "", env.TICKET_SECRET);
    if (!payload) return new Response("bad ticket", { status: 403 });

    // Passthrough: forward the upgrade to the Sprites proxy with our token; return its 101 as-is.
    const upstream = await fetch(`https://api.sprites.dev/v1/sprites/${payload.sprite}/proxy`, {
      headers: {
        Upgrade: "websocket",
        Connection: "Upgrade",
        Authorization: `Bearer ${env.SPRITES_TOKEN}`,
        // forward Sec-WebSocket-Protocol only if the client sent one
      },
    });
    if (upstream.status !== 101 || !upstream.webSocket) {
      return new Response(`upstream ${upstream.status}: ${await upstream.text()}`, { status: 502 });
    }
    return new Response(null, { status: 101, webSocket: upstream.webSocket });
  },
};
```

Fallback (only if U1 fails): Worker-in-the-loop bridge.

```ts
const [client, server] = Object.values(new WebSocketPair());
server.accept();
const up = upstream.webSocket; up.accept();
up.send(JSON.stringify({ host: "localhost", port: payload.port }));   // Worker sends init instead of browser
server.addEventListener("message", e => up.send(e.data));
up.addEventListener("message", e => server.send(e.data));
for (const [a, b] of [[server, up], [up, server]]) {
  a.addEventListener("close", ev => b.close(ev.code, ev.reason));
  a.addEventListener("error", () => b.close(1011, "peer error"));
}
return new Response(null, { status: 101, webSocket: client });
```

In fallback mode the browser must **not** send the init frame; gate that on a `mode` field in the ticket response so the client doesn't need a redeploy to switch.

### 6.4 `wrangler.jsonc` essentials

- `apps/web`: `main: .svelte-kit/cloudflare/_worker.js`, `assets` per adapter docs, `compatibility_date` current, `vars: { SPRITE_NAME, GATEWAY_WS_URL }`, secrets via `wrangler secret put`.
- `apps/gateway`: `main: src/index.ts`, custom domain `desktop-gw.<domain>`, secrets `SPRITES_TOKEN`, `TICKET_SECRET`.
- Both: no `nodejs_compat` needed if the shared package sticks to WebCrypto.

## 7. Local development (this is the primary environment for M0–M4)

- `pnpm dev:web` → `vite dev` with `getPlatformProxy` for env. Point `GATEWAY_WS_URL` at `ws://localhost:8788/vnc`.
- `pnpm dev:gateway` → `wrangler dev --port 8788` (local workerd). Outbound WebSocket upgrade to `api.sprites.dev` and returning the upstream `webSocket` are expected to work locally; if U3 proves otherwise, `wrangler dev --remote` for the gateway is the fallback, still no deploy.
- The sprite is always remote; there is no local sprite. `SPRITES_TOKEN` lives in `.dev.vars` (gitignored) for both apps.

## 8. Milestones and acceptance criteria

Do them in order; each one has a hard stop that de-risks the next.

**M0 — Tunnel sanity (no code in repo yet, 30 min).**
From a laptop: `websocat -b "wss://api.sprites.dev/v1/sprites/<name>/proxy" -H "Authorization: Bearer $SPRITES_TOKEN"`, send the init JSON for port 5900 after M1 is done, and confirm the RFB server banner `RFB 003.008` comes back. This is the single most important check in the whole spike.

**M1 — Sprite has a desktop.**
`pnpm provision` completes; `sprite exec -- ss -ltnp` shows `:5900`; after `sprite exec -- pkill Xvnc`, the Service restarts it within a few seconds; after letting the sprite go warm and waking it, `:5900` is listening again without intervention.

**M2 — Gateway passthrough.**
Browser devtools console on any page: open a `WebSocket` to the gateway with a ticket, send the init frame, receive the `RFB 003.008` banner as binary. Ticket with a bad signature or past `exp` → 403. Missing `Upgrade` → 426.

**M3 — Rendered desktop.**
`/` shows the xfce desktop in the card; mouse and keyboard work; Firefox launches and loads a page. Disconnect button cleanly tears down; reconnect works without reload.

**M4 — Persistence loop.**
Create a file on the desktop, close the tab, wait until `/check` reports `hibernated`, reopen, connect: the file is there, and the Service brought the desktop back. Record warm and cold wake-to-first-frame times in the README.

**M5 — Deploy + Access (stretch, closes the spike).**
`wrangler deploy` both Workers to custom hostnames; Cloudflare Access application covering both. Unauthenticated request to `/` and to `/vnc` both redirect/deny; authenticated flow reproduces M3. (Access is manual dashboard config; no code.)

Definition of done for the spike: M4 passes locally and the README has the numbers, the list of things that surprised us, and which of the §9 unknowns resolved which way. M5 is the nice-to-have that turns it into something usable from a phone.

## 9. Known unknowns — resolve these first, in this order

- **U1. Does the Sprites proxy tolerate the init frame coming from the end client through a passthrough Worker?** Expected yes (it's the same socket). If the proxy requires the init before some timeout or rejects a passthrough for header reasons, switch to §6.3 fallback. Also confirm whether it sends any acknowledgement frame after init.
- **U2. Does opening the proxy tunnel wake a hibernated sprite by itself?** Docs say URL requests wake it; the proxy isn't explicitly listed. If not, `wake` route calls the exec API with `true` and polls `/check` before the client connects.
- **U3. Does Cloudflare `wrangler dev` (local) support returning an upstream `webSocket` in a Response?** If not, develop the gateway with `--remote`.
- **U4. Does `xfce4` install cleanly on the Sprites Ubuntu 25.10 image without systemd interaction?** If the metapackage fights, install components explicitly (§5.1 note).
- **U5. Worker CPU limits on long WebSocket sessions.** Passthrough should be immune. If we end up in fallback mode, watch for CPU-limit disconnects on long sessions; the fix is a Durable Object, which is explicitly deferred.
- **U6. Region.** No region selection found in Sprites docs. Measure RTT from Tuscaloosa to the sprite over the tunnel and note it; VNC over ~40 ms vs ~100 ms is the difference between fine and annoying.

## 10. Explicitly deferred (write down, don't build)

- Agent loop on the desktop (xdotool/scrot/Playwright; separate X displays per bot).
- Multi-user: sprite-per-user keyed off the Access identity; ticket `sub` already carries the user id for this.
- Single-Worker deployment via the adapter fork; Durable Object per session with hibernation.
- Dynamic resize (`resizeSession` + Xvnc RandR), clipboard sync from remote → local, audio.
- Checkpoint/restore buttons in the UI (API exists; trivial once M4 is done).
- Connectors for credentials inside the sprite.
- KasmVNC/WebP-quality encoding if TigerVNC's Tight encoding looks bad over the tunnel.

## 11. References

- Sprites REST: https://docs.sprites.dev/api/rest
- Sprites TCP proxy: https://sprites.dev/api/sprites/proxy
- Sprites lifecycle/services/idle: https://docs.sprites.dev/working-with-sprites/
- noVNC API (RFB accepts a WebSocket object): https://novnc.com/noVNC/docs/API.html
- SvelteKit adapter-cloudflare: https://svelte.dev/docs/kit/adapter-cloudflare
- shadcn-svelte: https://www.shadcn-svelte.com
