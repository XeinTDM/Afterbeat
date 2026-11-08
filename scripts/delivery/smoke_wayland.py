"""Verify that the release app maps a native surface without an X server."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def wait_for(process, condition, log):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if process.poll() is not None:
            break
        if condition():
            return
        time.sleep(0.05)
    raise AssertionError(log.read_text(encoding="utf-8", errors="replace"))


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)


def smoke(executable):
    with tempfile.TemporaryDirectory(prefix="afterbeat-weston-") as directory:
        folder = Path(directory)
        socket = "afterbeat-wayland-test"
        env = {**os.environ, "XDG_RUNTIME_DIR": directory, "XDG_SESSION_TYPE": "wayland",
               "WAYLAND_DISPLAY": socket, "XDG_CONFIG_HOME": str(folder / "config")}
        env.pop("DISPLAY", None)
        env.pop("WAYLAND_SOCKET", None)
        weston_log = folder / "weston.log"
        with subprocess.Popen(
            ["weston", "--backend=headless-backend.so", "--use-pixman", "--no-config",
             "--idle-time=0", f"--socket={socket}", f"--log={weston_log}"],
            env=env,
        ) as compositor:
            try:
                wait_for(compositor, (folder / socket).is_socket, weston_log)
                app_log = folder / "app.log"
                with app_log.open("wb") as output, subprocess.Popen(
                    [executable], env={**env, "WAYLAND_DEBUG": "client"},
                    stdout=output, stderr=subprocess.STDOUT,
                ) as app:
                    try:
                        def mapped():
                            log = app_log.read_text(encoding="utf-8", errors="replace")
                            return ('set_app_id("io.github.xeintdm.Afterbeat")' in log
                                    and "wl_surface" in log and ".attach(wl_buffer" in log)

                        wait_for(app, mapped, app_log)
                    finally:
                        stop(app)
            finally:
                stop(compositor)
    print("Native Wayland window smoke passed")


if __name__ == "__main__":
    smoke(sys.argv[1])
