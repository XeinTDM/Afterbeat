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
                                         if (label(item.element_info.name) if callable(label)
                                             else item.element_info.name == label)), None)
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

        def test_finite_loops(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed.")
            order(start, [
                ("leftDown", "start", 0), ("leftUp", "start", 0),
                ("leftDown", "start", 0), ("leftUp", "start", 0),
            ])
            last_click = events[start + 3][3]
            assert (time.perf_counter_ns() - last_click) / 1_000_000 >= 110
        run_case("finite-loops-include-final-wait", [mixed[0], action("wait", 150)],
                 test_finite_loops, route_iterations="2", route_once=False)

        # Sample a stable part of the fixture background. The window is topmost;
        # the pixel condition must pass before the first click is dispatched.
        pixel_x, pixel_y = gui.ClientToScreen(fixture["window"], (12, 120))
        gdi = c.WinDLL("gdi32", use_last_error=True)
        gdi.GetPixel.argtypes = [c.c_void_p, c.c_int, c.c_int]
        gdi.GetPixel.restype = c.c_uint32
        dc = gui.GetDC(0)
        try:
            colorref = gdi.GetPixel(dc, pixel_x, pixel_y)
        finally:
            gui.ReleaseDC(0, dc)
        if colorref == 0xFFFFFFFF:
            raise RuntimeError("Could not sample the fixture pixel.")
        rgb = ((colorref & 0xFF) << 16) | (colorref & 0xFF00) | ((colorref >> 16) & 0xFF)

        def test_pick_pixel(find, focus, start, case):
            find("Targets", "Button").iface_toggle.Toggle()
            find("Add step", "Button").iface_invoke.Invoke()
            pixel = find("Pixel", "RadioButton")
            try:
                pixel.iface_selection_item.Select()
            except Exception:
                point = pixel.rectangle().mid_point()
                gui.ShowWindow(fixture["window"], con.SW_HIDE)
                api.SetCursorPos((point.x, point.y))
                api.mouse_event(con.MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
                api.mouse_event(con.MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
                gui.ShowWindow(fixture["window"], con.SW_SHOWNA)
            field = find("Pixel x, y, and RGB color", "Edit")
            field.iface_value.SetValue("1, 2, #123456")
            find("Pick pixel in 3s", "Button").iface_invoke.Invoke()
            find("Cancel capture", "Button")
            windows.tap_key(con.VK_F7, modifiers=modifiers)
            find("Pick pixel in 3s", "Button")
            assert field.iface_value.CurrentValue == "1, 2, #123456"
            find("Pick pixel in 3s", "Button").iface_invoke.Invoke()
            api.SetCursorPos((pixel_x, pixel_y))
            windows.tap_key(con.VK_F8)
            expected = f"{pixel_x}, {pixel_y}, #{rgb:06X}"
            windows.wait_for(lambda: field.iface_value.CurrentValue == expected, 3,
                             "Pixel capture did not populate the editor.")
            find("Save step", "Button").iface_invoke.Invoke()
            order(start, [])

        run_case("pick-pixel-and-cancel", [mixed[0]], test_pick_pixel)

        def test_pixel_match(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed.")
            order(start, [("leftDown", "start", 0), ("leftUp", "start", 0)])

        run_case("pixel-match-before-click", [
            action("wait_color", 0, point={"x": pixel_x, "y": pixel_y}, rgb=rgb), mixed[0],
        ], test_pixel_match)

        # The fixture's background is a stable, uniform 8 × 8 reference.
        gray = (29 * (rgb & 255) + 150 * ((rgb >> 8) & 255) + 77 * ((rgb >> 16) & 255)) >> 8
        image_step = action("wait_image", 0,
                            search={"x": pixel_x, "y": pixel_y, "width": 8, "height": 8},
                            template={"width": 8, "height": 8, "pixels": f"{gray:02x}" * 64},
                            threshold=100, timeout_ms=600)
        run_case("image-match-before-click", [image_step, mixed[0]], test_pixel_match)

        def test_image_timeout(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find(lambda name: "timed out waiting for" in name, timeout=3)
            order(start, [])

        different_image = action("wait_image", 0,
                                 search=image_step["action"]["search"],
                                 template={"width": 8, "height": 8,
                                           "pixels": ("00" if gray else "ff") * 64},
                                 threshold=100, timeout_ms=300)
        run_case("image-timeout-sends-no-click", [different_image, mixed[0]], test_image_timeout)

        def flow(kind, **fields):
            return action("flow", 0, operation={"kind": kind, **fields})

        pixel_test = {"point": {"x": pixel_x, "y": pixel_y}, "rgb": rgb,
                      "tolerance": 0, "different": False}
        start_click = [("leftDown", "start", 0), ("leftUp", "start", 0)]

        def test_nested_workflow(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed.")
            order(start, start_click * 2)

        finish_click = action("click", 0, target("finish"), button="Left", count=1)
        run_case("nested-repeat-branches-and-exit", [
            flow("repeat", count=2),
            flow("if", test=pixel_test),
            flow("repeat", count=3),
            mixed[0],
            flow("break_if", test=pixel_test),
            finish_click,
            flow("end_repeat"),
            flow("else"),
            finish_click,
            flow("end_if"),
            flow("end_repeat"),
        ], test_nested_workflow)

        def test_image_recovery(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed. 1 step skipped. Last skipped: step 2.")
            order(start, start_click)

        run_case("image-timeout-retries-then-skips", [
            flow("recover", retries=1, skip=True), different_image, mixed[0],
        ], test_image_recovery)

        image_if = action("wait_image", 0, **{
            **{key: value for key, value in different_image["action"].items() if key != "kind"},
            "output": "if",
        })
        run_case("image-absence-takes-else", [
            image_if, finish_click, flow("else"), mixed[0], flow("end_if"),
        ], test_pixel_match)

        _, _, button_right, button_bottom = gui.GetWindowRect(fixture["start"])
        image_x, image_y = button_right - 15, button_bottom - 15
        dc = gui.GetDC(0)
        try:
            sample = [gdi.GetPixel(dc, image_x + x, image_y + y)
                      for y in range(8) for x in range(8)]
        finally:
            gui.ReleaseDC(0, dc)
        assert 0xFFFFFFFF not in sample
        color_reference = "".join(f"{p & 255:02x}{(p >> 8) & 255:02x}{(p >> 16) & 255:02x}" for p in sample)
        click_image = action("wait_image", 0,
                             search={"x": image_x, "y": image_y, "width": 8, "height": 8},
                             template={"width": 8, "height": 8, "pixels": color_reference},
                             threshold=100, timeout_ms=1000, output="click")
        run_case("unique-image-click-delivers-to-match", [click_image], test_pixel_match)
        run_case("pixel-tolerance-before-click", [
            action("wait_color", 0, point={"x": pixel_x, "y": pixel_y},
                   rgb=rgb ^ 0x010101, tolerance=1), mixed[0],
        ], test_pixel_match)
        run_case("pixel-different-before-click", [
            action("wait_color", 0, point={"x": pixel_x, "y": pixel_y},
                   rgb=rgb ^ 0xFFFFFF, tolerance=0, different=True), mixed[0],
        ], test_pixel_match)

        def test_pixel_stability(find, focus, start, case):
            focus("start")
            started = time.perf_counter_ns()
            windows.tap_key(con.VK_F6)
            find("Completed.")
            order(start, [("leftDown", "start", 0), ("leftUp", "start", 0)])
            assert (events[start][3] - started) / 1_000_000 >= 190

        run_case("pixel-stability-before-click", [
            action("wait_color", 0, point={"x": pixel_x, "y": pixel_y},
                   rgb=rgb, stable_ms=200), mixed[0],
        ], test_pixel_stability)

        def test_pixel_timeout(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find(lambda name: "timed out waiting for" in name, timeout=3)
            order(start, [])

        run_case("pixel-timeout-sends-no-click", [
            action("wait_color", 0, point={"x": pixel_x, "y": pixel_y}, rgb=rgb ^ 0xFFFFFF), mixed[0],
        ], test_pixel_timeout, target_timeout="0.3")

        def test_pixel_timeout_branch(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed. 1 step skipped. Last skipped: step 2.")
            order(start, [("leftDown", "finish", 0), ("leftUp", "finish", 0)])

        branch = [
            action("wait_color", 0, point={"x": pixel_x, "y": pixel_y},
                   rgb=rgb ^ 0xFFFFFF, skip_next_on_timeout=True),
            mixed[0], action("click", 0, target("finish"), button="Left", count=1),
        ]
        run_case("pixel-timeout-skips-only-next-step", branch, test_pixel_timeout_branch, target_timeout="0.3")

        def test_pixel_match_branch(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed.")
            order(start, [
                ("leftDown", "start", 0), ("leftUp", "start", 0),
                ("leftDown", "finish", 0), ("leftUp", "finish", 0),
            ])

        run_case("pixel-match-keeps-next-step", [
            action("wait_color", 0, point={"x": pixel_x, "y": pixel_y},
                   rgb=rgb, skip_next_on_timeout=True),
            *branch[1:],
        ], test_pixel_match_branch)

        def test_pixel_check_mismatch(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed. 1 step skipped. Last skipped: step 2.", timeout=3)
            order(start, [("leftDown", "finish", 0), ("leftUp", "finish", 0)])

        run_case("pixel-check-mismatch-skips-immediately", [
            action("wait_color", 0, point={"x": pixel_x, "y": pixel_y},
                   rgb=rgb ^ 0xFFFFFF, check_once=True, skip_next_on_mismatch=True),
            *branch[1:],
        ], test_pixel_check_mismatch, target_timeout="10")

        run_case("pixel-check-match-keeps-next-step", [
            action("wait_color", 0, point={"x": pixel_x, "y": pixel_y},
                   rgb=rgb, check_once=True, skip_next_on_mismatch=True),
            *branch[1:],
        ], test_pixel_match_branch)

        def test_pixel_retries_and_skip(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed. 1 step skipped. Last skipped: step 3. 2 click steps were retried.")
            order(start, [("leftDown", "start", 0), ("leftUp", "start", 0)] * 4)

        retry_condition = action("wait_color", 0, point={"x": pixel_x, "y": pixel_y},
                                 rgb=rgb ^ 0xFFFFFF, check_once=True,
                                 skip_next_on_mismatch=True, retry_clicks=2)
        run_case("pixel-check-retries-before-skip", [
            mixed[0], retry_condition, action("click", 0, target("finish"), button="Left", count=1), mixed[0],
        ], test_pixel_retries_and_skip)

        def test_pixel_retry_action_limit(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            find("Completed. 1 click step was retried.")
            order(start, [("leftDown", "start", 0), ("leftUp", "start", 0)] * 2)

        run_case("pixel-retry-respects-action-limit", [
            mixed[0], retry_condition, mixed[0],
        ], test_pixel_retry_action_limit, limited=True, count="2")

        def test_stop_before_pixel_retry(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            windows.wait_for(lambda: len(events) >= start + 2, 2, "The first click was not received.")
            windows.tap_key(con.VK_F7, modifiers=modifiers)
            find("Stopped.")
            order(start, [("leftDown", "start", 0), ("leftUp", "start", 0)])

        run_case("stop-during-pixel-wait-prevents-retry", [
            mixed[0], action("wait_color", 0, point={"x": pixel_x, "y": pixel_y},
                             rgb=rgb ^ 0xFFFFFF, retry_clicks=2),
        ], test_stop_before_pixel_retry)

        def test_pixel_branch_stops(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            time.sleep(0.12)
            windows.tap_key(con.VK_F7, modifiers=modifiers)
            find("Stopped.")
            order(start, [])

        run_case("stop-during-pixel-condition-does-not-branch", branch, test_pixel_branch_stops)

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
            find(lambda name: name.startswith("Step 1 of 1 · Press Enter · Finish") and "s left." in name,
                 timeout=1.2)
            order(start, [])
            focus("finish")
            find("Completed.")
            order(start, [("keyDown", "finish", con.VK_RETURN), ("keyUp", "finish", con.VK_RETURN)])
        run_case("wrong-focus-withholds-key", [action("key", 80, target("finish"), key="Enter")], test_wrong_focus)

        def test_key_preview(find, focus, start, case):
            find("Targets", "Button").iface_toggle.Toggle()
            find("Dry run", "Button").iface_invoke.Invoke()
            step = find("Step", "Button")
            windows.wait_for(step.is_enabled, 5, "The dry-run control check did not finish.")
            step.iface_invoke.Invoke()
            find("Step 1 of 1 · Press Enter · Finish · Focus this control before the key step runs.")
            order(start, [])

        run_case("dry-run-explains-key-focus", [action("key", 0, target("finish"), key="Enter")],
                 test_key_preview)

        def test_stop(find, focus, start, case):
            focus("start")
            windows.tap_key(con.VK_F6)
            windows.wait_for(lambda: len(events) >= start+2, 2, "The initial click did not arrive.")
            windows.tap_key(con.VK_F7, modifiers=modifiers)
            find("Stopped.")
            time.sleep(0.55)
            order(start, [("leftDown", "start", 0), ("leftUp", "start", 0)])
        run_case("stop-during-wait", [mixed[0], action("wait", 480), mixed[2]], test_stop)

        def test_countdown_stop_zone(find, focus, start, case):
            focus("start")
            api.SetCursorPos(center("start"))
            windows.tap_key(con.VK_F6)
            find("Countdown")
            api.SetCursorPos(center("finish"))
            find("Stopped in the stop zone.")
            api.SetCursorPos(center("start"))
            order(start, [])

        finish = {"x": x, "y": y}
        stop_zone = {"from": finish, "to": finish}
        run_case("stop-zone-during-mixed-countdown", [mixed[0], action("wait", 50)],
                 test_countdown_stop_zone, delay_seconds=3, stop_zone=stop_zone)
        run_case("stop-zone-during-smart-wheel-countdown", [target("start")],
                 test_countdown_stop_zone, delay_seconds=3, stop_zone=stop_zone, scroll=True)

        def test_inherited_key_blocked(find, focus, start, case):
            find("Select Mouse or Wheel for unedited route steps, or set an action on each step.")
            focus("finish")
            windows.tap_key(con.VK_F6)
            time.sleep(0.2)
            order(start, [])

        run_case("mixed-route-cannot-inherit-key-output", [finish, action("wait", 50)],
                 test_inherited_key_blocked, keyboard=True)

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

        def test_blocked_start_recovery(find, focus, start, case):
            find("Clear the inside-only zone before running a route.")
            find("Fix in Targets", "Button").iface_invoke.Invoke()
            clear = find("Remove Inside only zone", "Button")
            windows.wait_for(clear.has_keyboard_focus, 2, "Recovery did not focus Clear.")
            windows.tap_key(con.VK_SPACE)
            run = find("Start run", "Button")
            windows.wait_for(run.is_enabled, 2, "Start remained disabled after clearing the zone.")
            order(start, [])

        run_case("blocked-start-opens-and-focuses-fix", [{"x": x, "y": y}],
                 test_blocked_start_recovery,
                 inside_zone={"from": {"x": 0, "y": 0}, "to": {"x": 200, "y": 200}})

        def test_off_route(find, focus, start, case):
            find("Targets", "Button").iface_toggle.Toggle()
            find("Add in 3s", "Button").iface_invoke.Invoke()
            api.SetCursorPos(center("start"))
            windows.tap_key(con.VK_F8)
            px, py = center("start")
            find(f"2. X {px}  Y {py}", "ListItem")
            find("Route off")
            order(start, [])

        run_case("adding-a-point-preserves-route-off", [{"x": x, "y": y}], test_off_route,
                 route_enabled=False)

        def test_zone_shortcut_context(find, focus, start, case):
            def select(label):
                radio = find(label, "RadioButton")
                try:
                    radio.iface_selection_item.Select()
                except Exception:
                    point = radio.rectangle().mid_point()
                    gui.ShowWindow(fixture["window"], con.SW_HIDE)
                    api.SetCursorPos((point.x, point.y))
                    api.mouse_event(con.MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0)
                    api.mouse_event(con.MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)
                    time.sleep(0.08)
                    gui.ShowWindow(fixture["window"], con.SW_SHOWNA)

            find("Targets", "Button").iface_toggle.Toggle()
            select("Zones & filters")
            find("Choose a zone’s Pick or Replace button to capture its corners.")
            api.SetCursorPos(center("start"))
            windows.tap_key(con.VK_F8)
            time.sleep(0.15)
            select("Route")
            find(f"1. X {x}  Y {y}", "ListItem")
            assert len(find("Route steps", "List").descendants(control_type="ListItem")) == 1
            find("Route off")

            select("Zones & filters")
            find("Pick corner", "Button").iface_invoke.Invoke()
            api.SetCursorPos((25, 25))
            windows.tap_key(con.VK_F8)
            find("First corner X 25  Y 25. Pick the opposite corner.")
            api.SetCursorPos((125, 125))
            windows.tap_key(con.VK_F8)
            find("X 25  Y 25 to X 125  Y 125")

            select("Route")
            api.SetCursorPos(center("start"))
            windows.tap_key(con.VK_F8)
            px, py = center("start")
            find(f"2. X {px}  Y {py}", "ListItem")
            find("Route off")
            order(start, [])

        run_case("f8-follows-the-visible-targets-tab", [{"x": x, "y": y}],
                 test_zone_shortcut_context, route_enabled=False)

        def test_zone_replacement(find, focus, start, case):
            find("Fix in Targets", "Button").iface_invoke.Invoke()
            find("Replace zone", "Button").iface_invoke.Invoke()
            api.SetCursorPos((25, 25))
            windows.tap_key(con.VK_F8)
            find("First corner X 25  Y 25. Pick the opposite corner.")
            saved = "X 0  Y 0 to X 200  Y 200"
            find(saved)
            find("Cancel Inside only zone replacement", "Button").iface_invoke.Invoke()
            find(saved)
            find("Replace zone", "Button")
            order(start, [])

        run_case("cancel-zone-replacement-keeps-saved-zone", [{"x": x, "y": y}],
                 test_zone_replacement,
                 inside_zone={"from": {"x": 0, "y": 0}, "to": {"x": 200, "y": 200}})

        def test_active_preset(find, focus, start, case):
            find("Presets", "Button").iface_toggle.Toggle()
            find("New preset name", "Edit").iface_value.SetValue("Timing test")
            find("Save as new", "Button").iface_invoke.Invoke()
            find("Preset · Timing test")
            find("Clicker", "Button").iface_toggle.Toggle()
            field = find("Click interval or rate", "Edit")
            field.iface_value.SetValue("200")
            find("Preset · Timing test · Modified")
            field.iface_value.SetValue("100")
            find("Preset · Timing test")
            order(start, [])

        run_case("preset-status-reflects-current-settings", [], test_active_preset,
                 route_enabled=False, interval="100")

        def test_shortcut_and_close_drafts(find, focus, start, case):
            find("Timing", "Button").iface_toggle.Toggle()
            find("Active shortcut · F6")
            field = find("Activation shortcut", "Edit")
            field.iface_value.SetValue("F9")
            find("Active shortcut · F6 · Unapplied change")
            field.set_focus()
            windows.tap_key(con.VK_ESCAPE)
            find("Active shortcut · F6")
            assert field.iface_value.CurrentValue == "F6"
            field.iface_value.SetValue("F9")
            windows.tap_key(con.VK_RETURN)
            find("Active shortcut · F9")

            nav = find("Targets", "Button")
            hwnd = nav.top_level_parent().handle
            nav.iface_toggle.Toggle()
            find("Add step", "Button").iface_invoke.Invoke()
            delay = find("Step delay", "Edit")
            delay.iface_value.SetValue("725")
            gui.PostMessage(hwnd, con.WM_CLOSE, 0, 0)
            find("Discard step edits?")
            find("Keep editing", "Button").iface_invoke.Invoke()
            assert find("Step delay", "Edit").iface_value.CurrentValue == "725"
            gui.PostMessage(hwnd, con.WM_CLOSE, 0, 0)
            find("Discard edits", "Button").iface_invoke.Invoke()
            windows.wait_for(lambda: not gui.IsWindow(hwnd) or not gui.IsWindowVisible(hwnd), 3,
                             "Discarding the draft should close Afterbeat.")
            order(start, [])

        run_case("shortcut-and-close-drafts", [], test_shortcut_and_close_drafts, route_enabled=False)

        def test_editor(find, focus, start, case):
            find("Targets", "Button").iface_toggle.Toggle()
            find("Pick control", "Button").iface_invoke.Invoke()
            api.SetCursorPos(center("start"))
            windows.tap_key(con.VK_F8)
            find("1. Start", "ListItem")
            find("Edit step", "Button").iface_invoke.Invoke()
            find("Right click ×2 · 10 /s")
            find("Save step", "Button").iface_invoke.Invoke()
            find("1. Start", "ListItem")
            find("Edit step", "Button").iface_invoke.Invoke()
            step_settings = find("Step settings", "ComboBox")
            step_settings.set_focus()
            windows.tap_key(con.VK_DOWN)
            find("Step delay", "Edit").iface_value.SetValue("125")
            find("Save step", "Button").iface_invoke.Invoke()
            find("1. Right click ×2 · Start · 125 ms delay", "ListItem")
            find("Add step", "Button").iface_invoke.Invoke()
            find("Step delay", "Edit").iface_value.SetValue("245")
            find("Save step", "Button").iface_invoke.Invoke()
            find("2. Wait 245 ms", "ListItem")
            focus("start")
            started = windows.tap_key(con.VK_F6)
            find("Completed.")
            order(start, [("rightDown", "start", 0), ("rightUp", "start", 0)] * 2)
            first_gap = (events[start][3] - started) / 1_000_000
            assert 122 <= first_gap <= 300, first_gap
            case["first_delay_ms"] = first_gap
            completed = (time.perf_counter_ns() - events[start+3][3]) / 1_000_000
            assert 242 <= completed <= 370, completed
            case["trailing_wait_to_status_ms"] = completed
        run_case("f8-and-editor-build-a-playable-route", [], test_editor, route_enabled=False,
                 button="Right", double=True, interval="100")

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
