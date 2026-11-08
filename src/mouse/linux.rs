use super::{Button, Input, Point, Press, Zone, route::Route};
use crate::{
    engine::{GUARD_POLL_INTERVAL, Gate, Outcome, PauseReason, RunControl},
    hotkeys::Binding,
    input_filter::{self, Hit},
    keys::{self, F7, F8, Keymap},
    wayland,
    x11::{self, Desktop, XResult},
};
use std::{
    io,
    time::{Duration, Instant},
};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::Connection,
    protocol::{
        xinput::ConnectionExt as _,
        xproto::{self, ConnectionExt as _},
        xtest::ConnectionExt as _,
    },
};
use xkeysym::{Keysym, key};

pub struct Injector(Backend);

enum Backend {
    X11(X11Injector),
    Wayland(super::wayland::Injector),
}

impl Injector {
    pub fn new(
        input: Input,
        double: bool,
        points: Vec<Point>,
        radius: i32,
        press: Press,
    ) -> Result<Self, String> {
        if wayland::active() {
            super::wayland::Injector::new(input, double, points, radius, press)
                .map(|injector| Self(Backend::Wayland(injector)))
        } else {
            X11Injector::new(input, double, points, radius, press)
                .map(|injector| Self(Backend::X11(injector)))
        }
    }

    pub fn with_activation(self, activation: Option<Binding>) -> Self {
        match self.0 {
            Backend::X11(injector) => Self(Backend::X11(injector.with_activation(activation))),
            Backend::Wayland(_) => self,
        }
    }

    pub fn batchable(&self) -> bool {
        matches!(&self.0, Backend::X11(injector) if injector.batchable())
    }

    pub fn has_targets(&self) -> bool {
        match &self.0 {
            Backend::X11(injector) => injector.has_targets(),
            Backend::Wayland(injector) => injector.has_targets(),
        }
    }

    pub fn has_changing_targets(&self) -> bool {
        match &self.0 {
            Backend::X11(injector) => injector.has_changing_targets(),
            Backend::Wayland(injector) => injector.has_changing_targets(),
        }
    }

    pub fn destination(&mut self) -> Result<Option<Point>, String> {
        match &mut self.0 {
            Backend::X11(injector) => injector.destination(),
            Backend::Wayland(injector) => injector.destination(),
        }
    }

    pub fn send(
        &mut self,
        actions: u32,
        control: &mut RunControl<'_>,
        check: Option<&mut dyn FnMut(Option<Point>) -> Gate>,
    ) -> Result<Option<Outcome>, String> {
        match &mut self.0 {
            Backend::X11(injector) => injector.send(actions, control, check),
            Backend::Wayland(injector) => {
                debug_assert_eq!(actions, 1);
                injector.send(control, check)
            }
        }
    }
}

pub fn cursor() -> io::Result<Point> {
    if wayland::active() {
        return Err(io::Error::other(
            "Wayland does not expose the live desktop pointer.",
        ));
    }
    x11::with(Desktop::cursor)
}

pub fn display_areas() -> Vec<Zone> {
    if wayland::active() {
        return wayland::displays();
    }
    x11::with(Desktop::displays).unwrap_or_default()
}

pub fn on_monitor(point: Point) -> bool {
    display_areas()
        .iter()
        .any(|display| display.contains(point))
}

