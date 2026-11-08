use crate::{input_filter::INPUT_TAG, keys, mouse::Point};
use std::{
    cell::{Cell, RefCell},
    io,
    os::windows::io::AsRawHandle,
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};
use windows_sys::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    System::{
        LibraryLoader::GetModuleHandleW,
        SystemInformation::GetTickCount64,
        Threading::{
            GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_NORMAL,
        },
    },
    UI::{
        Input::KeyboardAndMouse::*,
        WindowsAndMessaging::{
            CallNextHookEx, GetMessageW, HC_ACTION, HHOOK, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
            PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_MOUSE_LL,
            WM_APP, WM_HOTKEY, WM_QUIT, WM_XBUTTONDOWN, XBUTTON1, XBUTTON2,
        },
    },
};

const TOGGLE_A: i32 = 1;
const STOP: i32 = 2;
const CAPTURE: i32 = 3;
const TOGGLE_B: i32 = 4;
const REBIND: u32 = WM_APP + 1;
const MOUSE_TOGGLE: u32 = WM_APP + 2;

thread_local! {
    static MOUSE_BINDING: Cell<Option<Binding>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    pub vk: u16,
    mods: u32,
}

impl Binding {
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('+').map(str::trim).peekable();
        let mut mods = 0;
        let key = loop {
            let part = parts.next()?.to_ascii_lowercase();
            if parts.peek().is_none() {
                break part;
            }
            let flag = match part.as_str() {
                "ctrl" | "control" => MOD_CONTROL,
                "alt" => MOD_ALT,
                "shift" => MOD_SHIFT,
                _ => return None,
            };
            if mods & flag != 0 {
                return None;
            }
            mods |= flag;
        };
        let vk = match key.as_str() {
            "mouse4" => VK_XBUTTON1,
            "mouse5" => VK_XBUTTON2,
            _ => keys::parse(&key).filter(|key| !key.shift)?.vk,
        };
        (!matches!(vk, VK_F7 | VK_F8 | VK_F12)).then_some(Self { vk, mods })
    }

    pub fn matches_key(self, key: keys::Key) -> bool {
        self.vk == key.vk
    }

    fn mouse(self) -> bool {
        matches!(self.vk, VK_XBUTTON1 | VK_XBUTTON2)
    }

    pub fn pressed(self) -> bool {
        keys::down(self.vk) && self.required_modifiers_down()
    }

    fn required_modifiers_down(self) -> bool {
        [
            (MOD_CONTROL, VK_CONTROL),
            (MOD_ALT, VK_MENU),
            (MOD_SHIFT, VK_SHIFT),
        ]
        .into_iter()
        .all(|(flag, vk)| self.mods & flag == 0 || keys::down(vk))
    }

    pub fn has_modifiers(self) -> bool {
        self.mods != 0
    }

    pub fn modifiers_pressed(self) -> bool {
        [
            (MOD_CONTROL, VK_CONTROL),
            (MOD_ALT, VK_MENU),
            (MOD_SHIFT, VK_SHIFT),
        ]
        .into_iter()
        .any(|(flag, vk)| self.mods & flag != 0 && keys::down(vk))
    }

    fn modifiers_match(self, down: impl Fn(u16) -> bool) -> bool {
        !down(VK_LWIN)
            && !down(VK_RWIN)
            && [
                (MOD_CONTROL, VK_CONTROL),
                (MOD_ALT, VK_MENU),
                (MOD_SHIFT, VK_SHIFT),
            ]
            .into_iter()
            .all(|(flag, vk)| down(vk) == (self.mods & flag != 0))
    }
}

pub struct StopProbe {
    stop: Option<Binding>,
    toggle: Option<Binding>,
    toggle_armed: bool,
}

impl StopProbe {
    pub fn pending(&mut self) -> bool {
        if self
            .stop
            .is_some_and(|key| keys::down(key.vk) && key.modifiers_match(keys::down))
        {
            return true;
        }
        let Some(toggle) = self.toggle else {
            return false;
        };
        if !keys::down(toggle.vk) {
            self.toggle_armed = true;
            return false;
        }
        self.toggle_armed && toggle.modifiers_match(keys::down)
    }
}

#[derive(Clone)]
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

#[derive(Clone, Debug, Default)]
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

