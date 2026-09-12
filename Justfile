set dotenv-load
set unstable

# List all available commands
[private]
default:
    @just --list

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

release VERSION:
    scripts/package-release "{{ VERSION }}"

rustfmt-check:
    cd tools/rustfmt && cargo fmt --manifest-path "{{ justfile_directory() }}/Cargo.toml" --all -- --check

test: build-web
    pnpm --filter @waywire/web test
    cargo test --locked --workspace
    python3 -m unittest discover -s desktop -p 'test_*.py'
    shellcheck desktop/desktop.sh scripts/package-release scripts/setup-desktop

test-streamd *ARGS:
    cargo test --locked -p waywire-streamd -- --ignored {{ ARGS }}

typecheck:
    pnpm --filter @waywire/web check