pub fn at_edge(point: Point, corners_only: bool) -> bool {
    let displays = display_areas();
    displays.is_empty()
        || displays
            .iter()
            .filter(|display| display.contains(point))
            .any(|display| {
                let horizontal = point.x - display.from.x < 4 || display.to.x - point.x < 4;
                let vertical = point.y - display.from.y < 4 || display.to.y - point.y < 4;
                if corners_only {
                    horizontal && vertical
                } else {
                    horizontal || vertical
                }
            })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stroke {
    Button(u8),
    Key { code: u8, shift: Option<u8> },
}

impl Stroke {
    fn code(self) -> u8 {
        match self {
            Self::Key { code, .. } => code,
            Self::Button(button) => button,
        }
    }

    fn keyboard(map: &Keymap, symbol: u32) -> Option<Self> {
        let (code, shifted) = map.resolve(symbol, true)?;
        let shift = if shifted {
            Some(map.resolve(key::Shift_L, false)?.0)
        } else {
            None
        };
        Some(Self::Key { code, shift })
    }

    fn events(self, down: bool) -> [(u8, u8); 2] {
        let key_event = if down {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        match self {
            Self::Button(button) => {
                let event = if down {
                    xproto::BUTTON_PRESS_EVENT
                } else {
                    xproto::BUTTON_RELEASE_EVENT
                };
                [(event, button), (0, 0)]
            }
            Self::Key { code, shift } => {
                let key = (key_event, code);
                let shift = shift.map_or((0, 0), |shift| (key_event, shift));
                if down { [shift, key] } else { [key, shift] }
            }
        }
    }

    fn ready(self, desktop: &Desktop) -> XResult<Option<Self>> {
        let pointer = desktop
            .conn
            .xinput_xi_query_pointer(desktop.root, desktop.pointer)?
            .reply()?;
        match self {
            Self::Button(button) => Ok((!pointer
                .buttons
                .first()
                .is_some_and(|state| state & (1 << button) != 0))
            .then_some(self)),
            Self::Key { code, shift } => {
                let down = desktop.conn.query_keymap()?.reply()?.keys;
                if down[usize::from(code / 8)] & (1 << (code % 8)) != 0 {
                    return Ok(None);
                }
                Ok(Some(Self::Key {
                    code,
                    shift: shift.filter(|_| pointer.mods.effective & 1 == 0),
                }))
            }
        }
    }
}

struct TextMapping {
    code: u8,
    original: Vec<u32>,
    installed: Vec<u32>,
    last_used: Instant,
}

impl TextMapping {
    fn install(desktop: &Desktop, symbol: u32) -> XResult<Self> {
        let setup = desktop.conn.setup();
        let first = setup.min_keycode;
        let count = setup.max_keycode - first + 1;
        // The caller holds the server grab through resolution and dispatch.
        let mapping = desktop.conn.get_keyboard_mapping(first, count)?.reply()?;
        let width = usize::from(mapping.keysyms_per_keycode);
        if width == 0 {
            return Err("The X11 server returned an empty keyboard layout.".into());
        }
        let (index, original) = mapping
            .keysyms
            .chunks_exact(width)
            .enumerate()
            .rev()
            .find(|(_, keys)| keys.iter().all(|&symbol| symbol == 0))
            .ok_or("No unused X11 keycode is available for this character.")?;
        let code = first + index as u8;
        let mut installed = vec![0; width];
        installed[0] = symbol;
        desktop
            .conn
            .change_keyboard_mapping(1, code, width as u8, &installed)?
            .check()?;
        let mut mapping = Self {
            code,
            original: original.to_vec(),
            installed,
            last_used: Instant::now(),
        };
        // XKB may normalize the new mapping, so compare the server's representation on cleanup.
        if let Ok(cookie) = desktop.conn.get_keyboard_mapping(code, 1)
            && let Ok(reply) = cookie.reply()
        {
            mapping.installed = reply.keysyms;
        }
        Ok(mapping)
    }
}

impl Drop for TextMapping {
    fn drop(&mut self) {
        // Give receivers time to read the MappingNotify and queued key events.
        std::thread::sleep(Duration::from_millis(100).saturating_sub(self.last_used.elapsed()));
        let restored = x11::with(|desktop| {
            let _grab = desktop.grab()?;
            let current = desktop
                .conn
                .get_keyboard_mapping(self.code, 1)?
                .reply()?
                .keysyms;
            if current == self.installed {
                desktop
                    .conn
                    .change_keyboard_mapping(
                        1,
                        self.code,
                        self.original.len() as u8,
                        &self.original,
                    )?
                    .check()?;
            }
            Ok(())
        });
        if let Err(error) = restored {
            crate::diagnostics::record(&format!(
                "Could not restore the X11 text key mapping: {error}"
            ));
        }
    }
}

fn emit(desktop: &Desktop, events: impl IntoIterator<Item = (u8, u8)>) -> XResult<()> {
    let mut requests = Vec::new();
    for (kind, code) in events.into_iter().filter(|(kind, _)| *kind != 0) {
        requests.push(
            desktop
                .conn
                .xtest_fake_input(kind, code, CURRENT_TIME, NONE, 0, 0, 0)?,
        );
    }
    // One round trip acknowledges the bounded batch before inspecting errors.
    if let Some(last) = requests.pop() {
        last.check()?;
    }
    for request in requests {
        request.check()?;
    }
    Ok(())
}

struct Release<'a> {
    desktop: &'a Desktop,
    stroke: Stroke,
    pressed: bool,
}

impl Release<'_> {
    fn release(&mut self) -> XResult<()> {
        if self.pressed {
            emit(self.desktop, self.stroke.events(false))?;
            self.pressed = false;
        }
        Ok(())
    }
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

struct X11Injector {
    input: Input,
    press: Press,
    repeats: u32,
    route: Option<Route>,
    text_mapping: Option<TextMapping>,
    stroke: Option<Stroke>,
    activation: Option<Binding>,
}

enum Attempt<'a> {
    Paused(Gate),
    End(Outcome),
    Sent(Option<Release<'a>>),
}

impl X11Injector {
    pub fn new(
        input: Input,
        double: bool,
        points: Vec<Point>,
        radius: i32,
        press: Press,
    ) -> Result<Self, String> {
        Ok(Self {
            input,
            press,
            repeats: if double { 2 } else { 1 },
            route: Route::new(points, radius)?,
            text_mapping: None,
            stroke: None,
            activation: None,
        })
    }

    pub fn with_activation(mut self, activation: Option<Binding>) -> Self {
        self.activation = activation;
        self
    }

    pub fn batchable(&self) -> bool {
        matches!(self.press, Press::Tap)
    }

    pub fn has_targets(&self) -> bool {
        self.route.is_some()
    }

    pub fn has_changing_targets(&self) -> bool {
        self.route.as_ref().is_some_and(Route::changing)
    }

    pub fn destination(&mut self) -> Result<Option<Point>, String> {
        self.route
            .as_mut()
            .map(|route| {
                route.validate()?;
                Ok(route.destination())
            })
            .transpose()
    }

    fn stroke(&mut self, desktop: &Desktop) -> XResult<Stroke> {
        let symbol = match self.input {
            Input::Mouse(button) => {
                return Ok(Stroke::Button(match button {
                    Button::Left => 1,
                    Button::Middle => 2,
                    Button::Right => 3,
                }));
            }
            Input::Key(key) => key.0,
            Input::Text(character) => Keysym::from_char(character).raw(),
        };
        if let Some(stroke) = self.stroke
            && !keys::layout_changed(desktop)?
        {
            return Ok(stroke);
        }
        let map = Keymap::read(desktop)?;
        if self.stroke.is_none() {
            keys::watch_layout(desktop)?;
        }
        let conflicts = |stroke: Stroke| {
            let code = stroke.code();
            self.activation
                .is_some_and(|binding| binding.uses_code(code))
                || [F7, F8].into_iter().any(|symbol| {
                    map.resolve(symbol, false)
                        .is_some_and(|(reserved, _)| reserved == code)
                })
        };
        let stroke = if let Some(mapping) = &self.text_mapping {
            if desktop
                .conn
                .get_keyboard_mapping(mapping.code, 1)?
                .reply()?
                .keysyms
                != mapping.installed
            {
                return Err("The X11 keyboard layout changed. Start the run again.".into());
            }
            Stroke::Key {
                code: mapping.code,
                shift: None,
            }
        } else if let Some(stroke) =
            Stroke::keyboard(&map, symbol).filter(|&stroke| !conflicts(stroke))
        {
            stroke
        } else if matches!(self.input, Input::Text(_)) {
            let mapping = TextMapping::install(desktop, symbol)?;
            let code = mapping.code;
            self.text_mapping = Some(mapping);
            Stroke::Key { code, shift: None }
        } else {
            return Err(
                "The output key is unavailable in your X11 layout or conflicts with a shortcut."
                    .into(),
            );
        };
        if self.stroke.is_some_and(|previous| previous != stroke) {
            return Err("The X11 keyboard layout changed. Start the run again.".into());
        }
        self.stroke = Some(stroke);
        Ok(stroke)
    }

    pub fn send(
        &mut self,
        actions: u32,
        control: &mut RunControl<'_>,
        mut check_destination: Option<&mut dyn FnMut(Option<Point>) -> Gate>,
    ) -> Result<Option<Outcome>, String> {
        x11::with(|desktop| {
            let duration = match self.press {
                Press::Tap => None,
                Press::Timed(duration) => Some(duration),
                Press::Held => Some(Duration::from_secs(365 * 86_400)),
            };
            let presses = if duration.is_some() { self.repeats } else { 1 };
            for index in 0..presses {
                if index != 0 && !control.can_press()? {
                    break;
                }
                let target = self.destination()?;
                let mut check = || {
                    check_destination
                        .as_mut()
                        .map_or(Gate::Open, |check| check(target))
                };
                let release = loop {
                    let taps = duration.is_none().then_some(actions * self.repeats);
                    match attempt(
                        desktop,
                        || self.stroke(desktop),
                        target,
                        taps,
                        control,
                        &mut check,
                    )? {
                        Attempt::Paused(gate) => {
                            if let Some(outcome) = control.retry_dispatch(gate)? {
                                return Ok(Some(outcome));
                            }
                        }
                        Attempt::End(outcome) => return Ok(Some(outcome)),
                        Attempt::Sent(release) => break release,
                    }
                };
                if let Some(mapping) = &mut self.text_mapping {
                    mapping.last_used = Instant::now();
                }
                if let Some(duration) = duration {
                    let mut release = release;
                    let mut held_gate = || {
                        if let Some(route) = &mut self.route
                            && let Err(reason) = route.validate()
                        {
                            return Gate::Stop(reason);
                        }
                        check()
                    };
                    let held = control.hold(duration, Some((&mut held_gate, GUARD_POLL_INTERVAL)));
                    if let Some(release) = &mut release {
                        release.release()?;
                    }
                    if !held? {
                        break;
                    }
                }
            }
            if let Some(route) = &mut self.route {
                route.advance();
            }
            Ok(None)
        })
        .map_err(|error| error.to_string())
    }
}

fn attempt<'a>(
    desktop: &'a Desktop,
    resolve_stroke: impl FnOnce() -> XResult<Stroke>,
    target: Option<Point>,
    taps: Option<u32>,
    control: &mut RunControl<'_>,
    check: &mut impl FnMut() -> Gate,
) -> XResult<Attempt<'a>> {
    let grab = desktop.grab()?;
    let gate = check();
    if !matches!(gate, Gate::Open) {
        return Ok(Attempt::Paused(gate));
    }
    if let Some(point) = target {
        let x = i16::try_from(point.x)?;
        let y = i16::try_from(point.y)?;
        desktop
            .conn
            .warp_pointer(NONE, desktop.root, 0, 0, 0, 0, x, y)?
            .check()?;
        if desktop.cursor()? != point {
            return Ok(Attempt::Paused(Gate::Paused(PauseReason::CursorConfined)));
        }
        let gate = check();
        if !matches!(gate, Gate::Open) {
            return Ok(Attempt::Paused(gate));
        }
    }
    // Process queued layout events with the server grabbed, before using the keycode.
    let stroke = resolve_stroke()?;
    let hit = if matches!(stroke, Stroke::Button(_)) {
        let hit = input_filter::hit(desktop, desktop.cursor()?)?;
        if let Hit::External(window) = hit
            && !desktop.pointer_free(window)?
        {
            return Ok(Attempt::Paused(Gate::Paused(PauseReason::InputGrabbed)));
        }
        hit
    } else {
        let pid = desktop.focus_pid()?;
        if pid == 0 || pid == std::process::id() {
            return Ok(Attempt::Paused(Gate::Paused(PauseReason::KeyboardFocus)));
        }
        if !desktop.keyboard_free()? {
            return Ok(Attempt::Paused(Gate::Paused(PauseReason::InputGrabbed)));
        }
        Hit::External(desktop.root)
    };
    let Some(stroke) = stroke.ready(desktop)? else {
        return Ok(Attempt::Paused(Gate::Paused(PauseReason::InputGrabbed)));
    };
    if let Some(outcome) = control.begin_dispatch() {
        return Ok(Attempt::End(outcome));
    }
    if let Hit::External(_) = hit {
        let mut release = Release {
            desktop,
            stroke,
            pressed: true,
        };
        if let Some(taps) = taps {
            let tap = stroke.events(true).into_iter().chain(stroke.events(false));
            emit(desktop, (0..taps).flat_map(|_| tap.clone()))?;
            release.pressed = false;
        } else {
            emit(desktop, stroke.events(true))?;
            return Ok(Attempt::Sent(Some(release)));
        }
    } else if let Hit::Client(x, y) = hit {
        drop(grab);
        input_filter::count_pad(x, y, taps.unwrap_or(1));
    }
    Ok(Attempt::Sent(None))
}
