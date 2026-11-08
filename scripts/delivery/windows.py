"""Win32 process isolation and receiver for Afterbeat delivery tests."""

import ctypes as c
from ctypes import wintypes as w
from contextlib import contextmanager
from threading import Event, Thread
import time
import uuid

import pywintypes
import win32api as api
import win32con as con
import win32event as event
import win32gui as gui
import win32job as job
import win32process as process
import win32service as desktop

TAG = 0x50434C4B
DOWN = con.WM_LBUTTONDOWN
UP = con.WM_LBUTTONUP
USER32 = c.WinDLL("user32", use_last_error=True)
USER32.GetMessageExtraInfo.restype = c.c_ssize_t
USER32.GetUserObjectInformationW.argtypes = (
    w.HANDLE, c.c_int, c.c_void_p, w.DWORD, c.POINTER(w.DWORD),
)
USER32.RegisterHotKey.argtypes = (w.HWND, c.c_int, w.UINT, w.UINT)
USER32.UnregisterHotKey.argtypes = (w.HWND, c.c_int)


def active_desktop():
    handle = desktop.OpenInputDesktop(0, False, 1)
    try:
        name = c.create_unicode_buffer(256)
        needed = w.DWORD()
        if not USER32.GetUserObjectInformationW(int(handle), 2, name, c.sizeof(name), c.byref(needed)):
            raise c.WinError(c.get_last_error())
        return name.value
    finally:
        handle.CloseDesktop()


def wait_for(predicate, timeout, reason):
    deadline = time.monotonic() + timeout
    while True:
        if value := predicate():
            return value
        if time.monotonic() >= deadline:
            raise TimeoutError(reason)
        time.sleep(0.01)


def require_closed_afterbeat():
    try:
        handle = event.OpenMutex(con.SYNCHRONIZE, False, r"Local\PulseClick.Desktop.SingleInstance")
    except pywintypes.error as error:
        if error.winerror == 2:
            return
        raise
    handle.Close()
    raise RuntimeError("Close the running Afterbeat instance before starting a delivery test.")


@contextmanager
def exclusive_session():
    handle = event.CreateMutex(None, False, r"Local\Afterbeat.DeliveryHarness")
    try:
        if api.GetLastError() == 183:
            raise RuntimeError("Another Afterbeat delivery test is already running.")
        require_closed_afterbeat()
        yield
    finally:
        handle.Close()


class ProcessJob:
    def __init__(self, affinity=None):
        self.handle = job.CreateJobObject(None, f"AfterbeatDeliveryJob-{uuid.uuid4().hex}")
        try:
            info = job.QueryInformationJobObject(self.handle, job.JobObjectExtendedLimitInformation)
            limits = info["BasicLimitInformation"]
            limits["LimitFlags"] = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            if affinity is not None:
                limits.update(LimitFlags=0x2010, Affinity=affinity)
            job.SetInformationJobObject(self.handle, job.JobObjectExtendedLimitInformation, info)
        except BaseException:
            self.handle.Close()
            raise

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.handle.Close()

    def terminate(self):
        job.TerminateJobObject(self.handle, 1)

    def launch(self, executable, command, *, env=None, cwd=None, desktop_name=None):
        startup = process.STARTUPINFO()
        if desktop_name:
            startup.lpDesktop = f"WinSta0\\{desktop_name}"
        handle, thread, pid, _ = process.CreateProcess(
            str(executable), command, None, None, False,
            con.CREATE_SUSPENDED | con.CREATE_NO_WINDOW, env, cwd, startup,
        )
        try:
            job.AssignProcessToJobObject(self.handle, handle)
            process.ResumeThread(thread)
        except BaseException:
            process.TerminateProcess(handle, 1)
            handle.Close()
            raise
        finally:
            thread.Close()
        return handle, pid

    def cpu_ns(self):
        info = job.QueryInformationJobObject(self.handle, job.JobObjectBasicAccountingInformation)
        return 100 * (info["TotalUserTime"] + info["TotalKernelTime"])

    def active_processes(self):
        return job.QueryInformationJobObject(self.handle, job.JobObjectBasicAccountingInformation)["ActiveProcesses"]


