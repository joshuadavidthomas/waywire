# Second user recordings: 60 Hz and canvas resizing

The second pair has much closer framebuffer sizes. Waymote now has better typical
update spacing, but worse occasional pauses. Whole-run average update rates are
almost equal, which hides that difference.

Sources, preserved without edits:

- `results/waymote-2026-09-05T14-17-04.661Z.json`
- `results/vnc-2026-09-05T14-17-48.387Z.json`

Josh confirmed the same laptop/browser for both approaches and kept a normal
multi-tab workload. The browser still reports 14 versus 16 CPU threads; the cause
is unknown, and this field alone does not establish different hardware. Neither
recording reports a hidden tab, dimension change or socket closure.

Waymote used the new 60 Hz capture/encoding target and canvas observation. Its
8 Mbps encoder target and 60 ms presentation-latency target were unchanged.
VNC used automatic canvas resizing. The desktops still differ: LXQt/labwc versus
XFCE/Xvnc. These manual-motion runs compare the experienced setups, not isolated
transport implementations.

## Results

| Whole recording             |   Waymote |       VNC |
| --------------------------- | --------: | --------: |
| Remote framebuffer          |  1824×848 |  1833×862 |
| Duration                    |  30.001 s |  30.018 s |
| Canvas update frames/second |     39.50 |     39.21 |
| Update spacing, median      |   16.7 ms |   20.5 ms |
| Update spacing, p95         |   33.4 ms |   53.9 ms |
| Update spacing, p99         |   66.7 ms |   68.2 ms |
| Update gaps over 100 ms     |        10 |         6 |
| Largest update gap          | 1233.2 ms | 1239.8 ms |
| Received WebSocket payload  | 7.23 Mbps | 7.72 Mbps |
| Browser long tasks          |         0 |         0 |

The different control bars and Waymote's 16-pixel size alignment leave VNC with
about 2.1% more framebuffer pixels. The sizes remained stable throughout both
recordings. These update counts batch visible-canvas drawing into browser
animation callbacks; they are not physical screen-refresh measurements.

For the same middle interval used in the first analysis (2–28 seconds):

| Middle 26 seconds           |   Waymote |      VNC |
| --------------------------- | --------: | -------: |
| Canvas update frames/second |     40.54 |    41.08 |
| Update spacing, p95         |   33.4 ms |  54.0 ms |
| Update spacing, p99         |   50.2 ms |  66.8 ms |
| Update gaps over 100 ms     |         8 |        4 |
| Largest update gap          | 1233.2 ms | 625.5 ms |

VNC's 1.24-second gap was near the start, from 0.63 to 1.87 seconds. It also had
mid-run gaps of 626, 428 and 370 ms. A motion pause can cause missing updates;
VNC's recording cannot distinguish those from transport or server delays.

## Waymote's normal pacing improved, but it fell behind

The first Waymote run averaged 25.97 update frames/second with median/p95 gaps of
33.3/50.1 ms. This run averaged 39.50 with median/p95 gaps of 16.7/33.4 ms, despite
a larger framebuffer. That is consistent with the higher frame-rate target
helping normal motion. Workload and resolution also changed, so it is not a
controlled estimate of the frame-rate setting's effect alone.

Waymote usually delivered roughly 45–53 update frames per second during motion.
Between 18 and 22 seconds, the one-second counts fell to 7, 18, 10 and 2. Its
longest gap ran from 18.101 to 19.335 seconds. Two further gaps lasted 850 and
884 ms around seconds 20–22.

The SDK supplies evidence beyond a simple lack of changing pixels:

- At 19.318 seconds, the submitted frame was 1196.8 ms later than its scheduled
  presentation time. The video reflected an input sequence 74 events behind the
  client's current sequence.
- The next frames reported lateness of 1159.9, 1017.8 and 962.0 ms while that
  sequence gap fell from 71 to 56 to 27. This is consistent with delayed video
  arriving and catching up; it does not measure each input's causal response time.
- Around the same pause, received payload fell from about 892 KB in second 17–18
  to 73 KB in second 18–19, then rose to 1.15 MB in second 19–20. It fell to 5 KB
  and 53 KB in the next two one-second intervals. These are all-socket application
  payload counts, not a packet trace or a direct server-output measurement.
- Browser animation callbacks continued normally: maximum gap 20.1 ms, no long
  tasks. The cached RTT at the late samples was 57.4 ms; the run's median was
  57.0 ms. These observations argue against a browser main-thread freeze but do
  not rule out decoder/GPU, network, capture or encoding delays.
- Decoder queue was zero at sampled presentations. The clock estimate reported
  ±28.0 ms uncertainty, far smaller than the 1.2-second lateness spike, but not a
  precise independent calibration of end-to-end latency.

The first large pause therefore has evidence of actual delayed video, not merely
a user stopping the mouse. That does not prove that every later gap has the same
cause. The dropped-frame counter reached 22 and resets on upstream feedback;
its first/last values are not a total-drop count.

A read-only service-log check also found FFmpeg read-rate lag messages during the
run. Their magnitudes must not be treated as screen latency: raw-video timestamps
advance by submitted frame count, while unchanged desktops can stop supplying
frames. Those logs alone do not identify the pause's cause. No services or
settings were changed during this analysis.

## Next useful question

The 60 Hz setting is worth keeping for now. The next investigation should locate
where old video accumulates during a pause: capture/encoder output, gateway
writing, network delivery or browser presentation. There are no synchronized
server CPU samples or per-stage timestamps for this pair, so assigning blame
would be premature. Raising bitrate or presentation buffering before checking
those pauses would change another variable without explaining the current one.

Reproduce the analysis:

```sh
node probes/compare/analyze.mjs \
  probes/compare/results/waymote-2026-09-05T14-17-04.661Z.json \
  probes/compare/results/vnc-2026-09-05T14-17-48.387Z.json
```
