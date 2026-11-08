# Build from source

## Windows

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

## Linux

Use Rustup and a GNU/Linux toolchain. The portable x64 release is built on Ubuntu 24.04. On Ubuntu or Debian, install the build and verification dependencies.

```bash
sudo apt-get install build-essential libfontconfig1-dev libxcb1-dev libxkbcommon-dev \
    libxkbcommon-x11-0 dbus-daemon desktop-file-utils python3-gi python3-xlib weston xauth xvfb
cargo build --release --locked
./target/release/afterbeat
```

Launch the app in an X11 or Wayland session. Run the following checks from any Linux terminal with the verification dependencies installed.

```bash
bash scripts/verify.sh
bash scripts/package.sh
```

The verification script runs formatting, Clippy, Rust tests, offscreen preview renders, and desktop entry validation. The Xvfb release test covers exact counts, fixed points, routes, process filters, emergency stop, held input, side-button shortcuts, keyboard output, and Unicode delivery. The Wayland checks map the release window under Weston and use an isolated D-Bus portal fixture to verify grant handling, global activation and release, emergency stop, input delivery requests, and revocation.

### Wayland desktop verification

The Ubuntu 24.04 x64 archive was tested on Debian 13 with KWin 6.3.6 and the KDE portal 6.3.5 in a nested Wayland session under WSL. Afterbeat opened the desktop permission prompt, delivered counted mouse clicks and keystrokes to a separate Wayland app, and released a held mouse button on F7 and on release of the Hold shortcut.

When the portal closed the session during a held press, Afterbeat stopped and the receiver did not see a mouse-up event. The nested compositor also declined display sharing because it lacked OpenGL compositing. A standalone Wayland desktop test is still needed for fixed points and routes, and to determine whether the missed release occurs outside the nested session.

## macOS

Use an Apple silicon or Intel Mac running macOS 13 or later with Rustup, Python 3, and Xcode Command Line Tools. Use the Rustup host matching the Mac’s architecture. Run the verification and packaging scripts from the repository root.

```bash
bash scripts/verify-macos.sh
bash scripts/package-macos.sh
```

Verification runs formatting, Clippy, Rust tests, and offscreen preview renders. Packaging builds the Finder-launchable `Afterbeat.app` bundle around the verified executable and checks the signature after extracting the ZIP. Unzip it and follow the [macOS permission setup](usage.md#macos) before testing live input.

### Interactive delivery checks

Run these checks from a local macOS desktop session with the packaged app.

1. Open the app before granting permissions. Verify that Start is blocked and **Help → Input permissions** requests access. Grant both permissions, quit, and reopen. Confirm the shortcut bar shows F6, F7, and F8.
2. Run five counted taps over the Test pad and confirm the count increases by exactly five. Hover over another Afterbeat button during a run and confirm the generated presses leave the control alone. Run a double click over the pad and confirm it adds two hits for one action.
3. Run counted key presses in TextEdit with an uppercase letter in the current input source, then send a single emoji using **Text**. Verify that switching focus to Afterbeat pauses key output. Check that F7 stops a live run from TextEdit.
4. Capture fixed points on each connected display, including a display to the left of the primary one if available. Dry run the points, then run a short route with a stop zone. Confirm Dry run dispatches no clicks and that the live route ends before pressing inside the stop zone.
5. Test allow and block filters with an external app’s executable name for both mouse and keyboard output. Switch recipients during a timed press and confirm the old recipient gets the release and the new one receives no press until its guard permits it.
6. Test Hold activation by releasing the activation shortcut while the mouse button is down. Repeat with F7 and then with **Quit Afterbeat** from the Dock. Confirm the mouse button releases each time. If you have a side-button mouse, repeat Hold activation using Mouse4.

## Credits

Blur AutoClicker’s feature list and release notes informed some of Afterbeat’s controls. [Features](https://github.com/Blur009/Blur-AutoClicker#features) · [Release notes](https://github.com/Blur009/Blur-AutoClicker/releases)

Slint attribution appears under **Help → About Afterbeat**. [Third-party notices](../THIRD-PARTY-NOTICES.txt)
