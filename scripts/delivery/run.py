"""Run Afterbeat against a tagged Win32 receiver on an isolated input desktop."""

import argparse
import csv
from contextlib import ExitStack, redirect_stderr, redirect_stdout
from datetime import datetime, timezone
import hashlib
from importlib.metadata import PackageNotFoundError, version
import json
import os
from pathlib import Path
import random
import subprocess
import sys
import time
import traceback
import uuid

from peer_assets import LOCK, PEERS, cached, fetch, stage
from results import NS_PER_MS, NS_PER_SECOND, SMOKE_INITIAL_CLICKS, check_trial, input_tag, replay


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
OBSERVE_MS = 500
SMOKE_STOP_MS = 250
WARMUP_MS = 300
PEER_CACHE = ROOT / "target" / "delivery" / "peer-cache"
WINDOWS = {
    "blur": ("BlurAutoClicker", None),
    "vxclick": ("VxClick", None),
    "op": ("OP Auto Clicker 4.1", "AutoIt v3 GUI"),
    "playsaurus": ("Auto Clicker", "WinUIDesktopWin32WindowClass"),
    "speed": ("Speed AutoClicker", None),
}


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def fingerprint(path):
    with path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": digest}


def plan(args):
    if args.command in ("visual-targets", "route-actions"):
        return []
    if args.command == "smoke":
        cases = [
            {"mode": "counted", "condition": "idle", "rate": 100, "count": 12},
            {"mode": "emergency-stop", "condition": "idle", "rate": 100},
            {"mode": "held-button", "condition": "idle", "rate": 100},
        ]
    else:
        modes = [{"mode": mode} for mode in ("Precise", "Low CPU")]
        if args.command == "peers":
            modes = [{"mode": f"Afterbeat {mode['mode']}"} for mode in modes] + [
                {"mode": PEERS[key]["name"], "peer": key} for key in args.peer
            ]
        cases = [
            {**mode, "condition": condition, "rate": rate, "repetition": repeat}
            for mode in modes
            for condition in (("idle", "load") if args.load else ("idle",))
            for rate in args.rates
            for repeat in range(1, args.repetitions + 1)
        ]
        random.Random(args.seed).shuffle(cases)
    return [dict(case, id=f"trial-{number:04}") for number, case in enumerate(cases, 1)]


def settings_for(case, seconds):
    return {
        "interval": str(1000 / case["rate"]),
        "trigger": "F6", "delay_seconds": 0,
        "low_cpu": case["mode"] in ("Low CPU", "Afterbeat Low CPU"),
        "limited": "count" in case, "count": str(case.get("count", 1)),
        "hold_down": case["mode"] == "held-button",
        "time_limit": str(seconds + 4),
    }


def start_load(stack, mask, windows):
    load = stack.enter_context(windows.ProcessJob())
    for bit in range(mask.bit_length()):
        core = 1 << bit
        if mask & core:
            for _ in range(2):
                worker, _ = load.launch(
                    sys.executable,
                    subprocess.list2cmdline([sys.executable, "-c", "while True: pass"]),
                )
                try:
                    windows.process.SetProcessAffinityMask(worker, core)
                finally:
                    worker.Close()
    time.sleep(0.12)


