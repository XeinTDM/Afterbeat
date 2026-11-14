use crate::{
    keys::{self, F7, F8, Key, Keymap},
    mouse::Point,
    x11::{self, Desktop, XResult},
};
use rustix::event::{PollFd, PollFlags, poll};
use std::{
    cell::RefCell,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Instant,
};
use x11rb::{
    CURRENT_TIME,
    connection::Connection,
    protocol::{
        Event,
        xinput::{ConnectionExt as _, EventMask, XIEventMask},
        xproto::{ConnectionExt as _, GrabMode, ModMask},
    },
};
use xkeysym::key;

const SHIFT: u16 = 1;
const CONTROL: u16 = 4;
const ALT: u16 = 8;
const PORTAL_BINDING: u16 = u16::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Binding {
    symbol: u32,
    code: u8,
    mods: u16,
    locks: u16,
}

impl Binding {
    pub fn portal(symbol: u32, modified: bool) -> Self {
        Self {
            symbol,
            code: 0,
            mods: u16::from(modified),
            locks: PORTAL_BINDING,
        }
    }

    pub fn preferred_trigger(text: &str) -> Option<String> {
        let binding = Self::parse(text)?;
        if binding.mouse() {
            return None;
        }
        let mut trigger = String::new();
        for (flag, name) in [(CONTROL, "CTRL+"), (ALT, "ALT+"), (SHIFT, "SHIFT+")] {
            if binding.mods & flag != 0 {
                trigger.push_str(name);
            }
        }
        let symbol = xkeysym::Keysym::new(binding.symbol).name()?;
        // The portal wants XKB identifiers such as F6, without the Xlib prefix.
        trigger.push_str(symbol.strip_prefix("XK_").unwrap_or(symbol));
        Some(trigger)
    }

    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('+').map(str::trim).peekable();
        let mut mods = 0;
        let symbol = loop {
            let part = parts.next()?.to_ascii_lowercase();
            if parts.peek().is_none() {
                break match part.as_str() {
                    "mouse4" => 8,
                    "mouse5" => 9,
                    _ => keys::parse(&part)?.0,
                };
            }
            let flag = match part.as_str() {
                "ctrl" | "control" => CONTROL,
                "alt" => ALT,
                "shift" => SHIFT,
                _ => return None,
            };
            if mods & flag != 0 {
                return None;
            }
            mods |= flag;
        };
        (!matches!(symbol, F7 | F8 | key::F12)).then_some(Self {
            symbol,
            code: 0,
            mods,
            locks: 0,
        })
    }

    fn mouse(self) -> bool {
        matches!(self.symbol, 8 | 9)
    }

    fn resolved(mut self, desktop: &Desktop, map: &Keymap) -> XResult<Self> {
        let (alt, locks) = map.modifier_masks(desktop)?;
        self.locks = locks;
        if self.mods & ALT != 0 {
            if alt.count_ones() != 1 {
                return Err("Could not determine the X11 Alt modifier.".into());
            }
            self.mods = (self.mods & !ALT) | alt;
        }
        self.code = if self.mouse() {
            self.symbol as u8
        } else {
            map.resolve(self.symbol, false)
                .ok_or("The shortcut is unavailable in the keyboard layout.")?
                .0
        };
        Ok(self)
    }

    fn key_down(self, desktop: &Desktop) -> XResult<bool> {
        let index = usize::from(self.code / 8);
        let bit = 1 << (self.code % 8);
        Ok(desktop.conn.query_keymap()?.reply()?.keys[index] & bit != 0)
    }

    fn matches_mods(self, state: u16) -> bool {
        state & 0xff & !self.locks == self.mods
    }

    pub fn has_modifiers(self) -> bool {
        self.mods != 0
    }

    pub fn pressed(self) -> bool {
        if self.locks == PORTAL_BINDING {
            return crate::wayland::activation_down();
        }
        x11::with(|desktop| {
            let pointer = desktop
                .conn
                .xinput_xi_query_pointer(desktop.root, desktop.pointer)?
                .reply()?;
            if pointer.mods.effective as u16 & self.mods != self.mods {
                return Ok(false);
            }
            if self.mouse() {
                return Ok(pointer
                    .buttons
                    .first()
                    .is_some_and(|buttons| buttons & (1 << self.code) != 0));
            }
            self.key_down(desktop)
        })
        .unwrap_or(false)
    }

    pub fn modifiers_pressed(self) -> bool {
        if self.locks == PORTAL_BINDING {
            return crate::wayland::activation_down();
        }
        x11::with(|desktop| {
            let pointer = desktop
                .conn
                .xinput_xi_query_pointer(desktop.root, desktop.pointer)?
                .reply()?;
            Ok(pointer.mods.effective as u16 & self.mods != 0)
        })
        .unwrap_or(true)
    }

    pub fn matches_key(self, key: Key) -> bool {
        if self.locks == PORTAL_BINDING {
            return self.symbol == key.0;
        }
        if self.mouse() {
            return false;
        }
        if self.symbol == key.0 {
            return true;
        }
        x11::with(|desktop| {
            let map = Keymap::read(desktop)?;
            Ok(map
                .resolve(key.0, true)
                .is_some_and(|(code, _)| code == self.code))
        })
        .unwrap_or(false)
    }

    pub fn uses_code(self, code: u8) -> bool {
        !self.mouse() && self.code == code
    }

    pub fn matches_button(self, button: crate::mouse::Button) -> bool {
        self.locks != PORTAL_BINDING && self.mouse() && self.symbol == u32::from(button.x11())
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
    PortalChanged,
    PortalStatus,
}