def window_for(pid, title=None, class_name=None):
    matches = []

    def collect(hwnd, _):
        _, owner = process.GetWindowThreadProcessId(hwnd)
        if owner == pid and gui.IsWindowVisible(hwnd):
            caption = gui.GetWindowText(hwnd)
            if caption and (title is None or caption == title) and (class_name is None or gui.GetClassName(hwnd) == class_name):
                matches.append(hwnd)
        return True

    gui.EnumWindows(collect, None)
    return matches[0] if matches else None


class Receiver:
    def __init__(self, desktop_name):
        self.desktop_name = desktop_name
        self.events = []
        self.hwnd = None
        self.error = None
        self.ready = Event()
        Thread(target=self.pump, daemon=True).start()
        if not self.ready.wait(5):
            raise TimeoutError("The Win32 receiver did not open.")
        if self.error:
            raise RuntimeError("The Win32 receiver failed.") from self.error

    def pump(self):
        def handle(hwnd, message, wparam, lparam):
            if message in (DOWN, UP):
                self.events.append((time.perf_counter_ns(), message, USER32.GetMessageExtraInfo()))
                return 0
            return gui.DefWindowProc(hwnd, message, wparam, lparam)

        try:
            cls = gui.WNDCLASS()
            cls.hInstance = api.GetModuleHandle(None)
            cls.lpszClassName = "AfterbeatDeliveryReceiver"
            cls.lpfnWndProc = handle
            cls.hbrBackground = con.COLOR_WINDOW + 1
            atom = gui.RegisterClass(cls)
            self.hwnd = gui.CreateWindowEx(
                con.WS_EX_TOPMOST, atom, "Afterbeat delivery test", con.WS_POPUP | con.WS_VISIBLE,
                0, 0, api.GetSystemMetrics(0), api.GetSystemMetrics(1), 0, 0, cls.hInstance, None,
            )
            gui.CreateWindowEx(
                0, "STATIC", "Afterbeat delivery test. The original desktop will return automatically.",
                con.WS_CHILD | con.WS_VISIBLE, 24, 24, 680, 28, self.hwnd, 0, cls.hInstance, None,
            )
            self.ready.set()
            gui.PumpMessages()
        except BaseException as error:
            self.error = error
            self.ready.set()

    def check_destination(self):
        if self.error:
            raise RuntimeError("The Win32 receiver stopped.") from self.error
        if active_desktop() != self.desktop_name:
            raise RuntimeError("The isolated desktop lost focus during a trial.")
        if gui.WindowFromPoint(api.GetCursorPos()) != self.hwnd:
            raise RuntimeError("The pointer left the isolated receiver.")

    def aim(self):
        if active_desktop() != self.desktop_name:
            raise RuntimeError("The isolated desktop is no longer active.")
        gui.SetWindowPos(self.hwnd, con.HWND_TOPMOST, 0, 0, 0, 0,
                         con.SWP_NOMOVE | con.SWP_NOSIZE | con.SWP_SHOWWINDOW)
        api.SetCursorPos((api.GetSystemMetrics(0) // 2, api.GetSystemMetrics(1) // 2))
        self.check_destination()


def emergency_binding():
    # Probe the same registration order as Afterbeat on the same desktop.
    identifier = 0x4142
    no_repeat = 0x4000
    for flags, label, modifiers in (
        (0, "F7", ()),
        (con.MOD_CONTROL | con.MOD_ALT, "Ctrl+Alt+F7", (con.VK_CONTROL, con.VK_MENU)),
    ):
        if USER32.RegisterHotKey(None, identifier, flags | no_repeat, con.VK_F7):
            if not USER32.UnregisterHotKey(None, identifier):
                raise c.WinError(c.get_last_error())
            return label, modifiers
    raise RuntimeError("Neither F7 nor Ctrl+Alt+F7 is available on the test desktop.")


def tap_key(key, *, modifiers=(), hold_seconds=0.075):
    pressed = []
    try:
        for modifier in modifiers:
            scan = api.MapVirtualKey(modifier, 0)
            api.keybd_event(modifier, scan, 0, 0)
            pressed.append((modifier, scan))
        scan = api.MapVirtualKey(key, 0)
        at = time.perf_counter_ns()
        api.keybd_event(key, scan, 0, 0)
        pressed.append((key, scan))
        time.sleep(hold_seconds)
    finally:
        for pressed_key, scan in reversed(pressed):
            api.keybd_event(pressed_key, scan, con.KEYEVENTF_KEYUP, 0)
    return at
