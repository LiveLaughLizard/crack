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

# --- evals ------------------------------------------------------------------

# Run the coding evals against the scripted mock (checks the whole pipeline, free)
eval *args:
    cargo build --quiet --bin crack --example mock_provider
    evals/run.sh {{args}}

# Run the coding evals against a real model, stopping once BUDGET dollars are spent
eval-model model budget="2" *args:
    cargo build --quiet --bin crack
    evals/run.sh --model {{model}} --max-cost {{budget}} {{args}}

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
    cargo clippy --all-targets --no-default-features --features cloud,china -- -D warnings
    cargo clippy --all-targets --features force-sandbox -- -D warnings
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
    cargo build --release --no-default-features --features cloud,yolo,languages --target-dir target/no-china
    @echo "Binary: target/no-china/release/crack"

# Build for air-gapped networks: local and self-hosted servers only, no cloud code
build-airgap:
    cargo build --release --no-default-features --features yolo,languages --target-dir target/airgap
    @echo "Binary: target/airgap/release/crack"

# Build for companies that keep a human in the loop: no YOLO mode, sandbox always on
build-strict:
    cargo build --release --no-default-features --features cloud,china,force-sandbox,languages --target-dir target/strict
    @echo "Binary: target/strict/release/crack"

# Write THIRD-PARTY-NOTICES: every dependency's license (needs cargo-about)
notices:
    cargo about generate about.hbs -o THIRD-PARTY-NOTICES
    @echo "Wrote THIRD-PARTY-NOTICES"

# Package a built flavor for release: dist/crack-<version>-<flavor>-<arch>-linux.tar.gz
archive flavor="default": notices
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{flavor}}" in
        default) bin=target/release/crack ;;
        no-china|airgap|strict) bin=target/{{flavor}}/release/crack ;;
        *) echo "unknown flavor {{flavor}} (default, no-china, airgap, strict)" >&2; exit 1 ;;
    esac
    name="crack-{{version}}-{{flavor}}-$(uname -m)-linux"
    rm -rf "dist/$name"
    mkdir -p "dist/$name"
    install -m755 "$bin" "dist/$name/crack"
    cp LICENSE README.md THIRD-PARTY-NOTICES "dist/$name/"
    install -Dm644 packaging/crack.desktop "dist/$name/share/applications/crack.desktop"
    install -Dm644 assets/logo.png "dist/$name/share/pixmaps/crack.png"
    tar -C dist -czf "dist/$name.tar.gz" "$name"
    echo "dist/$name.tar.gz"

# Build an AppImage of a built flavor: dist/crack-<version>-<flavor>-<arch>.AppImage.
# Downloads linuxdeploy on first use; runs without FUSE (extract-and-run).
appimage flavor="default":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{flavor}}" in
        default) bin=target/release/crack ;;
        no-china|airgap|strict) bin=target/{{flavor}}/release/crack ;;
        *) echo "unknown flavor {{flavor}} (default, no-china, airgap, strict)" >&2; exit 1 ;;
    esac
    arch=$(uname -m)
    tool="target/linuxdeploy-$arch.AppImage"
    if [ ! -x "$tool" ]; then
        curl -fsSL -o "$tool" "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-$arch.AppImage"
        chmod +x "$tool"
    fi
    appdir="target/AppDir-{{flavor}}"
    rm -rf "$appdir"
    # The window opens these at run time rather than linking them, so name them.
    extra=()
    for lib in libxkbcommon.so.0 libxkbcommon-x11.so.0 libwayland-client.so.0 libwayland-cursor.so.0 libfontconfig.so.1 libfreetype.so.6; do
        path=$(ldconfig -p | awk -v l="$lib" '$1 == l { print $NF; exit }')
        [ -n "$path" ] && extra+=(--library "$path")
    done
    mkdir -p dist
    APPIMAGE_EXTRACT_AND_RUN=1 ARCH=$arch LDAI_OUTPUT="dist/crack-{{version}}-{{flavor}}-$arch.AppImage" \
        "$tool" --appdir "$appdir" --executable "$bin" \
        --desktop-file packaging/crack.desktop --icon-file assets/icon-512.png --icon-filename crack \
        "${extra[@]}" --output appimage
    echo "dist/crack-{{version}}-{{flavor}}-$arch.AppImage"

# Build the Nix package (all providers); `just package airgap` for another flavor
package flavor="default":
    nix build .#{{flavor}}

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