enum Active {
    Keyboard(i32),
    Mouse(HHOOK),
}

struct Rebind {
    choice: Choice,
    reply: mpsc::SyncSender<bool>,
}

pub struct Hotkeys {
    thread_id: u32,
    worker: Option<JoinHandle<()>>,
    listening: Arc<AtomicBool>,
    requests: mpsc::Sender<Rebind>,
    current: RefCell<Option<Choice>>,
    pub shortcuts: Shortcuts,
}

impl Hotkeys {
    pub fn start(
        preferred: &str,
        mut callback: impl FnMut(Action) + Send + 'static,
    ) -> io::Result<Self> {
        let preferred = Choice::parse(preferred);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (requests, receiver) = mpsc::channel::<Rebind>();
        let listening = Arc::new(AtomicBool::new(true));
        let worker_listening = listening.clone();
        let worker = thread::Builder::new()
            .name("afterbeat-shortcuts".into())
            .spawn(move || {
                let mut msg = MSG::default();
                unsafe { PeekMessageW(&mut msg, null_mut(), 0, 0, PM_NOREMOVE) };
                let shortcuts = Shortcuts {
                    stop: register_default(STOP, VK_F7, "F7", "Ctrl+Alt+F7"),
                    capture: register_default(CAPTURE, VK_F8, "F8", "Ctrl+Alt+F8"),
                };
                let mut current = [preferred, Choice::parse("F6"), Choice::parse("Ctrl+Alt+F6")]
                    .into_iter()
                    .flatten()
                    .find_map(|choice| {
                        install(choice.binding, TOGGLE_A).map(|active| (choice, active))
                    });
                let _ = ready_tx.send((
                    unsafe { GetCurrentThreadId() },
                    shortcuts,
                    current.as_ref().map(|(choice, _)| choice.clone()),
                ));
                while unsafe { GetMessageW(&mut msg, null_mut(), 0, 0) } > 0 {
                    match msg.message {
                        REBIND => {
                            // A failed PostThreadMessageW can leave an older request queued.
                            if let Some(Rebind { choice, reply }) = receiver.try_iter().last() {
                                let old = current.as_ref();
                                let reuse = old.is_some_and(|(previous, active)| {
                                    previous.binding == choice.binding
                                        || (matches!(active, Active::Mouse(_))
                                            && choice.binding.mouse())
                                });
                                let id = if matches!(old, Some((_, Active::Keyboard(TOGGLE_A)))) {
                                    TOGGLE_B
                                } else {
                                    TOGGLE_A
                                };
                                if reuse {
                                    if let Some((previous, _)) = &mut current {
                                        if choice.binding.mouse() {
                                            MOUSE_BINDING.set(Some(choice.binding));
                                        }
                                        *previous = choice;
                                    }
                                    let _ = reply.send(true);
                                } else if let Some(active) = install(choice.binding, id) {
                                    if let Some((_, previous)) = current.replace((choice, active)) {
                                        uninstall(previous);
                                    }
                                    let _ = reply.send(true);
                                } else {
                                    let _ = reply.send(false);
                                }
                            }
                        }
                        WM_HOTKEY => match msg.wParam as i32 {
                            STOP => {
                                callback(Action::Stop(extend_message_time(msg.time, tick_count())))
                            }
                            CAPTURE => callback(Action::Capture(
                                Point {
                                    x: msg.pt.x,
                                    y: msg.pt.y,
                                },
                                extend_message_time(msg.time, tick_count()),
                            )),
                            id => {
                                if let Some((choice, Active::Keyboard(active))) = &current
                                    && id == *active
                                {
                                    let code = ((msg.lParam as u32) >> 16) as u16;
                                    let mods = msg.lParam as u32
                                        & (MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN);
                                    if code == choice.binding.vk && mods == choice.binding.mods {
                                        callback(Action::Toggle(
                                            choice.binding,
                                            extend_message_time(msg.time, tick_count()),
                                        ));
                                    }
                                }
                            }
                        },
                        MOUSE_TOGGLE => {
                            if let Some((choice, Active::Mouse(_))) = &current
                                && msg.lParam == packed_binding(choice.binding)
                            {
                                callback(Action::Toggle(
                                    choice.binding,
                                    extend_message_time(msg.wParam as u32, tick_count()),
                                ));
                            }
                        }
                        _ => {}
                    }
                }
                worker_listening.store(false, Ordering::Release);
                callback(Action::Unavailable);
                if let Some((_, active)) = current {
                    uninstall(active);
                }
                for id in [STOP, CAPTURE] {
                    unsafe { UnregisterHotKey(null_mut(), id) };
                }
            })?;
        let (thread_id, shortcuts, current) = ready_rx.recv().map_err(io::Error::other)?;
        Ok(Self {
            thread_id,
            worker: Some(worker),
            listening,
            requests,
            current: RefCell::new(current),
            shortcuts,
        })
    }

