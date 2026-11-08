"""Exercise the release binary against a separate receiver on a private Xvfb server."""

import json
import os
from contextlib import contextmanager
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import time

from Xlib import X, XK, Xatom, display, protocol
from Xlib.ext import xtest


class Desktop:
    def __init__(self):
        self.display = display.Display()
        self.errors = []
        self.display.set_error_handler(lambda error, _: self.errors.append(error))
        screen = self.display.screen()
        self.receiver = screen.root.create_window(
            20, 20, 240, 180, 0, screen.root_depth,
            event_mask=X.ButtonPressMask | X.ButtonReleaseMask | X.KeyPressMask | X.KeyReleaseMask,
        )
        self.receiver.map()
        self.selection = self.display.intern_atom("_AFTERBEAT_INSTANCE")
        self.main_window = self.display.intern_atom("_AFTERBEAT_MAIN_WINDOW")
        self.buttons = []
        self.keys = []
        self.display.sync()

    def events(self, timeout=0):
        deadline = time.monotonic() + timeout
        while True:
            while self.display.pending_events():
                event = self.display.next_event()
                if event.type in (X.ButtonPress, X.ButtonRelease):
                    self.buttons.append((event.type, event.detail, event.root_x, event.root_y))
                elif event.type in (X.KeyPress, X.KeyRelease):
                    column = 1 if event.state & X.ShiftMask else 0
                    symbol = self.display.keycode_to_keysym(event.detail, column)
                    self.keys.append((event.type, symbol or self.display.keycode_to_keysym(event.detail, 0)))
                elif event.type == X.MappingNotify:
                    self.display.refresh_keyboard_mapping(event)
            remaining = deadline - time.monotonic()
            assert not self.errors, self.errors
            if remaining <= 0 or not select.select([self.display], [], [], remaining)[0]:
                return

    def wait_for(self, condition, message):
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            result = condition()
            if result:
                return result
            self.events(0.02)
        raise AssertionError(message)

    def cursor(self, x, y):
        xtest.fake_input(self.display, X.MotionNotify, x=x, y=y)
        self.display.sync()

    def key(self, name, event_type):
        keycode = self.display.keysym_to_keycode(XK.string_to_keysym(name))
        assert keycode, name
        xtest.fake_input(self.display, event_type, keycode)
        self.display.sync()

    def tap(self, name):
        self.key(name, X.KeyPress)
        time.sleep(0.03)
        self.key(name, X.KeyRelease)

    def window(self):
        owner = self.display.get_selection_owner(self.selection)
        if owner:
            reply = owner.get_full_property(self.main_window, Xatom.WINDOW)
            if reply:
                return self.display.create_resource_object("window", int(reply.value[0]))
        return None

    @contextmanager
    def app(self, executable, **settings):
        with tempfile.TemporaryDirectory(prefix="afterbeat-x11-") as folder:
            config = Path(folder) / "afterbeat"
            config.mkdir()
            (config / "settings.json").write_text(
                json.dumps({"delay_seconds": 0, "interval": "35", **settings}), encoding="utf-8"
            )
            env = {**os.environ, "XDG_CONFIG_HOME": folder}
            with subprocess.Popen([executable], env=env, stdout=subprocess.DEVNULL,
                                  stderr=subprocess.PIPE, text=True) as process:
                try:
                    def ready():
                        if process.poll() is not None:
                            raise RuntimeError(process.stderr.read())
                        return self.window()

                    window = self.wait_for(ready, "Afterbeat did not initialize X11 input protection.")
                    window.configure(x=480, y=80)
                    self.receiver.set_input_focus(X.RevertToPointerRoot, X.CurrentTime)
                    self.cursor(65, 65)
                    self.events(0.05)
                    self.buttons.clear()
                    self.keys.clear()
                    yield window, env
                    close = protocol.event.ClientMessage(
                        window=window,
                        client_type=self.display.intern_atom("WM_PROTOCOLS"),
                        data=(32, [self.display.intern_atom("WM_DELETE_WINDOW"), X.CurrentTime, 0, 0, 0]),
                    )
                    window.send_event(close)
                    self.display.flush()
                    assert process.wait(timeout=5) == 0, process.stderr.read()
                except BaseException:
                    process.kill()
                    process.wait(timeout=5)
                    log = config / "diagnostics.log"
                    if log.exists():
                        print(log.read_text(encoding="utf-8"), file=sys.stderr)
                    print(process.stderr.read(), file=sys.stderr)
                    raise

    def clicks(self, count):
        self.wait_for(lambda: len(self.buttons) >= count * 2, "Timed out waiting for clicks.")
        self.events(0.18)
        assert len(self.buttons) == count * 2, self.buttons
        assert [kind for kind, *_ in self.buttons] == [X.ButtonPress, X.ButtonRelease] * count


