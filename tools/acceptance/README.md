# Packaged-driver acceptance scenarios

These scenarios drive the built binaries from outside, through real agent
drivers, and decide pass or fail from ground truth. They are the specification:
when one fails, fix the code, not the check (see `AGENTS.md`).

There are two kinds:

- **Dogfood task replays** (`replay.py`, run by `suite.py`): the six tasks from
  the November 1, 2026 dogfood run, each replayed through the drivers whose
  calls passed, on Debian 12 and Debian 13.
- **Takeover scenarios** (`scenario.py`): a browser viewer takes control while
  a driver types, then hands it back.

Both need Docker and the host release binaries from `just build`.

## Dogfood task replays

Run the whole suite on both distros, one distro, or one task:

```sh
just acceptance-tasks                          # Debian 12 and Debian 13
just acceptance-tasks --distro bookworm        # Debian 12 only
just acceptance-tasks --distro trixie T3       # every T3 cell on Debian 13
just acceptance-tasks --distro trixie T4/cua   # one cell
just acceptance-tasks --no-build T1            # reuse the built images
```

The arguments go to `suite.py`; cell arguments are name prefixes. List the
cells with `python3 tools/acceptance/replay.py --list`.

`suite.py` builds `waywire-acceptance-<distro>` from `Dockerfile.debian`. The
image holds only Debian packages and the pinned drivers:

| Driver                     | Pin                                                                                                                                                        |
| -------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| computer-use-linux stock   | 0.7.35 crate, sha256 `2868a912…1362` (crates.io)                                                                                                           |
| computer-use-linux patched | upstream `3175c04` with #359 (`78892cf`) and #360 (`27ca862`) merged; merged tree `ff768579…3818`                                                          |
| cua-driver                 | 0.38.0 archive sha256 `31ab9709…98dd`, binary sha256 `bf57b2f3…7934`                                                                                       |
| xdotool loop               | Anthropic's `ComputerTool20250124` at anthropic-quickstarts `1a18c34`, `anthropic==2.3.0` from the hash-locked `x11-loop-requirements.txt`, Debian xdotool |

Both computer-use-linux builds compile on Debian 12 with `rustc-web` and are
copied into both runtime images. Their binaries differ from the dogfood run's
(another compiler), so the sources are pinned instead.

The runner starts one container per distro with `--init`, mounts the
repository read-only at `/repo`, and writes evidence to
`target/acceptance/<distro>/`: `results.jsonl`, `timing.json`, and per cell the
driver calls (`calls.jsonl`), the task server's submissions, the session and
application logs, and `start.png` and `end.png`. The host binaries must run on
Debian 12's glibc 2.36, so build them on Debian 12 or older.

### What a cell does

Each cell is one task replayed through one driver, in a fresh session:

1. Kill everything left in the container and empty the home directory.
2. Start the gateway, compositor and private session bus as the dogfood run
   did (1280×720, a `foot --title=Terminal` session program), and the task
   server. Turn on AT-SPI (`org.a11y.Status.IsEnabled`) before any application
   starts; in the dogfood run the long-lived driver servers had already done so.
3. Open the task's start state as the dogfood harness's `setup_task.sh` did.
   T2 cells also create the files earlier T2 runs left in the work directory,
   because the file chooser's location bar completes from them.
4. Start the driver's MCP server and replay the recorded calls of a passing
   run, in order, with its recorded sleeps. `rows` in `replay.py` names them in
   the dogfood call log (`df-calls.tsv`). Values the agent read from a driver
   result, such as a window id, an AT-SPI element or the secret value, are read
   from the replayed result the same way.
5. Judge the outcome from ground truth only.

