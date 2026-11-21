//! Guarded playback of native X11 recording events.
//!
//! The caller supplies the route guard. Checks, target identification, and
//! dispatch share one connection and one server grab to avoid target races.
use super::Event;
use crate::{
    engine::{Gate, Outcome, PauseReason, RunControl},
    input_filter::{self, Hit},
    mouse::{Button, Zone},
    x11::{self, Desktop, XResult},
};
use std::time::{Duration, Instant};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::Connection,
    protocol::{
        xinput::ConnectionExt as _,
        xkb::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _},
        xtest::ConnectionExt as _,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    Button(Button),
    Key(u8),
}

pub enum Attempt {
    Paused(Gate),
    End(Outcome),
    Sent,
}

pub struct Player {
    displays: Vec<Zone>,
    held: Vec<Held>,
    key_focus: Option<(u32, u32)>,
    button_target: Option<(u32, u32)>,
    layout: Option<KeyboardLayout>,
}

#[derive(PartialEq, Eq)]
struct KeyboardLayout {
    width: u8,
    symbols: Vec<u32>,
    modifiers: Vec<u8>,
    group: u8,
}

impl KeyboardLayout {
    fn read(desktop: &Desktop) -> XResult<Self> {
        let setup = desktop.conn.setup();
        let mapping = desktop
            .conn
            .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)?
            .reply()?;
        let modifiers = desktop.conn.get_modifier_mapping()?.reply()?;
        let state = desktop
            .conn
            .xkb_get_state(u16::from(xkb::ID::USE_CORE_KBD))?
            .reply()?;
        Ok(Self {
            width: mapping.keysyms_per_keycode,
            symbols: mapping.keysyms,
            modifiers: modifiers.keycodes,
            group: u8::from(state.group),
        })
    }
}

/// Execute a validated recording, checking held inputs throughout each delay.
pub fn run(
    settings: &crate::settings::Settings,
    schedule: crate::engine::Schedule,
    binding: Option<crate::hotkeys::Binding>,
    guard: &mut crate::automation::Guard,
    control: &mut RunControl<'_>,
    mut stop_pending: impl FnMut() -> bool,
) -> Result<Outcome, String> {
    use crate::targets::{Action, Step};
    let watch_keys = settings.route.iter().any(|step| matches!(step, Step::Action(step) if matches!(step.action, Action::Recorded { event: Event::Key { .. } })));
    let mut player = Player::new(watch_keys)?;
    let reserved = x11::with(|desktop| {
        let map = crate::keys::Keymap::read(desktop)?;
        Ok([crate::keys::F7, crate::keys::F8]
            .into_iter()
            .filter_map(|symbol| map.resolve(symbol, false).map(|(code, _)| code))
            .collect::<Vec<_>>())
    })
    .map_err(|error| error.to_string())?;
    for step in &settings.route {
        if let Step::Action(step) = step
            && let Action::Recorded { event } = &step.action
        {
            match event {
                Event::Key { code, .. }
                    if reserved.contains(&(*code as u8))
                        || binding.is_some_and(|binding| binding.uses_code(*code as u8)) =>
                {
                    return Err(
                        "A recorded key conflicts with an activation, stop, or capture shortcut."
                            .into(),
                    );
                }
                Event::Button { button, .. }
                    if binding.is_some_and(|binding| binding.matches_button(*button)) =>
                {
                    return Err(
                        "A recorded mouse button conflicts with the activation shortcut.".into(),
                    );
                }
                _ => {}
            }
        }
    }
    let timeout = settings.target_timeout().map_err(str::to_owned)?;
    loop {
        for step in &settings.route {
            if !player.has_held()
                && schedule
                    .limit
                    .is_some_and(|limit| control.route_actions() >= limit)
            {
                return Ok(Outcome::Completed);
            }
            let Step::Action(step) = step else {
                return Err("The recording contains another type of route step.".into());
            };
            let Action::Recorded { event } = &step.action else {
                return Err("The recording contains another type of route step.".into());
            };
            let deadline = Instant::now() + Duration::from_millis(step.delay_ms.into());
            loop {
                if stop_pending() {
                    return Ok(Outcome::Stopped);
                }
                match player.held_gate(guard)? {
                    Gate::Stop(reason) => return Ok(Outcome::Guarded(reason)),
                    Gate::Paused(_) if player.has_held() => {
                        return Ok(Outcome::Guarded(
                            "A safety condition changed while an input was held.",
                        ));
                    }
                    _ => {}
                }
                let now = Instant::now();
                if let Some(outcome) =
                    control.wait_route(deadline.min(now + Duration::from_millis(10)))?
                {
                    return Ok(outcome);
                }
                if now >= deadline {
                    break;
                }
            }
            let dispatch_deadline = Instant::now() + timeout;
            loop {
                if stop_pending() {
                    return Ok(Outcome::Stopped);
                }
                match player.send(event, guard, control, &mut stop_pending)? {
                    Attempt::Sent => {
                        if matches!(
                            event,
                            Event::Key { down: true, .. }
                                | Event::Button { down: true, .. }
                                | Event::Scroll { .. }
                        ) {
                            control.record_tap();
                        }
                        break;
                    }
                    Attempt::End(outcome) => return Ok(outcome),
                    Attempt::Paused(Gate::Stop(reason)) => return Ok(Outcome::Guarded(reason)),
                    Attempt::Paused(gate) => {
                        if player.has_held() {
                            return Ok(Outcome::Guarded(
                                "A safety condition changed while an input was held.",
                            ));
                        }
                        if Instant::now() >= dispatch_deadline {
                            return Err("The recorded input target was unavailable.".into());
                        }
                        if let Some(outcome) = control.retry_dispatch(gate)? {
                            return Ok(outcome);
                        }
                    }
                }
            }
        }
        if settings.route_once {
            return Ok(Outcome::Completed);
        }
    }
}

