//! Playback of recorded Quartz events, with held-input cleanup.
use super::Event;
use crate::{
    automation::Guard,
    engine::{Gate, Outcome, PauseReason, RunControl, Schedule},
    hotkeys::Binding,
    input_filter, macos,
    mouse::{self, Button, Point, Zone},
};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGError, CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventType,
    CGMouseButton, CGScrollEventUnit, CGWarpMouseCursorPosition,
};
use std::{
    sync::atomic::{AtomicI64, Ordering},
    time::{Duration, Instant},
};

const HARDWARE: CGEventSourceStateID = CGEventSourceStateID::HIDSystemState;
static NEXT_CLICK: AtomicI64 = AtomicI64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Held {
    Key(u16),
    Button(Button),
}

fn modifier(code: u16) -> Option<CGEventFlags> {
    match code {
        54 | 55 => Some(CGEventFlags::MaskCommand),
        56 | 60 => Some(CGEventFlags::MaskShift),
        57 => Some(CGEventFlags::MaskAlphaShift),
        58 | 61 => Some(CGEventFlags::MaskAlternate),
        59 | 62 => Some(CGEventFlags::MaskControl),
        63 => Some(CGEventFlags::MaskSecondaryFn),
        _ => None,
    }
}

fn held_flags(held: &[Held], caps: bool) -> CGEventFlags {
    let mut flags = if caps {
        CGEventFlags::MaskAlphaShift
    } else {
        CGEventFlags::empty()
    };
    for item in held {
        if let Held::Key(code) = *item
            && code != 57
            && let Some(flag) = modifier(code)
        {
            flags.insert(flag);
        }
    }
    flags
}

fn mouse_button(button: Button) -> (CGMouseButton, CGEventType, CGEventType, CGEventType) {
    match button {
        Button::Left => (
            CGMouseButton::Left,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::LeftMouseDragged,
        ),
        Button::Right => (
            CGMouseButton::Right,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::RightMouseDragged,
        ),
        Button::Middle => (
            CGMouseButton::Center,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::OtherMouseDragged,
        ),
        Button::Mouse4 => (
            CGMouseButton(3),
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::OtherMouseDragged,
        ),
        Button::Mouse5 => (
            CGMouseButton(4),
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::OtherMouseDragged,
        ),
    }
}

pub struct Player {
    source: CFRetained<CGEventSource>,
    displays: Vec<Zone>,
    held: Vec<Held>,
    keyboard_pid: Option<u32>,
    mouse_pid: Option<u32>,
    caps: bool,
    click_numbers: [Option<i64>; 5],
    click_counts: [u8; 5],
    last_click: Option<Click>,
    double_click_interval: Duration,
    layout: Option<u64>,
}

#[derive(Clone, Copy)]
struct Click {
    button: Button,
    point: Point,
    pid: u32,
    at: Instant,
    count: u8,
}

fn click_count(
    previous: Option<Click>,
    button: Button,
    point: Point,
    pid: u32,
    now: Instant,
    interval: Duration,
) -> u8 {
    previous
        .filter(|last| {
            last.button == button
                && last.pid == pid
                && point.x.abs_diff(last.point.x) <= 4
                && point.y.abs_diff(last.point.y) <= 4
                && now.saturating_duration_since(last.at) <= interval
        })
        .map_or(1, |last| last.count.saturating_add(1).min(3))
}

impl Player {
    pub fn new(watch_keys: bool, double_click_interval: Duration) -> Result<Self, String> {
        let source = CGEventSource::new(CGEventSourceStateID::Private)
            .ok_or("Could not create a macOS event source.")?;
        CGEventSource::set_local_events_suppression_interval(Some(&source), 0.0);
        CGEventSource::set_user_data(Some(&source), macos::event_tag());
        let displays = mouse::display_areas();
        if displays.is_empty() {
            return Err("The desktop did not report any displays.".into());
        }
        let caps = CGEventSource::flags_state(HARDWARE).contains(CGEventFlags::MaskAlphaShift);
        let layout = watch_keys.then(crate::keys::current_layout);
        if layout == Some(0) {
            return Err("The macOS keyboard layout is unavailable.".into());
        }
        Ok(Self {
            source,
            displays,
            held: Vec::new(),
            keyboard_pid: None,
            mouse_pid: None,
            caps,
            click_numbers: [None; 5],
            click_counts: [1; 5],
            last_click: None,
            double_click_interval,
            layout,
        })
    }

