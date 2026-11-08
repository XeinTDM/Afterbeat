"""Configure the pinned Windows peer releases in a disposable profile."""

from contextlib import contextmanager
import json
from pathlib import Path
import shutil
import time
from xml.etree import ElementTree as ET

from win32com.shell import shell, shellcon


def profile_paths(peer):
    roaming = Path(shell.SHGetFolderPath(0, shellcon.CSIDL_APPDATA, None, 0))
    local = Path(shell.SHGetFolderPath(0, shellcon.CSIDL_LOCAL_APPDATA, None, 0))
    return {
        "vxclick": {"known-roaming": roaming / "com.vxiey.vxclick", "known-local": local / "com.vxiey.vxclick"},
        "speed": {"known-local": local / "fabi.me"},
        "playsaurus": {"known-roaming": roaming / "Helba", "known-local": local / "Helba"},
    }.get(peer, {})


def require_clean_profile(peers):
    for peer in peers:
        for path in profile_paths(peer).values():
            if path.exists() or path.is_symlink() or path.is_junction():
                raise RuntimeError(f"Existing peer data at {path}. Use a clean disposable Windows profile.")


@contextmanager
def reserve_profile(peer, folder):
    reserved = {}
    try:
        for label, path in profile_paths(peer).items():
            path.mkdir()
            reserved[label] = path
        yield reserved
    finally:
        for label, path in reserved.items():
            destination = folder / label
            for attempt in range(4):
                try:
                    path.rename(destination)
                    break
                except PermissionError:
                    if attempt == 3:
                        raise
                    time.sleep(0.25)
                except OSError as error:
                    if error.winerror != 17:  # ERROR_NOT_SAME_DEVICE
                        raise
                    shutil.move(path, destination)
                    break


def seed(peer, rate, executable, folder, reserved):
    settings = {"requested_cps": rate, "activation": "F6", "button": "left", "mode": "toggle"}
    if peer == "blur":
        config = executable.parent / "Data/settings.json"
        data = {"settings": {
            "onboardingCompleted": True, "clickSpeed": rate, "savedClickSpeed": rate,
            "clickInterval": "s", "hotkey": "f6", "mode": "Toggle", "inputType": "mouse",
            "mouseButton": "Left", "rateInputMode": "rate", "extendedClickSpeedLimit": True,
            "dutyCycleEnabled": False, "speedRandomizationEnabled": False,
            "indicatorEnabled": False, "cornerStopEnabled": False, "edgeStopEnabled": False,
            "showStopOverlay": False, "showStopReason": False, "soundMuted": True,
            "minimizeToTray": False, "taskSwitcherStopEnabled": False, "advancedSettingsEnabled": True,
        }}
        paths = [config]
    elif peer == "vxclick":
        data = {
            "schema_version": 1, "active_profile_id": "default", "auto_switch_enabled": False,
            "profiles": [{
                "id": "default", "name": "Benchmark", "description": "", "process_names": [],
                "auto_switch": False,
                "clicker": {
                    "cps": rate, "button": "left", "mode": "toggle", "randomize": False,
                    "burst": False, "start_hotkey": "F6", "emergency_stop_hotkey": "F8",
                },
            }],
        }
        paths = [reserved["known-roaming"] / "profiles.json",
                 folder / "roaming/com.vxiey.vxclick/profiles.json"]
    elif peer == "speed":
        data = {
            "ActivationMode": "Switch", "ClickRate": str(rate), "ClickCycle": "0",
            "MouseButton": "Left", "SendStats": "False", "Startup": "False",
            "AllWindows": "True", "ShowAdvSettings": "True", "StartMinimized": "False",
            "UpgradeRequired": "False", "UnlimitedClickRate": "False",
            "RandomizeClickRate": "False", "EnableClickLimit": "False",
            "Hotkey": "<ArrayOfString><string>F6</string></ArrayOfString>",
        }
        cfg = ET.Element("configuration")
        section = ET.SubElement(ET.SubElement(cfg, "userSettings"), "AutoClicker.Properties.Settings")
        for key, value in data.items():
            node = ET.SubElement(section, "setting", name=key, serializeAs="Xml" if key == "Hotkey" else "String")
            node.text = value
        ET.ElementTree(cfg).write(reserved["known-local"] / "ac.config", encoding="utf-8", xml_declaration=True)
        paths = []
    else:
        data = {"interval_ms": 1000 // rate}
        paths = []
    for path in paths:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(data), encoding="utf-8")
    return {**settings, "configuration": data}