    pub fn label(&self) -> Option<String> {
        self.current
            .borrow()
            .as_ref()
            .map(|choice| choice.label.clone())
    }

    pub fn toggle_binding(&self) -> Option<Binding> {
        self.current.borrow().as_ref().map(|choice| choice.binding)
    }

    pub fn listening(&self) -> bool {
        self.listening.load(Ordering::Acquire)
    }

    pub fn set_paced_stop_priority(&self, enabled: bool) {
        if let Some(worker) = &self.worker {
            let priority = if enabled {
                THREAD_PRIORITY_HIGHEST
            } else {
                THREAD_PRIORITY_NORMAL
            };
            // Keep the stop shortcut ahead of the load-paced click worker.
            unsafe { SetThreadPriority(worker.as_raw_handle(), priority) };
        }
    }

    pub fn stop_probe(&self, invoked: Option<Binding>) -> StopProbe {
        let stop = self.shortcuts.stop.map(|label| Binding {
            vk: VK_F7,
            mods: if label == "F7" {
                0
            } else {
                MOD_CONTROL | MOD_ALT
            },
        });
        let toggle = self.toggle_binding().filter(|binding| !binding.mouse());
        StopProbe {
            stop,
            toggle,
            toggle_armed: invoked.is_none(),
        }
    }

    pub fn rebind(&self, text: &str) -> Result<(), &'static str> {
        let choice =
            Choice::parse(text).ok_or("Enter a shortcut such as F6, Ctrl+Alt+G, or Mouse4.")?;
        let (reply, response) = mpsc::sync_channel(1);
        let unavailable = "Could not update the global shortcut.";
        self.requests
            .send(Rebind {
                choice: choice.clone(),
                reply,
            })
            .map_err(|_| unavailable)?;
        if unsafe { PostThreadMessageW(self.thread_id, REBIND, 0, 0) } == 0 {
            return Err(unavailable);
        }
        if !response.recv().map_err(|_| unavailable)? {
            if choice.binding.mouse() {
                return Err(
                    "Windows could not monitor that mouse button. Your previous shortcut is still active.",
                );
            }
            return Err("That shortcut is already in use. Your previous shortcut is still active.");
        }
        *self.current.borrow_mut() = Some(choice);
        Ok(())
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        if posted != 0
            && let Some(worker) = self.worker.take()
        {
            let _ = worker.join();
        }
    }
}

fn install(binding: Binding, id: i32) -> Option<Active> {
    if binding.mouse() {
        let module = unsafe { GetModuleHandleW(null()) };
        let hook = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0) };
        if hook.is_null() {
            return None;
        }
        MOUSE_BINDING.set(Some(binding));
        Some(Active::Mouse(hook))
    } else {
        (unsafe {
            RegisterHotKey(
                null_mut(),
                id,
                MOD_NOREPEAT | binding.mods,
                binding.vk.into(),
            )
        } != 0)
            .then_some(Active::Keyboard(id))
    }
}

fn uninstall(active: Active) {
    match active {
        Active::Keyboard(id) => {
            unsafe { UnregisterHotKey(null_mut(), id) };
        }
        Active::Mouse(hook) => {
            MOUSE_BINDING.set(None);
            unsafe { UnhookWindowsHookEx(hook) };
        }
    }
}

unsafe extern "system" fn mouse_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32
        && message == WM_XBUTTONDOWN as WPARAM
        && let Some(binding) = MOUSE_BINDING.get()
    {
        // Windows supplies an MSLLHOOKSTRUCT for HC_ACTION.
        let event = unsafe { &*(data as *const MSLLHOOKSTRUCT) };
        queue_mouse_toggle(binding, event, keys::down);
    }
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

