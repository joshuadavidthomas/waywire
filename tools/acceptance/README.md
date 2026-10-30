# Packaged-driver acceptance scenarios

Build the host release binaries with `just build`, start Docker, and run one of:

```sh
just acceptance computer-use-linux
just acceptance cua-driver
just acceptance fedora-x11
```

The runner needs `sudo docker`, host networking, `agent-browser`, ImageMagick,
Tesseract, `git`, `gh` and `uv`. It builds the distribution images, mounts the
host binaries read-only, and removes its container on exit. It uses host port
18925 and the container name `waywire-acceptance`, so run one at a time. The
Debian image compiles the published computer-use-linux crate and needs a recent
backports Rust compiler. The cua archive is checked against the release's
published SHA-256 manifest. The Fedora scenario checks out a pinned upstream
ComputerTool and mounts its Python dependencies from a temporary host directory;
only Fedora packages are installed inside the container.

The runner records the private session bus and saves viewer screenshots in
`.amp/in/artifacts/`. It fails if OCR cannot find the before/after markers,
finds the during marker, or the paused notice is missing. Inspect the screenshots
when diagnosing an OCR failure. A failed run can stop before all three images
are captured; existing screenshots from earlier runs are not removed.