Page coordinates come from the page's own element boxes. Each task page reports
them on load and draws a 6×6 magenta square at its top-left corner; the runner
finds the square in a grim capture to place the page on screen. Other
coordinates (the terminal token, the editor's buttons, the Thunar icon, the
drop zone's visible strip, dialog buttons) are the recorded ones.

| Task | Cells                           | Ground truth                                                                                                                                             |
| ---- | ------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| T1   | cul-patched, cua, x11demo       | Exactly one `form` submission with every field equal to what the recorded run submitted                                                                  |
| T2   | cul-patched, cul-stock, x11demo | The file on disk is the three original lines plus `- milk (<driver> r<run>)`                                                                             |
| T3   | cul-patched, cua, x11demo       | Exactly one `paste` submission whose token equals the random token shown in the terminal                                                                 |
| T4   | cua, cul-patched, x11demo       | Exactly one `upload` submission with the file's name, size and contents                                                                                  |
| T5   | cul-patched, cua, x11demo       | One `switch` submission with the editor's random value, and the compositor's focus on exactly that editor's title (the task requires switching by title) |
| T6a  | cul-patched, x11demo            | As T1, for the run that dismissed the surprise alert                                                                                                     |
| T6b  | cul-stock, cua, x11demo         | The editor's title bar reappears at least 200 px to the right after the interloper's drag, and the file on disk is the original line plus `- verified`   |

Every expected value is exact: the T3 token and T5 value are the random ones
the cell's setup wrote, and T1 and T6a compare every field to what the recorded
run typed, not to a pattern or a list of plausible values.

The compositor's focus is read by `wl_probe.py`, a separate Wayland client,
through wlr-foreign-toplevel. A driver's own success report never counts.

Two cells differ from their recorded calls:

- `T4/x11demo` adds a second motion inside the drop zone before release, as the
  Waymote run did (rows 1492–1499). Thunar and Chromium speak XDND directly
  inside Xwayland, and Chromium accepted the drop only after a second position
  message: with one motion it failed 6 of 6 times, at `0548b28` too.
- `T2/cul-stock` runs on Debian 13 only. Its clicks target gnome-text-editor
  48's Open popover; Debian 12's 43.2 has no Browse item there.

### Bug regressions

Two cells check bugs the dogfood run found, outside any task:

| Cell                       | Ground truth                                                                                                                                                                        |
| -------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `screencopy-destroy/probe` | A wlr-screencopy client captures a frame, destroys it after `ready`, and its connection survives two round trips (fixed in `71e4772`).                                              |
| `chip-leak/probe`          | After the dogfood repro (`chipleak4.sh`: 15 `foot` clients killed with SIGKILL while mapped), the taskbar pixels and the toplevel count equal the ones before (fixed in `ffa52c6`). |

`T2/cul-stock/r3` covers the third: a click right after the GTK file chooser
closes must land where it was sent (fixed in `86cd002`).

### Known failures

A cell with a `known` cause for a distro fails there on main because of a known
bug. It reports `XFAIL` and does not fail the run. When the bug is fixed the
cell reports `XPASS`, which does fail the run: remove the cause in the same
change.

| Cell                         | Distro    | Cause                                                                                                                                                                                                                                                                                              |
| ---------------------------- | --------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| every x11demo cell except T2 | Debian 12 | Xwayland 22.1 names each window's surface with the `WL_SURFACE_ID` message. Smithay records the id but leaves matching it to the compositor, and waywire never does, so X11 windows map but are never drawn. Xwayland 23.1 and later use `xwayland_shell_v1`. Being fixed on branch `xwayland-22`. |
| `T2/x11demo/r2`              | Debian 13 | GTK 4.18's file chooser location bar inserts its inline completion while xdotool is still typing, so the path becomes `t2-x11demo-r2.txtxt`. The dogfood run hit it in `T2/x11demo/r1`; it fails the same way at `0548b28`.                                                                        |
| `typeloss/cul-patched`       | both      | Text typed through a Wayland virtual keyboard loses characters in Chromium (ROADMAP Pass 2, item 4). waywire's `NotifyKeyboardKeysym` path types it intact.                                                                                                                                        |

`T2/x11demo` passes on Debian 12 although its window is not drawn: keyboard
input still reaches it, and GTK 4.8 does not race.

Flaky dogfood cells are not replayed: batched T2 and T6a sequences without the
learned sleeps, whose outcome depends on timing in the application.

## Takeover scenarios

Build the host release binaries with `just build`, start Docker, and run one of:

```sh
just acceptance computer-use-linux
just acceptance cua-driver
just acceptance fedora-x11
```

The runner needs `sudo docker`, host networking, `agent-browser`, ImageMagick,
Tesseract, `git` and `uv`. It builds the distribution images, mounts the
host binaries read-only, and removes its container on exit. It uses host port
18925 and the container name `waywire-acceptance`, so run one at a time. The
Debian scenarios use the stock computer-use-linux and cua-driver from the
acceptance image described above. The Fedora scenario checks out a pinned
upstream ComputerTool and mounts its Python dependencies from a temporary host
directory; only Fedora packages are installed inside the container.

The runner records the private session bus and saves viewer screenshots in
`.amp/in/artifacts/`. It fails if OCR cannot find the before/after markers,
finds the during marker, or the paused notice is missing. Inspect the screenshots
when diagnosing an OCR failure. A failed run can stop before all three images
are captured; existing screenshots from earlier runs are not removed.
