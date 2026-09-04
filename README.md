# Sprite Desktop

A single-user prototype that renders an XFCE desktop from a Fly Sprite in a browser. A SvelteKit Worker owns the UI and short-lived tickets. A plain gateway Worker adds the Sprites token to a WebSocket upgrade and then leaves the RFB bytes alone.

The current implementation follows the completed [v0 spike spec](./SPEC.v0.md). [SPEC.v1.md](./SPEC.v1.md) plans the self-contained Sprite runtime that will replace the gateway path. The [v1 platform probe record](./probes/M0.md) contains the automated M0 results; browser cookie authentication is still pending, so the v0 path remains in place.

## Security boundary

TigerVNC runs with `-SecurityTypes None` and listens only on `localhost:5900` inside the sprite. The Sprites proxy token and the signed gateway ticket are the only transport credentials in this spike.

Never make the sprite public or expose port 5900. Do not treat `url_settings.auth: public` as safe because the HTTP proxy and TCP proxy are separate paths. A deployed copy also needs Cloudflare Access on both the web and gateway hostnames.

## Packages

- `apps/web` — SvelteKit 2/Svelte 5 UI on `@sveltejs/adapter-cloudflare`
- `apps/gateway` — framework-free Worker that verifies a 60-second HMAC ticket and passes through the Sprites WebSocket
- `packages/shared` — Zod contracts and WebCrypto ticket signing
- `packages/sprite-provision` — idempotent `@fly/sprites` provisioning CLI
- `sprite` — the package installer and foreground desktop service
- `scripts` — local secret setup and an RFB tunnel check

## Requirements

- Node 24 or newer
- pnpm 9 or newer
- a `SPRITES_TOKEN` in the shell
- Cloudflare credentials only for deployment

## Run the spike

Install the workspace and provision the sprite:

```sh
pnpm install
pnpm provision --name josh-desktop
```

The provision command creates the sprite when missing, uploads both scripts, installs TigerVNC, XFCE, and Firefox, registers the `desktop` service, and waits for port 5900.

Check the raw Sprites TCP relay:

```sh
pnpm tunnel --name josh-desktop
```

A passing check prints `Completed an RFB 003.008 handshake with josh-desktop.`

Create matching ignored `.dev.vars` files for both Workers, then start them:

```sh
pnpm setup:local
pnpm dev
```

Open <http://localhost:5173>. The gateway listens on port 8788. `pnpm dev:web` and `pnpm dev:gateway` run them separately. If local workerd cannot pass through the upstream socket, use `pnpm --filter @sprite-desktop/gateway dev:remote`.

After connecting, Xvnc matches the viewer frame and changes resolution again when the browser window changes.

`setup:local` copies `SPRITES_TOKEN` from the shell and creates a shared random `TICKET_SECRET`. Set `TICKET_SECRET` first if you need a stable value.

## Checks

```sh
pnpm typegen
pnpm check
pnpm test
pnpm build
pnpm format:check
```

`pnpm typegen` uses each app’s `.dev.vars.example` to include secret names in generated Worker bindings without storing secret values.

## Deployment

Update `SPRITE_NAME` and `GATEWAY_WS_URL` in `apps/web/wrangler.jsonc`, then set secrets without putting their values on the command line:

```sh
cd apps/gateway
pnpm wrangler secret put SPRITES_TOKEN
pnpm wrangler secret put TICKET_SECRET
pnpm deploy

cd ../web
pnpm wrangler secret put SPRITES_TOKEN
pnpm wrangler secret put TICKET_SECRET
pnpm deploy
```

Put both hostnames behind Cloudflare Access. Test `/` and `/vnc` without a session before calling the deployment usable.

## Spike record

### Milestones

