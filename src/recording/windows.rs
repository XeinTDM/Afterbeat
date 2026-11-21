//! Windows low-level input capture. The hooks observe input and never consume it.
use super::{CaptureOptions, Entry, Event, Recorder, RecordingStop};
use crate::mouse::{Button, Point};
use std::{
    cell::RefCell,
    io,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    System::{
        LibraryLoader::GetModuleHandleW,
        SystemInformation::GetTickCount64,
        Threading::{GetCurrentProcessId, GetCurrentThreadId},
    },
    UI::WindowsAndMessaging::{
        CallNextHookEx, GetForegroundWindow, GetMessageW, GetWindowThreadProcessId, HC_ACTION,
        KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT,
        PM_NOREMOVE, PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx,
        WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
        WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_QUIT,
        WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
        WindowFromPoint, XBUTTON1, XBUTTON2,
    },
};

/// Stops capture rather than allowing an unattended session to grow without bound.
// Leave room for releases of every possible held key and mouse button.
const MAX_EVENTS: usize = crate::settings::MAX_RECORDING_EVENTS - 515;

struct Capture {
    started: Instant,
    recorder: Option<Recorder>,
    reserved_keys: Vec<u32>,
    reserved_buttons: Vec<Button>,
    limit_reached: bool,
    stopped: bool,
    resume_cutoff: Option<u64>,
}

impl Capture {
    fn record(&mut self, event: Event, now: Duration) {
        let Some(recorder) = &mut self.recorder else {
            return;
        };
        if self.limit_reached || self.stopped {
            return;
        }
        recorder.record(event, now);
        if recorder.entries().len() >= MAX_EVENTS {
            recorder.pause(now);
            self.limit_reached = true;
        }
    }

    fn mouse(&mut self, message: u32, input: &MSLLHOOKSTRUCT, own: bool, now: Duration) {
        if input.flags & LLMHF_INJECTED != 0 || self.stale(input.time) {
            return;
        }
        if own {
            self.exclude(Recorder::exclude_buttons, now);
        } else if let Some(event) = mouse_event(message, input) {
            if matches!(&event, Event::Button { button, .. } if self.reserved_buttons.contains(button))
            {
                return;
            }
            self.record(event, now);
        }
    }

    fn keyboard(&mut self, message: u32, input: &KBDLLHOOKSTRUCT, own: bool, now: Duration) {
        if input.flags & LLKHF_INJECTED != 0 || self.stale(input.time) {
            return;
        }
        if own {
            self.exclude(Recorder::exclude_keys, now);
        } else if !self.reserved_keys.contains(&input.vkCode)
            && let Some(event) = keyboard_event(message, input)
        {
            self.record(event, now);
        }
    }

    fn stale(&self, event_time: u32) -> bool {
        self.resume_cutoff.is_some_and(|cutoff| {
            event_at_or_before(event_time, cutoff, unsafe { GetTickCount64() })
        })
    }

    fn exclude(&mut self, what: impl FnOnce(&mut Recorder, Duration), now: Duration) {
        if let Some(recorder) = &mut self.recorder {
            what(recorder, now);
            if recorder.entries().len() >= MAX_EVENTS {
                recorder.pause(now);
                self.limit_reached = true;
            }
        }
    }
}

fn event_at_or_before(event_time: u32, cutoff: u64, now: u64) -> bool {
    // Extend the 32-bit hook timestamp relative to the current 64-bit tick
    // count. Queued events are much younger than one 49-day wrap.
    let age = u64::from((now as u32).wrapping_sub(event_time));
    now.saturating_sub(age) <= cutoff
}

fn hook_elapsed(elapsed: Duration, now_tick: u64, event_time: u32) -> Duration {
    let age = u64::from((now_tick as u32).wrapping_sub(event_time));
    elapsed.saturating_sub(Duration::from_millis(age))
}

thread_local! {
    static CAPTURE: RefCell<Option<Arc<Mutex<Capture>>>> = const { RefCell::new(None) };
}