    fn flags(&self) -> CGEventFlags {
        held_flags(&self.held, self.caps)
    }

    pub fn has_held(&self) -> bool {
        !self.held.is_empty()
    }
    fn held_keys(&self) -> bool {
        self.held.iter().any(|held| matches!(held, Held::Key(_)))
    }
    fn held_buttons(&self) -> bool {
        self.held.iter().any(|held| matches!(held, Held::Button(_)))
    }

    fn safety(&self, guard: &mut Guard) -> Result<Gate, String> {
        if !input_filter::ready() || !macos::has_permissions() {
            return Ok(Gate::Stop("macOS input protection stopped."));
        }
        if macos::secure_input_enabled() {
            return Ok(Gate::Stop("macOS Secure Input is active."));
        }
        if mouse::display_areas() != self.displays {
            return Ok(Gate::Stop(
                "The display layout changed during the recording.",
            ));
        }
        if self
            .layout
            .is_some_and(|layout| layout != crate::keys::current_layout())
        {
            return Ok(Gate::Stop(
                "The macOS keyboard layout changed during the recording.",
            ));
        }
        if let Some(pid) = self.keyboard_pid {
            if macos::foreground_pid() != pid {
                return Ok(Gate::Stop(
                    "Keyboard focus changed while a recorded key was held.",
                ));
            }
            let gate = guard.check_route_key(pid);
            if !matches!(gate, Gate::Open) {
                return Ok(gate);
            }
        }
        if self.held_buttons() {
            let cursor = mouse::cursor().map_err(|error| error.to_string())?;
            let gate = guard.check_destination(Some(cursor));
            if !matches!(gate, Gate::Open) {
                return Ok(gate);
            }
        }
        Ok(guard.check())
    }