impl Player {
    pub fn new(watch_keys: bool) -> Result<Self, String> {
        let (displays, layout) = x11::with(|desktop| {
            if watch_keys {
                // Negotiate XKB before selecting XKB notifications on this connection.
                let _ = crate::keys::Keymap::read(desktop)?;
                crate::keys::watch_layout(desktop)?;
                let _ = crate::keys::layout_changed(desktop)?;
            }
            let layout = watch_keys
                .then(|| KeyboardLayout::read(desktop))
                .transpose()?;
            Ok((desktop.displays()?, layout))
        })
        .map_err(|error| error.to_string())?;
        if displays.is_empty() {
            return Err("The desktop did not report any displays.".into());
        }
        Ok(Self {
            displays,
            held: Vec::new(),
            key_focus: None,
            button_target: None,
            layout,
        })
    }

    pub fn has_held(&self) -> bool {
        !self.held.is_empty()
    }
    pub fn has_held_keys(&self) -> bool {
        self.held.iter().any(|item| matches!(item, Held::Key(_)))
    }
    pub fn has_held_buttons(&self) -> bool {
        self.held.iter().any(|item| matches!(item, Held::Button(_)))
    }

    /// Recheck identities and display layout while input is held between events.
    pub fn held_gate(&self, guard: &mut crate::automation::Guard) -> Result<Gate, String> {
        x11::with(|desktop| {
            if desktop.displays()? != self.displays {
                return Ok(Gate::Stop(
                    "The display layout changed during the recording.",
                ));
            }
            if let Some(layout) = &self.layout
                && crate::keys::layout_changed(desktop)?
                && *layout != KeyboardLayout::read(desktop)?
            {
                return Ok(Gate::Stop(
                    "The X11 keyboard layout changed during the recording.",
                ));
            }
            if let Some((window, pid)) = self.key_focus {
                if desktop.conn.get_input_focus()?.reply()?.focus != window
                    || desktop.focus_pid()? != pid
                {
                    return Ok(Gate::Stop(
                        "Keyboard focus changed while a recorded key was held.",
                    ));
                }
                let gate = guard.check_route_key(pid);
                if !matches!(gate, Gate::Open) {
                    return Ok(gate);
                }
            }
            if let Some((window, pid)) = self.button_target {
                if desktop.window_pid(window)? != pid {
                    return Ok(Gate::Stop(
                        "The recorded mouse target changed while a button was held.",
                    ));
                }
                let point = desktop.cursor()?;
                let gate = guard.check_destination(Some(point));
                if !matches!(gate, Gate::Open) {
                    return Ok(gate);
                }
            }
            Ok(guard.check())
        })
        .map_err(|error| error.to_string())
    }

