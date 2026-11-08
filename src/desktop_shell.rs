use crate::{App, StopSlot, diagnostics, signal_stop};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use std::{
    cell::{Cell, RefCell},
    io,
    ptr::null_mut,
    thread,
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::HWND as ComWindow,
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        UI::{
            Shell::{
                ITaskbarList3, THB_FLAGS, THB_ICON, THB_TOOLTIP, THBF_DISABLED,
                THBF_DISMISSONCLICK, THBF_HIDDEN, THBN_CLICKED, THUMBBUTTON, TaskbarList,
            },
            WindowsAndMessaging::HICON as ComIcon,
        },
    },
    core::{PCWSTR, w},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

const SUBCLASS_ID: usize = 3;
const STOP_BUTTON: u32 = 1;
const APP_ICON: usize = 1;
const RUNNING_ICON: usize = 2;
const STOP_ICON: usize = 3;
const WINDOW_MARKER: PCWSTR = w!("Afterbeat.Desktop.MainWindow");
const ACTIVATE_MESSAGE: PCWSTR = w!("Afterbeat.Desktop.Activate");

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Active,
    Stopping,
}

pub fn window_handle(app: &App) -> io::Result<HWND> {
    let native = app.window().window_handle();
    let handle = native.window_handle().map_err(io::Error::other)?;
    let RawWindowHandle::Win32(window) = handle.as_raw() else {
        return Err(io::Error::other("A Win32 window is required."));
    };
    Ok(window.hwnd.get() as HWND)
}

pub fn activate_existing() -> bool {
    let message = unsafe { RegisterWindowMessageW(ACTIVATE_MESSAGE.as_ptr()) };
    if message == 0 {
        return false;
    }
    // A second launch can reach the mutex while the first is still creating its window.
    for _ in 0..40 {
        let mut hwnd: HWND = null_mut();
        unsafe { EnumWindows(Some(find_main_window), (&mut hwnd as *mut HWND) as LPARAM) };
        if !hwnd.is_null() {
            let mut process = 0;
            unsafe {
                GetWindowThreadProcessId(hwnd, &mut process);
                if process != 0 {
                    AllowSetForegroundWindow(process);
                }
                if SendMessageTimeoutW(hwnd, message, 0, 0, SMTO_ABORTIFHUNG, 750, null_mut()) != 0
                {
                    return true;
                }
                if IsWindow(hwnd) != 0 {
                    if IsIconic(hwnd) != 0 {
                        ShowWindowAsync(hwnd, SW_RESTORE);
                    }
                    FlashWindow(hwnd, 1);
                    return true;
                }
            }
        }
        thread::sleep(Duration::from_millis(30));
    }
    false
}

unsafe extern "system" fn find_main_window(hwnd: HWND, result: LPARAM) -> i32 {
    if unsafe { GetPropW(hwnd, WINDOW_MARKER.as_ptr()) }.is_null() {
        return 1;
    }
    unsafe { *(result as *mut HWND) = hwnd };
    0
}

pub struct DesktopShell {
    window: Box<Window>,
    _apartment: Option<ComApartment>,
}

impl DesktopShell {
    pub fn attach(hwnd: HWND, stop: StopSlot) -> io::Result<Self> {
        let activate = unsafe { RegisterWindowMessageW(ACTIVATE_MESSAGE.as_ptr()) };
        let taskbar_created =
            unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated").as_ptr()) };
        if activate == 0 || taskbar_created == 0 {
            return Err(io::Error::last_os_error());
        }
        let apartment = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .is_ok()
            .then_some(ComApartment);
        let icons = Icons::load(hwnd)?;
        let window = Box::new(Window {
            hwnd,
            alive: Cell::new(true),
            stop,
            activate,
            taskbar_created,
            run: Cell::new(RunState::Idle),
            icons: RefCell::new(icons),
            taskbar: RefCell::new(None),
        });
        let shell = Self {
            window,
            _apartment: apartment,
        };
        unsafe {
            if SetWindowSubclass(
                hwnd,
                Some(window_proc),
                SUBCLASS_ID,
                (&raw const *shell.window) as usize,
            ) == 0
                || SetPropW(hwnd, WINDOW_MARKER.as_ptr(), 1usize as _) == 0
            {
                return Err(io::Error::last_os_error());
            }
            // Explorer can run at lower integrity than an elevated copy of Afterbeat.
            ChangeWindowMessageFilterEx(hwnd, taskbar_created, MSGFLT_ALLOW, null_mut());
            ChangeWindowMessageFilterEx(hwnd, activate, MSGFLT_ALLOW, null_mut());
        }
        shell.window.apply_icons();
        // The shell may have created the taskbar button before the window handle became available.
        shell.window.connect_taskbar();
        Ok(shell)
    }

    pub fn set_run_state(&self, state: RunState) {
        if !self.window.alive.get() || self.window.run.replace(state) == state {
            return;
        }
        self.window.update_taskbar();
    }
}

impl Drop for DesktopShell {
    fn drop(&mut self) {
        let window = &self.window;
        if window.alive.get() {
            unsafe {
                RemoveWindowSubclass(window.hwnd, Some(window_proc), SUBCLASS_ID);
                RemovePropW(window.hwnd, WINDOW_MARKER.as_ptr());
                SendMessageW(window.hwnd, WM_SETICON, ICON_SMALL as WPARAM, 0);
                SendMessageW(window.hwnd, WM_SETICON, ICON_BIG as WPARAM, 0);
            }
        }
    }
}

struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct Window {
    hwnd: HWND,
    alive: Cell<bool>,
    stop: StopSlot,
    activate: u32,
    taskbar_created: u32,
    run: Cell<RunState>,
    icons: RefCell<Icons>,
    taskbar: RefCell<Option<ITaskbarList3>>,
}

