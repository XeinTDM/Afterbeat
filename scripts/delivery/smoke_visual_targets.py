"""Exercise visual target recovery, abstention, and durable repair on Windows."""

import ctypes as c
import json
import os
from pathlib import Path
import subprocess
from threading import Event, Thread
import time


def check(output, manifest, windows):
    from pywinauto import Desktop

    api, gui, con = windows.api, windows.gui, windows.con
    folder = output / "visual-targets"
    data = folder / "local" / "Afterbeat"
    data.mkdir(parents=True)
    config = data / "settings.json"
    config.write_text(json.dumps({
        "delay_seconds": 0, "route_once": True, "target_timeout": "1",
        "capture_visual": True,
    }), encoding="utf-8")
    evidence = {"status": "failed", "cases": [], "received": []}
    ready = Event()
    fixture = {}
    title = "Afterbeat Visual Targets fixture"
    rename = title + " · updated"
    user = windows.USER32
    user.GetDpiForWindow.argtypes = (c.c_void_p,)
    user.GetDpiForWindow.restype = c.c_uint
    test_message = con.WM_APP + 20
    monitors = [rect for _, _, rect in api.EnumDisplayMonitors()]
    evidence["monitor_bounds"] = monitors

    def pump():
        controls = {}

        def layout(hwnd, dpi):
            def scale(n):
                return round(n * dpi / 96)

            # Keep the same rasterized text proportions when the window crosses
            # monitors. The native font and controls both receive the new DPI.
            import win32ui

            font = win32ui.CreateFont({
                "name": "Segoe UI", "height": -scale(22), "weight": 600,
            })
            for key, x, y, w, h in (
                ("anchor", 20, 18, 176, 32), ("target", 33, 56, 120, 40),
                ("other_anchor", 230, 18, 176, 32), ("decoy", 243, 56, 120, 40),
            ):
                child = controls[key]
                gui.MoveWindow(child, scale(x), scale(y), scale(w), scale(h), True)
                gui.SendMessage(child, con.WM_SETFONT, font.GetSafeHandle(), 1)
            fixture["font"] = font
            gui.RedrawWindow(hwnd, None, None, con.RDW_INVALIDATE | con.RDW_UPDATENOW | con.RDW_ALLCHILDREN)

        def handle(hwnd, message, wparam, lparam):
            if message == test_message:
                if wparam == 1:
                    gui.SetWindowLong(controls["target"], con.GWL_ID, lparam)
                elif wparam == 2:
                    gui.SetWindowLong(controls["decoy"], con.GWL_ID, lparam)
                elif wparam == 3:
                    gui.ShowWindow(controls["target"], con.SW_SHOWNA if lparam else con.SW_HIDE)
                elif wparam == 4:
                    gui.SetWindowText(controls["anchor"], "Invoices ledger" if lparam else "Archived invoices")
                elif wparam == 5:
                    gui.SetWindowText(controls["other_anchor"], "Invoices ledger" if lparam else "Reports overview")
                elif wparam == 6:
                    gui.SetWindowPos(hwnd, con.HWND_TOPMOST, 0, 0, 0, 0,
                                     con.SWP_NOMOVE | con.SWP_NOSIZE | con.SWP_NOACTIVATE)
                elif wparam == 7:
                    width, height = round(540 * lparam / 96), round(220 * lparam / 96)
                    gui.SetWindowPos(hwnd, con.HWND_TOPMOST, 0, 0, width, height,
                                     con.SWP_NOMOVE | con.SWP_NOACTIVATE)
                    layout(hwnd, lparam)
                elif wparam == 9:
                    dpi = user.GetDpiForWindow(hwnd)
                    x = 45 if lparam else 243
                    gui.MoveWindow(controls["decoy"], *(round(n * dpi / 96) for n in (x, 56, 120, 40)), True)
                elif wparam == 11:
                    gui.EnableWindow(controls["target"], bool(lparam))
                return 0
            if message == 0x02E0 and controls:
                rect = c.cast(lparam, c.POINTER(windows.w.RECT)).contents
                gui.SetWindowPos(
                    hwnd, con.HWND_TOPMOST, rect.left, rect.top, rect.right - rect.left, rect.bottom - rect.top,
                    con.SWP_NOACTIVATE,
                )
                layout(hwnd, wparam & 0xFFFF)
                return 0
            if message == con.WM_COMMAND and lparam in (controls.get("target"), controls.get("decoy")):
                if (wparam >> 16) == con.BN_CLICKED:
                    identity = "target" if lparam == controls["target"] else "decoy"
                    evidence["received"].append({"control": identity, "at_ns": time.perf_counter_ns()})
                return 0
            return gui.DefWindowProc(hwnd, message, wparam, lparam)

        try:
            cls = gui.WNDCLASS()
            cls.hInstance = api.GetModuleHandle(None)
            cls.lpszClassName = "AfterbeatVisualTargetsFixture"
            cls.lpfnWndProc = handle
            cls.hbrBackground = con.COLOR_WINDOW + 1
            atom = gui.RegisterClass(cls)
            left, top, right, _ = monitors[0]
            hwnd = gui.CreateWindowEx(
                con.WS_EX_TOPMOST, atom, title, con.WS_OVERLAPPEDWINDOW,
                max(left + 20, right - 570), top + 60, 540, 220, 0, 0, cls.hInstance, None,
            )

            def child(control, caption, identity):
                return gui.CreateWindowEx(
                    0, control, caption, con.WS_CHILD | con.WS_VISIBLE,
                    0, 0, 0, 0, hwnd, identity, cls.hInstance, None,
                )

            controls.update(
                anchor=child("STATIC", "Invoices ledger", 201),
                target=child("BUTTON", "Export", 202),
                other_anchor=child("STATIC", "Reports overview", 203),
                decoy=child("BUTTON", "Export", 204),
            )
            fixture.update(hwnd=hwnd, **controls)
            layout(hwnd, user.GetDpiForWindow(hwnd))
            gui.ShowWindow(hwnd, con.SW_SHOWNA)
        except Exception as error:
            fixture["error"] = error
        finally:
            ready.set()
        gui.PumpMessages()

    Thread(target=pump, name="visual-target-fixture", daemon=True).start()
    if not ready.wait(5):
        raise RuntimeError("The visual fixture did not start.")
    if "error" in fixture:
        raise fixture["error"]

    binary = Path(manifest["binary"]["path"])
    env = {**os.environ, "LOCALAPPDATA": str(data.parent), "APPDATA": str(folder / "roaming")}
    receive = evidence["received"]
    target_window = fixture["hwnd"]

    def modify(op, value):
        gui.SendMessage(target_window, test_message, op, value)

    def case(name, function, expected, *, may_abstain=False):
        start = len(receive)
        begin = time.perf_counter_ns()
        row = {"case": name, "status": "failed", "expected": expected}
        evidence["cases"].append(row)
        try:
            outcome = function()
        finally:
            row["received"] = [entry["control"] for entry in receive[start:]]
            row["elapsed_ms"] = (time.perf_counter_ns() - begin) / 1_000_000
        if may_abstain and not row["received"] and outcome == "timeout":
            row["status"] = "abstained"
            return
        if row["received"] != expected:
            raise RuntimeError(f"{name}: expected {expected!r}, received {row['received']!r}.")
        row["status"] = "passed"

    root = None
    with windows.ProcessJob() as app:
        handle, pid = app.launch(binary, subprocess.list2cmdline([str(binary)]), env=env)
        try:
            log = data / "diagnostics.log"

            def app_ready():
                hwnd = windows.window_for(pid, "Afterbeat")
                return hwnd if hwnd and log.exists() and "Input protection active." in log.read_text(
                    encoding="utf-8",
                ) else None

            hwnd = windows.wait_for(app_ready, 15, "Afterbeat did not start for visual tests.")
            root = Desktop(backend="uia").window(handle=hwnd)

            def find(name, kind="Text", *, timeout=4):
                def match():
                    try:
                        return next((item for item in root.descendants(control_type=kind)
                                     if (name(item.element_info.name) if callable(name) else item.element_info.name == name)), None)
                    except KeyError:
                        return None
                return windows.wait_for(match, timeout, f"Could not find {kind} {name!r}.")

            def invoke(label):
                find(label, "Button").iface_invoke.Invoke()

            def run(*, allow_timeout=False):
                modify(6, 0)
                _, _, right, bottom = gui.GetWindowRect(fixture["target"])
                if gui.IsWindowVisible(fixture["target"]):
                    api.SetCursorPos((right + 10, bottom + 10))
                windows.tap_key(con.VK_F6)
                result = find(lambda name: name == "Completed." or name.startswith("Step 1 timed out"))
                if result.element_info.name != "Completed.":
                    nodes = Desktop(backend="uia").window(handle=target_window).descendants()
                    evidence["fixture_at_failure"] = [
                        {"name": item.element_info.name, "rect": str(item.rectangle())}
                        for item in nodes
                    ]
                    if allow_timeout:
                        return "timeout"
                    raise RuntimeError(result.element_info.name)
                return "completed"

            def abstain():
                modify(6, 0)
                windows.tap_key(con.VK_F6)
                result = find(lambda name: name == "Completed." or name.startswith("Step 1 timed out"), timeout=3)
                if result.element_info.name == "Completed.":
                    raise RuntimeError("Afterbeat completed a case where it should have withheld input.")

            def proposal(kind):
                invoke("Dry run")
                invoke("Step")
                find(lambda name: name.startswith("Step 1 of 1 ·") and kind in name)

            def saved_identity():
                try:
                    return json.loads(config.read_text(encoding="utf-8")).get("route", [])
                except (PermissionError, FileNotFoundError):
                    # Afterbeat replaces the file atomically on Windows.
                    return []

            find("Targets", "Button").iface_toggle.Toggle()
            invoke("Pick control")
            left, top, right, bottom = gui.GetWindowRect(fixture["target"])
            api.SetCursorPos(((left + right) // 2, (top + bottom) // 2))
            windows.tap_key(con.VK_F8)
            find("1. Export", "ListItem")
            saved = windows.wait_for(saved_identity, 4, "The visual target was not saved.")
            assert saved[0]["selector"]["visual"]["anchor"]["identity"] == {"Name": "Invoices ledger"}, saved
            modify(1, 302)
            gui.SetWindowText(target_window, rename)
            case("renamed window and regenerated ID with identical decoy", run, ["target"])
            initial_dpi = user.GetDpiForWindow(target_window)
            left, top, _, _ = monitors[0]
            if initial_dpi <= 192 and monitors[0][2] - left >= round(810 * initial_dpi / 96) + 60:
                gui.SetWindowPos(target_window, con.HWND_TOPMOST, left + 50, top + 50, 0, 0,
                                 con.SWP_NOSIZE | con.SWP_NOACTIVATE)
                modify(7, round(initial_dpi * 1.5))
                case("native control and font redraw at 150 percent scale",
                     lambda: run(allow_timeout=True), ["target"], may_abstain=True)
                modify(7, initial_dpi)
            else:
                evidence["cases"].append({
                    "case": "native control and font redraw at 150 percent scale", "status": "skipped",
                    "reason": "The scaled fixture exceeds the primary monitor's test area.",
                })

            move_cases = []
            for left, top, _, _ in monitors[1:]:
                gui.SetWindowPos(
                    target_window, con.HWND_TOPMOST, left + 50, top + 60,
                    0, 0, con.SWP_NOSIZE | con.SWP_NOACTIVATE,
                )
                # WM_DPICHANGED runs in the fixture's window thread.
                gui.SendMessage(target_window, con.WM_NULL, 0, 0)
                current_dpi = user.GetDpiForWindow(target_window)
                move_cases.append(current_dpi)
                if current_dpi != initial_dpi:
                    case(f"mixed DPI monitor {initial_dpi} to {current_dpi}", run, ["target"])
                    break
            else:
                evidence["cases"].append({
                    "case": "mixed DPI monitors", "status": "skipped",
                    "reason": "No second monitor with a different effective DPI was available.",
                    "observed_dpi": [initial_dpi, *move_cases],
                })
            left, top, right, _ = monitors[0]
            gui.SetWindowPos(target_window, con.HWND_TOPMOST, max(left + 20, right - 570), top + 60,
                             0, 0, con.SWP_NOSIZE | con.SWP_NOACTIVATE)

            modify(3, 0)
            modify(9, 1)
            case("target removed and pixel-identical decoy moved within 12 logical pixels", abstain, [])
            modify(9, 0)
            modify(3, 1)
            modify(5, 1)
            case("duplicate anchor and identical controls", abstain, [])
            modify(5, 0)
            modify(4, 0)
            case("original anchor missing", abstain, [])
            modify(4, 1)

            gui.SetWindowText(target_window, title)
            modify(2, 202)
            modify(11, 0)
            case("disabled intended control with clickable old-ID decoy", abstain, [])
            modify(11, 1)
            case("old ID reused by convincing decoy", abstain, [])
            case("Dry Run proposes the anchored control without delivering input",
                 lambda: proposal("visual evidence disagree"), [])

            def stale_repair():
                left, top, _, _ = gui.GetWindowRect(target_window)
                gui.SetWindowPos(target_window, con.HWND_TOPMOST, left - 24, top + 12, 0, 0,
                                 con.SWP_NOSIZE | con.SWP_NOACTIVATE)
                invoke("Repair target")
                find("The proposed target changed. Check the route again before repairing.")
                stored = windows.wait_for(saved_identity, 4, "The route settings could not be read.")
                assert stored[0]["selector"]["control"]["identity"] == saved[0]["selector"]["control"]["identity"]
                invoke("Check again")
                invoke("Step")
                find(lambda name: name.startswith("Step 1 of 1 ·") and "visual evidence disagree" in name)

            case("repair rejects a stale preview", stale_repair, [])

            def repair():
                invoke("Repair target")
                find("Step 1 of 1 · Export · Control located and ready.")
                def repaired():
                    route = saved_identity()
                    return route and route[0]["selector"]["control"]["identity"] != saved[0]["selector"]["control"]["identity"]
                windows.wait_for(repaired, 4, "One-click repair did not persist the replacement.")
                invoke("Back to Targets")

            case("one-click repair persists a revalidated selector without input", repair, [])
            case("repaired route replays while the old-ID decoy stays visible", run, ["target"])
            evidence["status"] = "passed"
        except Exception as error:
            evidence["error"] = str(error)
            if root is not None:
                try:
                    evidence["tree"] = [item.element_info.name for item in root.descendants()]
                except Exception as tree_error:
                    evidence["tree_error"] = str(tree_error)
            raise
        finally:
            evidence["route_completion"] = {
                "passed": sum(item["status"] == "passed" and bool(item.get("expected")) for item in evidence["cases"]),
                "attempted": sum(bool(item.get("expected")) for item in evidence["cases"]),
            }
            evidence["wrong_target_clicks"] = sum(entry["control"] == "decoy" for entry in receive)
            (folder / "result.json").write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
            handle.Close()
