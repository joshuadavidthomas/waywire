set dotenv-load
set unstable

# List all available commands
[private]
default:
    @just --list

# Run one packaged-driver takeover scenario after `just build`.
acceptance SCENARIO:
    python3 tools/acceptance/scenario.py {{ SCENARIO }}

# Replay the dogfood tasks in Debian containers after `just build`.
acceptance-tasks *ARGS:
    python3 tools/acceptance/suite.py {{ ARGS }}

# Compare the desktops using the installed Waymote and release binaries.
benchmark *ARGS:
    python3 tools/benchmark/benchmark.py {{ ARGS }}

build-web:
    pnpm --filter @waywire/web build

bumpver *ARGS:
    uvx bumpver {{ ARGS }}
    cargo update --workspace

build: build-web
    cargo build --locked --release --workspace

check: build-web
    @just typecheck
    cargo check --locked --workspace --all-targets --all-features

clean:
    cargo clean

clippy *ARGS:
    cargo clippy --locked --workspace --all-targets --all-features --fix --allow-dirty {{ ARGS }} -- -D warnings

clippy-check:
    cargo clippy --locked --workspace --all-targets --all-features -- -D warnings

dev-web:
    pnpm --filter @waywire/web dev

# Run the decoder property tests with more cases than the 256 each run in CI.
fuzz CASES="100000":
    PROPTEST_CASES={{ CASES }} cargo test --locked -p waywire-protocol -p waywire-gateway -- pipe::tests video::rtp video::assembler

fmt:
    @just --fmt
    cd tools/rustfmt && cargo fmt --manifest-path "{{ justfile_directory() }}/Cargo.toml" --all
    pnpm exec prettier --write .

fmt-check:
    @just --fmt --check
    @just rustfmt-check
    @just prettier-check

lint *ARGS:
    @just --fmt --check
    uvx prek==0.5.2 run --all-files --show-diff-on-failure --color always {{ ARGS }}

prettier-check:
    pnpm exec prettier --check .

rustfmt-check:
    cd tools/rustfmt && cargo fmt --manifest-path "{{ justfile_directory() }}/Cargo.toml" --all -- --check

test: build-web
    pnpm --filter @waywire/web test
    cargo test --locked --workspace
    shellcheck .agents/setup .agents/serve-desktop

test-ffmpeg *ARGS:
    cargo test --locked -p waywire-compositor -- --ignored {{ ARGS }}

typecheck:
    pnpm --filter @waywire/web check