pub struct Service {
    shared: Arc<Mutex<Capture>>,
    closing: Arc<AtomicBool>,
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
}

impl Service {
    pub fn start(
        options: CaptureOptions,
        reserved_keys: Vec<u32>,
        reserved_buttons: Vec<Button>,
        stop_requested: Arc<RecordingStop>,
    ) -> io::Result<Self> {
        if !options.movement && !options.buttons && !options.keys {
            return Err(io::Error::other(
                "Choose at least one input type to record.",
            ));
        }
        let shared = Arc::new(Mutex::new(Capture {
            started: Instant::now(),
            recorder: Some(Recorder::with_stop_signal(
                options,
                Duration::ZERO,
                stop_requested,
            )),
            reserved_keys,
            reserved_buttons,
            limit_reached: false,
            stopped: false,
            resume_cutoff: None,
        }));
        let worker_shared = Arc::clone(&shared);
        let worker_state = Arc::clone(&shared);
        let closing = Arc::new(AtomicBool::new(false));
        let worker_closing = Arc::clone(&closing);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("macro-capture".into())
            .spawn(move || {
                // Create the message queue before publishing its thread ID.
                let mut msg = MSG::default();
                unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_NOREMOVE) };
                CAPTURE.with(|slot| *slot.borrow_mut() = Some(worker_shared));
                let module = unsafe { GetModuleHandleW(null()) };
                let mouse_needed = options.movement || options.buttons;
                let mouse = if mouse_needed {
                    unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0) }
                } else {
                    null_mut()
                };
                let keyboard = if options.keys {
                    unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0) }
                } else {
                    null_mut()
                };
                if (mouse_needed && mouse.is_null()) || (options.keys && keyboard.is_null()) {
                    let error = io::Error::last_os_error();
                    if !mouse.is_null() {
                        unsafe { UnhookWindowsHookEx(mouse) };
                    }
                    if !keyboard.is_null() {
                        unsafe { UnhookWindowsHookEx(keyboard) };
                    }
                    let _ = ready_tx.send(Err(error));
                    return;
                }
                let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));
                while !worker_closing.load(Ordering::Acquire)
                    && unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0
                {}
                if !mouse.is_null() {
                    unsafe { UnhookWindowsHookEx(mouse) };
                }
                if !keyboard.is_null() {
                    unsafe { UnhookWindowsHookEx(keyboard) };
                }
                CAPTURE.with(|slot| *slot.borrow_mut() = None);
                if let Ok(mut capture) = worker_state.lock() {
                    let now = capture.started.elapsed();
                    if let Some(recorder) = &mut capture.recorder {
                        recorder.pause(now);
                    }
                    capture.stopped = true;
                }
            })?;
        let thread_id = ready_rx.recv().map_err(io::Error::other)??;
        Ok(Self {
            shared,
            closing,
            thread_id,
            worker: Some(worker),
        })
    }

    pub fn pause(&self) {
        let mut capture = self.shared.lock().unwrap();
        let now = capture.started.elapsed();
        if let Some(recorder) = &mut capture.recorder {
            recorder.pause(now);
        }
    }

    pub fn resume(&self) -> io::Result<()> {
        let mut capture = self.shared.lock().unwrap();
        if capture.limit_reached
            || capture.stopped
            || capture
                .recorder
                .as_ref()
                .is_none_or(Recorder::stop_requested)
        {
            return Err(io::Error::other("The recording has stopped."));
        }
        let now = capture.started.elapsed();
        if let Some(recorder) = &mut capture.recorder {
            recorder.resume(now);
            // Low-level hook events can already be queued when the UI resumes.
            // Drop events from the paused period, including the boundary tick.
            capture.resume_cutoff = Some(unsafe { GetTickCount64() });
        }
        Ok(())
    }

    pub fn count(&self) -> (usize, bool, bool) {
        let capture = self.shared.lock().unwrap();
        (
            capture.recorder.as_ref().map_or(0, |r| r.entries().len()),
            capture.limit_reached,
            capture.stopped,
        )
    }

    pub fn finish(mut self) -> Vec<Entry> {
        let entries = {
            let mut capture = self.shared.lock().unwrap();
            let now = capture.started.elapsed();
            capture
                .recorder
                .take()
                .map_or_else(Vec::new, |recorder| recorder.finish(now))
        };
        self.shutdown();
        entries
    }

    fn shutdown(&mut self) {
        self.closing.store(true, Ordering::Release);
        // Disable collection even if Windows has already destroyed the message queue.
        if let Ok(mut capture) = self.shared.lock() {
            capture.recorder = None;
        }
        if let Some(worker) = self.worker.take() {
            let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) } != 0;
            if posted || worker.is_finished() {
                let _ = worker.join();
            }
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn own_window(window: windows_sys::Win32::Foundation::HWND) -> bool {
    if window.is_null() {
        return true;
    }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, &mut pid) };
    pid == 0 || pid == unsafe { GetCurrentProcessId() }
}

