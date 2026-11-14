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
        foreach ($mode in @('keyboard-check', 'dry-run', 'compact-dry-fix', 'compact-text-paused', 'smart-targets', 'compact-smart-targets', 'wheel', 'compact-triple')) {
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