def run_trial(output, manifest, case, sink, windows):
    folder = output / case["id"]
    folder.mkdir()
    data = folder / "local" / "Afterbeat"
    env = {**os.environ, "LOCALAPPDATA": str(data.parent), "APPDATA": str(folder / "roaming"),
           "WEBVIEW2_USER_DATA_FOLDER": str(folder / "webview")}
    peer = case.get("peer")
    executable = Path(manifest["peers"][peer]["executable"]["path"] if peer else manifest["binary"]["path"])
    trial = {"case": case, "status": "failed", "stop_ns": None}
    sink.events.clear()

    try:
        if manifest["command"] == "peers":
            trial["stop_binding"], stop_key, stop_modifiers = "F6", windows.con.VK_F6, ()
        else:
            trial["stop_binding"], stop_modifiers = windows.emergency_binding()
            stop_key = windows.con.VK_F7
        with ExitStack() as stack:
            if peer:
                import peer_windows

                reserved = stack.enter_context(peer_windows.reserve_profile(peer, folder))
                settings = peer_windows.seed(peer, case["rate"], executable, folder, reserved)
                windows.gui.ShowWindow(sink.hwnd, windows.con.SW_HIDE)
            else:
                settings = settings_for(case, manifest["seconds"])
                data.mkdir(parents=True)
                write_json(data / "settings.json", settings)
            write_json(folder / "settings.json", settings)
            app = stack.enter_context(windows.ProcessJob(manifest["app_affinity"]))
            handle, pid = app.launch(
                executable, subprocess.list2cmdline([str(executable)]), env=env, cwd=str(executable.parent),
            )
            hwnd = None
            try:
                trial["app_pid"] = pid
                log = data / "diagnostics.log"

                def ready():
                    if windows.event.WaitForSingleObject(handle, 0) == 0:
                        code = windows.process.GetExitCodeProcess(handle)
                        raise RuntimeError(f"{case['mode']} exited before it was ready (exit {code:#x}).")
                    if peer:
                        return windows.window_for(pid, *WINDOWS[peer])
                    hwnd = windows.window_for(pid, "Afterbeat")
                    return hwnd if hwnd and log.exists() and "Input protection active." in log.read_text(
                        encoding="utf-8",
                    ) else None

                hwnd = windows.wait_for(ready, 20, f"{case['mode']} did not initialize its window.")
                if peer:
                    time.sleep(2.5 if peer == "playsaurus" else 1.9 if peer in ("blur", "vxclick") else 0.5)
                    if peer in ("op", "playsaurus", "speed"):
                        trial["configuration_readback"] = peer_windows.configure(peer, hwnd, case["rate"], windows)
                elif manifest["command"] == "smoke" and case["mode"] == "counted":
                    from smoke_uia import check

                    windows.gui.ShowWindow(sink.hwnd, windows.con.SW_HIDE)
                    check(hwnd, sink, windows, output / "uia.json")
                windows.gui.ShowWindow(hwnd, windows.con.SW_MINIMIZE)
                if case["condition"] == "load":
                    start_load(stack, manifest["app_affinity"], windows)
                sink.aim()
                capture(trial, sink, app, manifest, windows, stop_key, stop_modifiers)
                check_trial(trial, sink.events[:], manifest)
            finally:
                try:
                    if hwnd and windows.gui.IsWindow(hwnd):
                        windows.gui.PostMessage(hwnd, windows.con.WM_CLOSE, 0, 0)
                    windows.event.WaitForSingleObject(handle, 1000)
                    if app.active_processes():
                        app.terminate()
                        windows.wait_for(lambda: app.active_processes() == 0, 3, f"{case['mode']} did not exit after cleanup.")
                finally:
                    handle.Close()
        trial["status"] = "passed"
    except BaseException as error:
        trial["error"] = str(error)
        raise
    finally:
        events = sink.events[:]
        with (folder / "events.csv").open("w", newline="", encoding="utf-8") as stream:
            writer = csv.writer(stream)
            writer.writerow(("time_ns", "message", "tag"))
            writer.writerows(events)
        write_json(folder / "trial.json", trial)
        if (
            events and events[-1][1] == windows.DOWN and events[-1][2] & 0xFFFFFFFF == input_tag(manifest, case)
            and windows.active_desktop() == sink.desktop_name
        ):
            sink.aim()
            windows.api.mouse_event(windows.con.MOUSEEVENTF_LEFTUP, 0, 0, 0, 0)


