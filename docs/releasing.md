# Releases

## Verify a download

Download the archive for your platform and `SHA256SUMS.txt` from the same release. Run the command for your platform with both files in the current directory. Substitute the version you downloaded.

```powershell
$zip = 'Afterbeat-v1.2.6-windows-x64.zip'
$entry = Get-Content -LiteralPath SHA256SUMS.txt | Where-Object { ($_ -split '\s+', 2)[1] -ceq $zip }
if (-not $entry) { throw 'The ZIP is missing from SHA256SUMS.txt.' }
$expected = ($entry -split '\s+', 2)[0]
if ((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash -ne $expected) {
    throw 'The ZIP does not match SHA256SUMS.txt.'
}
```

```bash
archive=Afterbeat-v1.2.6-linux-x64.tar.gz
awk -v archive="$archive" '$2 == archive' SHA256SUMS.txt | sha256sum --check --strict -
```

```bash
archive=Afterbeat-v1.2.6-macos-arm64.zip
# For Intel, set archive to Afterbeat-v1.2.6-macos-x64.zip.
awk -v archive="$archive" '$2 == archive {print; found=1} END {if (!found) exit 1}' \
    SHA256SUMS.txt > afterbeat.sha256 && shasum -a 256 -c afterbeat.sha256
```

For public releases, each archive carries GitHub build provenance. Use an authenticated GitHub CLI to verify the one you downloaded.

```powershell
gh attestation verify $zip --repo XeinTDM/Afterbeat --signer-workflow XeinTDM/Afterbeat/.github/workflows/release.yml
```

The Windows executable is unsigned. The macOS workflow uses an ad hoc signature. Attestations apply to the archives produced by GitHub. [GitHub build provenance](https://docs.github.com/en/actions/concepts/security/artifact-attestations)

## Prepare a release

1. Update `Cargo.toml`, the matching Afterbeat entry in `Cargo.lock`, and `CHANGELOG.md` for a new version. Run the Windows, Linux, and macOS checks, including native delivery verification. [Build and verification](build.md)
2. Merge to `main` and wait for CI on the release commit. Push a single matching annotated tag from that commit. For the first release, use `v1.2.6`.

   ```powershell
   git tag -a v1.2.6 -m 'Afterbeat v1.2.6'
   git push origin v1.2.6
   ```

3. The **Draft release** workflow checks that the tag is on the default branch and matches the version. It builds and verifies the Windows and Linux x64 packages and both macOS packages, attests them for public repositories, and creates a draft with the archives and their checksums.
4. Review the draft notes against the changelog. Verify and extract each archive, then check that the app starts and the Test pad counts clicks on each platform. Publish the reviewed draft on GitHub.

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