pub struct StopProbe {
    stop: Option<Binding>,
    toggle: Option<Binding>,
    toggle_armed: bool,
}

impl StopProbe {
    pub fn pending(&mut self) -> bool {
        x11::with(|desktop| {
            let pointer = desktop
                .conn
                .xinput_xi_query_pointer(desktop.root, desktop.pointer)?
                .reply()?;
            let mods = pointer.mods.effective as u16;
            if let Some(stop) = self.stop
                && stop.matches_mods(mods)
                && stop.key_down(desktop)?
            {
                return Ok(true);
            }
            if let Some(toggle) = self.toggle {
                if !toggle.key_down(desktop)? {
                    self.toggle_armed = true;
                } else if self.toggle_armed && toggle.matches_mods(mods) {
                    return Ok(true);
                }
            }
            Ok(false)
        })
        .unwrap_or(false)
    }
}

struct Rebind {
    choice: Choice,
    reply: mpsc::SyncSender<Option<Choice>>,
}

pub struct Hotkeys {
    worker: Option<JoinHandle<()>>,
    wake: UnixStream,
    listening: Arc<AtomicBool>,
    requests: mpsc::Sender<Rebind>,
    current: RefCell<Option<Choice>>,
    stop: Option<Binding>,
    pub shortcuts: Shortcuts,
}

