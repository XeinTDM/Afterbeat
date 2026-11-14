#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check
cargo clippy --release --locked --all-targets -- -D warnings
cargo test --release --locked
# Start the portal fixture after compilation so its timeout applies to the test.
test_binary=$(cargo test --release --locked --bin afterbeat --no-run --message-format=json |
    python3 -c 'import json, sys
artifacts = (json.loads(line) for line in sys.stdin)
binary, = (artifact["executable"] for artifact in artifacts
           if artifact["reason"] == "compiler-artifact" and artifact["profile"]["test"]
           and artifact["target"]["name"] == "afterbeat")
print(binary)')
dbus-run-session -- "${PYTHON:-python3}" -B scripts/delivery/wayland_portal.py "$test_binary" --ignored --exact wayland::tests::portal_lifecycle
cargo build --release --locked --example render_preview
cargo build --release --locked

target=$(cargo metadata --format-version 1 --no-deps --locked |
    python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
previews="$target/verification/previews"
mkdir -p "$previews"
(
    cd "$previews"
    for mode in route-details compact-route-details compact-text-route-details step-editor compact-step-editor summary-check preset-form-check hold-summary compact-hold-summary compact-text-summary compact-text-hold-summary compact-text-save-name compact-text-save-error preset-load-error ux-check preset-load compact-preset-load compact-text-preset-load zones compact-zones presets compact-presets compact-text-presets compact-text-preset-details timing compact-timing compact-text-timing keyboard compact-keyboard compact-text-keyboard compact-text-zones undo-route compact-undo-route keyboard-check shortcut-editing-check reorder-check route-reorder compact-route-reorder compact-text-route-reorder workflow-check light dry-run dry-results-check dry-count-check dry-wait-check compact-dry-fix compact-text compact-text-blocked compact-text-paused compact-text-running wayland-recovery; do
        "$target/release/examples/render_preview" "$mode"
    done
)

desktop-file-validate ui/io.github.xeintdm.Afterbeat.desktop
env -u WAYLAND_DISPLAY XDG_SESSION_TYPE=x11 xvfb-run -a --server-args="-screen 0 1600x1000x24 -nolisten tcp" "${PYTHON:-python3}" -B scripts/delivery/smoke_x11.py "$target/release/afterbeat"
dbus-run-session -- "${PYTHON:-python3}" -B scripts/delivery/smoke_wayland.py "$target/release/afterbeat"
