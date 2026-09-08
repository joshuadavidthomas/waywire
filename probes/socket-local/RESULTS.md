# Local Socket results

## Recorded runs

The latest complete 8,000 kbps run is [`socket-recheck-20260908-222734-796717`](./results/socket-recheck-20260908-222734-796717/):

- 30 FPS: quiet decoded/drawn 0/0; quiet browser/container CPU 1.298169%/1.107628%; moving decoded/drawn 1103/1103; moving browser/container CPU 30.481021%/63.699721%; whole-frame PSNR 45.038650 dB at 1280×720 and 46.212049 dB at 1920×1080.
- 60 FPS: quiet decoded/drawn 0/0; quiet browser/container CPU 2.001348%/1.070449%; moving decoded/drawn 1101/1101; moving browser/container CPU 29.004645%/63.627609%; whole-frame PSNR 36.811012 dB at 1280×720 and 39.698755 dB at 1920×1080.

The controlled 16,000 kbps comparison is the 60 FPS run in [`socket-recheck-20260908-223511-820524`](./results/socket-recheck-20260908-223511-820524/60/observations.json):

At 60 FPS, quiet decoded/drawn counts were 0/0 and browser/container CPU was 1.956566%/1.088942%. The moving counts were 1101/1102 and CPU was 27.161884%/59.538103%. Whole-frame PSNR was 43.267963 dB at 1280×720 and 47.392151 dB at 1920×1080.

The 8,000 and 16,000 kbps 60 FPS captures used the same source images:

```text
1e76b677261cd9b31b5a93bced9dddda163b419b41baf1894e1546916a0bd7d1  1280x720 source
232039bc030ed23f9cf8ab9e4706be3e748589e9e9f2d52e19af167bca535602  1920x1080 source
```

The 16,000 kbps run raised whole-frame PSNR by 6.456951 dB at 1280×720 and 7.693396 dB at 1920×1080. The CPU numbers are one sample per setting, so they support no claim of a statistical CPU improvement. This comparison is the basis for the 16,000 kbps, 60 FPS candidate defaults.

All three completed rate runs passed at least 20 seconds attached while idle, the 45-second quiet window with zero decoded and drawn frames, the 35-second hidden-tab return, and reconnection after a real Docker network break. During the break, the test held Shift, released it locally while offline, connected a new controller, and observed lowercase `a` in the terminal. Both requested sizes produced full native RGBA canvases, including 1920×1080. `canvas.toDataURL()` captures were stable while the bracketed `grim` source hashes matched. Decoder output reported I444 with sRGB transfer, BT.709 matrix and primaries, and full range.

The moving fixture yielded 1101–1103 frames in about 45 seconds, roughly 24.5 draws/s, because the fixture itself updates at that rate. These runs do not qualify full 60 FPS. They contain no unique visual frame IDs, physical-presentation measurement, end-to-end latency result, or p95 freeze proof. Container CPU includes qterminal and the fixture; browser CPU covers the whole headless `--disable-gpu` Chromium cgroup. Both use cumulative cgroup `usage_usec` differences and must not be compared as if they repeated the older headed-Chrome Sprite benchmark.

## Failed attempts kept as evidence

- [`socket-recheck-20260908-213644-733798`](./results/socket-recheck-20260908-213644-733798/30/failure.json) failed because a 1920×1080 request decoded as 1920×1072. This red-first regression led to the two-pixel resize-bound fix; later runs decode the full 1920×1080 frame.
- [`socket-recheck-20260908-214247-742900`](./results/socket-recheck-20260908-214247-742900/60/failure.json) reached the 60 FPS phase after the 30 FPS phase, but CDP was unavailable before the WebSocket client retry fix. It is a failed attempt, not a partial pass for 60 FPS.
- The first native-container attempt failed because `qt6-wayland` was absent. The pinned Dockerfile now installs that dependency.

The earlier 27.935506 dB result remains archived but invalid: it measured a CSS-clipped browser screenshot rather than raw decoded pixels.
