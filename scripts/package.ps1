#Requires -Version 7.0

param(
    [string]$Tag,
    [string]$OutputDirectory = 'target/dist'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
Push-Location $root
try {
    $metadata = cargo metadata --format-version 1 --no-deps --locked
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $metadata = $metadata | ConvertFrom-Json
    $version = ($metadata.packages | Where-Object name -eq 'afterbeat').version
    if ($Tag -and $Tag -cne "v$version") {
        throw "Release tag '$Tag' must match Cargo.toml version v$version."
    }

    $exe = Join-Path $metadata.target_directory 'release/afterbeat.exe'
    $binary = Get-Item -LiteralPath $exe
    if ($binary.VersionInfo.ProductVersion -cne $version) {
        throw "Executable version '$($binary.VersionInfo.ProductVersion)' does not match $version. Run scripts/verify.ps1."
    }

    # The COFF machine word follows the PE signature at the header offset in the DOS stub.
    $reader = [System.IO.BinaryReader]::new($binary.OpenRead())
    try {
        $reader.BaseStream.Position = 0x3c
        $reader.BaseStream.Position = $reader.ReadUInt32()
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne 0x8664) {
            throw 'The portable Windows release requires an x64 PE executable.'
        }
    } finally {
        $reader.Dispose()
    }

    $output = [System.IO.Path]::GetFullPath($OutputDirectory, $root)
    $archiveName = "Afterbeat-v$version-windows-x64.zip"
    $portableName = "Afterbeat-v$version-windows-x64.exe"
    $archive = Join-Path $output $archiveName
    $portable = Join-Path $output $portableName
    New-Item -ItemType Directory -Path $output -Force | Out-Null
    Compress-Archive -LiteralPath $exe, 'LICENSE', 'THIRD-PARTY-NOTICES.txt', 'docs/usage.md' -DestinationPath $archive -Force
    Copy-Item -LiteralPath $exe -Destination $portable -Force
    $checksums = foreach ($file in @($archive, $portable)) {
        $hash = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash  $(Split-Path $file -Leaf)"
    }
    Set-Content -LiteralPath (Join-Path $output 'SHA256SUMS.txt') -Value (($checksums -join [char]10) + [char]10) -Encoding ascii -NoNewline
    Write-Output $archive, $portable
} finally {
    Pop-Location
}
