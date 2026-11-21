//! Passive Quartz capture. Input is kept only in memory until the user saves it.
use super::{CaptureOptions, Entry, Event, Recorder, RecordingStop};
use crate::{
    hotkeys::Binding,
    keys, macos,
    mouse::{Button, Point},
};
use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use std::{
    ffi::c_void,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{self, NonNull},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_EVENTS: usize = crate::settings::MAX_RECORDING_EVENTS - 133;
const HARDWARE: CGEventSourceStateID = CGEventSourceStateID::HIDSystemState;

struct Capture {
    started: Instant,
    started_ns: u64,
    recorder: Option<Recorder>,
    binding: Option<Binding>,
    keys_enabled: bool,
    modifier_down: [bool; 128],
    resume_cutoff: Option<u64>,
    limit_reached: bool,
    secure_pause: bool,
    stopped: bool,
}

impl Capture {
    fn protect_secure_input(&mut self) {
        if macos::secure_input_enabled() && !self.secure_pause {
            let now = self.started.elapsed();
            if let Some(recorder) = &mut self.recorder {
                recorder.pause(now);
            }
            self.secure_pause = true;
        }
    }

    fn record(&mut self, event: Event, now: Duration) {
        if self.limit_reached || self.stopped {
            return;
        }
        if let Some(recorder) = &mut self.recorder {
            recorder.record(event, now);
            if recorder.entries().len() >= MAX_EVENTS {
                recorder.pause(now);
                self.limit_reached = true;
            }
        }
    }

    fn exclude(&mut self, now: Duration, action: impl FnOnce(&mut Recorder, Duration)) {
        if let Some(recorder) = &mut self.recorder {
            action(recorder, now);
        }
    }

    fn handle(&mut self, kind: CGEventType, event: &CGEvent) {
        if macos::is_ours(event) {
            return;
        }
        let timestamp = CGEvent::timestamp(Some(event));
        if self.resume_cutoff.is_some_and(|cutoff| timestamp <= cutoff) {
            return;
        }
        // Quartz timestamps describe when input occurred, even if the event
        // tap was delayed by a window lookup or another queued event.
        let elapsed = event_elapsed(timestamp, self.started_ns, self.started.elapsed());
        if macos::secure_input_enabled() {
            self.protect_secure_input();
            return;
        }
        let position = CGEvent::location(Some(event));
        if matches!(
            kind,
            CGEventType::MouseMoved
                | CGEventType::LeftMouseDragged
                | CGEventType::RightMouseDragged
                | CGEventType::OtherMouseDragged
                | CGEventType::LeftMouseDown
                | CGEventType::LeftMouseUp
                | CGEventType::RightMouseDown
                | CGEventType::RightMouseUp
                | CGEventType::OtherMouseDown
                | CGEventType::OtherMouseUp
                | CGEventType::ScrollWheel
        ) {
            let pid = macos::window_pid(position);
            if pid == 0 || pid == std::process::id() {
                self.exclude(elapsed, Recorder::exclude_buttons);
                return;
            }
            let point = Point {
                x: position.x.floor() as i32,
                y: position.y.floor() as i32,
            };
            if kind == CGEventType::ScrollWheel {
                for (horizontal, field) in [
                    (false, CGEventField::ScrollWheelEventDeltaAxis1),
                    (true, CGEventField::ScrollWheelEventDeltaAxis2),
                ] {
                    let lines =
                        CGEvent::integer_value_field(Some(event), field).clamp(-273, 273) as i32;
                    let delta = lines * 120 * if horizontal { -1 } else { 1 };
                    if delta != 0 {
                        self.record(
                            Event::Scroll {
                                point,
                                horizontal,
                                delta,
                            },
                            elapsed,
                        );
                    }
                }
                return;
            }
            let edge = match kind {
                CGEventType::LeftMouseDown => Some((Button::Left, true)),
                CGEventType::LeftMouseUp => Some((Button::Left, false)),
                CGEventType::RightMouseDown => Some((Button::Right, true)),
                CGEventType::RightMouseUp => Some((Button::Right, false)),
                CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
                    let button = match CGEvent::integer_value_field(
                        Some(event),
                        CGEventField::MouseEventButtonNumber,
                    ) {
                        2 => Some(Button::Middle),
                        3 => Some(Button::Mouse4),
                        4 => Some(Button::Mouse5),
                        _ => None,
                    };
                    button.map(|button| (button, kind == CGEventType::OtherMouseDown))
                }
                _ => None,
            };
            if let Some((button, down)) = edge {
                if !self
                    .binding
                    .is_some_and(|binding| binding.matches_button(button))
                {
                    self.record(
                        Event::Button {
                            button,
                            down,
                            point,
                        },
                        elapsed,
                    );
                }
            } else if matches!(
                kind,
                CGEventType::MouseMoved
                    | CGEventType::LeftMouseDragged
                    | CGEventType::RightMouseDragged
                    | CGEventType::OtherMouseDragged
            ) {
                self.record(Event::Move(point), elapsed);
            }
            return;
        }
        let code = CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
        let Ok(code) = u16::try_from(code) else {
            return;
        };
        if code > 127 {
            return;
        }
        self.key(
            kind,
            code,
            CGEvent::flags(Some(event)),
            macos::foreground_pid(),
            elapsed,
        );
    }

    fn key(
        &mut self,
        kind: CGEventType,
        code: u16,
        flags: CGEventFlags,
        pid: u32,
        elapsed: Duration,
    ) {
        if code == 57 && kind == CGEventType::FlagsChanged {
            let caps = flags.contains(CGEventFlags::MaskAlphaShift);
            if self.modifier_down[57] == caps {
                return;
            }
            self.modifier_down[57] = caps;
            if pid == 0 || pid == std::process::id() {
                self.exclude(elapsed, Recorder::exclude_keys);
            } else if !self.binding.is_some_and(|binding| binding.uses_code(code)) {
                for down in [true, false] {
                    self.record(
                        Event::Key {
                            code: u32::from(code),
                            extended: false,
                            down,
                        },
                        elapsed,
                    );
                }
            }
            return;
        }
        let down = match kind {
            CGEventType::KeyDown => true,
            CGEventType::KeyUp => false,
            CGEventType::FlagsChanged => modifier_transition(&mut self.modifier_down, code),
            _ => return,
        };
        if pid == 0 || pid == std::process::id() {
            self.exclude(elapsed, Recorder::exclude_keys);
        } else if code != keys::F7
            && code != keys::F8
            && !self.binding.is_some_and(|binding| binding.uses_code(code))
        {
            self.record(
                Event::Key {
                    code: u32::from(code),
                    extended: false,
                    down,
                },
                elapsed,
            );
        }
    }
}

