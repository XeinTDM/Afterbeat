"""Exercise the release app through its native Windows UI Automation provider."""

import json
import time


def check(hwnd, sink, windows, path):
    from pywinauto import Desktop

    root = Desktop(backend="uia").window(handle=hwnd)
    evidence = {"status": "failed", "checks": []}
    step = "Connect to the Afterbeat UI Automation tree"

    def controls():
        return root.descendants()

    def find(name, control_type, *, timeout=4):
        def expected_name():
            return name() if callable(name) else name

        def match():
            expected = expected_name()
            try:
                matches = [item for item in controls()
                           if item.element_info.control_type == control_type and item.element_info.name == expected]
            except KeyError as error:
                # A disappearing element can lose its control type before pywinauto wraps it.
                if error.args != (None,):
                    raise
                return None
            if len(matches) > 1:
                raise RuntimeError(f"UI Automation exposed duplicate {control_type} controls named {expected!r}.")
            return matches[0] if matches else None

        return windows.wait_for(match, timeout, f"UI Automation did not expose {control_type} {expected_name()!r}.")

    def button(name):
        item = find(name, "Button")
        if not item.is_enabled():
            raise RuntimeError(f"UI Automation reported {name!r} disabled.")
        return item

    def invoke(name):
        button(name).iface_invoke.Invoke()

    def select_page(name):
        button(name).iface_toggle.Toggle()

    def record(detail=None):
        evidence["checks"].append({"check": step, "observed": detail})

    try:
        step = "Expose the cadence value and an enabled Start run button"
        field = find("Click interval or rate", "Edit")
        value = field.iface_value.CurrentValue
        find("Cadence unit", "ComboBox")
        if float(value) != 10 or not find("Start run", "Button").is_enabled():
            raise RuntimeError(f"Expected an enabled Start run with a 10 ms cadence, got {value!r}.")
        record(value)

        step = "Invoke the test pad and expose its incremented accessible name"
        invoke("Test pad, 0 presses")
        find("Test pad, 1 presses", "Button")
        record("Test pad, 1 presses")

        step = "Block activation while typing a shortcut, without disabling emergency Stop"
        select_page("Timing")
        shortcut = find("Activation shortcut", "Edit")
        shortcut.set_focus()
        shortcut.iface_value.SetValue("Ctrl+Alt+G")
        windows.wait_for(lambda: not find("Start run", "Button").is_enabled(), 3,
                         "Start stayed enabled while the shortcut editor had focus.")
        windows.tap_key(windows.con.VK_F6)
        time.sleep(0.25)
        if find("Start run", "Button").is_enabled() or any(
                item.element_info.name in ("Running", "Countdown", "Last run") for item in controls()):
            raise RuntimeError("The global activation shortcut started a run while editing its binding.")
        if sink.events:
            raise RuntimeError("Editing a shortcut delivered input to the receiver.")
        windows.tap_key(windows.con.VK_F7)
        windows.tap_key(windows.con.VK_ESCAPE)
        if shortcut.iface_value.CurrentValue != "F6":
            raise RuntimeError("Escape did not restore the registered shortcut.")
        shortcut.set_focus()
        windows.wait_for(lambda: not find("Start run", "Button").is_enabled(), 3,
                         "Refocusing the editor did not block new starts.")
        invoke("Apply")
        windows.wait_for(lambda: find("Start run", "Button").is_enabled(), 3,
                         "Apply did not finish shortcut editing.")
        select_page("Clicker")
        windows.wait_for(lambda: find("Start run", "Button").is_enabled(), 3,
                         "Start did not recover after leaving the shortcut editor.")
        record("F6 blocked; F7 exercised; Escape restored F6; Apply finished editing; Start recovered")

        step = "Navigate to Dry run through Targets"
        select_page("Targets")
        invoke("Dry run")
        find("Virtual desktop map with preview points and safety zones", "Image")
        find("Simulation only · no input is sent", "Text")
        record("Simulation only · no input is sent")

        step = "Announce the Dry run sampling countdown through UI Automation"
        sink.aim()
        begin = time.perf_counter()
        invoke("Sample pointer in 3 s")
        find("Sampling in 3 s. Move the pointer to the target.", "Text", timeout=2)
        find("Cancel sample", "Button")
        find("Sampling in 2 s. Move the pointer to the target.", "Text", timeout=2)
        find("Sampling in 1 s. Move the pointer to the target.", "Text", timeout=2)
        record("3 s → 2 s → 1 s")

        step = "Report the sampled location without delivering clicks"

        def sampled_location():
            x, y = windows.api.GetCursorPos()
            return f"X {x}  Y {y} · Spatial checks passed."

        result = find(sampled_location, "Text", timeout=6)
        elapsed = time.perf_counter() - begin
        if elapsed < 2.75:
            raise RuntimeError(f"Dry run sampling completed early after {elapsed:.3f} s.")
        sink.check_destination()
        if sink.events:
            raise RuntimeError(f"Dry run sent {len(sink.events)} button edges to the receiver.")
        record({"result": result.element_info.name, "elapsed_seconds": elapsed, "receiver_edges": 0})

        step = "Return from Dry run through the accessible navigation controls"
        invoke("Back to Targets")
        select_page("Clicker")
        find("Test pad, 1 presses", "Button")
        record("Test pad, 1 presses")
        step = "Move a captured step directly to a position and reject invalid positions"
        select_page("Targets")
        points = [(150, 200), (180, 200), (210, 200)]
        for x, y in points:
            invoke("Add in 3s")
            find("Cancel", "Button")
            windows.api.SetCursorPos((x, y))
            time.sleep(0.05)
            windows.tap_key(windows.con.VK_F8)
            find("Add in 3s", "Button")

        def route_labels():
            return [item.element_info.name for item in controls()
                    if item.element_info.control_type == "ListItem"]

        def route_matches(order):
            return route_labels() == [f"{index}. {target}" for index, target in enumerate(order, 1)]

        position = find("Route position, from 1 to 3", "Edit")
        captured = [label.partition(". ")[2] for label in route_labels()]
        if len(captured) != 3 or len(set(captured)) != 3:
            raise RuntimeError(f"Expected three distinct captured targets, got {captured!r}.")
        position.iface_value.SetValue("1")
        invoke("Move step")
        windows.wait_for(lambda: route_matches([captured[2], captured[0], captured[1]]), 3,
                         "Move step did not preserve the order of the remaining targets.")
        position.iface_value.SetValue("0")
        invoke("Move step")
        find("Enter a position from 1 to 3.", "Text")
        if not route_matches([captured[2], captured[0], captured[1]]):
            raise RuntimeError("An invalid route position changed the route.")
        position.iface_value.SetValue("3")
        invoke("Move step")
        windows.wait_for(lambda: route_matches(captured), 3, "Moving back did not restore the route order.")
        for _ in points:
            invoke("Remove")
        select_page("Clicker")
        record("Moved step 3 to 1 and back; position 0 rejected; captured test route removed")
        evidence["status"] = "passed"
    except Exception as error:
        evidence.update(failed_check=step, error=f"{type(error).__name__}: {error}")
        try:
            evidence["tree"] = [{"name": item.element_info.name, "type": item.element_info.control_type}
                                for item in controls()]
        except Exception as tree_error:
            evidence["tree_error"] = str(tree_error)
        raise RuntimeError(f"Native UI Automation smoke failed at {step}. See {path}.") from error
    finally:
        path.write_text(json.dumps(evidence, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
