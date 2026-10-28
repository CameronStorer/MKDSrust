#!/bin/sh
# Build a native game on Linux or macOS: ./build.sh (a menu asks for the ROM) or ./build.sh ROM [OUTPUT]
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
if [ "$#" -gt 2 ]; then
    echo "Usage: $0 [/path/to/game.nds [output-folder]]   (no arguments: a menu asks for the ROM)" >&2
    exit 1
fi
if [ "$#" -ge 1 ] && [ ! -f "$1" ]; then
    echo "ROM not found: $1" >&2
    exit 1
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "Rust is not installed. Install it from https://rustup.rs and reopen your terminal." >&2
    exit 1
fi

# Separate host artifacts, including when this checkout is shared with Windows/WSL.
TARGET="$ROOT/1_build_tool/target/$(uname -s)-$(uname -m)"
OUT=${2:-"$ROOT/builds/mkds"}
echo "Preparing the MKDSrust builder (the first time this takes a few minutes)..."
cargo build --release --locked --manifest-path "$ROOT/1_build_tool/Cargo.toml" --target-dir "$TARGET"
if [ "$#" -eq 0 ]; then
    # the menu asks for the ROM (paste its path), builds into builds/mkds and can start the game afterwards
    exec "$TARGET/release/mkds_builder" --pick-rom --source "$ROOT"
fi
"$TARGET/release/mkds_builder" "$1" --build --source "$ROOT" --out "$OUT"
printf '\nDone! Start the game with: "%s/play"\n' "$OUT"