    fn send(
        &mut self,
        event: &Event,
        guard: &mut Guard,
        control: &mut RunControl<'_>,
        stop_pending: &mut impl FnMut() -> bool,
    ) -> Result<Attempt, String> {
        let gate = self.safety(guard)?;
        if !matches!(gate, Gate::Open) {
            return Ok(Attempt::Paused(gate));
        }
        let (item, down, point) = match *event {
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
            } if code <= 127 && !extended => (Some(Held::Key(code as u16)), down, None),
            Event::Key { .. } => return Err("A recorded macOS key is invalid.".into()),
        };
        let held = item.is_some_and(|item| self.held.contains(&item));
        if item.is_some() && !down && !held {
            return Err("The recording has an unmatched input release.".into());
        }
        if down && held && matches!(item, Some(Held::Button(_))) {
            return Err("A recorded button is pressed twice.".into());
        }
        if down && !held {
            let busy = match item {
                Some(Held::Key(code)) => CGEventSource::key_state(HARDWARE, code),
                Some(Held::Button(button)) => {
                    CGEventSource::button_state(HARDWARE, mouse_button(button).0)
                }
                None => false,
            };
            if busy {
                return Ok(Attempt::Paused(Gate::Paused(PauseReason::InputGrabbed)));
            }
        }
        let mut expected_target = None;
        let pid = if let Some(point) = point {
            if !self.displays.iter().any(|display| display.contains(point)) {
                return Err("A recorded point is outside your displays.".into());
            }
            let gate = guard.check_destination(Some(point));
            if !matches!(gate, Gate::Open) {
                return Ok(Attempt::Paused(gate));
            }
            let pid = macos::window_pid(mouse::to_quartz(point));
            if pid == 0 || pid == std::process::id() {
                return Ok(Attempt::Paused(Gate::Paused(
                    PauseReason::TargetUnavailable,
                )));
            }
            expected_target = Some(pid);
            self.mouse_pid.unwrap_or(pid)
        } else {
            let pid = macos::foreground_pid();
            let gate = guard.check_route_key(pid);
            if !matches!(gate, Gate::Open) {
                return Ok(Attempt::Paused(gate));
            }
            if self.keyboard_pid.is_some_and(|saved| saved != pid) {
                return Ok(Attempt::Paused(Gate::Paused(PauseReason::KeyboardFocus)));
            }
            pid
        };
        if let Some(outcome) = control.begin_dispatch() {
            return Ok(Attempt::End(outcome));
        }
        if stop_pending() {
            return Ok(Attempt::End(Outcome::Stopped));
        }
        if let Some(point) = point {
            if CGWarpMouseCursorPosition(mouse::to_quartz(point)) != CGError::Success
                || mouse::cursor().map_err(|error| error.to_string())? != point
            {
                return Ok(Attempt::Paused(Gate::Paused(PauseReason::CursorConfined)));
            }
            if Some(macos::window_pid(mouse::to_quartz(point))) != expected_target
                || !matches!(guard.check_destination(Some(point)), Gate::Open)
            {
                return Ok(Attempt::End(Outcome::Guarded(
                    "The recorded mouse target changed before input was sent.",
                )));
            }
        } else if macos::foreground_pid() != pid
            || !matches!(guard.check_route_key(pid), Gate::Open)
        {
            return Ok(Attempt::End(Outcome::Guarded(
                "Keyboard focus changed before recorded input was sent.",
            )));
        }
        if macos::secure_input_enabled() || !input_filter::ready() || !macos::has_permissions() {
            return Ok(Attempt::End(Outcome::Guarded(
                "macOS input protection changed before recorded input was sent.",
            )));
        }
        if let Event::Scroll {
            horizontal, delta, ..
        } = *event
        {
            let lines = delta / 120;
            let event = CGEvent::new_scroll_wheel_event2(
                Some(&self.source),
                CGScrollEventUnit::Line,
                2,
                if horizontal { 0 } else { lines },
                if horizontal { -lines } else { 0 },
                0,
            )
            .ok_or("Could not create a recorded scroll event.")?;
            if let Some(point) = point {
                CGEvent::set_location(Some(&event), mouse::to_quartz(point));
            }
            CGEvent::set_flags(Some(&event), self.flags());
            CGEvent::post_to_pid(pid as i32, Some(&event));
        } else if let Some(item) = item {
            if down && !held {
                self.held.push(item);
                if matches!(item, Held::Key(_)) {
                    self.keyboard_pid = Some(pid);
                }
                if matches!(item, Held::Button(_)) && self.mouse_pid.is_none() {
                    self.mouse_pid = Some(pid);
                }
                if let Held::Button(button) = item {
                    self.click_numbers[button.index() as usize] =
                        Some(NEXT_CLICK.fetch_add(1, Ordering::Relaxed));
                    if let Some(point) = point {
                        let now = Instant::now();
                        let count = click_count(
                            self.last_click,
                            button,
                            point,
                            pid,
                            now,
                            self.double_click_interval,
                        );
                        self.click_counts[button.index() as usize] = count;
                        self.last_click = Some(Click {
                            button,
                            point,
                            pid,
                            at: now,
                            count,
                        });
                    }
                }
                if item == Held::Key(57) {
                    self.caps = !self.caps;
                }
            } else if !down {
                self.held.retain(|entry| *entry != item);
            }
            let event = match self.edge(
                item,
                down,
                point.map(mouse::to_quartz).unwrap_or(CGPoint::ZERO),
            ) {
                Ok(event) => event,
                Err(error) => {
                    if !down {
                        self.held.push(item);
                    }
                    return Err(error);
                }
            };
            if down && held && matches!(item, Held::Key(_)) {
                CGEvent::set_integer_value_field(
                    Some(&event),
                    CGEventField::KeyboardEventAutorepeat,
                    1,
                );
            }
            CGEvent::post_to_pid(pid as i32, Some(&event));
            if !down && let Held::Button(button) = item {
                self.click_numbers[button.index() as usize] = None;
                if let (Some(point), Some(last)) = (point, self.last_click)
                    && last.button == button
                    && (point.x.abs_diff(last.point.x) > 4 || point.y.abs_diff(last.point.y) > 4)
                {
                    self.last_click = None;
                }
            }
            if !self.held_keys() {
                self.keyboard_pid = None;
            }
            if !self.held_buttons() {
                self.mouse_pid = None;
            }
        } else if let Some(point) = point {
            let held_button = self.held.iter().find_map(|held| {
                if let Held::Button(button) = held {
                    Some(*button)
                } else {
                    None
                }
            });
            let (button, kind) =
                held_button.map_or((CGMouseButton::Left, CGEventType::MouseMoved), |button| {
                    let (native, _, _, drag) = mouse_button(button);
                    (native, drag)
                });
            let event =
                CGEvent::new_mouse_event(Some(&self.source), kind, mouse::to_quartz(point), button)
                    .ok_or("Could not create a recorded mouse event.")?;
            CGEvent::set_flags(Some(&event), self.flags());
            CGEvent::post_to_pid(pid as i32, Some(&event));
        }
        Ok(Attempt::Sent)
    }

    fn edge(&self, item: Held, down: bool, point: CGPoint) -> Result<CFRetained<CGEvent>, String> {
        let event = match item {
            Held::Key(code) => {
                let event = CGEvent::new_keyboard_event(Some(&self.source), code, down)
                    .ok_or("Could not create a recorded key event.")?;
                if modifier(code).is_some() {
                    CGEvent::set_type(Some(&event), CGEventType::FlagsChanged);
                }
                event
            }
            Held::Button(button) => {
                let (native, press, release, _) = mouse_button(button);
                let event = CGEvent::new_mouse_event(
                    Some(&self.source),
                    if down { press } else { release },
                    point,
                    native,
                )
                .ok_or("Could not create a recorded mouse event.")?;
                CGEvent::set_integer_value_field(
                    Some(&event),
                    CGEventField::MouseEventClickState,
                    i64::from(self.click_counts[button.index() as usize]),
                );
                CGEvent::set_integer_value_field(
                    Some(&event),
                    CGEventField::MouseEventNumber,
                    self.click_numbers[button.index() as usize].unwrap_or(0),
                );
                event
            }
        };
        CGEvent::set_flags(Some(&event), self.flags());
        Ok(event)
    }
}

