# Using Afterbeat

## Install and update

On Windows, open the direct x64 `.exe` if listed, or extract the ZIP and open `afterbeat.exe`. On Linux, extract the x64 tarball and run `./afterbeat` from the extracted folder. To update, close the app and open the new executable or extract the newer archive. The new version reads the same local settings and presets. [Downloads](https://github.com/XeinTDM/Afterbeat/releases) · [Verify a download](https://github.com/XeinTDM/Afterbeat/blob/main/docs/releasing.md#verify-a-download)

### Linux

The x64 archive targets Ubuntu 24.04 or a compatible distribution with glibc 2.39 or newer, Fontconfig, and your desktop’s input libraries. **Open data folder** uses `xdg-open`.

To add a launcher, install the following files from the archive. Make sure your desktop session includes `~/.local/bin` in its `PATH`, or set the launcher's `Exec` and `TryExec` to the absolute path of the installed binary.

```bash
install -Dm755 afterbeat ~/.local/bin/afterbeat
install -Dm644 io.github.xeintdm.Afterbeat.desktop "${XDG_DATA_HOME:-$HOME/.local/share}/applications/io.github.xeintdm.Afterbeat.desktop"
install -Dm644 afterbeat.svg "${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/scalable/apps/afterbeat.svg"
```

#### X11

Use a local X11/Xorg session with XTEST 2.2, XInput 2.1, RandR 1.5, and X-Resource 1.2. On Ubuntu the runtime packages include `libx11-6`, `libx11-xcb1`, `libxcb1`, `libfontconfig1`, `libxkbcommon0`, and `libxkbcommon-x11-0`. Use **F7** or the in-app **Stop** button. Afterbeat uses **Ctrl+Shift+F7** as the fallback when the desktop already uses F7.

#### Wayland

Use a Wayland session whose XDG Desktop Portal backend provides **Global Shortcuts** and **Remote Desktop** with keyboard or pointer notification support. Install the launcher so the desktop can identify Afterbeat. Choose **Enable input**, bind keyboard shortcuts for Start/Stop and Emergency Stop in the desktop dialog, then grant the input devices you want to use. Afterbeat displays the shortcuts your desktop assigned. The **Desktop shortcuts** button opens the portal’s settings when the backend provides that feature. Presets leave those shortcut assignments in the desktop’s settings. [Global Shortcuts](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html) · [Remote Desktop](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html)

Use a native Linux desktop for Wayland automation. The bundled Slint backend selects X11 when it detects WSL.

For fixed points and routes, choose **Enable with displays** and select the displays you want to use. If the portal returns logical display bounds, enter whole X and Y coordinates within those bounds and choose **Use coordinates** on Clicker or **Add coordinates** on Targets. Afterbeat uses the shared display geometry without opening a screen video stream. Wayland points and routes are stored separately from physical desktop coordinates and must be re-entered when switching coordinate systems. Fixed and route moves use the desktop’s absolute-input API. The portal cannot confirm where the cursor landed if another app confines it.

The emergency shortcut, Hold release, master switch, countdown, repeat limits, and timed release are available. Keyboard output pauses while Afterbeat is focused. Use an unmodified activation shortcut or start keyboard output from the app, since the portal only reports when a shortcut becomes inactive. During mouse automation, select **Stop run**, use the emergency shortcut, or press **Escape** while Afterbeat has focus. Afterbeat filters presses on its other controls and counts presses on the **Test pad**. **Disconnect** ends the permission session. If the desktop revokes access or the emergency shortcut disappears, Afterbeat stops the run.

Wayland setups that require live cursor capture, screen-edge stops, cursor zones, or an app process filter are blocked. Those guards depend on live cursor or process data, which Afterbeat’s input and display portal session does not provide. **Dry run** can preview manually entered targets on shared displays. Wayland delivers actions individually and the achievable rate depends on the desktop portal. [Wayland protocol](https://wayland.freedesktop.org/docs/book/Protocol.html) · [Input Capture portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.InputCapture.html)

### macOS

On macOS 13 or later, use the `macos-arm64` ZIP for Apple silicon or the `macos-x64` ZIP for Intel. Extract it, move `Afterbeat.app` to Applications, and open it. GitHub builds use ad hoc signing. If macOS asks you to approve the verified download, use the per-app **Open Anyway** control in **System Settings → Privacy & Security**. [Apple’s instructions](https://support.apple.com/en-us/102445)

Choose **Help → Input permissions** in Afterbeat, then enable Afterbeat under **System Settings → Privacy & Security → Accessibility** and **Input Monitoring**. Quit and reopen Afterbeat after granting access. If an update asks for access again, remove the old entries and add the current app.

Use **Fn+F6**, **Fn+F7**, and **Fn+F8** if your keyboard uses those keys for media controls. In **Timing**, you can also use chords such as `Cmd+F6`, `Ctrl+Option+G`, or `Option+Mouse4`. Reapply a letter or symbol shortcut after changing the keyboard layout to use the new layout’s key. Afterbeat stops automation while macOS Secure Input is active.

## Input and timing

Change units next to the interval on **Clicker** or **Timing**. In a compact window, **More options** on Clicker opens the quick intervals, click type, and start delay. Select **Shortcuts** to see the full key bindings.

- **Mouse** supports left, right, middle, **Mouse4**, and **Mouse5**. Choose **Single**, **Double**, or **Triple**. A double or triple click counts as one action. Use a different button for output and the activation shortcut.
- **Wheel** scrolls up, down, left, or right. Each action sends 1–32 ticks. It works with Follow cursor, Fixed point, and routes. On Windows, focus the window at the wheel target. Wayland requires a portal backend that supports discrete scrolling. [Remote Desktop portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html#notifypointeraxisdiscrete)
- **Key press** sends a key event, such as `Space`, `Enter`, or `F5`. For uppercase letters, Afterbeat uses Shift when needed. **Text** sends one Unicode character in Tap mode. Keyboard output pauses while Afterbeat is in the foreground. On macOS and X11, key presses use the active keyboard layout. On macOS, use **Text** for characters that require Option or a composition sequence. X11 Text can use a temporary unused keycode for characters outside the layout. On Wayland, the portal interprets the key symbol and the receiving app must support the character. Use recognizable desktop shortcut labels such as F6 and F7 so Afterbeat can detect key conflicts.
- Set a delay or a rate per second, minute, hour, or day. Delays range from 0.1 ms to one day. Enter `Max` in any cadence unit or zero in a delay unit to select **Max speed**. **Variable rate** picks a rate from a range for each interval. Decimal points and commas are accepted.
- **Tap** sends a quick press and release. **Timed** lets you set the press duration. **Hold down** keeps the input pressed while the run is active and its guards allow it. It requires continuous repeat and a single target. On X11, use **Tap** for keyboard output so a focus change during a held press cannot redirect the release.
- A **Counted** run supports up to 100 million actions. A **Time limit** starts with the first dispatch and includes pauses. Set both to stop at whichever limit comes first.

### Precise and Low CPU

**Precise** schedules fixed-rate taps individually. At high rates, the worker can spin briefly near each deadline to keep spacing steady.

**Low CPU** sends two clicks at a time from 50 to 500 clicks per second and three at a time above 500, up to 1,000. It sleeps between groups. It applies to fixed-rate single mouse taps with Toggle activation, an unmodified shortcut, and no route or guards. Afterbeat uses Precise when the setup falls outside those conditions. [Measured trade-offs](benchmarks.md)

At 500–1,000 clicks per second, both modes can briefly shorten the gaps between submissions after the desktop delays input.

## Activation and stop

- **Toggle** uses the selected start delay. **Hold** starts as soon as you hold the activation shortcut and stops when you release it. With **Key press** output, Hold requires a shortcut without modifiers.
- Edit the shortcut in **Timing** and select **Apply**. Examples include `F6`, `Ctrl+Alt+G`, `Mouse4`, and `Mouse5`. For side-button chords, hold only the configured modifiers before pressing the button. Side-button events still reach the foreground app.
- **F7** stops the run or cancels capture. On Windows, the taskbar thumbnail also offers **Stop run** while active, and Afterbeat tries the corresponding Ctrl+Alt combination if F7 or F8 is occupied. Afterbeat requires a working stop shortcut to start. The shortcut bar shows the active bindings.
- In compact mode, the **Stop run** button shows the emergency shortcut during a run. When a run pauses, the status explains what to do to resume.
- Turn off the **Master switch** to prevent new runs. Input already submitted to the desktop can finish after Stop.

## Targets and guards

| Control | Behavior |
| --- | --- |
| **Follow cursor** | Click wherever the pointer is. |
| **Fixed point** | Capture the pointer with **Pick in 3s** or **F8** on Clicker. |
| **Route** | On Targets, capture up to 64 points. Reorder them, then choose **Loop** or **Once**. Set a radius to vary each click position. |
| **Inside only** | Pause outside the rectangle. Works with Follow cursor or key output without a route. |
| **Stop zone** | End the run when the pointer enters the rectangle or the next target falls inside it. |
| **Screen stop** | End the run at a monitor corner or edge. |
| **App filter** | Allow or block comma-separated executable names. Mouse output checks the window at the target. Keyboard output checks the foreground app. A press releases if the filter pauses the run. On Linux and macOS, enter the exact executable basename, such as `firefox` or `Safari`. Afterbeat pauses if it cannot identify the receiving process. |

On Windows, X11, and macOS, runs with saved targets pause if the desktop confines the cursor away from the next point. Release cursor confinement to resume. Delivery also pauses while the output key or mouse button is already down on these platforms. On X11, an input grab in another app can also pause delivery.

Capture two opposite corners to define a zone. Saved points use desktop coordinates in pixels on Windows and X11 and logical points on macOS. Layouts can include negative coordinates on displays above or to the left of the primary display. **Targets → Dry run** shows setup blockers separately from the geometry results for the saved fixed point or route centers. If a setting blocks Start, use **Fix in Clicker**, **Fix in Timing**, or **Fix in Targets** to go to it. For Follow cursor or keyboard output without a route, choose **Sample pointer in 3 s** and move to the location you want to check. The preview shows the coordinates it checked and sends no input. Live pointer movement, app filters, randomized positions, and cadence can affect a real run.

### Smart Targets on Windows

1. Open the target app and choose **Targets → Pick control** in Afterbeat. Point at a button or another accessible control and press **F8**, or keep the pointer there until the countdown ends. The shortcut bar shows the active capture binding and mode.
2. Add the rest of the steps and choose **Once** or **Loop**. You can mix control targets and coordinate points. Use single mouse taps and a zero radius.
3. Open **Dry run** to check the desktop. Select **Step** to see each result or **Check again** after changing the target app. Blue outlines show where Afterbeat found the controls. Open any dialogs you want to inspect before checking.
4. Leave the target app visible and start the route. Afterbeat waits for each control to be uniquely identified, enabled, and unobstructed. **Target timeout** sets how long it waits for a step before ending the run. Select a control step and choose **Wait until gone** to give that step a second timeout after its click. If verification times out, Afterbeat stops with the click already counted. **F7** stops a waiting run.

By default Afterbeat matches the app’s executable path, the captured window class and title, and the control’s type and automation ID or name. For duplicate controls it can save the identity of a unique enclosing control.

Choose **Control capture → With visual anchor** to make new F8 captures survive a changed control ID or window title. Point at the target as usual. Afterbeat chooses a nearby unique accessible label and saves two small grayscale fingerprints, one for the target and one for the label. Both stay in local settings and in any presets you save.

At run time, Afterbeat checks the label, both fingerprints, and the target’s position relative to the label at the current display scale. If the saved accessibility selector points somewhere else or the visual evidence is ambiguous, Afterbeat waits. In **Dry run**, choose **Step** to review the proposed location. Blue outlines mark targets, and gold outlines mark their anchors. **Repair target** rechecks the proposal and updates the current route if the target still matches. Save the repaired setup as a preset if you want to use it from **Presets** later.

### Per-step actions on Windows

- Capture a point or control with **F8** as usual. Select it and choose **Edit step** to set its action and delay. Use **Add step** to insert a wait or foreground key press after the selected row.
- A click can send one, two, or three taps with the selected button. Scroll steps send vertical or horizontal wheel ticks while the target window is foreground. A control-targeted key waits for a distinct native keyboard focus handle. Click the control earlier in the route if the app requires that to give it focus. For controls that share their window's native handle, use a foreground key step.
- A step delay is the minimum time before that step can send input. A **Wait** step adds its whole duration and sends no input. Target lookup and outcome verification can extend the route. The next step keeps its full delay.
- **Once** traverses the route once. Counted runs count each dispatched click group, scroll, or key press as one action. Waits consume no actions. The mixed-route time limit starts after the countdown and includes waits and target lookup. Unedited points and controls in a mixed route use the shared mouse or wheel output and cadence.

## Presets, delivery, and recovery

Save a named setup on **Presets**. Presets include the activation shortcut and pacing choice. If a preset's shortcut is occupied, Afterbeat keeps the current setup. The Presets page also shows lifetime runs, actions, and elapsed time.

The session counter tracks dispatched actions. The **Test pad** counts presses that reach it. Windows allows input injection into apps at the same or a lower privilege level. Afterbeat keeps generated input away from its own controls and counts generated presses over the Test pad.

Open **Help → User guide** for setup instructions or **Help → Open data folder** for settings, presets, and the local diagnostics log. On Windows, Afterbeat uses `%LOCALAPPDATA%\Afterbeat` or an existing `PulseClick` folder if the new folder is absent. On Linux, it uses `$XDG_CONFIG_HOME/afterbeat`, with `~/.config/afterbeat` as the default. On macOS, it uses `~/Library/Application Support/Afterbeat`. Afterbeat saves with atomic replacement, keeps a previous good copy, and preserves damaged files with numbered `.corrupt` suffixes as needed. These files stay on the computer. Review them for private paths, app names, and coordinates before sharing them in a report.

## If a run does not start or reach the target

- Check the Master switch and any configuration message on Clicker. The shortcut bar shows whether Afterbeat registered the expected shortcuts or is using a fallback.
- Start with the Test pad to confirm mouse delivery. For key output, focus another app after starting the run.
- Check the target window, active app filter, stop zones, and whether the target is running at a higher privilege level. A cadence describes Afterbeat's requested pace. An app can process input at a different rate.
