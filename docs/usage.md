# Using Afterbeat

## Install and update

Extract the Windows x64 ZIP and open `afterbeat.exe`. Afterbeat runs from the extracted folder. For an update, close the app and extract the newer ZIP. The new executable reads the same local settings and presets. [Downloads](https://github.com/XeinTDM/Afterbeat/releases) · [Verify a download](https://github.com/XeinTDM/Afterbeat/blob/main/docs/releasing.md#verify-a-download)

## Input and timing

Change units next to the interval on **Clicker** or **Timing**. In a compact window, **More options** on Clicker opens the quick intervals, click type, and start delay. Select **Shortcuts** to see the full key bindings.

- **Mouse** supports left, right, and middle buttons. Choose a single or double click.
- **Key press** sends a Windows key event, such as `Space`, `Enter`, or `F5`. For uppercase letters, Afterbeat uses Shift when needed. **Text** sends one Unicode character in Tap mode. Keyboard output pauses while Afterbeat is in the foreground.
- Set a delay or a rate per second, minute, hour, or day. Delays range from 0.1 ms to one day. Enter `Max` in any cadence unit or zero in a delay unit to select **Max speed**. **Variable rate** picks a rate from a range for each interval. Decimal points and commas are accepted.
- **Tap** sends a quick press and release. **Timed** lets you set the press duration. **Hold down** keeps the input pressed while the run is active and its guards allow it. It requires continuous repeat and a single target.
- A **Counted** run supports up to 100 million actions. A **Time limit** starts with the first dispatch and includes pauses. Set both to stop at whichever limit comes first. A double click counts as one action.

### Precise and Low CPU

**Precise** schedules fixed-rate taps individually. At high rates, the worker can spin briefly near each deadline to keep spacing steady.

**Low CPU** sends two clicks at a time from 50 to 500 clicks per second and three at a time above 500, up to 1,000. It sleeps between groups. It applies to fixed-rate single mouse taps with Toggle activation, an unmodified shortcut, and no route or guards. Afterbeat uses Precise when the setup falls outside those conditions. [Measured trade-offs](benchmarks.md)

At 500–1,000 clicks per second, both modes can briefly shorten the gaps between submissions after Windows delays input.

## Activation and stop

- **Toggle** uses the selected start delay. **Hold** starts as soon as you hold the activation shortcut and stops when you release it. With **Key press** output, Hold requires a shortcut without modifiers.
- Edit the shortcut in **Timing** and select **Apply**. Examples include `F6`, `Ctrl+Alt+G`, `Mouse4`, and `Mouse5`. For side-button chords, hold only the configured modifiers before pressing the button. Side-button events still reach the foreground app.
- **F7** stops the run or cancels capture. The taskbar thumbnail also offers **Stop run** while active. Afterbeat requires a registered stop shortcut to start. If F7 or F8 is occupied, it tries the corresponding Ctrl+Alt combination. The shortcut bar shows the active bindings.
- In compact mode, the **Stop run** button shows the emergency shortcut during a run. When a run pauses, the status explains what to do to resume.
- Turn off the **Master switch** to prevent new runs. Input already submitted to Windows can finish after Stop.

## Targets and guards

| Control | Behavior |
| --- | --- |
| **Follow cursor** | Click wherever the pointer is. |
| **Fixed point** | Capture the pointer with **Pick in 3s** or **F8** on Clicker. |
| **Route** | On Targets, capture up to 64 points. Reorder them, then choose **Loop** or **Once**. Set a radius to vary each click position. |
| **Inside only** | Pause outside the rectangle. Works with Follow cursor or key output without a route. |
| **Stop zone** | End the run when the pointer enters the rectangle or the next target falls inside it. |
| **Screen stop** | End the run at a monitor corner or edge. |
| **App filter** | Allow or block comma-separated executable names. Mouse output checks the window at the target and any window with mouse capture. Keyboard output checks the foreground app. A press releases if the filter pauses the run. |

Runs with saved targets pause if Windows confines the cursor away from the next point. Release cursor confinement to resume.

Capture two opposite corners to define a zone. Saved points use physical desktop coordinates, including negative coordinates on other displays. **Targets → Dry run** shows setup blockers separately from the geometry results for the saved fixed point or route centers. If a setting blocks Start, use **Fix in Clicker**, **Fix in Timing**, or **Fix in Targets** to go to it. For Follow cursor or keyboard output without a route, choose **Sample pointer in 3 s** and move to the location you want to check. The preview shows the coordinates it checked and sends no input. Live pointer movement, app filters, randomized positions, and cadence can affect a real run.

## Presets, delivery, and recovery

Save a named setup on **Presets**. Presets include the activation shortcut and pacing choice. If a preset's shortcut is occupied, Afterbeat keeps the current setup. The Presets page also shows lifetime runs, actions, and elapsed time.

The session counter tracks actions accepted by Windows. The **Test pad** counts presses that reach it. Windows allows input injection into apps at the same or a lower privilege level. Afterbeat tags its generated input so its own controls ignore it while the test pad still counts delivered presses.

Open **Help → User guide** for setup instructions or **Help → Open data folder** for settings, presets, and the local diagnostics log. Afterbeat uses `%LOCALAPPDATA%\Afterbeat` or an existing `PulseClick` folder if the new folder is absent. It saves with atomic replacement, keeps a previous good copy, and preserves damaged files with numbered `.corrupt` suffixes as needed. These files stay on the computer. Review them for private paths, app names, and coordinates before sharing them in a report.

## If a run does not start or reach the target

- Check the Master switch and any configuration message on Clicker. The shortcut bar shows whether Afterbeat registered the expected shortcuts or is using a fallback.
- Start with the Test pad to confirm mouse delivery. For key output, focus another app after starting the run.
- Check the target window, active app filter, stop zones, and whether the target is running at a higher privilege level. A cadence describes Afterbeat's requested pace. An app can process input at a different rate.
