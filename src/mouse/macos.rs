use super::{Button, Input, Point, Press, Zone, route::Route};
use crate::{
    engine::{GUARD_POLL_INTERVAL, Gate, Outcome, PauseReason, RunControl},
    input_filter, keys, macos,
};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGDisplayBounds, CGError, CGEvent, CGEventField, CGEventFlags, CGEventSource,
    CGEventSourceStateID, CGEventTapLocation, CGEventType, CGGetActiveDisplayList, CGMouseButton,
    CGWarpMouseCursorPosition,
};
use std::{
    io,
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

const INPUT_LOST: &str = "macOS input protection stopped. Check permissions and reopen Afterbeat.";
const SECURE_INPUT: &str = "macOS Secure Input is active. Leave the secure field and start again.";
const KEYBOARD_CHANGED: &str = "The macOS keyboard layout changed. Start the run again.";
static NEXT_CLICK: AtomicI64 = AtomicI64::new(1);
type DestinationCheck<'a> = dyn FnMut(Option<Point>, Point) -> Result<Option<u32>, Gate> + 'a;

fn from_quartz(point: CGPoint) -> Point {
    Point {
        x: point.x.floor() as i32,
        y: point.y.floor() as i32,
    }
}

pub fn to_quartz(point: Point) -> CGPoint {
    CGPoint::new(f64::from(point.x), f64::from(point.y))
}

fn cursor_position() -> io::Result<CGPoint> {
    let event =
        CGEvent::new(None).ok_or_else(|| io::Error::other("macOS cursor access was lost."))?;
    Ok(CGEvent::location(Some(&event)))
}

pub fn cursor() -> io::Result<Point> {
    cursor_position().map(from_quartz)
}

pub fn display_areas() -> Vec<Zone> {
    let mut displays = [0; 64];
    let mut count = 0;
    let status =
        unsafe { CGGetActiveDisplayList(displays.len() as u32, displays.as_mut_ptr(), &mut count) };
    if status != CGError::Success {
        return Vec::new();
    }
    let mut areas = displays[..(count as usize).min(displays.len())]
        .iter()
        .map(|&id| CGDisplayBounds(id))
        .filter(|bounds| !bounds.is_empty())
        .map(|bounds| Zone {
            from: from_quartz(bounds.origin),
            to: from_quartz(CGPoint::new(
                (bounds.origin.x + bounds.size.width).ceil() - 1.0,
                (bounds.origin.y + bounds.size.height).ceil() - 1.0,
            )),
        })
        .collect::<Vec<_>>();
    areas.sort_unstable_by_key(|area| (area.from.x, area.from.y, area.to.x, area.to.y));
    areas.dedup();
    areas
}

pub fn on_monitor(point: Point) -> bool {
    display_areas().iter().any(|area| area.contains(point))
}

pub fn at_edge(point: Point, corners_only: bool) -> bool {
    let areas = display_areas();
    areas.is_empty()
        || areas
            .iter()
            .filter(|area| area.contains(point))
            .any(|area| {
                let horizontal = point.x - area.from.x < 4 || area.to.x - point.x < 4;
                let vertical = point.y - area.from.y < 4 || area.to.y - point.y < 4;
                if corners_only {
                    horizontal && vertical
                } else {
                    horizontal || vertical
                }
            })
}

impl Button {
    fn quartz(self) -> (CGMouseButton, CGEventType, CGEventType) {
        match self {
            Self::Left => (
                CGMouseButton::Left,
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
            ),
            Self::Right => (
                CGMouseButton::Right,
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
            ),
            Self::Middle => (
                CGMouseButton::Center,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
            ),
        }
    }
}

thread_local! {
    static SOURCE: Option<CFRetained<CGEventSource>> = CGEventSource::new(CGEventSourceStateID::Private)
        .inspect(|source| {
            CGEventSource::set_local_events_suppression_interval(Some(source), 0.0);
            CGEventSource::set_user_data(Some(source), macos::event_tag());
        });
}

fn event_pair(
    input: Input,
    position: CGPoint,
    clicks: i64,
) -> Result<[CFRetained<CGEvent>; 2], String> {
    SOURCE.with(|source| {
        let source = source
            .as_deref()
            .ok_or("Could not create a macOS event source.")?;
        let click_number = NEXT_CLICK.fetch_add(1, Ordering::Relaxed);
        let event = |down| {
            let event = match input {
                Input::Mouse(button) => {
                    let (button, press, release) = button.quartz();
                    let event = CGEvent::new_mouse_event(
                        Some(source),
                        if down { press } else { release },
                        position,
                        button,
                    )?;
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::MouseEventClickState,
                        clicks,
                    );
                    CGEvent::set_integer_value_field(
                        Some(&event),
                        CGEventField::MouseEventNumber,
                        click_number,
                    );
                    event
                }
                Input::Key(key) => {
                    let event = CGEvent::new_keyboard_event(Some(source), key.code, down)?;
                    let flags = if key.shift {
                        CGEventFlags::MaskShift
                    } else {
                        CGEventFlags::empty()
                    };
                    CGEvent::set_flags(Some(&event), flags);
                    event
                }
                Input::Text(character) => {
                    let event = CGEvent::new_keyboard_event(Some(source), 0, down)?;
                    CGEvent::set_flags(Some(&event), CGEventFlags::empty());
                    let mut utf16 = [0; 2];
                    let text = character.encode_utf16(&mut utf16);
                    unsafe {
                        CGEvent::keyboard_set_unicode_string(
                            Some(&event),
                            text.len() as _,
                            text.as_ptr(),
                        );
                    }
                    event
                }
            };
            Some(event)
        };
        Ok([
            event(true).ok_or("Could not create a macOS input event.")?,
            event(false).ok_or("Could not create a macOS release event.")?,
        ])
    })
}

