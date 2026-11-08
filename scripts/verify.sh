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
    for mode in keyboard-check dry-run compact-dry-fix compact-text-paused; do
        "$target/release/examples/render_preview" "$mode"
    done
)

desktop-file-validate ui/io.github.xeintdm.Afterbeat.desktop
env -u WAYLAND_DISPLAY XDG_SESSION_TYPE=x11 xvfb-run -a --server-args="-screen 0 1600x1000x24 -nolisten tcp" "${PYTHON:-python3}" -B scripts/delivery/smoke_x11.py "$target/release/afterbeat"
dbus-run-session -- "${PYTHON:-python3}" -B scripts/delivery/smoke_wayland.py "$target/release/afterbeat"