def control_value(control):
    from pywinauto.uia_defines import NoPatternInterfaceError

    for attr in ("iface_value", "iface_range_value"):
        try:
            return float(str(getattr(control, attr).CurrentValue).replace(",", "."))
        except (NoPatternInterfaceError, ValueError):
            continue
    raise RuntimeError("The peer rate control has no readable numeric value.")


def configure(peer, hwnd, rate, windows):
    from pywinauto import Desktop

    gui, con = windows.gui, windows.con
    interval = 1000 // rate
    if peer == "op":
        control = Desktop(backend="win32").window(handle=gui.GetDlgItem(hwnd, 10)).wrapper_object()
        control.set_edit_text(str(interval))
        actual = control.text_block()
        if actual != str(interval):
            raise RuntimeError(f"OP Auto Clicker retained {actual!r} ms after setting {interval} ms.")
        for control_id, expected in ((13, 0), (28, 1), (31, 1)):
            button = gui.GetDlgItem(hwnd, control_id)
            if gui.SendMessage(button, con.BM_GETCHECK, 0, 0) != expected:
                gui.SendMessage(button, con.BM_CLICK, 0, 0)
            if gui.SendMessage(button, con.BM_GETCHECK, 0, 0) != expected:
                raise RuntimeError(f"OP Auto Clicker control {control_id} did not retain state {expected}.")
        if ("F6" not in gui.GetWindowText(gui.GetDlgItem(hwnd, 39))
                or any(gui.SendMessage(gui.GetDlgItem(hwnd, n), con.CB_GETCURSEL, 0, 0) != 0 for n in (19, 21))):
            raise RuntimeError("OP Auto Clicker did not expose the expected F6, left button, and single-click settings.")
        return {"interval_ms": interval, "activation": "F6"}

    win = Desktop(backend="uia").window(handle=hwnd)
    if peer == "speed":
        actual = control_value(win.child_window(auto_id="numClickRate").descendants(control_type="Edit")[0])
        cycle = control_value(win.child_window(auto_id="numClickCycle").descendants(control_type="Edit")[0])
        key = win.child_window(auto_id="lbActKey").window_text()
        toggle = win.child_window(auto_id="rbtnModeSwitch").wrapper_object().is_selected()
        if (actual, cycle, key, toggle) != (rate, 0, "F6", True):
            raise RuntimeError(f"Speed AutoClicker settings read back as {actual} CPS, cycle {cycle}, {key}, toggle={toggle}.")
        return {"rate_cps": actual, "duty_cycle": cycle, "activation": key, "toggle": toggle}

    label = win.child_window(title="milliseconds", control_type="Text").wrapper_object().rectangle()
    spinners = [control for control in win.descendants(control_type="Spinner")
                if abs(control.rectangle().top - label.top) < 10 and control.rectangle().right <= label.left]
    if len(spinners) != 1:
        raise RuntimeError(f"Playsaurus exposed {len(spinners)} candidate millisecond fields.")
    spinner = spinners[0]
    spinner.iface_range_value.SetValue(interval)
    time.sleep(0.25)
    actual = control_value(spinner)
    if actual != interval:
        raise RuntimeError(f"Playsaurus retained {actual:g} ms after setting {interval} ms.")
    labels = [control.window_text() for control in win.descendants(control_type="Text")]
    if "F6" not in labels:
        raise RuntimeError("Playsaurus did not expose F6 in its configuration window.")
    return {"interval_ms": actual, "activation": "F6", "displayed_rate": [label for label in labels if "CPS" in label]}