fn modifier_transition(state: &mut [bool; 128], code: u16) -> bool {
    let down = &mut state[usize::from(code)];
    *down = !*down;
    *down
}

fn event_elapsed(timestamp: u64, started_ns: u64, processed: Duration) -> Duration {
    Duration::from_nanos(timestamp.saturating_sub(started_ns)).min(processed)
}

fn clock_ns() -> io::Result<u64> {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut now) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64)
}

fn modifiers() -> [bool; 128] {
    let mut state = [false; 128];
    for (code, down) in state.iter_mut().enumerate().take(64).skip(54) {
        *down = if code == 57 {
            CGEventSource::flags_state(HARDWARE).contains(CGEventFlags::MaskAlphaShift)
        } else {
            CGEventSource::key_state(HARDWARE, code as u16)
        };
    }
    state
}

fn resume_boundary(keys: bool) -> io::Result<([bool; 128], u64)> {
    if !keys {
        return Ok(([false; 128], clock_ns()?));
    }
    // Sample around the timestamp while the callback is locked. If a modifier
    // changes during sampling, retry so queued events are interpreted once.
    for _ in 0..8 {
        let before = modifiers();
        let cutoff = clock_ns()?;
        let after = modifiers();
        if before == after {
            return Ok((after, cutoff));
        }
    }
    Err(io::Error::other("Release modifier keys and try again."))
}

