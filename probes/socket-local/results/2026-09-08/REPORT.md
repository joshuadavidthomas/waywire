# Local Socket verification — 2026-09-08 UTC

Owner `pi-20260908-complete`; Docker Engine 29.7.2. No cloud or Sprite resource was used. The final image is `sha256:dabf5b2ce90794021bbfb8e0e391088acef6f630412fd7f215a55edb2d2ea24f`, based on Ubuntu 26.04 digest `sha256:2260313b31c8c011cd2eebe728008efac1b3982be73eb71348ea2648d2c0e09b`. It contains FFmpeg 8.0.1-3ubuntu2 and labwc 0.9.3. Gateway and streamd hashes are in `measurements.json`.

The first retained failure (`diag-labwc.log`) found missing Qt Wayland shell integration. Adding `qt6-wayland` fixed startup. A second retained failure (`corrected-attempt-labwc.log`) found an unsupported qterminal `--geometry` option. The final fixture launches qterminal directly under labwc, shows terminal text and stores one native key without exiting. `fixture-corrected.png` records that raster.

## Results

Both 30 and 60 configured FPS passed a fresh browser attach after 45 seconds idle. Chromium WebCodecs decoded 1280x720 I444 frames and reported full-range BT.709 with the expected sRGB transfer. Native keys reached the terminal and were stored as `q` and `z` respectively.

The 60 FPS run passed a 35-second background-tab return: decoded output advanced from 11 to 17 after return and pointer motion. A real Docker network detach produced WebSocket close 1006 and both channels entered reconnecting state; reconnect restored `Connected` / `Input active` and decoded count reached 24. The browser also decoded 1920x1080 I444 full-range output after native resize. The later source capture was lost when the daemon failed, so only decoded 1920 evidence exists.

Whole-container cumulative cgroup CPU over fixed 45-second windows:

- 60 quiet: 0.127543 CPU seconds, 0.28343% of one core.
- 60 testsrc2 motion: 96.495641 CPU seconds, 214.43476% of one core, including the ffplay workload.
- 30 quiet: 0.130854 CPU seconds, 0.290787% of one core.
- 30 motion has no valid fixed endpoint because the sampler command was aborted. Its counters remain diagnostic only.

The earlier 27.935506 dB PSNR is invalid. `decoded-60-1280b.png` is a browser screenshot containing a 553 px CSS-clipped canvas plus browser background, rather than a raw decoded 1280x720 image. The files remain as evidence of that measurement error; they make no codec-quality claim. The reproducible recheck captures the source with `grim` immediately before and after an unchanged-hash interval and reads decoded pixels from the canvas with `toDataURL()`.

## Reproduction and limits

The 60 FPS process reproduced `video metadata/RTP correlation stalled` after idle, motion, reconnect, and repeated resize; the container then exited. Docker logs also showed FFmpeg resuming after lags of 75.035, 117.993, and 136.204 seconds. Existing sparse FFmpeg tests passed earlier, so they do not cover this full compositor/browser sequence.

Held-key release was not tested: agent-browser supplies complete native key presses but no separate held-key command. A dedicated whole-browser cgroup was unavailable, so no process-tree CPU estimate was substituted. The 30 FPS background/reconnect/1920 cases were not repeated after their 60 FPS passes.

All owned containers were removed and all agent-browser sessions closed by 20:22 UTC. Owned images remain for parent reuse. Exact counters and observations are in `measurements.json`.
