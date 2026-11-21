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
    for mode in recording-check compact-text-recording-check recorded-step-editor compact-text-recorded-step-editor recorded-key-editor compact-text-recorded-key-editor compact-text-recorded-key-editor-bottom route-details compact-route-details compact-text-route-details step-editor compact-step-editor summary-check preset-form-check hold-summary compact-hold-summary compact-text-summary compact-text-hold-summary compact-text-save-name compact-text-save-error preset-load-error ux-check preset-load compact-preset-load compact-text-preset-load zones compact-zones presets compact-presets compact-text-presets compact-text-preset-details timing compact-timing compact-text-timing keyboard compact-keyboard compact-text-keyboard compact-text-zones undo-route compact-undo-route keyboard-check shortcut-editing-check reorder-check route-reorder compact-route-reorder compact-text-route-reorder dry-run compact-dry-fix compact-text-paused compact-text-running; do
        "$target/release/examples/render_preview" "$mode"
    done
)
plutil -lint ui/Afterbeat-Info.plist
