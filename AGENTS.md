# Project scope

- The current gateway/compositor split is fine. A single Linux/Wayland binary is
  a possible later simplification, not part of the current cleanup.
- Keep development conveniences separate from product requirements. The scripts
  in `.agents/` prepare and run our development environment; they are not an
  end-user installer or a prescribed desktop stack.
- Do not prescribe Firefox, Foot, a particular installation directory, or host
  provisioning for users. Prefer the existing Chromium tooling for browser tests.
- Focus on the implementation and regression tests. Do not add deployment
  launchers, installers, or release bundles without an explicit request.