    pub fn send(
        &mut self,
        event: &Event,
        guard: &mut crate::automation::Guard,
        control: &mut RunControl<'_>,
        stop_pending: &mut impl FnMut() -> bool,
    ) -> Result<Attempt, String> {
        x11::with(|desktop| {
            let _grab = desktop.grab()?;
            if desktop.displays()? != self.displays {
                return Ok(Attempt::End(Outcome::Guarded(
                    "The display layout changed during the recording.",
                )));
            }
            let gate = self
                .held_gate(guard)
                .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
            if !matches!(gate, Gate::Open) {
                return Ok(Attempt::Paused(gate));
            }
            let (held, down, point) = match *event {
                Event::Move(point) => (None, false, Some(point)),
                Event::Scroll { point, .. } => (None, false, Some(point)),
                Event::Button {
                    button,
                    down,
                    point,
                } => (Some(Held::Button(button)), down, Some(point)),
                Event::Key {
                    code,
                    extended,
                    down,
                } => {
                    if extended || !(8..=255).contains(&code) {
                        return Err("A recorded X11 key is invalid.".into());
                    }
                    (Some(Held::Key(code as u8)), down, None)
                }
            };
            let already_held = held.is_some_and(|item| self.held.contains(&item));
            if held.is_some() && !down && !already_held {
                return Err("The recording has an unmatched input release.".into());
            }
            if matches!(held, Some(Held::Button(_))) && down && already_held {
                return Err("A recorded button is pressed twice.".into());
            }
            let mut focus = None;
            let mut target = None;
            if let Some(point) = point {
                if !self.displays.iter().any(|display| display.contains(point)) {
                    return Err("A recorded point is outside your displays.".into());
                }
                let gate = guard.check_destination(Some(point));
                if !matches!(gate, Gate::Open) {
                    return Ok(Attempt::Paused(gate));
                }
                let Hit::External(window) = input_filter::hit(desktop, point)? else {
                    return Ok(Attempt::Paused(Gate::Paused(
                        PauseReason::TargetUnavailable,
                    )));
                };
                let pid = desktop.window_pid(window)?;
                if pid == 0 || pid == std::process::id() {
                    return Ok(Attempt::Paused(Gate::Paused(
                        PauseReason::TargetUnavailable,
                    )));
                }
                if !self.has_held_buttons() && !desktop.pointer_free(window)? {
                    return Ok(Attempt::Paused(Gate::Paused(PauseReason::InputGrabbed)));
                }
                target = Some((window, pid));
            } else {
                let window = desktop.conn.get_input_focus()?.reply()?.focus;
                let pid = desktop.focus_pid()?;
                let gate = guard.check_route_key(pid);
                if !matches!(gate, Gate::Open) {
                    return Ok(Attempt::Paused(gate));
                }
                if self.key_focus.is_some_and(|saved| saved != (window, pid)) {
                    return Ok(Attempt::Paused(Gate::Paused(PauseReason::KeyboardFocus)));
                }
                if !self.has_held_keys() && !desktop.keyboard_free()? {
                    return Ok(Attempt::Paused(Gate::Paused(PauseReason::InputGrabbed)));
                }
                focus = Some((window, pid));
            }
            if down && !already_held {
                let busy = match held {
                    Some(Held::Key(code)) => {
                        let keys = desktop.conn.query_keymap()?.reply()?.keys;
                        keys[usize::from(code / 8)] & (1 << (code % 8)) != 0
                    }
                    Some(Held::Button(button)) => {
                        let pointer = desktop
                            .conn
                            .xinput_xi_query_pointer(desktop.root, desktop.pointer)?
                            .reply()?;
                        pointer
                            .buttons
                            .first()
                            .is_some_and(|bits| bits & (1 << button.x11()) != 0)
                    }
                    None => false,
                };
                if busy {
                    return Ok(Attempt::Paused(Gate::Paused(PauseReason::InputGrabbed)));
                }
            }
            if let Some(outcome) = control.begin_dispatch() {
                return Ok(Attempt::End(outcome));
            }
            if stop_pending() {
                return Ok(Attempt::End(Outcome::Stopped));
            }
            if let Some(point) = point {
                let (x, y) = (i16::try_from(point.x)?, i16::try_from(point.y)?);
                desktop
                    .conn
                    .warp_pointer(NONE, desktop.root, 0, 0, 0, 0, x, y)?
                    .check()?;
                if desktop.cursor()? != point {
                    return Ok(Attempt::Paused(Gate::Paused(PauseReason::CursorConfined)));
                }
            }
            if let Event::Scroll {
                horizontal, delta, ..
            } = *event
            {
                let code = match (horizontal, delta > 0) {
                    (false, true) => 4,
                    (false, false) => 5,
                    (true, false) => 6,
                    (true, true) => 7,
                };
                for _ in 0..(delta.unsigned_abs() / 120) {
                    desktop
                        .conn
                        .xtest_fake_input(
                            xproto::BUTTON_PRESS_EVENT,
                            code,
                            CURRENT_TIME,
                            NONE,
                            0,
                            0,
                            0,
                        )?
                        .check()?;
                    desktop
                        .conn
                        .xtest_fake_input(
                            xproto::BUTTON_RELEASE_EVENT,
                            code,
                            CURRENT_TIME,
                            NONE,
                            0,
                            0,
                            0,
                        )?
                        .check()?;
                }
            }
            if let Some(item) = held {
                if down && !already_held {
                    self.held.push(item);
                    if matches!(item, Held::Key(_)) {
                        self.key_focus = focus;
                    }
                    if matches!(item, Held::Button(_)) && self.button_target.is_none() {
                        self.button_target = target;
                    }
                }
                emit(desktop, item, down)?;
                if !down {
                    self.held.retain(|entry| *entry != item);
                }
                if !self.has_held_keys() {
                    self.key_focus = None;
                }
                if !self.has_held_buttons() {
                    self.button_target = None;
                }
            }
            Ok(Attempt::Sent)
        })
        .map_err(|error| error.to_string())
    }
}

