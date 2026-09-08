# Reproducible local Socket verification

`run.sh` tests the current paired release binaries and `apps/web` build in owned local processes. It builds a fresh image from [`Dockerfile`](./Dockerfile), pinned to Ubuntu resolute digest `sha256:2260313b31c8c011cd2eebe728008efac1b3982be73eb71348ea2648d2c0e09b`. It does not require a retained local image.

Run both rates at the default 16,000 kbps from the repository root:

```sh
probes/socket-local/run.sh
```

Select one or both rates and either controlled bitrate:

```sh
SOCKET_TEST_BITRATE=8000 probes/socket-local/run.sh 30 60
SOCKET_TEST_BITRATE=16000 probes/socket-local/run.sh 60
```

The only accepted rates are 30 and 60; the only accepted bitrates are 8,000 and 16,000 kbps. The run builds the web app and Rust workspace, runs seven ignored FFmpeg media tests, builds the Docker image, and starts each requested rate. The media tests include SSRC generation bounds from 1 through `i32::MAX`.

Each container and headless Chromium unit has a 15-minute limit. Chromium runs with `--disable-gpu`, an isolated profile, and a separate user-systemd cgroup. CPU values come from cumulative `usage_usec` differences for the whole container and the whole browser cgroup. The container includes qterminal and the moving fixture. These values are not comparable to the older headed-Chrome Sprite benchmark.

Each rate checks:

- an attached idle session for at least 20 seconds;
- an instrumentation-free 45-second quiet window with zero decoded and drawn frames;
- a separate 45-second moving-fixture window;
- 35 seconds with `document.hidden === true`, followed by return;
- a real Docker network disconnect while Shift is held, local key release while offline, reconnect with a new controller, and lowercase `a` in the terminal fixture;
- 1280×720 and 1920×1080 requests through the browser resize path, with full native RGBA canvas dimensions;
- stable `canvas.toDataURL('image/png')` captures while matching `grim` source hashes stay unchanged before and after capture;
- decoded I444, sRGB transfer, BT.709 matrix and primaries, and full-range color;
- RGB24 PSNR for each whole image and its four quadrant crops;
- absence of the late `video metadata/RTP correlation stalled` failure.

Outputs go under `results/socket-recheck-<UTC timestamp>-<pid>/`. `observations.json` separates decoder outputs from canvas draw callbacks and records bitrate, cgroup windows, source hashes, dimensions, generations, reconnect state, input log, and PSNR rows. Cleanup records logs before removing owned containers and stops only units created by the run.

See [`RESULTS.md`](./RESULTS.md) for the latest measurements and their limits.

## Invalid earlier quality number

The 27.935506 dB value in the 2026-09-08 evidence is invalid. `decoded-60-1280b.png` captured a browser layout where CSS clipped the canvas to 553 px and included browser background below it. It was not a raw decoded frame. The old files remain to document the error and must not be quoted as codec PSNR.
