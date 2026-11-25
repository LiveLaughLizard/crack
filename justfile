# crack tasks. Run inside the dev shell (`nix-shell` / `nix develop`).
# `just` with no arguments lists the recipes.

set shell := ["bash", "-euo", "pipefail", "-c"]

version   := `sed -nE 's/^version = "([^"]+)".*/\1/p' Cargo.toml`
mock_addr := "127.0.0.1:8765"
prefix    := env_var_or_default("PREFIX", env_var("HOME") + "/.local")

default:
    @just --list

# --- run --------------------------------------------------------------------

# Open the window on a directory (default: here) with a debug build
dev dir="." *args:
    cargo run -- -C {{dir}} {{args}}

# Open the window with an optimised build
run dir="." *args:
    cargo build --release
    ./target/release/crack -C {{dir}} {{args}}

# One headless turn, as in CI: just headless "fix the tests" -m anthropic/claude-opus-5-5
headless prompt *args:
    cargo run --quiet -- run {{args}} "{{prompt}}"

# List providers and whether they're configured
providers:
    cargo run --quiet -- providers

# List usable models
models:
    cargo run --quiet -- models

# List the skills and instruction files crack finds for a directory
skills dir=".":
    cargo run --quiet -- -C {{dir}} skills

# --- try without an API key ---------------------------------------------------

# Start the scripted model server (OpenAI and Anthropic protocols)
mock:
    MOCK_ADDR={{mock_addr}} cargo run --example mock_provider

# Open the window against the mock server (start `just mock` first)
dev-mock dir="." *args:
    CRACK_BASE_URL=http://{{mock_addr}}/v1 cargo run -- -C {{dir}} -m custom/mock {{args}}

# Headless turn against the mock server; "run: <cmd>" makes it call bash
headless-mock prompt="run: ls" *args:
    CRACK_BASE_URL=http://{{mock_addr}}/v1 cargo run --quiet -- run -m custom/mock {{args}} "{{prompt}}"

# --- check ------------------------------------------------------------------

# Run the unit tests
test *args:
    cargo test {{args}}

# Format, lint and test every build type: what CI should run
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings
    cargo clippy --all-targets --no-default-features --features cloud -- -D warnings
    cargo clippy --all-targets --no-default-features -- -D warnings
    cargo test
    cargo test --no-default-features

# Format the code
fmt:
    cargo fmt

# --- build and install ------------------------------------------------------

# Build optimised binaries (all providers)
build:
    cargo build --release

# Build without the Chinese model clouds (Qwen, Kimi, Z.ai, MiniMax, DeepSeek)
build-no-china:
    cargo build --release --no-default-features --features cloud --target-dir target/no-china
    @echo "Binary: target/no-china/release/crack"

# Build for air-gapped networks: local and self-hosted servers only, no cloud code
build-airgap:
    cargo build --release --no-default-features --target-dir target/airgap
    @echo "Binary: target/airgap/release/crack"

# Install the binary into $PREFIX/bin (default ~/.local/bin)
install: build
    install -Dm755 target/release/crack {{prefix}}/bin/crack
    @echo "Installed crack {{version}} to {{prefix}}/bin/crack"

# Remove the installed binary
uninstall:
    rm -f {{prefix}}/bin/crack

# Print a shell completion script: just completion zsh > ~/.zfunc/_crack
completion shell="bash":
    @cargo run --quiet -- completion {{shell}}

# Print the version from Cargo.toml
version:
    @echo {{version}}

# Remove build outputs
clean:
    cargo clean