def capture(trial, sink, app, manifest, windows, stop_key, stop_modifiers):
    case = trial["case"]
    bench = manifest["command"] != "smoke"
    hold_seconds = manifest["key_hold_ms"] / 1000

    def observe_until(deadline):
        while (left := deadline - time.perf_counter_ns()) > 0:
            sink.check_destination()
            time.sleep(min(0.025, left / NS_PER_SECOND))
        sink.check_destination()

    def enough_events():
        sink.check_destination()
        return len(sink.events) >= needed

    trial["cpu_start"] = (time.perf_counter_ns(), app.cpu_ns())
    trial["start_ns"] = windows.tap_key(windows.con.VK_F6, hold_seconds=hold_seconds)
    if bench:
        observe_until(trial["start_ns"] + manifest["warmup_ms"] * NS_PER_MS)
        trial["cpu_start"] = (time.perf_counter_ns(), app.cpu_ns())
        trial["measure_begin_ns"] = time.perf_counter_ns()
        observe_until(trial["measure_begin_ns"] + round(manifest["seconds"] * NS_PER_SECOND))
    else:
        needed = (
            2 * case["count"] if "count" in case else
            1 if case["mode"] == "held-button" else 2 * SMOKE_INITIAL_CLICKS
        )
        windows.wait_for(enough_events, 2, f"{case['mode']} did not deliver its expected initial events.")
        trial["measure_begin_ns"] = trial["start_ns"]
    trial["measure_end_ns"] = time.perf_counter_ns()
    trial["cpu_end"] = (time.perf_counter_ns(), app.cpu_ns())
    if "count" not in case:
        if case["mode"] == "held-button":
            observe_until(time.perf_counter_ns() + 100 * NS_PER_MS)
        sink.check_destination()
        trial["stop_ns"] = windows.tap_key(stop_key, modifiers=stop_modifiers, hold_seconds=hold_seconds)
    trial["observed_until_ns"] = (trial["stop_ns"] or trial["measure_end_ns"]) + OBSERVE_MS * NS_PER_MS
    observe_until(trial["observed_until_ns"])


def worker(output, desktop_name, windows):
    manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
    if not windows.USER32.SetProcessDpiAwarenessContext(windows.c.c_void_p(-4)):
        windows.USER32.SetProcessDPIAware()
    if manifest["receiver_affinity"] is not None:
        current = windows.api.GetCurrentProcess()
        windows.process.SetProcessAffinityMask(current, manifest["receiver_affinity"])
    sink = windows.Receiver(desktop_name)
    (output / "ready").touch()
    windows.wait_for(lambda: windows.active_desktop() == desktop_name, 15, "The isolated desktop did not activate.")
    if manifest["command"] in ("visual-targets", "route-actions"):
        if manifest["command"] == "visual-targets":
            from smoke_visual_targets import check
        else:
            from smoke_route_actions import check
        windows.gui.ShowWindow(sink.hwnd, windows.con.SW_HIDE)
        check(output, manifest, windows)
        return
    for case in manifest["cases"]:
        print(f"{case['id']} {case['mode']} {case['condition']} {case['rate']} CPS", flush=True)
        run_trial(output, manifest, case, sink, windows)
        time.sleep(0.12)
    if manifest["command"] == "smoke":
        from smoke_targets import check
        check(output, manifest, windows)


def launch_worker(output, manifest, windows):
    name = f"AfterbeatDelivery-{uuid.uuid4().hex[:12]}"
    original_name = windows.active_desktop()
    original = windows.desktop.OpenInputDesktop(0, False, 0x0101)
    isolated = None
    handle = None
    switched = False
    try:
        cursor = windows.api.GetCursorPos()
        isolated = windows.desktop.CreateDesktop(name, 0, 0x01FF, None)
        args = [sys.executable, "-B", str(Path(__file__).resolve()), "--worker", str(output), name]
        with windows.ProcessJob() as supervisor:
            handle, _ = supervisor.launch(sys.executable, subprocess.list2cmdline(args), desktop_name=name)

            def ready():
                if windows.event.WaitForSingleObject(handle, 0) == 0:
                    raise RuntimeError(f"The receiver failed to start. See {output / 'run.log'}.")
                return (output / "ready").exists()

            windows.wait_for(ready, 15, "The isolated receiver did not become ready.")
            if windows.active_desktop() != original_name:
                raise RuntimeError("The input desktop changed before the test could start.")
            isolated.SwitchDesktop()
            switched = True
            timeout = len(manifest["cases"]) * (manifest["seconds"] + 30) + 10
            if manifest["command"] in ("smoke", "visual-targets", "route-actions"):
                timeout += 120  # The native targeting scenarios run after delivery trials.
            windows.wait_for(
                lambda: windows.event.WaitForSingleObject(handle, 0) == 0, timeout,
                f"The delivery test exceeded its {timeout:g} second watchdog.",
            )
            if windows.process.GetExitCodeProcess(handle):
                raise RuntimeError(f"The delivery test failed. See {output / 'run.log'}.")
    finally:
        # The supervisor job has closed before returning focus to the original desktop.
        try:
            if handle is not None:
                windows.event.WaitForSingleObject(handle, 3000)
                handle.Close()
            if switched:
                try:
                    if windows.active_desktop() == name:
                        original.SwitchDesktop()
                        windows.api.SetCursorPos(cursor)
                except (PermissionError, windows.pywintypes.error) as error:
                    if getattr(error, "winerror", None) != 5:
                        raise
        finally:
            if isolated is not None:
                isolated.CloseDesktop()
            original.CloseDesktop()