fn post(event: &CGEvent, pid: Option<u32>) {
    let mut uptime = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut uptime) } == 0 {
        let timestamp = uptime.tv_sec as u64 * 1_000_000_000 + uptime.tv_nsec as u64;
        CGEvent::set_timestamp(Some(event), timestamp);
    }
    if let Some(pid) = pid {
        CGEvent::post_to_pid(pid as i32, Some(event));
    } else {
        CGEvent::post(CGEventTapLocation::SessionEventTap, Some(event));
    }
}

struct Release {
    event: CFRetained<CGEvent>,
    pid: Option<u32>,
    track_cursor: bool,
}

impl Drop for Release {
    fn drop(&mut self) {
        if self.track_cursor
            && let Ok(position) = cursor_position()
        {
            CGEvent::set_location(Some(&self.event), position);
        }
        post(&self.event, self.pid);
    }
}

pub struct Injector {
    input: Input,
    press: Press,
    repeats: u32,
    route: Option<Route>,
}

impl Injector {
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
        })
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

    fn check_layout(&self) -> Result<(), &'static str> {
        if let Input::Key(key) = self.input
            && !key.layout_unchanged()
        {
            return Err(KEYBOARD_CHANGED);
        }
        Ok(())
    }

    pub fn send(
        &mut self,
        actions: u32,
        control: &mut RunControl<'_>,
        check: &mut DestinationCheck<'_>,
    ) -> Result<Option<Outcome>, String> {
        let duration = match self.press {
            Press::Tap => None,
            Press::Timed(duration) => Some(duration),
            Press::Held => Some(Duration::from_secs(365 * 86_400)),
        };
        let mouse = matches!(self.input, Input::Mouse(_));
        let repeats = actions * self.repeats;
        for index in 0..repeats {
            if index != 0 && !control.can_press()? {
                break;
            }
            let (target, position, pid) = loop {
                let target = self.destination()?;
                self.check_layout()?;
                match dispatch(self.input, target, check) {
                    Ok((position, pid)) => break (target, position, pid),
                    Err(gate) => {
                        if let Some(outcome) = control.retry_dispatch(gate)? {
                            return Ok(Some(outcome));
                        }
                    }
                }
            };
            let clicks = i64::from(index % self.repeats + 1);
            let [down, up] = event_pair(self.input, position, clicks)?;
            if !input_filter::ready() {
                return Err(INPUT_LOST.into());
            }
            if let Some(outcome) = control.begin_dispatch() {
                return Ok(Some(outcome));
            }
            post(&down, pid);
            let release = Release {
                event: up,
                pid,
                track_cursor: mouse && duration.is_some() && pid.is_none(),
            };
            if let Some(duration) = duration {
                let mut held_gate = || {
                    if !input_filter::ready() {
                        return Gate::Stop(INPUT_LOST);
                    }
                    if macos::secure_input_enabled() {
                        return Gate::Stop(SECURE_INPUT);
                    }
                    if let Err(reason) = self.check_layout() {
                        return Gate::Stop(reason);
                    }
                    if let Some(route) = &mut self.route
                        && let Err(reason) = route.validate()
                    {
                        return Gate::Stop(reason);
                    }
                    let Ok(current) = cursor() else {
                        return Gate::Stop("Cursor access was lost.");
                    };
                    match check(target, current) {
                        Ok(recipient) if recipient == pid => Gate::Open,
                        Ok(_) => Gate::Paused(if !mouse {
                            PauseReason::KeyboardFocus
                        } else if target.is_some() {
                            PauseReason::TargetFilter
                        } else {
                            PauseReason::PointerFilter
                        }),
                        Err(gate) => gate,
                    }
                };
                let held = control.hold(duration, Some((&mut held_gate, GUARD_POLL_INTERVAL)));
                drop(release);
                if !held? {
                    break;
                }
            }
        }
        if let Some(route) = &mut self.route {
            route.advance();
        }
        Ok(None)
    }
}

