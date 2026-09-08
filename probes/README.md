# M0 platform probe

> **HISTORICAL:** This probe belongs to the former Go/VNC design. The commands below are preserved evidence, not current project instructions. See [`socket-local`](./socket-local/) for the active local probe.

Disposable HTTP/WebSocket echo service for the M0 checks in `SPEC.v1.md`. This is test code, not the desktop runtime. See [M0.md](./M0.md) for measured results, commands, and the remaining browser-auth gate.

Build and run:

```sh
cd probes
CGO_ENABLED=0 go build -o /tmp/sprite-desktop-m0 .
/tmp/sprite-desktop-m0 -origin https://SPRITE_HOST -ping-interval 20s
```

The service listens on `:8080`. Open the installed Sprite URL in a browser. The page connects to same-origin `/echo`, then checks a text frame and a 1 MiB binary frame. Set `-ping-interval 0` (the default) for the no-ping control. `/healthz` reports probe counters and the latest Origin/Host observations; it never records cookies or authorization values.

`-task-hold` enables the task-held trial using the verified Tasks API over `/.sprite/api.sock`. It upserts `desktop-m0-hold` with a 90-second expiry while viewers are attached and deletes it after the last viewer leaves. Leave this flag off for the pings-only trial.

Local checks:

```sh
go -C probes test -race ./...
go -C probes vet ./...
pnpm exec tsc -p probes/tsconfig.json
```

The Go probe and the TypeScript acceptance client use real sockets. `blanking.py` queries X11 screen-saver state on the test Sprite; it is not part of the installed product.
