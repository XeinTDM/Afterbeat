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
        def match():
            matches = [item for item in controls()
                       if item.element_info.control_type == control_type and item.element_info.name == name]
            if len(matches) > 1:
                raise RuntimeError(f"UI Automation exposed duplicate {control_type} controls named {name!r}.")
            return matches[0] if matches else None

        return windows.wait_for(match, timeout, f"UI Automation did not expose {control_type} {name!r}.")

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

        step = "Navigate to Dry run through Targets"
        select_page("Targets")
        invoke("Dry run")
        find("Virtual desktop map with preview points and safety zones", "Image")
        find("Simulation only · no input is sent", "Text")
        record("Simulation only · no input is sent")

        step = "Announce the Dry run sampling countdown through UI Automation"
        sink.aim()
        cursor = windows.api.GetCursorPos()
        begin = time.perf_counter()
        invoke("Sample pointer in 3 s")
        find("Sampling in 3 s. Move the pointer to the target.", "Text", timeout=2)
        find("Cancel sample", "Button")
        find("Sampling in 2 s. Move the pointer to the target.", "Text", timeout=2)
        find("Sampling in 1 s. Move the pointer to the target.", "Text", timeout=2)
        record("3 s → 2 s → 1 s")

        step = "Report the sampled location without delivering clicks"
        result = find(f"X {cursor[0]}  Y {cursor[1]} · Spatial checks passed.", "Text", timeout=6)
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
