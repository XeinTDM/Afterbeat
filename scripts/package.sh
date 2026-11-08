#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

tag=${1:-}
output=${2:-target/dist}
readarray -t package < <(cargo metadata --format-version 1 --no-deps --locked |
    python3 -c 'import json, sys
metadata = json.load(sys.stdin)
print(next(package["version"] for package in metadata["packages"] if package["name"] == "afterbeat"))
print(metadata["target_directory"])')
version=${package[0]}
binary="${package[1]}/release/afterbeat"
if [[ -n "$tag" && "$tag" != "v$version" ]]; then
    printf "Release tag '%s' must match Cargo.toml version v%s.\n" "$tag" "$version" >&2
    exit 1
fi

header=$(LC_ALL=C readelf -h "$binary")
if [[ "$header" != *ELF64* || "$header" != *'Advanced Micro Devices X86-64'* ]]; then
    echo 'The portable Linux release requires an x86-64 ELF executable.' >&2
    exit 1
fi
if [[ "$("$binary" --version)" != "afterbeat $version" ]]; then
    echo 'Executable version does not match Cargo.toml. Run scripts/verify.sh.' >&2
    exit 1
fi

archive="Afterbeat-v$version-linux-x64.tar.gz"
stage=$(mktemp -d)
trap 'rm -r -- "$stage"' EXIT
install -m 755 "$binary" "$stage/afterbeat"
install -m 644 LICENSE THIRD-PARTY-NOTICES.txt docs/usage.md ui/io.github.xeintdm.Afterbeat.desktop ui/afterbeat.svg "$stage"
mkdir -p "$output"
tar -czf "$output/$archive" -C "$stage" afterbeat io.github.xeintdm.Afterbeat.desktop afterbeat.svg LICENSE THIRD-PARTY-NOTICES.txt usage.md
(cd "$output" && sha256sum "$archive" > SHA256SUMS.txt)
printf '%s\n' "$output/$archive"
