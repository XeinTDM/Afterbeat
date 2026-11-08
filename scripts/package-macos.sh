#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

tag=${1:-}
output=${2:-target/dist}
identity=${AFTERBEAT_CODESIGN_IDENTITY:--}
notary_profile=${AFTERBEAT_NOTARY_PROFILE:-}
IFS=$'\t' read -r version target < <(cargo metadata --format-version 1 --no-deps --locked |
    python3 -c 'import json, sys
metadata = json.load(sys.stdin)
version = next(package["version"] for package in metadata["packages"] if package["name"] == "afterbeat")
print(version, metadata["target_directory"], sep="\t")')

if [[ -n "$tag" && "$tag" != "v$version" ]]; then
    printf "Release tag '%s' must match Cargo.toml version v%s.\n" "$tag" "$version" >&2
    exit 1
fi
if [[ -n "$notary_profile" && "$identity" == - ]]; then
    echo 'Notarization requires an AFTERBEAT_CODESIGN_IDENTITY.' >&2
    exit 1
fi

binary="$target/release/afterbeat"
case "$(lipo -archs "$binary")" in
    arm64) arch=arm64 ;;
    x86_64) arch=x64 ;;
    *) echo 'Expected a native arm64 or x86_64 Mach-O executable.' >&2; exit 1 ;;
esac
if [[ "$("$binary" --version)" != "afterbeat $version" ]]; then
    echo 'Executable version does not match Cargo.toml. Run scripts/verify-macos.sh.' >&2
    exit 1
fi

archive="Afterbeat-v$version-macos-$arch.zip"
stage=$(mktemp -d)
trap 'rm -r -- "$stage"' EXIT
bundle="$stage/payload/Afterbeat.app"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources" "$output"
install -m 755 "$binary" "$bundle/Contents/MacOS/afterbeat"
install -m 644 ui/afterbeat.icns "$bundle/Contents/Resources/afterbeat.icns"
install -m 644 ui/Afterbeat-Info.plist "$bundle/Contents/Info.plist"
install -m 644 LICENSE THIRD-PARTY-NOTICES.txt docs/usage.md "$stage/payload"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" \
    -c "Set :CFBundleVersion $version" "$bundle/Contents/Info.plist"
plutil -lint "$bundle/Contents/Info.plist"

if [[ "$identity" == - ]]; then
    codesign --force --sign - "$bundle"
else
    codesign --force --options runtime --timestamp --sign "$identity" "$bundle"
fi
if [[ -n "$notary_profile" ]]; then
    ditto -c -k --keepParent "$bundle" "$stage/notarize.zip"
    xcrun notarytool submit "$stage/notarize.zip" --keychain-profile "$notary_profile" --wait
    xcrun stapler staple "$bundle"
    xcrun stapler validate "$bundle"
fi
codesign --verify --strict "$bundle"
ditto -c -k --sequesterRsrc "$stage/payload" "$stage/$archive"
ditto -x -k "$stage/$archive" "$stage/check"
codesign --verify --strict "$stage/check/Afterbeat.app"
mv -f -- "$stage/$archive" "$output/$archive"
(cd "$output" && shasum -a 256 "$archive" > SHA256SUMS.txt)
printf '%s\n' "$output/$archive"
