# First user recordings: September 5, 2026

These recordings support Josh's impression that VNC felt smoother. They do not
isolate transport performance: the framebuffer sizes and reported browser CPU
thread counts differ, and the desktops use different applications/compositors.

Sources, preserved without edits:

- `results/waymote-2026-09-05T13-46-05.090Z.json`
- `results/vnc-2026-09-05T13-48-03.668Z.json`

Both ran for about 30 seconds with `continuous motion` selected. Neither logged
a hidden tab, resize during capture, or socket closure. The analysis recomputes
and checks every saved summary against its raw arrays.

## Results

| Whole recording              |   Waymote |                VNC |
| ---------------------------- | --------: | -----------------: |
| Remote framebuffer           |  1280×720 |           1833×862 |
| Reported browser CPU threads |        14 |                 16 |
| Canvas update frames/second  |     25.97 |              33.19 |
| Update spacing, median       |   33.3 ms |            26.9 ms |
| Update spacing, p95          |   50.1 ms |            47.4 ms |
| Update spacing, p99          |   83.5 ms |            75.8 ms |
| Update gaps over 100 ms      |         5 |                  3 |
| Largest update gap           |  416.7 ms |          1126.3 ms |
| Received WebSocket payload   | 8.10 Mbps |          8.19 Mbps |
| Browser long tasks           |         0 | 3, totaling 166 ms |

VNC's largest gap was near the start, from 0.42 to 1.55 seconds. To reduce the
effect of starting and stopping the mouse gesture, the same middle interval
(2–28 seconds) gives:

| Middle 26 seconds           |  Waymote |      VNC |
| --------------------------- | -------: | -------: |
| Canvas update frames/second |    26.96 |    34.92 |
| Update spacing, p95         |  50.1 ms |  47.0 ms |
| Update spacing, p99         |  83.5 ms |  69.1 ms |
| Update gaps over 100 ms     |        5 |        1 |
| Largest update gap          | 416.7 ms | 196.0 ms |

This interval is an additional view of the data, not a replacement for the whole
recording. Mouse-motion pauses can cause gaps even when the transport works.

## What the data suggests

VNC presented canvas updates more often, at nearly the same payload bandwidth.
Waymote was configured for only 30 frames per second, while VNC often reached
35–40 update frames per second. That limit is one plausible reason for the
perceived difference. Canvas update frames are not guaranteed physical screen
presentations, and VNC can update only part of the framebuffer.

Four of Waymote's five gaps over 100 ms occurred between roughly 4.6 and 7.0
seconds. After that, it usually delivered 27–30 updates per second. It did not
show a sustained browser main-thread stall: its animation callback spacing was
about 16.7 ms, with a 19.3 ms maximum and no long tasks.

Waymote's 780 frame-sampled SDK records show:

- RTT median 57.2 ms, p95 58.3 ms.
- A constant 60 ms presentation-latency target. This is a setting, not measured
  input-to-screen delay.
- Lateness median 5.1 ms and p95 11.8 ms against its scheduled presentation time.
  Its clock estimate reported ±28.2 ms uncertainty; do not treat this as a precise
  end-to-end latency measurement.
- Decoder queue zero at each recorded sample. This does not observe every instant
  between frames.
- A dropped-frame counter between zero and two. Upstream resets this counter in
  `sendFeedback()`, so first/last subtraction would falsely imply zero total drops.

There are no server CPU samples or controlled input-response measurements for
this pair. Stable RTT alone cannot rule out media delivery or encoder stalls.

## What needs correcting before another pair

The VNC framebuffer did not stay at the intended 1280×720. A later Xrandr check
confirmed 1833×862. A viewer without `?record` can resize the shared desktop even
while the recording viewer has automatic resizing disabled. That is a possible
cause, not a confirmed account of what happened.

Both files report Chrome 152 on Linux, but one reports 14 CPU threads and the other 16. Confirm whether the browser/device and any CPU-emulation settings were the
same. Different CSS scaling also remains recorded in the raw files.

Before repeating, close other VNC viewers and reset that Sprite to 1280×720.
Keep the same browser/device and motion workload. Once that comparison is matched,
a short Waymote run at 60 Hz capture/encoding would test whether its 30 Hz limit
explains the smoother VNC experience. No settings were changed during this analysis.

Reproduce the analysis:

```sh
node probes/compare/analyze.mjs \
  probes/compare/results/waymote-2026-09-05T13-46-05.090Z.json \
  probes/compare/results/vnc-2026-09-05T13-48-03.668Z.json
```
