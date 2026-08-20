#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$HERE"

LOCAL_CARGO_HOME="$HERE/.tools/cargo"
LOCAL_RUSTUP_HOME="$HERE/.tools/rustup"
LOCAL_CARGO="$LOCAL_CARGO_HOME/bin/cargo"

if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1 \
    && cargo --version >/dev/null 2>&1 && rustc --version >/dev/null 2>&1; then
    CARGO_BIN="$(command -v cargo)"
elif [[ -x "$LOCAL_CARGO" ]]; then
    # rustup proxies need the same homes on every invocation, not only on the
    # first bootstrap run.
    export CARGO_HOME="$LOCAL_CARGO_HOME"
    export RUSTUP_HOME="$LOCAL_RUSTUP_HOME"
    CARGO_BIN="$LOCAL_CARGO"
else
    if ! command -v curl >/dev/null 2>&1; then
        echo "nost build: Rust is not installed and curl is unavailable." >&2
        echo "Arch: sudo pacman -S rustup && rustup default stable" >&2
        exit 1
    fi

    echo "nost build: Rust toolchain not found; bootstrapping a local minimal toolchain..." >&2
    export CARGO_HOME="$LOCAL_CARGO_HOME"
    export RUSTUP_HOME="$LOCAL_RUSTUP_HOME"
    mkdir -p "$CARGO_HOME" "$RUSTUP_HOME"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable
    CARGO_BIN="$LOCAL_CARGO"
fi

if ! command -v cc >/dev/null 2>&1 && ! command -v clang >/dev/null 2>&1; then
    echo "nost build: a native linker is required (cc or clang)." >&2
    echo "Arch: sudo pacman -S base-devel" >&2
    exit 1
fi

echo "nost build: tests" >&2
"$CARGO_BIN" test --release --locked --offline

echo "nost build: release" >&2
"$CARGO_BIN" build --release --locked --offline

BIN="$HERE/target/release/nost"
echo "nost build: demo smoke test" >&2
"$BIN" --demo --once --no-color >/dev/null

printf '\nBuilt: %s\n' "$BIN"
"$BIN" --version