impl Window {
    fn apply_icons(&self) {
        let icons = self.icons.borrow();
        unsafe {
            SendMessageW(
                self.hwnd,
                WM_SETICON,
                ICON_SMALL as WPARAM,
                icons.small.0 as LPARAM,
            );
            SendMessageW(
                self.hwnd,
                WM_SETICON,
                ICON_BIG as WPARAM,
                icons.large.0 as LPARAM,
            );
        }
    }

    fn connect_taskbar(&self) {
        let Ok(mut taskbar) = self.taskbar.try_borrow_mut() else {
            return;
        };
        let Ok(api) = (unsafe {
            CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER)
        }) else {
            return;
        };
        if unsafe { api.HrInit() }.is_err() {
            return;
        }
        let Ok(icons) = self.icons.try_borrow() else {
            return;
        };
        let hwnd = ComWindow(self.hwnd);
        let button = [self.stop_button(&icons)];
        if unsafe { api.ThumbBarAddButtons(hwnd, &button) }.is_err()
            && unsafe { api.ThumbBarUpdateButtons(hwnd, &button) }.is_err()
        {
            return;
        }
        if let Err(error) = self.update_overlay(&api, &icons) {
            diagnostics::record(&format!("Windows taskbar badge: {error}"));
        }
        *taskbar = Some(api);
        diagnostics::record("Windows taskbar controls ready.");
    }

    fn update_taskbar(&self) {
        let Ok(taskbar) = self.taskbar.try_borrow() else {
            return;
        };
        let Some(api) = taskbar.as_ref() else {
            drop(taskbar);
            self.connect_taskbar();
            return;
        };
        let Ok(icons) = self.icons.try_borrow() else {
            return;
        };
        if let Err(error) =
            unsafe { api.ThumbBarUpdateButtons(ComWindow(self.hwnd), &[self.stop_button(&icons)]) }
        {
            diagnostics::record(&format!("Windows taskbar Stop button: {error}"));
        }
        if let Err(error) = self.update_overlay(api, &icons) {
            diagnostics::record(&format!("Windows taskbar badge: {error}"));
        }
    }

    fn update_overlay(&self, api: &ITaskbarList3, icons: &Icons) -> windows::core::Result<()> {
        let (icon, description) = if self.run.get() == RunState::Idle {
            (null_mut(), PCWSTR::null())
        } else {
            (icons.running.0, w!("Afterbeat automation is active"))
        };
        unsafe { api.SetOverlayIcon(ComWindow(self.hwnd), ComIcon(icon), description) }
    }

    fn stop_button(&self, icons: &Icons) -> THUMBBUTTON {
        let (label, flags) = match self.run.get() {
            RunState::Idle => ("Stop run", THBF_HIDDEN),
            RunState::Active => ("Stop run", THBF_DISMISSONCLICK),
            RunState::Stopping => ("Stopping run", THBF_DISABLED),
        };
        let mut button = THUMBBUTTON {
            dwMask: THB_FLAGS | THB_ICON | THB_TOOLTIP,
            iId: STOP_BUTTON,
            hIcon: ComIcon(icons.stop.0),
            dwFlags: flags,
            ..Default::default()
        };
        for (slot, code) in button.szTip.iter_mut().zip(label.encode_utf16()) {
            *slot = code;
        }
        button
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    state: usize,
) -> LRESULT {
    let window = unsafe { &*(state as *const Window) };
    if message == window.activate {
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            } else if IsWindowVisible(hwnd) == 0 {
                ShowWindow(hwnd, SW_SHOW);
            }
            if SetForegroundWindow(hwnd) == 0 {
                FlashWindow(hwnd, 1);
            }
        }
        return 1;
    }
    if message == window.taskbar_created {
        window.connect_taskbar();
    }
    if message == WM_COMMAND
        && wparam >> 16 == THBN_CLICKED as usize
        && wparam as u16 == STOP_BUTTON as u16
    {
        signal_stop(&window.stop);
        return 0;
    }
    if message == WM_NCDESTROY {
        window.alive.set(false);
        unsafe {
            RemovePropW(hwnd, WINDOW_MARKER.as_ptr());
            RemoveWindowSubclass(hwnd, Some(window_proc), SUBCLASS_ID);
        }
    }
    let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    if message == WM_DPICHANGED
        && window.alive.get()
        && let Ok(icons) = Icons::load(hwnd)
        && let Ok(mut current) = window.icons.try_borrow_mut()
    {
        let previous = std::mem::replace(&mut *current, icons);
        drop(current);
        window.apply_icons();
        window.update_taskbar();
        drop(previous);
    }
    result
}

struct Icons {
    small: Icon,
    large: Icon,
    running: Icon,
    stop: Icon,
}

impl Icons {
    fn load(hwnd: HWND) -> io::Result<Self> {
        let instance = unsafe { GetWindowLongPtrW(hwnd, GWLP_HINSTANCE) } as _;
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
        let small = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) };
        let large = unsafe { GetSystemMetricsForDpi(SM_CXICON, dpi) };
        let load = |id, size| {
            let handle =
                unsafe { LoadImageW(instance, id as *const u16, IMAGE_ICON, size, size, 0) };
            if handle.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(Icon(handle))
            }
        };
        Ok(Self {
            small: load(APP_ICON, small)?,
            large: load(APP_ICON, large)?,
            running: load(RUNNING_ICON, small)?,
            stop: load(STOP_ICON, small)?,
        })
    }
}

struct Icon(HICON);

impl Drop for Icon {
    fn drop(&mut self) {
        unsafe { DestroyIcon(self.0) };
    }
}