fn emit(desktop: &Desktop, item: Held, down: bool) -> XResult<()> {
    let (kind, code) = match item {
        Held::Button(button) => (
            if down {
                xproto::BUTTON_PRESS_EVENT
            } else {
                xproto::BUTTON_RELEASE_EVENT
            },
            button.x11(),
        ),
        Held::Key(code) => (
            if down {
                xproto::KEY_PRESS_EVENT
            } else {
                xproto::KEY_RELEASE_EVENT
            },
            code,
        ),
    };
    desktop
        .conn
        .xtest_fake_input(kind, code, CURRENT_TIME, NONE, 0, 0, 0)?
        .check()?;
    Ok(())
}

impl Drop for Player {
    fn drop(&mut self) {
        for item in self.held.drain(..).rev() {
            let _ = x11::with(|desktop| emit(desktop, item, false));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_x11_route_replays_drag_and_keys() {
        if std::env::var_os("AFTERBEAT_TEST_X11_RECORDING").is_none() {
            return;
        }
        use std::{
            io::{BufRead, BufReader, Write},
            process::{Command, Stdio},
            thread,
        };
        use x11rb::{
            COPY_DEPTH_FROM_PARENT,
            connection::Connection,
            protocol::xproto::{CreateWindowAux, EventMask, InputFocus, WindowClass},
        };
        if std::env::var_os("AFTERBEAT_X11_PLAYER_CHILD").is_some() {
            x11::with(|desktop| {
                let window = desktop.conn.generate_id()?;
                desktop
                    .conn
                    .create_window(
                        COPY_DEPTH_FROM_PARENT,
                        window,
                        desktop.root,
                        100,
                        100,
                        300,
                        300,
                        0,
                        WindowClass::INPUT_OUTPUT,
                        0,
                        &CreateWindowAux::new().event_mask(
                            EventMask::POINTER_MOTION
                                | EventMask::BUTTON_PRESS
                                | EventMask::BUTTON_RELEASE
                                | EventMask::KEY_PRESS
                                | EventMask::KEY_RELEASE,
                        ),
                    )?
                    .check()?;
                desktop.conn.map_window(window)?.check()?;
                println!("X11_WINDOW={window}");
                std::io::stdout().flush()?;
                if std::env::var_os("AFTERBEAT_X11_RECORD_CHILD").is_some() {
                    std::thread::sleep(Duration::from_secs(8));
                    return Ok(());
                }
                let deadline = Instant::now() + Duration::from_secs(8);
                let mut edges = Vec::new();
                while Instant::now() < deadline && edges.len() < 4 {
                    if let Some(event) = desktop.conn.poll_for_event()? {
                        match event {
                            x11rb::protocol::Event::ButtonPress(_) => edges.push("button-down"),
                            x11rb::protocol::Event::ButtonRelease(_) => edges.push("button-up"),
                            x11rb::protocol::Event::KeyPress(_) => edges.push("key-down"),
                            x11rb::protocol::Event::KeyRelease(_) => edges.push("key-up"),
                            _ => {}
                        }
                    } else {
                        thread::sleep(Duration::from_millis(5));
                    }
                }
                assert_eq!(edges, ["button-down", "button-up", "key-down", "key-up"]);
                Ok(())
            })
            .unwrap();
            return;
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "recording::x11_player::tests::isolated_x11_route_replays_drag_and_keys",
                "--nocapture",
            ])
            .env("AFTERBEAT_X11_PLAYER_CHILD", "1")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let window: u32 = loop {
            let line = lines
                .next()
                .expect("Child exited before creating a window")
                .unwrap();
            if let Some(value) = line.split("X11_WINDOW=").nth(1) {
                break value.trim().parse().unwrap();
            }
        };
        let own = x11::with(|desktop| {
            let own = desktop.conn.generate_id()?;
            desktop
                .conn
                .create_window(
                    COPY_DEPTH_FROM_PARENT,
                    own,
                    desktop.root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    0,
                    &CreateWindowAux::new(),
                )?
                .check()?;
            desktop
                .conn
                .set_input_focus(InputFocus::PARENT, window, CURRENT_TIME)?
                .check()?;
            Ok(own)
        })
        .unwrap();
        let previous = crate::input_filter::test_window(own);
        let point = |x, y| crate::mouse::Point { x, y };
        let events = [
            Event::Move(point(120, 120)),
            Event::Button {
                button: Button::Left,
                down: true,
                point: point(120, 120),
            },
            Event::Move(point(130, 130)),
            Event::Button {
                button: Button::Left,
                down: false,
                point: point(130, 130),
            },
            Event::Key {
                code: 38,
                extended: false,
                down: true,
            },
            Event::Key {
                code: 38,
                extended: false,
                down: false,
            },
        ];
        let settings = crate::settings::Settings {
            route_enabled: true,
            route_once: true,
            delay_seconds: 0,
            recording_origin: super::super::Origin::X11,
            route: events
                .into_iter()
                .map(|event| {
                    crate::targets::Step::Action(Box::new(crate::targets::ActionStep {
                        action: crate::targets::Action::Recorded { event },
                        target: None,
                        delay_ms: 0,
                    }))
                })
                .collect(),
            ..Default::default()
        };
        let (schedule, _, mut guard) = crate::automation::prepare(&settings, None, None).unwrap();
        let stop = crate::engine::StopSignal::new().unwrap();
        let progress = crate::engine::Progress::default();
        let result = crate::engine::run_sequence(schedule, &stop, &progress, None, |control| {
            run(&settings, schedule, None, &mut guard, control, || false)
        });
        crate::input_filter::test_window(previous);
        let status = child.wait().unwrap();
        assert_eq!(result.unwrap(), Outcome::Completed);
        assert_eq!(progress.actions(), 2);
        assert!(
            status.success(),
            "The recipient did not observe the expected input sequence"
        );
    }

    #[test]
    fn isolated_x11_player_releases_held_inputs_on_drop() {
        if std::env::var_os("AFTERBEAT_TEST_X11_RECORDING").is_none() {
            return;
        }
        let mut player = Player::new(false).unwrap();
        let key = Held::Key(38);
        let button = Held::Button(Button::Left);
        x11::with(|desktop| {
            emit(desktop, key, true)?;
            emit(desktop, button, true)?;
            Ok(())
        })
        .unwrap();
        player.held.extend([key, button]);
        drop(player);
        x11::with(|desktop| {
            let keys = desktop.conn.query_keymap()?.reply()?.keys;
            assert_eq!(keys[38 / 8] & (1 << (38 % 8)), 0);
            let pointer = desktop
                .conn
                .xinput_xi_query_pointer(desktop.root, desktop.pointer)?
                .reply()?;
            assert!(
                !pointer
                    .buttons
                    .first()
                    .is_some_and(|bits| bits & (1 << 1) != 0)
            );
            Ok(())
        })
        .unwrap();
    }
}