fn with_capture(f: impl FnOnce(&mut Capture, Duration)) {
    CAPTURE.with(|slot| {
        if let Some(shared) = slot.borrow().as_ref()
            && let Ok(mut capture) = shared.lock()
        {
            let now = capture.started.elapsed();
            f(&mut capture, now);
        }
    });
}

fn mouse_event(message: u32, input: &MSLLHOOKSTRUCT) -> Option<Event> {
    if input.flags & LLMHF_INJECTED != 0 {
        return None;
    }
    let point = Point {
        x: input.pt.x,
        y: input.pt.y,
    };
    if matches!(message, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) {
        let delta = i32::from((input.mouseData >> 16) as u16 as i16);
        return (delta != 0).then_some(Event::Scroll {
            point,
            horizontal: message == WM_MOUSEHWHEEL,
            delta,
        });
    }
    let button = match message {
        WM_LBUTTONDOWN => Some((Button::Left, true)),
        WM_LBUTTONUP => Some((Button::Left, false)),
        WM_RBUTTONDOWN => Some((Button::Right, true)),
        WM_RBUTTONUP => Some((Button::Right, false)),
        WM_MBUTTONDOWN => Some((Button::Middle, true)),
        WM_MBUTTONUP => Some((Button::Middle, false)),
        WM_XBUTTONDOWN | WM_XBUTTONUP => match (input.mouseData >> 16) as u16 {
            XBUTTON1 => Some((Button::Mouse4, message == WM_XBUTTONDOWN)),
            XBUTTON2 => Some((Button::Mouse5, message == WM_XBUTTONDOWN)),
            _ => None,
        },
        _ => None,
    };
    if let Some((button, down)) = button {
        Some(Event::Button {
            button,
            down,
            point,
        })
    } else if message == WM_MOUSEMOVE {
        Some(Event::Move(point))
    } else {
        None
    }
}

fn keyboard_event(message: u32, input: &KBDLLHOOKSTRUCT) -> Option<Event> {
    if input.flags & LLKHF_INJECTED != 0 || !(1..=255).contains(&input.vkCode) {
        return None;
    }
    let down = match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => true,
        WM_KEYUP | WM_SYSKEYUP => false,
        _ => return None,
    };
    Some(Event::Key {
        code: input.vkCode,
        extended: input.flags & LLKHF_EXTENDED != 0,
        down,
    })
}

unsafe extern "system" fn mouse_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let input = unsafe { &*(data as *const MSLLHOOKSTRUCT) };
        let own =
            input.flags & LLMHF_INJECTED == 0 && own_window(unsafe { WindowFromPoint(input.pt) });
        with_capture(|capture, now| {
            let at = hook_elapsed(now, unsafe { GetTickCount64() }, input.time);
            capture.mouse(message as u32, input, own, at);
        });
    }
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

