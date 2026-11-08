# Build from source

Use x64 Windows, PowerShell 7, Rustup, and Visual Studio Build Tools with the **Desktop development with C++** workload and a Windows SDK. Use the `x86_64-pc-windows-msvc` Rustup host. The build links the MSVC runtime statically. Rustup installs the pinned compiler, formatter, and linter on the first Cargo invocation. [Rust toolchain](../rust-toolchain.toml) · [Dependency lockfile](../Cargo.lock)

Build from the repository root.

```powershell
cargo build --release --locked
```

Open `target\release\afterbeat.exe`.

## Verify changes

```powershell
.\scripts\verify.ps1
py -3.12 -B -m unittest discover -s scripts/delivery -p 'test_*.py'
```

The PowerShell script checks formatting, runs Clippy and Rust tests, builds the release executable, and runs offscreen keyboard, Dry run, and compact preview checks. The Python command runs the delivery harness unit tests using Python 3.12 and its standard library. For a local scheduler check, use the following command.

```powershell
cargo test --release --locked benchmark_timed_scheduler -- --ignored --nocapture
```

The scheduler test measures in-process dispatch. The [delivery benchmarks](benchmarks.md) measure clicks received by a separate Windows app.

### Native delivery verification

On an interactive Windows test session with Afterbeat closed, install the pinned Python dependencies and opt in to the delivery checks.

```powershell
py -3.12 -m pip install -r scripts/delivery/requirements.txt
.\scripts\verify.ps1 -DeliverySmoke
```

The native smoke test temporarily switches to a private Windows desktop. It launches the release binary with isolated settings and checks exact counted clicks, emergency stop, and release of a held mouse button. It probes F7 and the Ctrl+Alt+F7 fallback in Afterbeat's registration order.

The same test uses Windows UI Automation to read the cadence, invoke the Test pad, navigate to Dry run, observe the countdown, and read the sampled location. It checks that Dry run sends no clicks to the receiver. The run directory under `target/delivery` includes `uia.json` with each assertion and a tree snapshot on failure. Use a session you can leave untouched until the desktop returns.

## Credits

Blur AutoClicker’s feature list and release notes informed some of Afterbeat’s controls. [Features](https://github.com/Blur009/Blur-AutoClicker#features) · [Release notes](https://github.com/Blur009/Blur-AutoClicker/releases)

Slint attribution appears under **Help → About Afterbeat**. [Third-party notices](../THIRD-PARTY-NOTICES.txt)