fn queue_mouse_toggle(binding: Binding, event: &MSLLHOOKSTRUCT, down: impl Fn(u16) -> bool) {
    let vk = match (event.mouseData >> 16) as u16 {
        XBUTTON1 => VK_XBUTTON1,
        XBUTTON2 => VK_XBUTTON2,
        _ => 0,
    };
    if event.dwExtraInfo != INPUT_TAG && vk == binding.vk && binding.modifiers_match(down) {
        // Queue work so the hook can pass through the mouse event promptly.
        unsafe {
            PostThreadMessageW(
                GetCurrentThreadId(),
                MOUSE_TOGGLE,
                event.time as WPARAM,
                packed_binding(binding),
            )
        };
    }
}

fn packed_binding(binding: Binding) -> LPARAM {
    ((u32::from(binding.vk) << 16) | binding.mods) as LPARAM
}

pub fn tick_count() -> u64 {
    unsafe { GetTickCount64() }
}

fn extend_message_time(time: u32, now: u64) -> u64 {
    // Win32 message timestamps carry the low 32 bits of the system tick count.
    now.saturating_sub(u64::from((now as u32).wrapping_sub(time)))
}

fn register_default(
    id: i32,
    vk: u16,
    bare: &'static str,
    fallback: &'static str,
) -> Option<&'static str> {
    [(0, bare), (MOD_CONTROL | MOD_ALT, fallback)]
        .into_iter()
        .find_map(|(mods, label)| install(Binding { vk, mods }, id).map(|_| label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::PM_REMOVE;

    #[test]
    fn native_message_time_preserves_order_across_tick_rollover() {
        let now = 0x1_0000_0010;
        assert_eq!(extend_message_time(0xffff_fff0, now), 0xffff_fff0);
        assert_eq!(extend_message_time(0x0000_0008, now), 0x1_0000_0008);
    }

    #[test]
    fn parses_shortcut_chords_and_protects_safety_keys() {
        assert_eq!(
            Binding::parse("Ctrl+Alt+F10"),
            Some(Binding {
                vk: VK_F10,
                mods: MOD_CONTROL | MOD_ALT
            })
        );
        assert!(Binding::parse("Mouse4").unwrap().mouse());
        for invalid in ["F7", "F8", "Ctrl+F7", "F12", "Ctrl+Ctrl+F6", ""] {
            assert!(Binding::parse(invalid).is_none());
        }
    }

    #[test]
    fn side_button_down_queues_the_shortcut_without_sampling_button_state() {
        let mut msg = MSG::default();
        let mut take_toggle =
            || unsafe { PeekMessageW(&mut msg, null_mut(), MOUSE_TOGGLE, MOUSE_TOGGLE, PM_REMOVE) };
        assert_eq!(take_toggle(), 0);

        for (name, button, other_button) in [
            ("Mouse4", XBUTTON1, XBUTTON2),
            ("Mouse5", XBUTTON2, XBUTTON1),
        ] {
            let binding = Binding::parse(name).unwrap();
            let mut event = MSLLHOOKSTRUCT {
                mouseData: u32::from(other_button) << 16,
                ..Default::default()
            };
            queue_mouse_toggle(binding, &event, |_| false);

            event.mouseData = u32::from(button) << 16;
            event.dwExtraInfo = INPUT_TAG;
            queue_mouse_toggle(binding, &event, |_| false);
            assert_eq!(take_toggle(), 0);

            event.dwExtraInfo = 0;
            queue_mouse_toggle(binding, &event, |vk| vk == VK_CONTROL);
            assert_eq!(take_toggle(), 0);
            queue_mouse_toggle(binding, &event, |_| false);
            assert_ne!(take_toggle(), 0);
            assert_eq!(take_toggle(), 0);
        }

        let chord = Binding::parse("Ctrl+Mouse4").unwrap();
        let event = MSLLHOOKSTRUCT {
            mouseData: u32::from(XBUTTON1) << 16,
            ..Default::default()
        };
        queue_mouse_toggle(chord, &event, |_| false);
        queue_mouse_toggle(chord, &event, |vk| matches!(vk, VK_CONTROL | VK_SHIFT));
        assert_eq!(take_toggle(), 0);
        queue_mouse_toggle(chord, &event, |vk| vk == VK_CONTROL);
        assert_ne!(take_toggle(), 0);
    }
}
