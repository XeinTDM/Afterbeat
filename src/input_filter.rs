use crate::App;
use slint::{ComponentHandle, Weak};
use std::{
    cell::{Cell, RefCell},
    io,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};

pub const INPUT_TAG: usize = 0x5043_4C4B;
const SUBCLASS_ID: usize = 1;

thread_local! {
    static APP: RefCell<Option<Weak<App>>> = const { RefCell::new(None) };
    static HITS: Cell<u64> = const { Cell::new(0) };
}

pub struct InputFilter(HWND);

impl InputFilter {
    pub fn attach(app: &App, hwnd: HWND) -> io::Result<Self> {
        if unsafe { SetWindowSubclass(hwnd, Some(filter), SUBCLASS_ID, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        APP.with(|slot| *slot.borrow_mut() = Some(app.as_weak()));
        Ok(Self(hwnd))
    }
}

impl Drop for InputFilter {
    fn drop(&mut self) {
        unsafe { RemoveWindowSubclass(self.0, Some(filter), SUBCLASS_ID) };
        APP.with(|slot| *slot.borrow_mut() = None);
        HITS.with(|hits| hits.set(0));
    }
}

fn disposition(message: u32, tagged: bool) -> (bool, bool) {
    if !tagged {
        return (false, false);
    }
    let press = matches!(
        message,
        WM_LBUTTONDOWN
            | WM_LBUTTONDBLCLK
            | WM_RBUTTONDOWN
            | WM_RBUTTONDBLCLK
            | WM_MBUTTONDOWN
            | WM_MBUTTONDBLCLK
            | WM_XBUTTONDOWN
            | WM_XBUTTONDBLCLK
    );
    let block = press
        || matches!(message,
            WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP |
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL |
            WM_NCLBUTTONDOWN..=WM_NCMBUTTONDBLCLK | WM_NCXBUTTONDOWN..=WM_NCXBUTTONDBLCLK |
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP |
            WM_CHAR | WM_SYSCHAR | WM_DEADCHAR | WM_SYSDEADCHAR
        );
    (block, press)
}

unsafe extern "system" fn filter(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_NCDESTROY {
        unsafe { RemoveWindowSubclass(hwnd, Some(filter), SUBCLASS_ID) };
    }
    let (block, press) = disposition(
        message,
        unsafe { GetMessageExtraInfo() } as usize == INPUT_TAG,
    );
    if !block {
        return unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    }
    // Never forward a tagged button or key message to application controls.
    if press {
        let _ = std::panic::catch_unwind(|| record_pad_hit(lparam));
    }
    0
}

fn app() -> Option<App> {
    APP.try_with(|slot| {
        slot.try_borrow()
            .ok()
            .and_then(|slot| slot.as_ref()?.upgrade())
    })
    .ok()
    .flatten()
}

pub fn clear_hits() {
    HITS.with(|hits| hits.set(0));
}

fn record_pad_hit(lparam: LPARAM) {
    let Some(ui) = app() else {
        return;
    };
    let scale = ui.window().scale_factor();
    let x = f32::from(lparam as i16) / scale;
    let y = f32::from((lparam >> 16) as i16) / scale;
    if !ui.invoke_test_pad_at(x, y) {
        return;
    }
    HITS.with(|hits| {
        let count = hits.get();
        hits.set(count.saturating_add(1));
        if count == 0 {
            slint::Timer::single_shot(std::time::Duration::from_millis(32), || {
                let hits = HITS.with(|hits| hits.replace(0));
                if let Some(ui) = app() {
                    ui.invoke_test_hits(hits.min(i32::MAX as u64) as i32);
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr::null_mut;

    #[test]
    fn only_tagged_buttons_count_and_tagged_keys_are_filtered() {
        assert_eq!(disposition(WM_LBUTTONDOWN, false), (false, false));
        assert_eq!(disposition(WM_LBUTTONDBLCLK, true), (true, true));
        assert_eq!(disposition(WM_NCLBUTTONDOWN, true), (true, false));
        assert_eq!(disposition(WM_KEYDOWN, true), (true, false));
        assert_eq!(disposition(WM_MOUSEMOVE, true), (false, false));
    }

    #[test]
    fn native_subclass_blocks_tagged_input_on_an_isolated_window() {
        unsafe extern "system" fn probe(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
            _id: usize,
            _data: usize,
        ) -> LRESULT {
            if msg == WM_LBUTTONDOWN {
                return 123;
            }
            unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
        }

        let class: Vec<u16> = "STATIC".encode_utf16().chain([0]).collect();
        let window = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                WS_POPUP,
                0,
                0,
                10,
                10,
                null_mut(),
                null_mut(),
                null_mut(),
                std::ptr::null(),
            )
        };
        assert!(!window.is_null());
        assert_ne!(unsafe { SetWindowSubclass(window, Some(probe), 2, 0) }, 0);
        assert_ne!(
            unsafe { SetWindowSubclass(window, Some(filter), SUBCLASS_ID, 0) },
            0
        );
        let original = unsafe { SetMessageExtraInfo(0) };
        let physical = unsafe { SendMessageW(window, WM_LBUTTONDOWN, 0, 0) };
        unsafe { SetMessageExtraInfo(INPUT_TAG as LPARAM) };
        let synthetic = unsafe { SendMessageW(window, WM_LBUTTONDOWN, 0, 0) };
        unsafe {
            SetMessageExtraInfo(original);
            DestroyWindow(window);
        }
        assert_eq!((physical, synthetic), (123, 0));
    }
}
