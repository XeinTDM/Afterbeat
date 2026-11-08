# Releases

## Verify a download

Download the Windows x64 ZIP and `SHA256SUMS.txt` from the same release. In PowerShell, run the following with both files in the current directory. Substitute the ZIP name for the version you downloaded.

```powershell
$zip = 'Afterbeat-v1.2.6-windows-x64.zip'
$expected = (Get-Content -LiteralPath SHA256SUMS.txt).Split(' ')[0]
if ((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash -ne $expected) {
    throw 'The ZIP does not match SHA256SUMS.txt.'
}
```

For public releases, the ZIP also carries GitHub build provenance. Use an authenticated GitHub CLI to verify it.

```powershell
gh attestation verify $zip --repo XeinTDM/Afterbeat --signer-workflow XeinTDM/Afterbeat/.github/workflows/release.yml
```

The portable executable is unsigned in Windows. The attestation applies to the ZIP produced by the GitHub build. [GitHub build provenance](https://docs.github.com/en/actions/concepts/security/artifact-attestations)

## Prepare a release

1. Update `Cargo.toml`, the matching Afterbeat entry in `Cargo.lock`, and `CHANGELOG.md` for a new version. Verify on Windows x64, including the [native delivery smoke test](build.md#native-delivery-verification).
2. Merge to `main` and wait for CI on the release commit. Push a single matching annotated tag from that commit. For the first release, use `v1.2.6`.

   ```powershell
   git tag -a v1.2.6 -m 'Afterbeat v1.2.6'
   git push origin v1.2.6
   ```

3. The **Draft release** workflow checks that the tag is on the default branch and matches the version. It builds and verifies the app, packages the ZIP, attests it for public repositories, and creates a draft with the ZIP and checksum.
4. Review the draft notes against the changelog. Download and verify the assets, extract the ZIP, and check that the app starts and that the Test pad receives clicks. Publish the reviewed draft on GitHub.

If draft creation fails after the draft appears, inspect its assets before proceeding. Delete an incomplete draft without deleting its tag, then rerun the workflow. The workflow fails when a release already exists for the tag.

To assemble the same package layout locally after building and verifying the x64 release, run `./scripts/package.ps1`. It writes the ZIP and checksum to `target/dist`.

## First GitHub setup

Use `XeinTDM/Afterbeat` with `main` as the default branch to match the project links. Add the repository description **Portable Windows auto-clicker and key repeater with routes, presets, and safety controls** and the topics `windows`, `autoclicker`, `rust`, `slint`, and `accessibility`.

When ready to publish the source, run this from the local `main` checkout with an authenticated GitHub CLI.

```powershell
gh repo create XeinTDM/Afterbeat --public --source . --remote origin --push --description 'Portable Windows auto-clicker and key repeater with routes, presets, and safety controls'
```

After the initial push, require pull requests and the **Windows x64 verification** check on `main`. Enable Dependabot alerts and private vulnerability reporting. Enable immutable releases before publishing the first draft. [Private reporting setup](https://docs.github.com/en/code-security/how-tos/report-and-fix-vulnerabilities/configure-vulnerability-reporting/configure-for-a-repository) · [Immutable release setup](https://docs.github.com/en/code-security/how-tos/secure-your-supply-chain/establish-provenance-and-integrity/prevent-release-changes)
