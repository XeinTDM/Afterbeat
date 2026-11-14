"""Check mixed route order, recipients, delays, and cancellation on Win32."""

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
    folder = output / "route-actions"
    folder.mkdir()
    events = []
    cases = []
    fixture = {}
    ready = Event()
    title = "Afterbeat mixed route fixture"
    class_name = "AfterbeatRouteActionsFixture"
    tag = windows.TAG
    keys = {con.WM_KEYDOWN: "keyDown", con.WM_KEYUP: "keyUp"}
    buttons = {
        con.WM_LBUTTONDOWN: "leftDown", con.WM_LBUTTONUP: "leftUp",
        con.WM_RBUTTONDOWN: "rightDown", con.WM_RBUTTONUP: "rightUp",
        con.WM_LBUTTONDBLCLK: "leftDown", con.WM_RBUTTONDBLCLK: "rightDown",
    }
    side_buttons = {0x020B: "sideDown", 0x020C: "sideUp", 0x020D: "sideDown"}
    fail = None

    def pump():
        try:
            children = {}
            names = {}

            def collect(hwnd, msg, wp, lp):
                if windows.USER32.GetMessageExtraInfo() & 0xFFFFFFFF == tag:
                    if msg in keys:
                        key = con.VK_SHIFT if wp in (con.VK_LSHIFT, con.VK_RSHIFT) else wp
                        events.append((keys[msg], names.get(hwnd, "window"), key, time.perf_counter_ns()))
                    elif msg in buttons:
                        events.append((buttons[msg], names.get(hwnd, "window"), 0, time.perf_counter_ns()))
                    elif msg in side_buttons:
                        events.append((side_buttons[msg],
                                       names.get(hwnd, "window"), (wp >> 16) & 0xFFFF, time.perf_counter_ns()))
                    elif msg in (con.WM_MOUSEWHEEL, 0x020E):
                        # Windows may deliver wheel input to the focused child or hovered child.
                        x, y = c.c_short(lp & 0xFFFF).value, c.c_short((lp >> 16) & 0xFFFF).value
                        hit = gui.WindowFromPoint((x, y))
                        events.append(("wheel" if msg == con.WM_MOUSEWHEEL else "hwheel",
                                       names.get(hit, "window"), c.c_short((wp >> 16) & 0xFFFF).value,
                                       time.perf_counter_ns()))

            def handler(hwnd, msg, wp, lp):
                collect(hwnd, msg, wp, lp)
                if msg == con.WM_APP:
                    child = children["finish"]
                    gui.EnableWindow(child, bool(wp))
                    return 0
                if msg == con.WM_APP + 1:
                    gui.SetFocus(children["start"] if wp == 0 else children["finish"])
                    return 0
                return gui.DefWindowProc(hwnd, msg, wp, lp)

            wc = gui.WNDCLASS()
            wc.hInstance = api.GetModuleHandle(None)
            wc.lpszClassName = class_name
            wc.lpfnWndProc = handler
            wc.hbrBackground = con.COLOR_WINDOW + 1
            gui.RegisterClass(wc)
            hwnd = gui.CreateWindowEx(con.WS_EX_TOPMOST, class_name, title, con.WS_OVERLAPPEDWINDOW,
                                      70, 70, 500, 260, 0, 0, wc.hInstance, None)

            def add(name, identity, x, y):
                child = gui.CreateWindowEx(0, "BUTTON", name, con.WS_CHILD | con.WS_VISIBLE | con.BS_PUSHBUTTON,
                                           x, y, 140, 42, hwnd, identity, wc.hInstance, None)
                names[child] = name.lower()

                def subclass(window, message, wparam, lparam):
                    collect(window, message, wparam, lparam)
                    if message in (con.WM_MOUSEWHEEL, 0x020E):
                        return 0
                    return gui.CallWindowProc(previous, window, message, wparam, lparam)

                previous = gui.SetWindowLong(child, con.GWL_WNDPROC, subclass)
                return child

            children.update(start=add("Start", 101, 20, 25), finish=add("Finish", 102, 205, 25))
            fixture.update(window=hwnd, **children)
            gui.ShowWindow(hwnd, con.SW_SHOWNA)
            ready.set()
            gui.PumpMessages()
        except Exception as error:
            fixture["error"] = error
            ready.set()

    Thread(target=pump, daemon=True).start()
    if not ready.wait(5) or "error" in fixture:
        raise RuntimeError("Native mixed-route fixture did not start.") from fixture.get("error")

    def center(control):
        a, b, c_, d = gui.GetWindowRect(fixture[control])
        return ((a+c_)//2, (b+d)//2)

    def target(name):
        import win32process

        image = win32process.GetModuleFileNameEx(api.GetCurrentProcess(), 0)
        return {"selector": {
            "window": {"title": title, "class": class_name, "executable": image},
            "control": {"role": 50000, "identity": {"Name": name.title()}},
            "label": name.title(),
        }}

    def action(kind, ms, destination=None, **data):
        step = {"action": {"kind": kind, **data}, "delay_ms": ms}
        if destination is not None:
            step["target"] = destination
        return step

    def configure(path, route, **extra):
        path.write_text(json.dumps({
            "delay_seconds": 0, "route_once": True, "route_enabled": True, "interval": "0.1",
            "target_timeout": "1.5", "route": route, **extra,
        }), encoding="utf-8")

    def run_case(name, route, check_run, **extra):
        data = folder / name / "local" / "Afterbeat"
        data.mkdir(parents=True)
        path = data / "settings.json"
        configure(path, route, **extra)
        binary = Path(manifest["binary"]["path"])
        env = {**os.environ, "LOCALAPPDATA": str(data.parent), "APPDATA": str(data.parent)}
        start = len(events)
        case = {"case": name, "status": "failed"}
        cases.append(case)
        with windows.ProcessJob() as app:
            handle, pid = app.launch(binary, subprocess.list2cmdline([str(binary)]), env=env)
            try:
                log = data / "diagnostics.log"

                def app_ready():
                    hwnd = windows.window_for(pid, "Afterbeat")
                    return hwnd if hwnd and log.exists() and "Input protection active." in log.read_text(
                        encoding="utf-8",
                    ) else None

                hwnd = windows.wait_for(app_ready, 15, "Afterbeat did not start for the route test.")
                root = Desktop(backend="uia").window(handle=hwnd)

                def find(label, kind="Text", timeout=3):
                    def query():
                        try:
                            return next((item for item in root.descendants(control_type=kind)
                                         if item.element_info.name == label), None)
                        except KeyError:
                            return None
                    return windows.wait_for(query, timeout, f"Could not find {label!r}.")

                def focus(name):
                    if gui.GetForegroundWindow() != fixture["window"]:
                        api.SetCursorPos(gui.ClientToScreen(fixture["window"], (12, 120)))
                        api.mouse_event(con.MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
                        api.mouse_event(con.MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
                        windows.wait_for(lambda: gui.GetForegroundWindow() == fixture["window"], 2,
                                         "The fixture could not take foreground focus.")
                    gui.SendMessage(fixture["window"], con.WM_APP+1, int(name == "finish"), 0)

                check_run(find, focus, start, case)
                case["status"] = "passed"
            except Exception as error:
                case["error"] = str(error)
                try:
                    case["ui"] = [item.element_info.name for item in root.descendants()]
                except Exception:
                    pass
                raise
            finally:
                case["events"] = events[start:]
                handle.Close()
        time.sleep(0.12)

    def order(start, expected):
        actual = [record[:3] for record in events[start:]]
        assert actual == expected, (actual, expected)

    def between(start, first, last, minimum, slack=115):
        elapsed = (events[start+last][3] - events[start+first][3]) / 1_000_000
        assert minimum - 3 <= elapsed <= minimum + slack, (first, last, elapsed, minimum)
        return elapsed

    try:
        stop_binding, modifiers = windows.emergency_binding()
        x, y = center("finish")
        mixed = [
            action("click", 0, target("start"), button="Left", count=1),
            action("wait", 135),
            action("scroll", 55, target("start"), direction="Down", ticks=2),
            action("scroll", 90, target("start"), direction="Right", ticks=1),
            action("key", 80, target("start"), key="A"),
            action("click", 100, {"x": x, "y": y}, button="Right", count=2),
        ]

        def test_mixed(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed.")
            order(start, [
                ("leftDown", "start", 0), ("leftUp", "start", 0), ("wheel", "start", -240),
                ("hwheel", "start", 120), ("keyDown", "start", con.VK_SHIFT),
                ("keyDown", "start", ord("A")), ("keyUp", "start", ord("A")),
                ("keyUp", "start", con.VK_SHIFT),
                ("rightDown", "finish", 0), ("rightUp", "finish", 0),
                ("rightDown", "finish", 0), ("rightUp", "finish", 0),
            ])
            case["gaps_ms"] = [
                between(start, 1, 2, 190, 65), between(start, 2, 3, 90, 65),
                between(start, 3, 4, 80, 65), between(start, 7, 8, 100, 65),
                between(start, 8, 11, 0, 45),
            ]
        run_case("mixed-order", mixed, test_mixed)

        def test_gate(find, focus, start, case):
            gui.EnableWindow(fixture["finish"], False)
            focus("start")
            windows.tap_key(con.VK_F6)
            time.sleep(0.23)
            order(start, [("leftDown", "start", 0), ("leftUp", "start", 0)])
            gui.SendMessage(fixture["window"], con.WM_APP, 1, 0)
            find("Completed.")
            order(start, [
                ("leftDown", "start", 0), ("leftUp", "start", 0),
                ("leftDown", "finish", 0), ("leftUp", "finish", 0),
                ("keyDown", "finish", con.VK_SPACE), ("keyUp", "finish", con.VK_SPACE),
            ])
            assert (events[start+2][3] - events[start+1][3]) / 1_000_000 >= 205
            case["post_gate_gap_ms"] = between(start, 3, 4, 145, 65)
        run_case("late-target-does-not-compress-next-delay", [
            mixed[0], action("click", 25, target("finish"), button="Left", count=1),
            action("key", 145, target("finish"), key="Space"),
        ], test_gate)

        def test_wrong_focus(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            time.sleep(0.21)
            order(start, [])
            focus("finish")
            find("Completed.")
            order(start, [("keyDown", "finish", con.VK_RETURN), ("keyUp", "finish", con.VK_RETURN)])
        run_case("wrong-focus-withholds-key", [action("key", 80, target("finish"), key="Enter")], test_wrong_focus)

        def test_stop(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            windows.wait_for(lambda: len(events) >= start+2, 2, "The initial click did not arrive.")
            windows.tap_key(con.VK_F7, modifiers=modifiers)
            find("Stopped.")
            time.sleep(0.55)
            order(start, [("leftDown", "start", 0), ("leftUp", "start", 0)])
        run_case("stop-during-wait", [mixed[0], action("wait", 480), mixed[2]], test_stop)

        def test_limit(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed.")
            order(start, [])
        run_case("time-limit-in-first-wait", [action("wait", 500), mixed[0]], test_limit, time_limit="0.15")

        def test_loop_limit(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed.")
            order(start, [
                ("keyDown", "start", con.VK_RETURN), ("keyUp", "start", con.VK_RETURN),
                ("wheel", "start", 120),
                ("keyDown", "start", con.VK_RETURN), ("keyUp", "start", con.VK_RETURN),
            ])
            case["loop_wait_ms"] = between(start, 2, 3, 105)
        run_case("loop-count-skips-waits-and-stops-mid-cycle", [
            action("wait", 105),
            action("key", 0, target("start"), key="Enter"),
            action("scroll", 65, target("start"), direction="Up", ticks=1),
        ], test_loop_limit, route_once=False, limited=True, count="3")

        def test_editor(find, focus, start, case):
            find("Targets", "Button").iface_toggle.Toggle()
            find("Pick control", "Button").iface_invoke.Invoke()
            api.SetCursorPos(center("start"))
            windows.tap_key(con.VK_F8)
            find("1. Start", "ListItem")
            find("Edit step", "Button").iface_invoke.Invoke()
            find("Step delay", "Edit").iface_value.SetValue("125")
            find("Save step", "Button").iface_invoke.Invoke()
            find("1. Left click ×1 · Start · 125 ms delay", "ListItem")
            find("Add step", "Button").iface_invoke.Invoke()
            find("Step delay", "Edit").iface_value.SetValue("245")
            find("Save step", "Button").iface_invoke.Invoke()
            find("2. Wait 245 ms", "ListItem")
            focus("start")
            started = windows.tap_key(con.VK_F6)
            find("Completed.")
            order(start, [("leftDown", "start", 0), ("leftUp", "start", 0)])
            first_gap = (events[start][3] - started) / 1_000_000
            assert 122 <= first_gap <= 300, first_gap
            case["first_delay_ms"] = first_gap
            completed = (time.perf_counter_ns() - events[start+1][3]) / 1_000_000
            assert 242 <= completed <= 370, completed
            case["trailing_wait_to_status_ms"] = completed
        run_case("f8-and-editor-build-a-playable-route", [], test_editor, route_enabled=False)

        def clicker_output(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed.")
            order(start, case["expected"])

        for button, code in (("Mouse4", 1), ("Mouse5", 2)):
            def test_side(find, focus, start, case, code=code):
                case["expected"] = [("sideDown", "start", code), ("sideUp", "start", code)] * 6
                clicker_output(find, focus, start, case)
            x, y = center("start")
            run_case(f"clicker-{button.lower()}-triple", [], test_side, route_enabled=False,
                     fixed=True, point={"x": x, "y": y}, button=button, triple=True,
                     limited=True, count="2", interval="100")

        def test_wheel(find, focus, start, case):
            def select(label):
                button = find(label, "RadioButton")
                try:
                    button.iface_selection_item.Select()
                    return
                except Exception:
                    pass
                point = button.rectangle().mid_point()
                gui.ShowWindow(fixture["window"], con.SW_HIDE)
                api.SetCursorPos((point.x, point.y))
                api.mouse_event(con.MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
                api.mouse_event(con.MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
                time.sleep(0.08)
                gui.ShowWindow(fixture["window"], con.SW_SHOWNA)
            select("Wheel")
            find("Scroll amount", "Edit").iface_value.SetValue("3")
            select("Left")
            case["expected"] = [("hwheel", "start", -360)] * 2
            clicker_output(find, focus, start, case)
        run_case("clicker-wheel-from-ui", [], test_wheel, route_enabled=False,
                 fixed=True, point={"x": x, "y": y}, limited=True, count="2", interval="100")

        def test_fixed_wheel_guard(find, focus, start, case):
            windows.tap_key(con.VK_F6)
            find("Focus the window at the wheel target to resume.")
            order(start, [])
            focus("start")
            find("Completed.")
            order(start, [("wheel", "start", -240)])
        run_case("clicker-wheel-waits-for-target-focus", [], test_fixed_wheel_guard,
                 route_enabled=False, fixed=True, point={"x": x, "y": y}, scroll=True,
                 scroll_ticks="2", limited=True, count="1")

        def test_control_wheel(find, focus, start, case):
            case["expected"] = [("wheel", "start", 120)]
            clicker_output(find, focus, start, case)
        run_case("clicker-wheel-through-smart-target", [target("start")], test_control_wheel,
                 scroll=True, scroll_direction="Up", interval="100")

        def test_control_triple(find, focus, start, case):
            case["expected"] = [("sideDown", "start", 2), ("sideUp", "start", 2)] * 3
            clicker_output(find, focus, start, case)
        run_case("clicker-triple-through-smart-target", [target("start")], test_control_triple,
                 button="Mouse5", triple=True, interval="100")

        def test_activation_conflict(find, focus, start, case):
            find("Use a different mouse button for output and activation.")
            order(start, [])
        run_case("clicker-side-button-cannot-trigger-itself", [], test_activation_conflict,
                 route_enabled=False, button="Mouse4", trigger="Mouse4")
    except Exception as error:
        fail = str(error)
        raise
    finally:
        (folder / "result.json").write_text(json.dumps({
            "status": "failed" if fail is not None else "passed", "error": fail, "cases": cases,
        }, indent=2) + "\n", encoding="utf-8")