unsafe extern "system" fn keyboard_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let input = unsafe { &*(data as *const KBDLLHOOKSTRUCT) };
        let own = input.flags & LLKHF_INJECTED == 0 && own_window(unsafe { GetForegroundWindow() });
        with_capture(|capture, now| {
            let at = hook_elapsed(now, unsafe { GetTickCount64() }, input.time);
            capture.keyboard(message as u32, input, own, at);
        });
    }
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(keys: bool) -> Capture {
        Capture {
            started: Instant::now(),
            recorder: Some(Recorder::new(
                CaptureOptions {
                    keys,
                    ..CaptureOptions::default()
                },
                Duration::ZERO,
            )),
            reserved_keys: vec![0x76],
            reserved_buttons: vec![Button::Mouse4],
            limit_reached: false,
            stopped: false,
            resume_cutoff: None,
        }
    }

    #[test]
    fn translates_physical_edges_and_excludes_injected_events() {
        assert!(own_window(null_mut()), "An unknown target must be excluded");
        let mut mouse = MSLLHOOKSTRUCT::default();
        mouse.pt.x = -20;
        mouse.pt.y = 40;
        mouse.mouseData = u32::from(XBUTTON2) << 16;
        assert_eq!(
            mouse_event(WM_XBUTTONDOWN, &mouse),
            Some(Event::Button {
                button: Button::Mouse5,
                down: true,
                point: Point { x: -20, y: 40 }
            })
        );
        assert_eq!(
            mouse_event(WM_MOUSEMOVE, &mouse),
            Some(Event::Move(Point { x: -20, y: 40 }))
        );
        mouse.mouseData = (u32::from((-120i16) as u16)) << 16;
        assert_eq!(
            mouse_event(WM_MOUSEWHEEL, &mouse),
            Some(Event::Scroll {
                point: Point { x: -20, y: 40 },
                horizontal: false,
                delta: -120
            })
        );
        mouse.flags = LLMHF_INJECTED;
        assert_eq!(mouse_event(WM_MOUSEMOVE, &mouse), None);
        let mut key = KBDLLHOOKSTRUCT {
            vkCode: 65,
            flags: LLKHF_EXTENDED,
            ..Default::default()
        };
        assert_eq!(
            keyboard_event(WM_SYSKEYUP, &key),
            Some(Event::Key {
                code: 65,
                extended: true,
                down: false
            })
        );
        key.flags |= LLKHF_INJECTED;
        assert_eq!(keyboard_event(WM_KEYDOWN, &key), None);
        key.flags = 0;
        key.vkCode = 0;
        assert_eq!(keyboard_event(WM_KEYDOWN, &key), None);
    }

    #[test]
    fn capture_filters_private_controls_and_balances_pauses() {
        let mut capture = capture(true);
        let mut mouse = MSLLHOOKSTRUCT::default();
        mouse.pt.x = 10;
        mouse.pt.y = 20;
        capture.mouse(WM_LBUTTONDOWN, &mouse, false, Duration::from_millis(5));
        mouse.pt.x = 30;
        capture.mouse(WM_MOUSEMOVE, &mouse, false, Duration::from_millis(10));
        // Entering Afterbeat closes the drag at the last captured position.
        capture.mouse(WM_MOUSEMOVE, &mouse, true, Duration::from_millis(15));
        let mut key = KBDLLHOOKSTRUCT {
            vkCode: 65,
            ..Default::default()
        };
        capture.keyboard(WM_KEYDOWN, &key, false, Duration::from_millis(20));
        capture
            .recorder
            .as_mut()
            .unwrap()
            .pause(Duration::from_millis(25));
        key.vkCode = 66;
        capture.keyboard(WM_KEYDOWN, &key, false, Duration::from_millis(30));
        capture
            .recorder
            .as_mut()
            .unwrap()
            .resume(Duration::from_millis(100));
        // The stop shortcut and injected input never enter the timeline.
        key.vkCode = 0x76;
        capture.keyboard(WM_KEYDOWN, &key, false, Duration::from_millis(105));
        key.vkCode = 67;
        key.flags = LLKHF_INJECTED;
        capture.keyboard(WM_KEYDOWN, &key, false, Duration::from_millis(110));
        mouse.mouseData = u32::from(XBUTTON1) << 16;
        capture.mouse(WM_XBUTTONDOWN, &mouse, false, Duration::from_millis(112));
        mouse.mouseData = 0;
        capture.mouse(WM_MOUSEMOVE, &mouse, false, Duration::from_millis(115));
        let entries = capture
            .recorder
            .take()
            .unwrap()
            .finish(Duration::from_millis(120));
        assert_eq!(entries.len(), 6);
        assert_eq!(
            entries[2].event,
            Event::Button {
                button: Button::Left,
                down: false,
                point: Point { x: 30, y: 20 }
            }
        );
        assert_eq!(
            entries[4].event,
            Event::Key {
                code: 65,
                extended: false,
                down: false
            }
        );
        assert_eq!(entries[5].delay_ms, 15);
        assert_eq!(super::super::validate(&entries), Ok(()));
    }

    #[test]
    fn resume_excludes_queued_events_at_and_before_the_boundary() {
        let mut capture = capture(true);
        let cutoff = unsafe { GetTickCount64() };
        capture.resume_cutoff = Some(cutoff);
        let mut key = KBDLLHOOKSTRUCT {
            vkCode: 65,
            time: (cutoff as u32).wrapping_sub(1),
            ..Default::default()
        };
        capture.keyboard(WM_KEYDOWN, &key, false, Duration::from_millis(1));
        key.time = cutoff as u32;
        capture.keyboard(WM_KEYDOWN, &key, false, Duration::from_millis(2));
        assert!(capture.recorder.as_ref().unwrap().entries().is_empty());
        assert!(!event_at_or_before(101, 100, 101));
        let wrap = u64::from(u32::MAX);
        assert!(!event_at_or_before(1, wrap - 1, wrap + 2));
        assert!(event_at_or_before(u32::MAX - 2, wrap - 1, wrap + 2));
    }

    #[test]
    fn queued_hook_events_keep_their_original_timing_across_tick_wrap() {
        let elapsed = Duration::from_millis(80);
        assert_eq!(hook_elapsed(elapsed, 1_000, 975), Duration::from_millis(55));
        let now = u64::from(u32::MAX) + 8;
        assert_eq!(
            hook_elapsed(elapsed, now, u32::MAX - 1),
            Duration::from_millis(71)
        );
        assert_eq!(
            hook_elapsed(Duration::from_millis(5), 1_000, 975),
            Duration::ZERO
        );
    }

    #[test]
    fn event_limit_closes_holds_and_stops_collecting() {
        let mut capture = capture(true);
        let key = KBDLLHOOKSTRUCT {
            vkCode: 65,
            ..Default::default()
        };
        capture.keyboard(WM_KEYDOWN, &key, false, Duration::ZERO);
        let mouse = MSLLHOOKSTRUCT::default();
        for n in 1..MAX_EVENTS {
            capture.mouse(WM_MOUSEMOVE, &mouse, false, Duration::from_millis(n as u64));
        }
        assert!(capture.limit_reached);
        let before = capture.recorder.as_ref().unwrap().entries().len();
        capture.keyboard(WM_KEYDOWN, &key, false, Duration::from_secs(20));
        let entries = capture
            .recorder
            .take()
            .unwrap()
            .finish(Duration::from_secs(21));
        assert_eq!(entries.len(), before);
        assert!(entries.len() <= crate::settings::MAX_RECORDING_EVENTS);
        assert_eq!(super::super::validate(&entries), Ok(()));
    }

    #[test]
    fn stopped_listener_does_not_accept_more_input() {
        let mut capture = capture(true);
        capture.stopped = true;
        let key = KBDLLHOOKSTRUCT {
            vkCode: 65,
            ..Default::default()
        };
        capture.keyboard(WM_KEYDOWN, &key, false, Duration::from_millis(1));
        let mouse = MSLLHOOKSTRUCT::default();
        capture.mouse(WM_MOUSEMOVE, &mouse, false, Duration::from_millis(2));
        assert!(capture.recorder.as_ref().unwrap().entries().is_empty());
    }
}