impl Hotkeys {
    pub fn start(
        preferred: &str,
        mut callback: impl FnMut(Action) + Send + 'static,
    ) -> io::Result<Self> {
        let preferred = Choice::parse(preferred);
        let (wake, mut reader) = UnixStream::pair()?;
        let (ready, response) = mpsc::sync_channel(1);
        let (requests, receiver) = mpsc::channel();
        let listening = Arc::new(AtomicBool::new(true));
        let worker_listening = listening.clone();
        let worker = thread::Builder::new()
            .name("afterbeat-shortcuts".into())
            .spawn(move || {
                let mut ready = Some(ready);
                let result = listen(preferred, &mut reader, &receiver, &mut callback, &mut ready);
                worker_listening.store(false, Ordering::Release);
                if let Some(ready) = ready {
                    let _ = ready.send(Err(io::Error::other(result.unwrap_err())));
                } else {
                    if let Err(error) = result {
                        crate::diagnostics::record(&format!("X11 shortcuts: {error}"));
                    }
                    callback(Action::Unavailable);
                }
            })?;
        let (shortcuts, stop, current) = response.recv().map_err(io::Error::other)??;
        Ok(Self {
            worker: Some(worker),
            wake,
            listening,
            requests,
            current: RefCell::new(current),
            stop,
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

    pub fn set_paced_stop_priority(&self, _enabled: bool) {}

    pub fn stop_probe(&self, invoked: Option<Binding>) -> StopProbe {
        StopProbe {
            stop: self.stop,
            toggle: self.toggle_binding().filter(|binding| !binding.mouse()),
            toggle_armed: invoked.is_none(),
        }
    }

    pub fn rebind(&self, text: &str) -> Result<(), &'static str> {
        let choice =
            Choice::parse(text).ok_or("Enter a shortcut such as F6, Ctrl+Alt+G, or Mouse4.")?;
        let (reply, response) = mpsc::sync_channel(1);
        let unavailable = "Could not update the global shortcut.";
        self.requests
            .send(Rebind { choice, reply })
            .map_err(|_| unavailable)?;
        (&self.wake).write_all(&[1]).map_err(|_| unavailable)?;
        let choice = response.recv().map_err(|_| unavailable)?
            .ok_or("That shortcut is in use or unavailable in your keyboard layout. Your previous shortcut is still active.")?;
        *self.current.borrow_mut() = Some(choice);
        Ok(())
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        let _ = self.wake.shutdown(Shutdown::Write);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

type Ready = mpsc::SyncSender<io::Result<(Shortcuts, Option<Binding>, Option<Choice>)>>;

fn listen(
    preferred: Option<Choice>,
    wake: &mut UnixStream,
    requests: &mpsc::Receiver<Rebind>,
    callback: &mut impl FnMut(Action),
    ready: &mut Option<Ready>,
) -> XResult<()> {
    let desktop = Desktop::connect()?;
    let map = Keymap::read(&desktop)?;
    keys::enable_detectable_repeat(&desktop)?;
    let conn = &desktop.conn;
    conn.xinput_xi_select_events(
        desktop.root,
        &[EventMask {
            deviceid: 1,
            mask: vec![XIEventMask::RAW_BUTTON_PRESS | XIEventMask::RAW_KEY_RELEASE],
        }],
    )?
    .check()?;
    let (stop, stop_label) = register_default(&desktop, &map, F7, "F7", "Ctrl+Shift+F7");
    let (capture, capture_label) = register_default(&desktop, &map, F8, "F8", "Ctrl+Shift+F8");
    let mut current = [
        preferred,
        Choice::parse("F6"),
        Choice::parse("Ctrl+Shift+F6"),
    ]
    .into_iter()
    .flatten()
    .find_map(|choice| register_choice(&desktop, &map, choice, None));
    let _ = ready.take().unwrap().send(Ok((
        Shortcuts {
            stop: stop_label,
            capture: capture_label,
        },
        stop,
        current.clone(),
    )));
    let mut pressed = [false; 256];
    loop {
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::KeyPress(event) if event.response_type & 0x80 == 0 => {
                    let mods = u16::from(event.state);
                    let code = event.detail;
                    let repeat = std::mem::replace(&mut pressed[usize::from(code)], true);
                    // Release the active grab while retaining the passive registration.
                    // A held activation key must not redirect Afterbeat's keyboard output.
                    conn.ungrab_keyboard(CURRENT_TIME)?;
                    conn.flush()?;
                    if repeat {
                        continue;
                    }
                    let matches = |binding: Binding| {
                        !binding.mouse() && binding.code == code && binding.matches_mods(mods)
                    };
                    let now = tick_count();
                    if stop.is_some_and(matches) {
                        callback(Action::Stop(now));
                    } else if capture.is_some_and(matches) && event.same_screen {
                        callback(Action::Capture(
                            Point {
                                x: event.root_x.into(),
                                y: event.root_y.into(),
                            },
                            now,
                        ));
                    } else if let Some(choice) = &current
                        && matches(choice.binding)
                    {
                        callback(Action::Toggle(choice.binding, now));
                    }
                }
                Event::KeyRelease(event) => pressed[usize::from(event.detail)] = false,
                Event::XinputRawKeyRelease(event) if event.detail < 256 => {
                    pressed[event.detail as usize] = false
                }
                Event::XinputRawButtonPress(event) => {
                    if let Some(choice) = &current
                        && choice.binding.mouse()
                        && event.detail == u32::from(choice.binding.code)
                    {
                        let pointer = conn
                            .xinput_xi_query_pointer(desktop.root, desktop.pointer)?
                            .reply()?;
                        if choice.binding.matches_mods(pointer.mods.effective as u16) {
                            callback(Action::Toggle(choice.binding, tick_count()));
                        }
                    }
                }
                _ => {}
            }
        }
        let mut fds = [
            PollFd::new(conn, PollFlags::IN),
            PollFd::new(&*wake, PollFlags::IN),
        ];
        match poll(&mut fds, None) {
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        if fds[0]
            .revents()
            .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL)
        {
            return Err("The X11 shortcut connection closed.".into());
        }
        if !fds[1].revents().is_empty() {
            if wake.read(&mut [0; 64])? == 0 {
                return Ok(());
            }
            for Rebind { choice, reply } in requests.try_iter() {
                let updated = Keymap::read(&desktop).ok().and_then(|map| {
                    register_choice(
                        &desktop,
                        &map,
                        choice,
                        current.as_ref().map(|choice| choice.binding),
                    )
                });
                if let Some(choice) = &updated {
                    current = Some(choice.clone());
                }
                let _ = reply.send(updated);
            }
        }
    }
}

fn register_default(
    desktop: &Desktop,
    map: &Keymap,
    symbol: u32,
    bare: &'static str,
    fallback: &'static str,
) -> (Option<Binding>, Option<&'static str>) {
    // Ctrl+Alt+Fn is commonly assigned to virtual-terminal switching on Xorg.
    for (mods, label) in [(0, bare), (CONTROL | SHIFT, fallback)] {
        if let Ok(binding) = (Binding {
            symbol,
            code: 0,
            mods,
            locks: 0,
        })
        .resolved(desktop, map)
            && install(desktop, binding, None).is_ok()
        {
            return (Some(binding), Some(label));
        }
    }
    (None, None)
}

fn register_choice(
    desktop: &Desktop,
    map: &Keymap,
    mut choice: Choice,
    old: Option<Binding>,
) -> Option<Choice> {
    choice.binding = choice.binding.resolved(desktop, map).ok()?;
    install(desktop, choice.binding, old).ok()?;
    Some(choice)
}

fn grab_masks(binding: Binding) -> Vec<ModMask> {
    if binding.mouse() {
        return Vec::new();
    }
    (0..=binding.locks)
        .filter(|locks| locks & !binding.locks == 0)
        .map(|locks| (binding.mods | locks).into())
        .collect()
}

fn install(desktop: &Desktop, binding: Binding, previous: Option<Binding>) -> XResult<()> {
    let old_masks = previous.map_or_else(Vec::new, grab_masks);
    let new_masks = grab_masks(binding);
    let same_code = previous.is_some_and(|old| old.code == binding.code);
    let conn = &desktop.conn;
    let mut installed = Vec::new();
    for &mask in new_masks
        .iter()
        .filter(|mask| !same_code || !old_masks.contains(mask))
    {
        let result = conn
            .grab_key(
                false,
                desktop.root,
                mask,
                binding.code,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )?
            .check();
        if let Err(error) = result {
            for mask in installed {
                conn.ungrab_key(binding.code, desktop.root, mask)?.check()?;
            }
            return Err(error.into());
        }
        installed.push(mask);
    }
    if let Some(old) = previous {
        for mask in old_masks
            .into_iter()
            .filter(|mask| !same_code || !new_masks.contains(mask))
        {
            conn.ungrab_key(old.code, desktop.root, mask)?.check()?;
        }
    }
    Ok(())
}

pub fn tick_count() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}