fn dispatch(
    input: Input,
    target: Option<Point>,
    check: &mut DestinationCheck<'_>,
) -> Result<(CGPoint, Option<u32>), Gate> {
    if !input_filter::ready() {
        return Err(Gate::Stop(INPUT_LOST));
    }
    if macos::secure_input_enabled() {
        return Err(Gate::Stop(SECURE_INPUT));
    }
    let physically_held = match input {
        Input::Mouse(button) => {
            CGEventSource::button_state(CGEventSourceStateID::HIDSystemState, button.quartz().0)
        }
        Input::Key(key) => keys::down(key.code),
        Input::Text(_) => false,
    };
    if physically_held {
        return Err(Gate::Paused(PauseReason::InputGrabbed));
    }
    let read_cursor = || cursor_position().map_err(|_| Gate::Stop("Cursor access was lost."));
    let mut position = read_cursor()?;
    let mut pid = check(target, from_quartz(position))?;
    if let Some(target) = target
        && from_quartz(position) != target
    {
        if CGWarpMouseCursorPosition(to_quartz(target)) != CGError::Success {
            return Err(Gate::Paused(PauseReason::CursorConfined));
        }
        position = read_cursor()?;
        if from_quartz(position) != target {
            return Err(Gate::Paused(PauseReason::CursorConfined));
        }
        pid = check(Some(target), target)?;
    }
    Ok((position, pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quartz_points_preserve_negative_display_coordinates() {
        let saved = from_quartz(CGPoint::new(-0.5, -320.8));
        assert_eq!(saved, Point { x: -1, y: -321 });
        assert_eq!(from_quartz(to_quartz(saved)), saved);
    }

    #[test]
    fn quartz_text_events_keep_utf16_and_the_protection_tag() {
        let [down, up] = event_pair(Input::Text('🦀'), CGPoint::ZERO, 1).unwrap();
        for (event, kind) in [(down, CGEventType::KeyDown), (up, CGEventType::KeyUp)] {
            assert_eq!(CGEvent::r#type(Some(&event)), kind);
            assert!(macos::is_ours(&event));
            let mut units = [0; 2];
            let mut len = 0;
            unsafe {
                CGEvent::keyboard_get_unicode_string(
                    Some(&event),
                    units.len() as _,
                    &mut len,
                    units.as_mut_ptr(),
                );
            }
            assert_eq!(String::from_utf16(&units[..len as usize]).unwrap(), "🦀");
        }
    }

    #[test]
    fn quartz_double_click_pairs_share_a_click_number() {
        let [down, up] = event_pair(Input::Mouse(Button::Middle), CGPoint::ZERO, 2).unwrap();
        assert_eq!(CGEvent::r#type(Some(&down)), CGEventType::OtherMouseDown);
        assert_eq!(CGEvent::r#type(Some(&up)), CGEventType::OtherMouseUp);
        assert!(macos::is_ours(&down) && macos::is_ours(&up));
        let number =
            |event| CGEvent::integer_value_field(Some(event), CGEventField::MouseEventNumber);
        assert_eq!(number(&down), number(&up));
        assert_eq!(
            CGEvent::integer_value_field(Some(&down), CGEventField::MouseEventClickState),
            2
        );
    }
}
