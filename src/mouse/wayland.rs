use super::{Button, Input, Point, Press, route::Route};
use crate::{
    engine::{GUARD_POLL_INTERVAL, Gate, Outcome, PauseReason, RunControl},
    wayland::{self, Remote},
};
use std::{sync::Arc, time::Duration};

pub struct Injector {
    remote: Arc<Remote>,
    route: Option<Route>,
    press: Press,
    keyboard: bool,
    code: i32,
    repeats: u32,
    wheel: Option<(crate::targets::Direction, u8)>,
}

impl Injector {
    pub fn new(
        input: Input,
        repeats: u8,
        points: Vec<Point>,
        radius: i32,
        press: Press,
    ) -> Result<Self, String> {
        let remote = wayland::remote().ok_or("Enable Wayland input before starting.")?;
        let (keyboard, code) = match input {
            Input::Mouse(button) => (
                false,
                match button {
                    Button::Left => 0x110,
                    Button::Right => 0x111,
                    Button::Middle => 0x112,
                    Button::Mouse4 => 0x113,
                    Button::Mouse5 => 0x114,
                },
            ),
            Input::Key(key) => (true, key.0 as i32),
            Input::Text(ch) => (true, xkeysym::Keysym::from_char(ch).raw() as i32),
            Input::Wheel(..) => (false, 0),
        };
        Ok(Self {
            remote,
            route: Route::new(points, radius)?,
            press,
            keyboard,
            code,
            repeats: u32::from(repeats),
            wheel: match input {
                Input::Wheel(direction, ticks) => Some((direction, ticks)),
                _ => None,
            },
        })
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

    fn ready(&self) -> Gate {
        if !wayland::can_dispatch(&self.remote) {
            return Gate::Stop(
                "Wayland input permission or the emergency shortcut became unavailable.",
            );
        }
        if self.keyboard && wayland::focused() {
            return Gate::Paused(PauseReason::KeyboardFocus);
        }
        Gate::Open
    }

    pub fn send(
        &mut self,
        control: &mut RunControl<'_>,
        mut check_destination: Option<&mut dyn FnMut(Option<Point>) -> Gate>,
    ) -> Result<Option<Outcome>, String> {
        let target = self.destination()?;
        let duration = match self.press {
            Press::Tap => None,
            Press::Timed(duration) => Some(duration),
            Press::Held => Some(Duration::from_secs(365 * 86_400)),
        };
        for _ in 0..self.repeats {
            let mut movement = target;
            loop {
                let mut gate = self.ready();
                if matches!(gate, Gate::Open)
                    && let Some(check) = &mut check_destination
                {
                    gate = check(target);
                }
                if matches!(gate, Gate::Open) {
                    if let Some(point) = movement.take() {
                        if !control.allow_press(Gate::Open) {
                            return Ok(control.begin_dispatch());
                        }
                        self.remote
                            .move_to(point)
                            .map_err(|error| error.to_string())?;
                        continue;
                    }
                    break;
                }
                if let Some(outcome) = control.retry_dispatch(gate)? {
                    return Ok(Some(outcome));
                }
            }
            if self.wheel.is_some() && wayland::pointer_over_own_window() {
                return Ok(Some(Outcome::Guarded(
                    "Move the pointer outside Afterbeat before sending wheel input.",
                )));
            }
            if let Some(outcome) = control.begin_dispatch() {
                return Ok(Some(outcome));
            }
            if let Some((direction, ticks)) = self.wheel {
                self.remote.scroll(direction, ticks).map_err(|error| {
                    format!("The Wayland portal could not send wheel input. {error}")
                })?;
                break;
            }
            let local = target.is_none() && !self.keyboard && wayland::click_own_window();
            let mut release = Release {
                remote: &self.remote,
                keyboard: self.keyboard,
                code: self.code,
                pressed: !local,
            };
            if !local {
                self.remote
                    .send(self.keyboard, self.code, true)
                    .map_err(|error| error.to_string())?;
            }
            let pause = duration.or_else(|| local.then_some(Duration::from_millis(1)));
            let held = if let Some(duration) = pause {
                let mut gate = || {
                    let ready = self.ready();
                    if !matches!(ready, Gate::Open) {
                        return ready;
                    }
                    check_destination
                        .as_mut()
                        .map_or(Gate::Open, |check| check(target))
                };
                control.hold(duration, Some((&mut gate, GUARD_POLL_INTERVAL)))
            } else {
                Ok(true)
            };
            release.up()?;
            if !held? {
                break;
            }
        }
        if let Some(route) = &mut self.route {
            route.advance();
        }
        Ok(None)
    }
}

struct Release<'a> {
    remote: &'a Remote,
    keyboard: bool,
    code: i32,
    pressed: bool,
}

impl Release<'_> {
    fn up(&mut self) -> Result<(), String> {
        if self.pressed {
            if let Err(error) = self.remote.send(self.keyboard, self.code, false) {
                self.remote.close();
                self.pressed = false;
                return Err(format!("Could not release the Wayland input. {error}"));
            }
            self.pressed = false;
        }
        Ok(())
    }
}

impl Drop for Release<'_> {
    fn drop(&mut self) {
        let _ = self.up();
    }
}