pub struct Service {
    shared: Arc<Mutex<Capture>>,
    closing: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Service {
    pub fn start(
        options: CaptureOptions,
        binding: Option<Binding>,
        stop_requested: Arc<RecordingStop>,
    ) -> io::Result<Self> {
        if !macos::has_permissions() || macos::secure_input_enabled() {
            return Err(io::Error::other(
                "Check input permissions and leave secure input before recording.",
            ));
        }
        let started_ns = clock_ns()?;
        let shared = Arc::new(Mutex::new(Capture {
            started: Instant::now(),
            started_ns,
            recorder: Some(Recorder::with_stop_signal(
                options,
                Duration::ZERO,
                stop_requested,
            )),
            binding,
            keys_enabled: options.keys,
            modifier_down: [false; 128],
            resume_cutoff: None,
            limit_reached: false,
            secure_pause: false,
            stopped: false,
        }));
        let closing = Arc::new(AtomicBool::new(false));
        let worker_capture = shared.clone();
        let worker_closing = closing.clone();
        let (ready, response) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("macro-capture-macos".into())
            .spawn(move || {
                let mut ready = Some(ready);
                let result = listen(options, &worker_capture, &worker_closing, &mut ready);
                if let Some(ready) = ready {
                    let _ = ready.send(Err(io::Error::other(
                        result
                            .err()
                            .map_or_else(|| "Recording stopped.".into(), |error| error.to_string()),
                    )));
                }
                if let Ok(mut capture) = worker_capture.lock() {
                    let now = capture.started.elapsed();
                    if let Some(recorder) = &mut capture.recorder {
                        recorder.pause(now);
                    }
                    capture.stopped = true;
                }
            })?;
        response.recv().map_err(io::Error::other)??;
        Ok(Self {
            shared,
            closing,
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
        if macos::secure_input_enabled() {
            return Err(io::Error::other("Leave secure input before resuming."));
        }
        if capture.limit_reached
            || capture.stopped
            || capture
                .recorder
                .as_ref()
                .is_none_or(Recorder::stop_requested)
        {
            return Err(io::Error::other("The recording has stopped."));
        }
        let (state, cutoff) = resume_boundary(capture.keys_enabled)?;
        capture.modifier_down = state;
        capture.resume_cutoff = Some(cutoff);
        capture.secure_pause = false;
        let now = capture.started.elapsed();
        if let Some(recorder) = &mut capture.recorder {
            recorder.resume(now);
        }
        Ok(())
    }

    pub fn count(&self) -> (usize, bool, bool, bool) {
        let capture = self.shared.lock().unwrap();
        (
            capture.recorder.as_ref().map_or(0, |r| r.entries().len()),
            capture.limit_reached,
            capture.stopped,
            capture.secure_pause,
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
        if let Ok(mut capture) = self.shared.lock() {
            capture.recorder = None;
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Listener {
    shared: Arc<Mutex<Capture>>,
    disabled: bool,
}

unsafe extern "C-unwind" fn event_callback(
    _: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    context: *mut c_void,
) -> *mut CGEvent {
    let listener = unsafe { &mut *context.cast::<Listener>() };
    if matches!(
        kind,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        listener.disabled = true;
        return event.as_ptr();
    }
    if catch_unwind(AssertUnwindSafe(|| {
        if let Ok(mut capture) = listener.shared.lock() {
            capture.handle(kind, unsafe { event.as_ref() });
        }
    }))
    .is_err()
    {
        listener.disabled = true;
    }
    event.as_ptr()
}

fn listen(
    options: CaptureOptions,
    shared: &Arc<Mutex<Capture>>,
    closing: &AtomicBool,
    ready: &mut Option<mpsc::SyncSender<io::Result<()>>>,
) -> io::Result<()> {
    let run_loop =
        CFRunLoop::current().ok_or_else(|| io::Error::other("No macOS run loop is available."))?;
    let mode = unsafe { kCFRunLoopDefaultMode };
    let mut mask = 0;
    if options.movement {
        mask |= crate::hotkeys::event_mask(&[
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
        ]);
    }
    if options.buttons {
        mask |= crate::hotkeys::event_mask(&[
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::ScrollWheel,
        ]);
    }
    if options.keys {
        mask |= crate::hotkeys::event_mask(&[
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
        ]);
    }
    let mut listener = Listener {
        shared: shared.clone(),
        disabled: false,
    };
    let port: CFRetained<CFMachPort> = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::TailAppendEventTap,
            CGEventTapOptions::ListenOnly,
            mask,
            Some(event_callback),
            ptr::from_mut(&mut listener).cast(),
        )
    }
    .ok_or_else(|| io::Error::other("Could not start the macOS input listener."))?;
    let source: CFRetained<CFRunLoopSource> = CFMachPort::new_run_loop_source(None, Some(&port), 0)
        .ok_or_else(|| io::Error::other("Could not start the macOS recording run loop."))?;
    run_loop.add_source(Some(&source), mode);
    CGEvent::tap_enable(&port, true);
    if !port.is_valid() || !CGEvent::tap_is_enabled(&port) {
        return Err(io::Error::other("macOS disabled the recording listener."));
    }
    {
        let mut capture = shared
            .lock()
            .map_err(|_| io::Error::other("The recording state is unavailable."))?;
        let (state, cutoff) = resume_boundary(capture.keys_enabled)?;
        capture.modifier_down = state;
        capture.resume_cutoff = Some(cutoff);
    }
    let _ = ready.take().unwrap().send(Ok(()));
    while !closing.load(Ordering::Acquire)
        && !listener.disabled
        && port.is_valid()
        && CGEvent::tap_is_enabled(&port)
        && macos::has_permissions()
    {
        CFRunLoop::run_in_mode(mode, 0.05, false);
        if let Ok(mut capture) = shared.lock() {
            capture.protect_secure_input();
        }
    }
    port.invalidate();
    source.invalidate();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture() -> Capture {
        Capture {
            started: Instant::now(),
            started_ns: 0,
            recorder: Some(Recorder::new(
                CaptureOptions {
                    keys: true,
                    ..CaptureOptions::default()
                },
                Duration::ZERO,
            )),
            binding: None,
            keys_enabled: true,
            modifier_down: [false; 128],
            resume_cutoff: None,
            limit_reached: false,
            secure_pause: false,
            stopped: false,
        }
    }

    #[test]
    fn modifier_edges_remain_independent_for_left_and_right_keys() {
        let mut state = [false; 128];
        assert!(modifier_transition(&mut state, 56));
        assert!(modifier_transition(&mut state, 60));
        assert!(!modifier_transition(&mut state, 56));
        assert!(state[60]);
        assert!(!modifier_transition(&mut state, 60));
    }

    #[test]
    fn queued_input_keeps_its_event_time_and_future_timestamps_are_clamped() {
        assert_eq!(
            event_elapsed(1_500_000, 1_000_000, Duration::from_millis(3)),
            Duration::from_micros(500)
        );
        assert_eq!(
            event_elapsed(9_000_000, 1_000_000, Duration::from_millis(3)),
            Duration::from_millis(3)
        );
        assert_eq!(
            event_elapsed(900_000, 1_000_000, Duration::from_millis(3)),
            Duration::ZERO
        );
    }

    #[test]
    fn captures_modifier_and_caps_edges_without_recording_own_window_input() {
        let mut capture = capture();
        let external = u32::MAX;
        let time = |ms| Duration::from_millis(ms);
        capture.key(
            CGEventType::FlagsChanged,
            56,
            CGEventFlags::MaskShift,
            external,
            time(1),
        );
        capture.key(
            CGEventType::KeyDown,
            0,
            CGEventFlags::MaskShift,
            external,
            time(2),
        );
        capture.key(
            CGEventType::KeyUp,
            0,
            CGEventFlags::MaskShift,
            external,
            time(3),
        );
        capture.key(
            CGEventType::FlagsChanged,
            56,
            CGEventFlags::empty(),
            external,
            time(4),
        );
        capture.key(
            CGEventType::FlagsChanged,
            57,
            CGEventFlags::MaskAlphaShift,
            external,
            time(5),
        );
        capture.key(
            CGEventType::FlagsChanged,
            57,
            CGEventFlags::MaskAlphaShift,
            external,
            time(6),
        );
        capture.key(
            CGEventType::KeyDown,
            1,
            CGEventFlags::empty(),
            external,
            time(7),
        );
        capture.key(
            CGEventType::KeyDown,
            2,
            CGEventFlags::empty(),
            std::process::id(),
            time(8),
        );
        capture.key(
            CGEventType::KeyUp,
            1,
            CGEventFlags::empty(),
            external,
            time(9),
        );
        let entries = capture.recorder.take().unwrap().finish(time(10));
        assert_eq!(entries.len(), 8);
        assert_eq!(
            super::super::validate_for(&entries, super::super::Origin::Macos),
            Ok(())
        );
        assert!(
            !entries
                .iter()
                .any(|entry| matches!(entry.event, Event::Key { code: 2, .. }))
        );
        assert_eq!(entries[6].delay_ms, 2);
        assert_eq!(entries[7].delay_ms, 1);
    }
}
