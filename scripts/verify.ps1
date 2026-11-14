param([switch]$DeliverySmoke)

$ErrorActionPreference = 'Stop'
Push-Location (Split-Path $PSScriptRoot)
try {
    cargo fmt --all --check
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo clippy --release --locked --all-targets -- -D warnings
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo test --release --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

    cargo build --release --locked --example render_preview
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $metadata = cargo metadata --format-version 1 --no-deps --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $target = ($metadata | ConvertFrom-Json).target_directory
    $preview = Join-Path $target 'release/examples/render_preview.exe'
    $artifacts = Join-Path $target 'verification/previews'
    New-Item -ItemType Directory -Path $artifacts -Force | Out-Null
    Push-Location $artifacts
    try {
        foreach ($mode in @('route-details', 'compact-route-details', 'compact-text-route-details', 'step-editor', 'compact-step-editor', 'summary-check', 'preset-form-check', 'hold-summary', 'compact-hold-summary', 'compact-text-summary', 'compact-text-hold-summary', 'compact-text-save-name', 'compact-text-save-error', 'preset-load-error', 'ux-check', 'preset-load', 'compact-preset-load', 'compact-text-preset-load', 'zones', 'compact-zones', 'presets', 'compact-presets', 'compact-text-presets', 'compact-text-preset-details', 'timing', 'compact-timing', 'compact-text-timing', 'keyboard', 'compact-keyboard', 'compact-text-keyboard', 'compact-text-zones', 'undo-route', 'compact-undo-route', 'keyboard-check', 'shortcut-editing-check', 'reorder-check', 'route-reorder', 'compact-route-reorder', 'compact-text-route-reorder', 'workflow-check', 'light', 'dry-run', 'dry-results-check', 'dry-count-check', 'dry-wait-check', 'compact-dry-fix', 'compact-text', 'compact-text-blocked', 'compact-text-paused', 'compact-text-running', 'wayland-recovery', 'smart-targets', 'compact-smart-targets', 'wheel', 'compact-triple')) {
            & $preview $mode
            if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        }
    } finally {
        Pop-Location
    }

    if ($DeliverySmoke) {
        py -3.12 -B -m unittest discover -s scripts/delivery -p 'test_*.py'
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        py -3.12 -B scripts/delivery/run.py smoke --exe (Join-Path $target 'release/afterbeat.exe')
        exit $LASTEXITCODE
    }
    exit 0
} finally {
    Pop-Location
}
