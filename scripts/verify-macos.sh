#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

export MACOSX_DEPLOYMENT_TARGET=13.0
cargo fmt --all --check
cargo clippy --release --locked --all-targets -- -D warnings
cargo test --release --locked
cargo build --release --locked --example render_preview
cargo build --release --locked

target=$(cargo metadata --format-version 1 --no-deps --locked |
    python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
previews="$target/verification/previews"
mkdir -p "$previews"
(
    cd "$previews"
    for mode in keyboard-check dry-run compact-dry-fix compact-text-paused; do
        "$target/release/examples/render_preview" "$mode"
    done
)
plutil -lint ui/Afterbeat-Info.plist