| Milestone                | Result                                                                                                                                                    |
| ------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| M0 — direct tunnel       | Passed on 2026-09-04: proxy acknowledged the target and completed an RFB 3.8 handshake                                                                    |
| M1 — sprite desktop      | Passed on 2026-09-04: a full `xfce4-session` and Firefox run on the actual Ubuntu 26.04 image; the service restarts and listens on `:5900`                |
| M2 — gateway passthrough | Passed locally: `pnpm tunnel --web http://localhost:5173` received the proxy acknowledgement and completed an RFB handshake through workerd               |
| M3 — rendered desktop    | Passed locally: noVNC rendered the XFCE panel, wallpaper, desktop icons, and applications menu; Firefox launched from that menu; input and reconnect work |
| M4 — persistence         | Passed with a platform caveat: the test file survived suspension and an explicit machine restart; the service returned and completed an RFB handshake     |
| M5 — deploy and Access   | Deferred                                                                                                                                                  |

### Wake timing

| Wake path                         | Observed time                                                                           |
| --------------------------------- | --------------------------------------------------------------------------------------- |
| Warm, Connect to noVNC `connect`  | 286 ms, 290 ms, 290 ms                                                                  |
| Suspended, proxy to RFB handshake | 6.57 s; browser first-frame timing was not captured                                     |
| Cold                              | Platform did not report this state; an explicit machine restart recovered in about 45 s |

Warm timings include the local browser automation command overhead. The suspended timing ends after RFB negotiation, so it is a lower bound for a usable frame.

### Known unknowns

| Item                                                    | Result                                                                                                                                                                                                               |
| ------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| U1 — end-client init through passthrough                | Resolved: yes. The proxy emits one checked text acknowledgement before binary RFB data; noVNC attaches first while a temporary handler removes only that frame, which avoids losing the immediately following banner |
| U2 — proxy wakes a hibernated sprite                    | Resolved for the observed suspended state: opening the proxy woke it without `/wake`; the raw state was `needs_repair`, not `hibernated`                                                                             |
| U3 — local workerd passes through an upstream WebSocket | Resolved: yes                                                                                                                                                                                                        |
| U4 — XFCE installs without systemd trouble              | Resolved: yes. Ubuntu's proposed Glycin 2.1.5 SRU and its scoped `SNAP` path avoid the unsupported nested bubblewrap sandbox, so the full session stays up                                                           |
| U5 — fallback bridge CPU limits                         | Not exercised because passthrough works                                                                                                                                                                              |
| U6 — region latency from Tuscaloosa                     | Pending                                                                                                                                                                                                              |

### Surprises

- `@novnc/novnc` 1.7 exports `RFB` from the package root. The old `lib/rfb.js` path in the draft is no longer exported.
- `@fly/sprites` 0.2.2 uses `getSprite`, `createSprite`, `filesystem().writeFile`, `spawn`, and `execFile`; `sprite(name)` only creates a local handle.
- The current Sprite image is Ubuntu 26.04 LTS, not the Ubuntu 25.10 image listed in the draft. Its release Glycin 2.1.1 crashes GTK clients because the Sprite cannot create Glycin's nested bubblewrap sandbox. The provisioner pins only Glycin's verified 2.1.5 SRU from `resolute-proposed`; setting `SNAP` for the desktop takes Ubuntu's patched path, which skips bubblewrap while retaining seccomp. The full `xfce4-session`, panel, desktop, wallpaper, icons, and applications menu then stay up.
- The Sprites proxy sends `{"status":"connected","target":"10.0.0.1:5900"}` before the RFB banner. noVNC attaches before the init frame while a temporary handler filters that acknowledgement; attaching afterward can lose the banner between browser tasks.
- Closing a sanity tunnel after only reading the RFB banner counts as a failed VNC login and can blacklist the proxy IP. The `pnpm tunnel` check now finishes the no-auth RFB handshake before closing.
- Repeated `/check` requests appeared to keep the Sprite awake. After five quiet minutes, one check returned `{"status":"needs_repair","reason":"machine in suspended state"}`. The next proxy connection woke it and preserved both the filesystem and running process IDs. A later explicit machine restart replaced the process, restarted the `desktop` service, preserved `Desktop/persistence-proof.txt`, and restored the RFB endpoint.
- A passthrough ticket authorizes access to the sprite proxy, not only its signed `port`: a caller can change the init frame before the gateway sees it. This is accepted for the single-user spike; enforcing the port requires the Worker-in-the-loop bridge.
- Wrangler can generate secret binding types from `.dev.vars.example` with `--env-file`, which avoids a handwritten `Env` interface.
