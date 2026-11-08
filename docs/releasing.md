# Releases

## Verify a download

Download the file for your platform and `SHA256SUMS.txt` from the same release. Run the command for your platform with both files in the current directory. Substitute the filename you downloaded.

```powershell
$download = 'Afterbeat-v1.2.7-windows-x64.exe' # Use the actual .exe or .zip filename.
$entry = Get-Content -LiteralPath SHA256SUMS.txt | Where-Object { ($_ -split '\s+', 2)[1] -ceq $download }
if (-not $entry) { throw 'The download is missing from SHA256SUMS.txt.' }
$expected = ($entry -split '\s+', 2)[0]
if ((Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash -ne $expected) {
    throw 'The download does not match SHA256SUMS.txt.'
}
```

```bash
archive=Afterbeat-v1.2.7-linux-x64.tar.gz
awk -v archive="$archive" '$2 == archive' SHA256SUMS.txt | sha256sum --check --strict -
```

```bash
archive=Afterbeat-v1.2.7-macos-arm64.zip
# For Intel, set archive to Afterbeat-v1.2.7-macos-x64.zip.
awk -v archive="$archive" '$2 == archive {print; found=1} END {if (!found) exit 1}' \
    SHA256SUMS.txt > afterbeat.sha256 && shasum -a 256 -c afterbeat.sha256
```

For public releases, each executable and archive carries GitHub build provenance. Use an authenticated GitHub CLI to verify the file you downloaded.

```powershell
gh attestation verify $download --repo XeinTDM/Afterbeat --signer-workflow XeinTDM/Afterbeat/.github/workflows/release.yml
```

The Windows executable is unsigned. The macOS workflow uses an ad hoc signature. [GitHub build provenance](https://docs.github.com/en/actions/concepts/security/artifact-attestations)

To read the embedded license and third-party notices from a standalone Windows executable, set `$download` to its filename and export the text from PowerShell.

```powershell
Start-Process -FilePath (Resolve-Path -LiteralPath $download).Path -ArgumentList '--licenses' -Wait -RedirectStandardOutput afterbeat-licenses.txt
```

## Prepare a release

1. Update `Cargo.toml`, the matching Afterbeat entry in `Cargo.lock`, and `CHANGELOG.md` for a new version. Run the Windows, Linux, and macOS checks, including native delivery verification. [Build and verification](build.md)
2. Merge to `main` and wait for CI on the release commit. Push a single matching annotated tag from that commit. Replace `v1.2.7` below with the version you are releasing.

   ```powershell
   git tag -a v1.2.7 -m 'Afterbeat v1.2.7'
   git push origin v1.2.7
   ```

3. The **Draft release** workflow checks that the tag is on the default branch and matches the version. It builds and verifies the Windows x64 executable and ZIP, the Linux x64 archive, and both macOS archives. It attests each download for public repositories and attaches all downloads and their checksums to the draft.
4. Review the draft notes against the changelog. Verify the downloads, extract the archives, and check that the app starts and the Test pad counts clicks on each platform. Publish the reviewed draft on GitHub.

If draft creation fails after the draft appears, inspect its assets before proceeding. Delete an incomplete draft without deleting its tag, then rerun the workflow. The workflow fails when a release already exists for the tag.

After building and verifying the release, run `./scripts/package.ps1` on Windows, `bash scripts/package.sh` on Linux, or `bash scripts/package-macos.sh` on macOS. Each writes its archive and checksum to `target/dist`.

For a Developer ID macOS build, set `AFTERBEAT_CODESIGN_IDENTITY` to the installed Developer ID Application identity. For notarization, also set `AFTERBEAT_NOTARY_PROFILE` to a configured `notarytool` keychain profile. The packaging script submits the signed bundle and staples the returned ticket before making the release ZIP. [Apple’s notarization instructions](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)

## First GitHub setup

Use `XeinTDM/Afterbeat` with `main` as the default branch to match the project links. Add the topics `windows`, `linux`, `x11`, `macos`, `autoclicker`, `rust`, `slint`, and `accessibility`.

For an existing GitHub repository, connect the local checkout before the first push.

```powershell
git remote add origin https://github.com/XeinTDM/Afterbeat.git
git push -u origin main
```

After the initial push, require pull requests and the **Windows x64 verification**, **Linux x64 verification**, **macOS arm64 verification**, and **macOS x64 verification** checks on `main`. Enable Dependabot alerts and private vulnerability reporting. Enable immutable releases before publishing the first draft. [Private reporting setup](https://docs.github.com/en/code-security/how-tos/report-and-fix-vulnerabilities/configure-vulnerability-reporting/configure-for-a-repository) · [Immutable release setup](https://docs.github.com/en/code-security/how-tos/secure-your-supply-chain/establish-provenance-and-integrity/prevent-release-changes)