def run(args, windows):
    binary = args.exe.resolve(strict=True)
    output = args.output.resolve() if args.output else ROOT / "target" / "delivery" / uuid.uuid4().hex
    current = windows.api.GetCurrentProcess()
    available, _ = windows.process.GetProcessAffinityMask(current)
    cores = [1 << bit for bit in range(available.bit_length()) if available & (1 << bit)]
    affinity = None
    benchmark = args.command in ("benchmark", "peers")
    peers = args.command == "peers"
    if benchmark and len(cores) > 1:
        app_cores = min(2, len(cores) - 1)
        affinity = sum(cores[-app_cores:])
    if benchmark and args.load and affinity is None:
        raise RuntimeError("The load comparison needs at least two available logical CPUs.")
    harness = ["run.py", "windows.py", "results.py", "requirements.txt"]
    if peers:
        from peer_windows import require_clean_profile

        harness += ["peer_assets.py", "peer_windows.py", LOCK.name]
        archives = {key: cached(args.cache.resolve(), key) for key in args.peer}
    elif args.command == "smoke":
        harness += ["smoke_uia.py", "smoke_targets.py"]
    elif args.command == "visual-targets":
        harness.append("smoke_visual_targets.py")
    elif args.command == "route-actions":
        harness.append("smoke_route_actions.py")
    dependencies = ("pywin32",) if args.command == "benchmark" else ("pywin32", "pywinauto", "comtypes", "six")
    try:
        versions = {name: version(name) for name in dependencies}
    except PackageNotFoundError as error:
        raise RuntimeError("Install the pinned dependencies with pip install -r scripts/delivery/requirements.txt.") from error
    manifest = {
        "schema_version": 1, "command": args.command, "status": "running",
        "started_utc": datetime.now(timezone.utc).isoformat(), "binary": fingerprint(binary),
        "harness": [fingerprint(HERE / name) for name in harness],
        "environment": {
            "windows_version": sys.getwindowsversion().platform_version,
            "python": sys.version, **versions,
            "processor": os.environ.get("PROCESSOR_IDENTIFIER"), "logical_cpus": os.cpu_count(),
            "available_affinity": available, "power_status": windows.api.GetSystemPowerStatus(),
        },
        "seconds": args.seconds, "seed": args.seed, "observe_ms": OBSERVE_MS,
        "smoke_stop_ms": SMOKE_STOP_MS, "activation": "F6",
        "emergency_stop_candidates": [] if peers else ["F7", "Ctrl+Alt+F7"],
        "stop_protocol": "F6 toggle" if peers else "emergency stop",
        "key_hold_ms": 150 if peers else 75,
        "warmup_ms": WARMUP_MS if benchmark else 0,
        "input_tag": windows.TAG,
        "app_affinity": affinity, "receiver_affinity": available ^ affinity if affinity else None,
        "cases": plan(args),
    }
    with windows.exclusive_session():
        if peers:
            require_clean_profile(args.peer)
        output.mkdir(parents=True)
        print(f"Delivery evidence will be written to {output}", flush=True)
        try:
            if peers:
                manifest["peers"] = {}
                for key, archive in archives.items():
                    package = fingerprint(archive)
                    if package["sha256"] != PEERS[key]["sha256"]:
                        raise ValueError(f"{archive} changed during staging.")
                    executable = stage(archive, key, output / "portable" / key)
                    manifest["peers"][key] = {**PEERS[key], "package": package, "executable": fingerprint(executable)}
            write_json(output / "manifest.json", manifest)
            launch_worker(output, manifest, windows)
            if fingerprint(binary) != manifest["binary"]:
                raise RuntimeError("The executable changed during the run.")
            for peer in manifest.get("peers", {}).values():
                executable = peer["executable"]
                if fingerprint(Path(executable["path"])) != executable:
                    raise RuntimeError(f"{peer['name']} changed its staged executable during the run.")
            manifest["status"] = "passed"
            if args.command in ("visual-targets", "route-actions"):
                print(f"Results are in {output / args.command / 'result.json'}.")
            else:
                rows = replay(output)
                print(f"{len(rows)} delivery trials recorded. Reports are in {output}.")
        except BaseException as error:
            manifest.update(status="failed", error=str(error))
            raise
        finally:
            manifest["finished_utc"] = datetime.now(timezone.utc).isoformat()
            write_json(output / "manifest.json", manifest)