enum Attempt {
    Paused(Gate),
    End(Outcome),
    Sent,
}

impl Drop for Player {
    fn drop(&mut self) {
        while let Some(item) = self.held.pop() {
            let pid = match item {
                Held::Key(_) => self.keyboard_pid,
                Held::Button(_) => self.mouse_pid,
            };
            let point = mouse::cursor()
                .map(mouse::to_quartz)
                .unwrap_or(CGPoint::ZERO);
            if let (Some(pid), Ok(event)) = (pid, self.edge(item, false, point)) {
                CGEvent::post_to_pid(pid as i32, Some(&event));
            }
        }
    }
}

pub fn run(
    settings: &crate::settings::Settings,
    schedule: Schedule,
    binding: Option<Binding>,
    double_click_interval: Duration,
    guard: &mut Guard,
    control: &mut RunControl<'_>,
    mut stop_pending: impl FnMut() -> bool,
) -> Result<Outcome, String> {
    use crate::targets::{Action, Step};
    for step in &settings.route {
        if let Step::Action(step) = step
            && let Action::Recorded { event } = &step.action
        {
            match event {
                Event::Key { code, .. }
                    if *code == u32::from(crate::keys::F7)
                        || *code == u32::from(crate::keys::F8)
                        || binding.is_some_and(|binding| binding.uses_code(*code as u16)) =>
                {
                    return Err("A recorded key conflicts with a shortcut.".into());
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
    let watch_keys = settings.route.iter().any(|step| matches!(step, Step::Action(step) if matches!(step.action, Action::Recorded { event: Event::Key { .. } })));
    let mut player = Player::new(watch_keys, double_click_interval)?;
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
                match player.safety(guard)? {
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
            let deadline = Instant::now() + timeout;
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
                        if Instant::now() >= deadline {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_keep_a_modifier_active_until_both_sides_are_released() {
        let mut held = vec![Held::Key(56), Held::Key(60), Held::Key(59)];
        assert!(
            held_flags(&held, false).contains(CGEventFlags::MaskShift | CGEventFlags::MaskControl)
        );
        held.remove(0);
        assert!(held_flags(&held, false).contains(CGEventFlags::MaskShift));
        held.remove(0);
        assert!(!held_flags(&held, false).contains(CGEventFlags::MaskShift));
        assert!(held_flags(&held, true).contains(CGEventFlags::MaskAlphaShift));
    }

    #[test]
    fn nearby_clicks_obey_the_system_interval_and_target() {
        let now = Instant::now();
        let point = Point { x: 50, y: 60 };
        let first = Click {
            button: Button::Left,
            point,
            pid: 12,
            at: now,
            count: 1,
        };
        let interval = Duration::from_millis(500);
        assert_eq!(
            click_count(
                Some(first),
                Button::Left,
                point,
                12,
                now + Duration::from_millis(100),
                interval
            ),
            2
        );
        assert_eq!(
            click_count(
                Some(first),
                Button::Left,
                point,
                12,
                now + Duration::from_millis(501),
                interval
            ),
            1
        );
        assert_eq!(
            click_count(Some(first), Button::Left, point, 13, now, interval),
            1
        );
        assert_eq!(
            click_count(
                Some(first),
                Button::Left,
                Point { x: 60, y: 60 },
                12,
                now,
                interval
            ),
            1
        );
    }

    #[test]
    fn creates_modifier_and_mouse_edges_with_the_expected_metadata() {
        let source = CGEventSource::new(CGEventSourceStateID::Private).unwrap();
        CGEventSource::set_user_data(Some(&source), macos::event_tag());
        let mut player = Player {
            source,
            displays: Vec::new(),
            held: vec![Held::Key(56)],
            keyboard_pid: None,
            mouse_pid: None,
            caps: false,
            click_numbers: [None; 5],
            click_counts: [1; 5],
            last_click: None,
            double_click_interval: Duration::from_millis(500),
            layout: None,
        };
        let shift = player.edge(Held::Key(56), true, CGPoint::ZERO).unwrap();
        assert_eq!(CGEvent::r#type(Some(&shift)), CGEventType::FlagsChanged);
        assert!(CGEvent::flags(Some(&shift)).contains(CGEventFlags::MaskShift));
        assert!(macos::is_ours(&shift));
        player.click_numbers[Button::Left.index() as usize] = Some(42);
        player.click_counts[Button::Left.index() as usize] = 2;
        let click = player
            .edge(Held::Button(Button::Left), true, CGPoint::ZERO)
            .unwrap();
        assert_eq!(CGEvent::r#type(Some(&click)), CGEventType::LeftMouseDown);
        assert_eq!(
            CGEvent::integer_value_field(Some(&click), CGEventField::MouseEventNumber),
            42
        );
        assert_eq!(
            CGEvent::integer_value_field(Some(&click), CGEventField::MouseEventClickState),
            2
        );
        // No events were posted; prevent the synthetic test state from releasing anything.
        player.held.clear();
    }
}
