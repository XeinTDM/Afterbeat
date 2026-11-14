use crate::{App, hotkeys::event_mask, macos};
use objc2_app_kit::NSView;
use objc2_core_foundation::{CFMachPort, CFRetained, CGPoint};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, Timer};
use std::{
    collections::HashMap,
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{self, NonNull},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

static READY: AtomicBool = AtomicBool::new(false);
type PadHits = HashMap<(i64, i32, i32), u32>;
static HITS: Mutex<Option<PadHits>> = Mutex::new(None);

pub fn set_ready(ready: bool) {
    READY.store(ready, Ordering::Release);
}

pub fn ready() -> bool {
    READY.load(Ordering::Acquire)
}

pub fn clear_hits() {
    if let Some(hits) = HITS.lock().unwrap().as_mut() {
        hits.clear();
    }
}

pub fn tap() -> Option<CFRetained<CFMachPort>> {
    let mask = event_mask(&[
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::OtherMouseDragged,
        CGEventType::ScrollWheel,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
    ]);
    // The process tap discards our input before AppKit receives it, including
    // while a native menu or window control is tracking events.
    unsafe {
        CGEvent::tap_create_for_pid(
            macos::own_pid(),
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask,
            Some(filter_event),
            ptr::null_mut(),
        )
    }
}

unsafe extern "C-unwind" fn filter_event(
    _: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    _: *mut c_void,
) -> *mut CGEvent {
    if matches!(
        kind,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        set_ready(false);
        return event.as_ptr();
    }
    let event_ref = unsafe { event.as_ref() };
    if !macos::is_ours(event_ref) {
        return event.as_ptr();
    }
    if matches!(
        kind,
        CGEventType::LeftMouseDown | CGEventType::RightMouseDown | CGEventType::OtherMouseDown
    ) && catch_unwind(AssertUnwindSafe(|| count_pad(event_ref))).is_err()
    {
        set_ready(false);
    }
    ptr::null_mut()
}

fn count_pad(event: &CGEvent) {
    let point = CGEvent::unflipped_location(Some(event));
    let window = CGEvent::integer_value_field(
        Some(event),
        CGEventField::MouseEventWindowUnderMousePointerThatCanHandleThisEvent,
    );
    let location = (window, point.x.floor() as i32, point.y.floor() as i32);
    let mut queue = HITS.lock().unwrap();
    let schedule = queue.is_none();
    let hits = queue.get_or_insert_with(HashMap::new);
    if hits.len() < 4096 || hits.contains_key(&location) {
        let count = hits.entry(location).or_default();
        *count = count.saturating_add(1);
    }
    if schedule
        && slint::invoke_from_event_loop(|| {
            Timer::single_shot(Duration::from_millis(32), flush_pad);
        })
        .is_err()
    {
        *queue = None;
    }
}

fn flush_pad() {
    let hits = HITS.lock().unwrap().take().unwrap_or_default();
    let app: Option<App> = crate::ACTIVE_CONTROLLER
        .with(|local| local.borrow().upgrade())
        .and_then(|controller| controller.app.upgrade());
    let Some(app) = app else { return };
    let handle = app.window().window_handle();
    let Ok(handle) = handle.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    // Slint owns this NSView. Its window handle remains borrowed on the main thread.
    let view: &NSView = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let Some(window) = view.window() else { return };
    let bounds = view.bounds();
    let flipped = view.isFlipped();
    let count = hits
        .into_iter()
        .filter(|((window_number, x, y), _)| {
            if *window_number > 0 && *window_number != window.windowNumber() as i64 {
                return false;
            }
            let base = window.convertPointFromScreen(CGPoint::new(f64::from(*x), f64::from(*y)));
            let point = view.convertPoint_fromView(base, None);
            let x = point.x - bounds.origin.x;
            let y = point.y - bounds.origin.y;
            let y = if flipped { y } else { bounds.size.height - y };
            app.invoke_test_pad_at(x as f32, y as f32)
        })
        .map(|(_, count)| u64::from(count))
        .sum::<u64>();
    if count != 0 {
        app.invoke_test_hits(count.min(i32::MAX as u64) as i32);
    }
}
