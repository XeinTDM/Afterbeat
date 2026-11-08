use crate::{
    input_filter,
    keys::{self, F7, F8, F12, Key},
    macos,
    mouse::Point,
};
use objc2_core_foundation::{
    CFMachPort, CFRetained, CFRunLoop, CFRunLoopSource, kCFRunLoopDefaultMode,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation,
    CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy, CGEventType, CGMouseButton,
};
use std::{
    ffi::c_void,
    io,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::{self, NonNull},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MODIFIERS: CGEventFlags = CGEventFlags::MaskControl
    .union(CGEventFlags::MaskAlternate)
    .union(CGEventFlags::MaskShift)
    .union(CGEventFlags::MaskCommand);
const HARDWARE: CGEventSourceStateID = CGEventSourceStateID::HIDSystemState;
const STOP: Binding = Binding::key(F7);
const CAPTURE: Binding = Binding::key(F8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trigger {
    Key(u16),
    Mouse(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    trigger: Trigger,
    mods: CGEventFlags,
}

impl Binding {
    const fn key(code: u16) -> Self {
        Self {
            trigger: Trigger::Key(code),
            mods: CGEventFlags::empty(),
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('+').map(str::trim).peekable();
        let mut mods = CGEventFlags::empty();
        loop {
            let part = parts.next()?.to_ascii_lowercase();
            if parts.peek().is_none() {
                let trigger = match part.as_str() {
                    "mouse4" => Trigger::Mouse(3),
                    "mouse5" => Trigger::Mouse(4),
                    _ => {
                        let key = keys::parse(&part)?;
                        if key.is_safety_key() || key.code == F12 {
                            return None;
                        }
                        if key.shift {
                            mods.insert(CGEventFlags::MaskShift);
                        }
                        Trigger::Key(key.code)
                    }
                };
                return Some(Self { trigger, mods });
            }
            let flag = match part.as_str() {
                "ctrl" | "control" => CGEventFlags::MaskControl,
                "alt" | "option" | "opt" => CGEventFlags::MaskAlternate,
                "shift" => CGEventFlags::MaskShift,
                "cmd" | "command" => CGEventFlags::MaskCommand,
                _ => return None,
            };
            if mods.contains(flag) {
                return None;
            }
            mods.insert(flag);
        }
    }

    pub fn has_modifiers(self) -> bool {
        !self.mods.is_empty()
    }

    fn down(self) -> bool {
        match self.trigger {
            Trigger::Key(code) => keys::down(code),
            Trigger::Mouse(button) => CGEventSource::button_state(HARDWARE, CGMouseButton(button)),
        }
    }

    fn matches(self, trigger: Trigger, flags: CGEventFlags) -> bool {
        self.trigger == trigger && flags.intersection(MODIFIERS) == self.mods
    }

    pub fn pressed(self) -> bool {
        CGEventSource::flags_state(HARDWARE).contains(self.mods) && self.down()
    }

    pub fn modifiers_pressed(self) -> bool {
        CGEventSource::flags_state(HARDWARE).intersects(self.mods)
    }

    pub fn matches_key(self, key: Key) -> bool {
        self.trigger == Trigger::Key(key.code)
    }
}

struct Choice {
    binding: Binding,
    label: String,
}

impl Choice {
    fn parse(text: &str) -> Option<Self> {
        let label = text.trim();
        if label.len() > 40 {
            return None;
        }
        Some(Self {
            binding: Binding::parse(label)?,
            label: label.to_owned(),
        })
    }
}

pub struct Shortcuts {
    pub stop: Option<&'static str>,
    pub capture: Option<&'static str>,
}

pub enum Action {
    Toggle(Binding, u64),
    Stop(u64),
    Capture(Point, u64),
    Unavailable,
}

pub struct StopProbe {
    listening: Arc<AtomicBool>,
    toggle: Option<Binding>,
    toggle_armed: bool,
}

impl StopProbe {
    pub fn pending(&mut self) -> bool {
        if !self.listening.load(Ordering::Acquire) {
            return true;
        }
        let mods = CGEventSource::flags_state(HARDWARE);
        if STOP.matches(STOP.trigger, mods) && STOP.down() {
            return true;
        }
        if let Some(toggle) = self.toggle {
            if !toggle.down() {
                self.toggle_armed = true;
            } else if self.toggle_armed && toggle.matches(toggle.trigger, mods) {
                return true;
            }
        }
        false
    }
}

pub struct Hotkeys {
    worker: Option<JoinHandle<()>>,
    closing: Arc<AtomicBool>,
    listening: Arc<AtomicBool>,
    current: Arc<Mutex<Option<Choice>>>,
    pub shortcuts: Shortcuts,
}

impl Hotkeys {
    pub fn start(
        preferred: &str,
        mut callback: impl FnMut(Action) + Send + 'static,
    ) -> io::Result<Self> {
        let mut hotkeys = Self {
            worker: None,
            closing: Arc::new(AtomicBool::new(false)),
            listening: Arc::new(AtomicBool::new(false)),
            current: Arc::new(Mutex::new(None)),
            shortcuts: Shortcuts {
                stop: None,
                capture: None,
            },
        };
        if !macos::has_permissions() {
            return Ok(hotkeys);
        }
        *hotkeys.current.lock().unwrap() = Choice::parse(preferred);
        let closing = hotkeys.closing.clone();
        let listening = hotkeys.listening.clone();
        let current = hotkeys.current.clone();
        let (ready, response) = mpsc::sync_channel(1);
        hotkeys.worker = Some(
            thread::Builder::new()
                .name("afterbeat-shortcuts".into())
                .spawn(move || {
                    let mut listener = Listener {
                        current,
                        swallowed: [false; 128],
                        disabled: false,
                        callback: &mut callback,
                    };
                    let mut ready = Some(ready);
                    let result = listen(&mut listener, &closing, &listening, &mut ready);
                    listening.store(false, Ordering::Release);
                    if let Some(ready) = ready {
                        let _ = ready.send(result);
                    } else if !closing.load(Ordering::Acquire) {
                        if let Err(error) = result {
                            crate::diagnostics::record(&format!("macOS input listener: {error}"));
                        }
                        callback(Action::Unavailable);
                    }
                })?,
        );
        response.recv().map_err(io::Error::other)??;
        hotkeys.shortcuts = Shortcuts {
            stop: Some("F7"),
            capture: Some("F8"),
        };
        Ok(hotkeys)
    }

    pub fn label(&self) -> Option<String> {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .map(|choice| choice.label.clone())
    }

    pub fn toggle_binding(&self) -> Option<Binding> {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .map(|choice| choice.binding)
    }

    pub fn listening(&self) -> bool {
        self.listening.load(Ordering::Acquire)
    }

    pub fn set_paced_stop_priority(&self, _enabled: bool) {}

    pub fn stop_probe(&self, invoked: Option<Binding>) -> StopProbe {
        StopProbe {
            listening: self.listening.clone(),
            toggle: self.toggle_binding(),
            toggle_armed: invoked.is_none(),
        }
    }

    pub fn rebind(&self, text: &str) -> Result<(), &'static str> {
        if !self.listening() {
            return Err(
                "Global shortcuts are unavailable. Check input permissions and reopen Afterbeat.",
            );
        }
        let choice = Choice::parse(text)
            .ok_or("Enter a shortcut such as F6, Ctrl+Option+G, Cmd+F6, or Mouse4.")?;
        *self.current.lock().unwrap() = Some(choice);
        Ok(())
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Listener<'a> {
    current: Arc<Mutex<Option<Choice>>>,
    swallowed: [bool; 128],
    disabled: bool,
    callback: &'a mut dyn FnMut(Action),
}

impl Listener<'_> {
    fn handle(&mut self, kind: CGEventType, event: &CGEvent) -> bool {
        if matches!(
            kind,
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
        ) {
            self.disabled = true;
            input_filter::set_ready(false);
            return false;
        }
        if macos::is_ours(event) {
            return false;
        }
        let event = Some(event);
        let mods = CGEvent::flags(event);
        let (trigger, swallowed) = if kind == CGEventType::OtherMouseDown {
            let button = CGEvent::integer_value_field(event, CGEventField::MouseEventButtonNumber);
            if !matches!(button, 3 | 4) {
                return false;
            }
            (Trigger::Mouse(button as u32), None)
        } else {
            let code = CGEvent::integer_value_field(event, CGEventField::KeyboardEventKeycode);
            let Some(swallowed) = usize::try_from(code)
                .ok()
                .and_then(|code| self.swallowed.get_mut(code))
            else {
                return false;
            };
            if kind == CGEventType::KeyUp {
                return std::mem::take(swallowed);
            }
            if CGEvent::integer_value_field(event, CGEventField::KeyboardEventAutorepeat) != 0 {
                return *swallowed;
            }
            (Trigger::Key(code as u16), Some(swallowed))
        };
        let timestamp = tick_count();
        let action = if STOP.matches(trigger, mods) {
            Some(Action::Stop(timestamp))
        } else if CAPTURE.matches(trigger, mods) {
            crate::mouse::cursor()
                .ok()
                .map(|point| Action::Capture(point, timestamp))
        } else {
            self.current
                .lock()
                .unwrap()
                .as_ref()
                .filter(|choice| choice.binding.matches(trigger, mods))
                .map(|choice| Action::Toggle(choice.binding, timestamp))
        };
        if let Some(action) = action {
            (self.callback)(action);
            if let Some(swallowed) = swallowed {
                *swallowed = true;
                return true;
            }
        }
        false
    }
}

unsafe extern "C-unwind" fn shortcut_event(
    _: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    context: *mut c_void,
) -> *mut CGEvent {
    // The context stays exclusively on the run loop thread until its tap is invalidated.
    let listener = unsafe { &mut *context.cast::<Listener<'_>>() };
    match catch_unwind(AssertUnwindSafe(|| {
        listener.handle(kind, unsafe { event.as_ref() })
    })) {
        Ok(true) => ptr::null_mut(),
        Ok(false) => event.as_ptr(),
        Err(_) => {
            listener.disabled = true;
            input_filter::set_ready(false);
            event.as_ptr()
        }
    }
}

fn listen(
    listener: &mut Listener<'_>,
    closing: &AtomicBool,
    listening: &AtomicBool,
    ready: &mut Option<mpsc::SyncSender<io::Result<()>>>,
) -> io::Result<()> {
    let run_loop =
        CFRunLoop::current().ok_or_else(|| io::Error::other("No macOS run loop is available."))?;
    let mode = unsafe { kCFRunLoopDefaultMode };
    let mask = event_mask(&[
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::OtherMouseDown,
    ]);
    let port = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask,
            Some(shortcut_event),
            ptr::from_mut(listener).cast(),
        )
    };
    let shortcuts = Tap::new(port, "Could not monitor global shortcuts.")?;
    let protection = Tap::new(
        input_filter::tap(),
        "Could not protect the Afterbeat window.",
    )?;
    for tap in [&shortcuts, &protection] {
        run_loop.add_source(Some(&tap.source), mode);
        CGEvent::tap_enable(&tap.port, true);
    }
    if !shortcuts.active() || !protection.active() {
        return Err(io::Error::other(
            "macOS disabled the input taps during startup.",
        ));
    }
    input_filter::set_ready(true);
    listening.store(true, Ordering::Release);
    let _ = ready.take().unwrap().send(Ok(()));
    let mut check_permissions_at = Instant::now() + Duration::from_secs(1);
    while !closing.load(Ordering::Acquire) {
        CFRunLoop::run_in_mode(mode, 0.05, false);
        if listener.disabled
            || !shortcuts.active()
            || !protection.active()
            || !input_filter::ready()
        {
            break;
        }
        if Instant::now() >= check_permissions_at {
            if !macos::has_permissions() {
                break;
            }
            check_permissions_at = Instant::now() + Duration::from_secs(1);
        }
    }
    input_filter::set_ready(false);
    Ok(())
}

pub fn event_mask(types: &[CGEventType]) -> u64 {
    types.iter().fold(0, |mask, kind| mask | (1 << kind.0))
}

struct Tap {
    port: CFRetained<CFMachPort>,
    source: CFRetained<CFRunLoopSource>,
}

impl Tap {
    fn new(port: Option<CFRetained<CFMachPort>>, message: &str) -> io::Result<Self> {
        let port = port.ok_or_else(|| io::Error::other(message.to_owned()))?;
        let source = CFMachPort::new_run_loop_source(None, Some(&port), 0)
            .ok_or_else(|| io::Error::other(message.to_owned()))?;
        Ok(Self { port, source })
    }

    fn active(&self) -> bool {
        self.port.is_valid() && CGEvent::tap_is_enabled(&self.port)
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.port.invalidate();
        self.source.invalidate();
    }
}

pub fn tick_count() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mac_modifiers_and_reserves_safety_keys() {
        assert_eq!(
            Binding::parse("Cmd+Option+Mouse5"),
            Binding::parse("Command+Alt+Mouse5")
        );
        assert!(Binding::parse("Option+Alt+F6").is_none());
        assert!(Binding::parse("Ctrl+F7").is_none());
        assert!(Binding::parse("Cmd+F8").is_none());
        let binding = Binding::parse("Cmd+F6").unwrap();
        assert!(binding.matches(
            binding.trigger,
            CGEventFlags::MaskCommand | CGEventFlags::MaskAlphaShift
        ));
        assert!(!binding.matches(binding.trigger, MODIFIERS));
    }
}