def smoke(executable):
    desktop = Desktop()
    with desktop.app(executable, limited=True, count="4") as (window, env):
        assert subprocess.run([executable], env=env, timeout=5).returncode == 0
        assert desktop.window().id == window.id, "A second instance took over the display."
        desktop.tap("F6")
        desktop.clicks(4)

    point = {"x": 90, "y": 90}
    with desktop.app(executable, limited=True, count="2", fixed=True, point=point, double=True):
        desktop.tap("F6")
        desktop.clicks(4)
        assert all(event[2:] == (90, 90) for event in desktop.buttons)

    route = [point, {"x": 150, "y": 120}]
    with desktop.app(executable, route_enabled=True, route_once=True, route=route,
                     process_mode=1, process_names=Path("/proc/self/exe").resolve().name):
        desktop.tap("F6")
        desktop.clicks(2)
        assert [event[2:] for event in desktop.buttons[::2]] == [(90, 90), (150, 120)]

    with desktop.app(executable, hold_down=True):
        desktop.key("F6", X.KeyPress)
        desktop.wait_for(lambda: desktop.buttons, "The held mouse button did not go down.")
        desktop.events(0.85)
        assert len(desktop.buttons) == 1, "Keyboard auto-repeat toggled the run."
        desktop.key("F6", X.KeyRelease)
        desktop.tap("F7")
        desktop.clicks(1)

    root = desktop.display.screen().root
    f7 = desktop.display.keysym_to_keycode(XK.string_to_keysym("F7"))
    root.grab_key(f7, 0, False, X.GrabModeAsync, X.GrabModeAsync)
    desktop.display.sync()
    try:
        with desktop.app(executable, hold_down=True):
            desktop.tap("F6")
            desktop.wait_for(lambda: desktop.buttons, "The held mouse button did not go down.")
            desktop.key("Control_L", X.KeyPress)
            desktop.key("Shift_L", X.KeyPress)
            desktop.tap("F7")
            desktop.key("Shift_L", X.KeyRelease)
            desktop.key("Control_L", X.KeyRelease)
            desktop.clicks(1)
    finally:
        root.ungrab_key(f7, 0)
        desktop.display.sync()

    with desktop.app(executable, hold_activation=True):
        desktop.key("F6", X.KeyPress)
        desktop.wait_for(lambda: len(desktop.buttons) >= 6, "Hold activation did not repeat.")
        desktop.key("F6", X.KeyRelease)
        desktop.events(0.08)
        count = len(desktop.buttons)
        desktop.events(0.15)
        assert len(desktop.buttons) == count, "Clicks continued after releasing activation."

    with desktop.app(executable, trigger="Mouse4", limited=True, count="2"):
        xtest.fake_input(desktop.display, X.ButtonPress, 8)
        xtest.fake_input(desktop.display, X.ButtonRelease, 8)
        desktop.display.sync()
        desktop.wait_for(lambda: len(desktop.buttons) >= 6, "Mouse4 activation did not deliver clicks.")
        desktop.events(0.18)
        assert [event[1] for event in desktop.buttons].count(8) == 2, desktop.buttons
        assert [event[1] for event in desktop.buttons].count(1) == 4, desktop.buttons

    with desktop.app(executable, keyboard=True, key="A", limited=True, count="3") as (window, _):
        window.set_input_focus(X.RevertToPointerRoot, X.CurrentTime)
        desktop.display.sync()
        desktop.tap("F6")
        desktop.events(0.15)
        assert (X.KeyPress, ord("A")) not in desktop.keys
        desktop.receiver.set_input_focus(X.RevertToPointerRoot, X.CurrentTime)
        desktop.display.sync()
        desktop.wait_for(lambda: desktop.keys.count((X.KeyPress, ord("A"))) >= 3, "Uppercase key output was not delivered.")
        desktop.events(0.18)
        assert desktop.keys.count((X.KeyPress, ord("A"))) == 3, desktop.keys

    with desktop.app(executable, limited=True, count="3") as (window, _):
        # An independent X11 child detects input that escapes protection of Afterbeat's controls.
        probe = window.create_window(
            12, 12, 40, 40, 0, X.CopyFromParent,
            event_mask=X.ButtonPressMask | X.ButtonReleaseMask,
        )
        probe.map()
        desktop.cursor(505, 105)
        desktop.tap("F6")
        desktop.events(0.25)
        desktop.cursor(65, 65)
        desktop.events(0.18)
        assert not desktop.buttons, "Afterbeat sent mouse events to its own window."
        probe.destroy()

    text_symbol = 0x01000000 | ord("🙂")
    with desktop.app(executable, keyboard=True, text_output=True, text_character="🙂", limited=True, count="1"):
        assert not desktop.display.keysym_to_keycode(text_symbol)
        desktop.tap("F6")
        desktop.wait_for(lambda: (X.KeyPress, text_symbol) in desktop.keys, "Unicode text was not delivered.")
        desktop.wait_for(lambda: not desktop.display.keysym_to_keycode(text_symbol), "The temporary key mapping was not restored.")

    with desktop.app(executable, keyboard=True, text_output=True, text_character="a", trigger="a", limited=True, count="3"):
        desktop.tap("a")
        desktop.wait_for(lambda: desktop.keys.count((X.KeyPress, ord("a"))) >= 3, "Text output triggered its activation shortcut.")
        desktop.events(0.18)
        assert desktop.keys.count((X.KeyPress, ord("a"))) == 3, desktop.keys

    with desktop.app(executable, process_mode=1, process_names="no-such-afterbeat-test-app"):
        desktop.tap("F6")
        desktop.events(0.2)
        assert not desktop.buttons, "The app filter allowed clicks to an unlisted process."
        desktop.tap("F7")

    print("X11 delivery smoke passed")


if __name__ == "__main__":
    smoke(sys.argv[1])