def main():
    if sys.argv[1:2] == ["--worker"]:
        import windows

        output = Path(sys.argv[2])
        with (output / "run.log").open("w", encoding="utf-8", buffering=1) as log:
            with redirect_stdout(log), redirect_stderr(log):
                try:
                    worker(output, sys.argv[3], windows)
                except BaseException:
                    traceback.print_exc()
                    return 1
        return 0

    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("smoke", "benchmark", "peers", "visual-targets", "route-actions"):
        command = commands.add_parser(name)
        command.add_argument("--exe", type=Path, default=ROOT / "target" / "release" / "afterbeat.exe")
        command.add_argument("--output", type=Path, help="New directory for this run.")
        command.set_defaults(seconds=1.0, seed=None)
    for name in ("benchmark", "peers"):
        command = commands.choices[name]
        command.add_argument("--rates", type=int, nargs="+", default=[100, 500, 1000])
        command.add_argument("--seconds", type=float, default=5.0)
        command.add_argument("--repetitions", type=int, default=5)
        command.add_argument("--seed", type=int, default=1145)
        command.add_argument("--load", action="store_true", help="Also test with two busy workers per app CPU.")
    command = commands.choices["peers"]
    command.add_argument("--disposable-profile", action="store_true", required=True,
                         help="Acknowledge that the current Windows profile is a disposable test profile.")
    for command in (command, commands.add_parser("fetch-peers")):
        command.add_argument("--cache", type=Path, default=PEER_CACHE)
        command.add_argument("--peer", nargs="+", choices=PEERS, default=list(PEERS))
    args = parser.parse_args()
    if args.command in ("benchmark", "peers") and (
        not 1 <= args.seconds <= 300 or not 1 <= args.repetitions <= 100
        or any(not 1 <= rate <= 1000 for rate in args.rates)
        or len(set(args.rates)) != len(args.rates)
    ):
        parser.error("Use 1–300 seconds, 1–100 repetitions, and distinct rates from 1 to 1000 CPS.")
    if args.command in ("peers", "fetch-peers") and len(set(args.peer)) != len(args.peer):
        parser.error("Select each peer once.")
    if args.command == "peers" and {"op", "playsaurus"}.intersection(args.peer) and any(
        1000 % rate for rate in args.rates
    ):
        parser.error("OP Auto Clicker and Playsaurus require rates with an exact whole-millisecond interval.")
    if args.command == "fetch-peers":
        try:
            fetch(args.cache.resolve(), args.peer)
        except (OSError, RuntimeError, ValueError) as error:
            print(error, file=sys.stderr)
            return 1
        return 0
    if sys.platform != "win32":
        parser.error("Live delivery testing requires an interactive Windows session.")
    try:
        import windows
    except ImportError:
        parser.error("Install the pinned dependency with pip install -r scripts/delivery/requirements.txt.")
    try:
        run(args, windows)
    except (OSError, RuntimeError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
