"""Exercise control capture and replay against native Win32 buttons."""

import json
import os
from pathlib import Path
import subprocess
from threading import Event, Thread
import time


def check(output, manifest, windows):
    from pywinauto import Desktop

    api, gui, con = windows.api, windows.gui, windows.con
    events = []
    fixture = {}
    ready = Event()
    title = "Afterbeat Smart Targets test"
    folder = output / "smart-targets"
    folder.mkdir()
    data = folder / "local" / "Afterbeat"
    data.mkdir(parents=True)
    config = data / "settings.json"
    config.write_text(json.dumps({"delay_seconds": 0, "route_once": True, "target_timeout": "3"}))
    evidence = {"status": "failed", "checks": []}
    ready_to_click_ms = []
    resolved_runs = 10

    def pump():
        def handle(hwnd, message, wparam, lparam):
            if message == con.WM_APP:
                if wparam:
                    fixture["duplicate_control"] = button(hwnd, "Other export", 101, 92)
                else:
                    gui.DestroyWindow(fixture.pop("duplicate_control"))
                return 0
            if message == con.WM_COMMAND and lparam and (wparam >> 16) == con.BN_CLICKED:
                identity = wparam & 0xFFFF
                events.append((identity, time.perf_counter_ns()))
                if identity == 102:
                    gui.ShowWindow(lparam, con.SW_HIDE)
                return 0
            return gui.DefWindowProc(hwnd, message, wparam, lparam)

        try:
            cls = gui.WNDCLASS()
            cls.hInstance = api.GetModuleHandle(None)
            cls.lpszClassName = "AfterbeatSmartTargetsFixture"
            cls.lpfnWndProc = handle
            cls.hbrBackground = con.COLOR_WINDOW + 1
            atom = gui.RegisterClass(cls)

            def window(x, y):
                return gui.CreateWindowEx(
                    con.WS_EX_TOPMOST, atom, title, con.WS_OVERLAPPEDWINDOW,
                    x, y, 335, 190, 0, 0, cls.hInstance, None,
                )

            def button(parent, name, control_id, y):
                return gui.CreateWindowEx(
                    0, "BUTTON", name, con.WS_CHILD | con.WS_VISIBLE | con.BS_PUSHBUTTON,
                    20, y, 110, 30, parent, control_id, cls.hInstance, None,
                )

            main = window(20, 45)
            duplicate = window(10, 290)
            fixture.update(
                main=main, duplicate=duplicate, export=button(main, "Export", 101, 12),
                save=button(main, "Save", 102, 52), decoy=button(duplicate, "Export", 101, 12),
            )
        except Exception as error:
            fixture["error"] = error
        finally:
            ready.set()
        gui.PumpMessages()

    Thread(target=pump, name="smart-target-fixture", daemon=True).start()
    if not ready.wait(4):
        raise RuntimeError("Smart target fixture did not start.")
    if "error" in fixture:
        raise fixture["error"]

    stop_label, stop_modifiers = windows.emergency_binding()
    evidence["stop_binding"] = stop_label
    binary = Path(manifest["binary"]["path"])
    env = {**os.environ, "LOCALAPPDATA": str(data.parent), "APPDATA": str(folder / "roaming")}
    with windows.ProcessJob() as app:
        handle, pid = app.launch(binary, subprocess.list2cmdline([str(binary)]), env=env)
        root = None
        try:
            log = data / "diagnostics.log"

            def app_ready():
                hwnd = windows.window_for(pid, "Afterbeat")
                return hwnd if hwnd and log.exists() and "Input protection active." in log.read_text(
                    encoding="utf-8",
                ) else None

            hwnd = windows.wait_for(app_ready, 15, "Afterbeat did not start.")
            root = Desktop(backend="uia").window(handle=hwnd)
            gui.SetWindowPos(
                fixture["main"], con.HWND_TOPMOST, max(20, api.GetSystemMetrics(0) - 365), 45,
                0, 0, con.SWP_SHOWWINDOW | con.SWP_NOSIZE | con.SWP_NOACTIVATE,
            )

            def find(name, kind="Text", timeout=4):
                def match():
                    try:
                        return next((item for item in root.descendants(control_type=kind)
                                     if item.element_info.name == name), None)
                    except KeyError:
                        return None

                return windows.wait_for(match, timeout, f"Could not find {kind} {name!r}.")

            def invoke(name):
                find(name, "Button").iface_invoke.Invoke()

            def capture(name, control, first=False):
                if first:
                    invoke("Pick control")
                left, top, right, bottom = gui.GetWindowRect(fixture[control])
                api.SetCursorPos(((left + right) // 2, (top + bottom) // 2))
                windows.tap_key(con.VK_F8)
                find(name, "ListItem")

            find("Targets", "Button").iface_toggle.Toggle()
            capture("1. Export", "export", first=True)
            capture("2. Save", "save")
            invoke("Wait until gone")
            find("2. Save · Wait until gone", "ListItem")
            evidence["checks"].append("F8 captures real controls and keeps the capture mode")

            invoke("Dry run")
            find("Control targets and saved points")
            invoke("Step")
            find("Step 1 of 2 · Export · Control located and ready.")
            assert not events, events
            invoke("Back to Targets")
            evidence["checks"].append("Dry run resolves a live control without input")

            for trial in range(resolved_runs):
                begin = len(events)
                gui.ShowWindow(fixture["save"], con.SW_HIDE)
                gui.SetWindowPos(
                    fixture["main"], con.HWND_TOPMOST, 60 + 30 * (trial % 3), 85 + 20 * (trial % 2),
                    0, 0, con.SWP_NOSIZE | con.SWP_NOACTIVATE,
                )
                windows.tap_key(con.VK_F6)
                windows.wait_for(lambda: len(events) > begin, 2, "Export was not pressed in the relocated window.")
                time.sleep(0.08 + (trial % 4) * 0.025)
                assert len(events) == begin + 1, events
                gui.EnableWindow(fixture["save"], False)
                gui.ShowWindow(fixture["save"], con.SW_SHOWNA)
                time.sleep(0.08 + (trial % 3) * 0.025)
                assert len(events) == begin + 1, events
                enabled = time.perf_counter_ns()
                gui.EnableWindow(fixture["save"], True)
                windows.wait_for(lambda: len(events) >= begin + 2, 2, "Save was not pressed after it was enabled.")
                assert [identity for identity, _ in events[begin:]] == [101, 102], events
                find("Completed.")
                ready_to_click_ms.append((events[-1][1] - enabled) / 1_000_000)

            evidence["checks"].append("Relocated window, delayed and disabled target, verified disappearance")
            evidence["completed_routes"] = {"passed": len(ready_to_click_ms), "planned": resolved_runs}
            evidence["enable_to_click_ms"] = ready_to_click_ms
            evidence["enable_to_click_p95_ms"] = sorted(ready_to_click_ms)[(95 * resolved_runs - 1) // 100]

            begin = len(events)
            gui.SendMessage(fixture["main"], con.WM_APP, 1, 0)
            windows.tap_key(con.VK_F6)
            find("Multiple controls or windows match. Leave one match visible.")
            assert len(events) == begin, events
            gui.SendMessage(fixture["main"], con.WM_APP, 0, 0)
            windows.wait_for(lambda: len(events) == begin + 1, 2, "Export did not resume after removing the duplicate control.")
            windows.tap_key(con.VK_F7, modifiers=stop_modifiers)
            find("Stopped.")
            assert len(events) == begin + 1, events
            evidence["checks"].append("Duplicate control pauses replay until it is removed")

            gui.ShowWindow(fixture["duplicate"], con.SW_SHOWNA)
            windows.tap_key(con.VK_F6)
            find("Multiple controls or windows match. Leave one match visible.")
            requested = windows.tap_key(con.VK_F7, modifiers=stop_modifiers)
            find("Stopped.")
            evidence["stop_to_status_ms"] = (time.perf_counter_ns() - requested) / 1_000_000
            gui.ShowWindow(fixture["duplicate"], con.SW_HIDE)
            time.sleep(0.3)
            assert len(events) == begin + 1, events
            evidence["checks"].append("Ambiguous window pauses and F7 prevents a click after it clears")

            gui.SetWindowText(fixture["duplicate"], title + " overlay")
            left, top, _, _ = gui.GetWindowRect(fixture["main"])
            gui.SetWindowPos(
                fixture["duplicate"], con.HWND_TOPMOST, left, top, 0, 0,
                con.SWP_NOSIZE | con.SWP_SHOWWINDOW | con.SWP_NOACTIVATE,
            )
            windows.tap_key(con.VK_F6)
            find("Waiting for the control to be enabled and unobstructed.")
            windows.tap_key(con.VK_F7, modifiers=stop_modifiers)
            find("Stopped.")
            gui.ShowWindow(fixture["duplicate"], con.SW_HIDE)
            assert len(events) == begin + 1, events
            evidence["checks"].append("An unrelated overlapping window receives no click")

            capture("3. Export", "export")
            invoke("Wait until gone")
            invoke("Move up")
            invoke("Move up")
            find("1. Export · Wait until gone", "ListItem")
            windows.tap_key(con.VK_F6)
            windows.wait_for(lambda: len(events) == begin + 2, 2, "Export was not pressed for outcome verification.")
            gui.SetWindowText(fixture["main"], title + " (busy)")
            find("Step 1 timed out waiting for Export. The click was sent, but the control did not disappear.", timeout=5)
            assert len(events) == begin + 2, events
            evidence["checks"].append("Verification timeout stops subsequent steps without retrying the click")
            evidence["status"] = "passed"
        except Exception as error:
            evidence["error"] = str(error)
            try:
                if root is not None:
                    evidence["tree"] = [item.element_info.name for item in root.descendants()]
            except Exception as tree_error:
                evidence["tree_error"] = str(tree_error)
            raise
        finally:
            evidence["received_controls"] = [identity for identity, _ in events]
            expected = [101, 102] * resolved_runs + [101, 101]
            evidence["unexpected_clicks"] = sum(
                received != expected_click for received, expected_click in zip(evidence["received_controls"], expected)
            ) + max(0, len(events) - len(expected))
            (folder / "result.json").write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
            handle.Close()
