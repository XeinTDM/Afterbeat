//! In-memory recording timeline. Capture backends must filter injected events and
//! recorder controls before passing events here.
use crate::mouse::{Button, Point};
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

/// The first emergency stop timestamp, in the shortcut clock's milliseconds.
pub struct RecordingStop(AtomicU64);

impl Default for RecordingStop {
    fn default() -> Self {
        Self(AtomicU64::new(u64::MAX))
    }
}

impl RecordingStop {
    pub fn request(&self, tick: u64) {
        self.0.fetch_min(tick, Ordering::AcqRel);
    }
    pub fn reset(&self) {
        self.0.store(u64::MAX, Ordering::Release);
    }
    pub fn timestamp(&self) -> Option<u64> {
        let tick = self.0.load(Ordering::Acquire);
        (tick != u64::MAX).then_some(tick)
    }
}

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub mod macos_player;
#[cfg(windows)]
pub mod windows;
#[cfg(target_os = "linux")]
pub mod x11;
#[cfg(target_os = "linux")]
pub mod x11_player;

/// Recorded key codes and pointer coordinates use the originating desktop's
/// native conventions and must not be replayed on another backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    #[default]
    Windows,
    X11,
    Macos,
}

pub fn current_origin() -> Option<Origin> {
    #[cfg(windows)]
    {
        Some(Origin::Windows)
    }
    #[cfg(target_os = "macos")]
    {
        Some(Origin::Macos)
    }
    #[cfg(target_os = "linux")]
    {
        (!crate::wayland::active()).then_some(Origin::X11)
    }
}

pub fn valid_key_code(origin: Origin, code: u32, extended: bool) -> bool {
    match origin {
        Origin::Windows => (1..=255).contains(&code),
        Origin::X11 => (8..=255).contains(&code) && !extended,
        Origin::Macos => code <= 127 && !extended,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Move(Point),
    Scroll {
        point: Point,
        horizontal: bool,
        delta: i32,
    },
    Button {
        button: Button,
        down: bool,
        point: Point,
    },
    Key {
        code: u32,
        extended: bool,
        down: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Elapsed active recording time before this event, in milliseconds.
    pub delay_ms: u64,
    pub event: Event,
}

/// Refuse malformed edited timelines before injecting any input.
#[cfg(test)]
pub fn validate(entries: &[Entry]) -> Result<(), &'static str> {
    validate_for(entries, Origin::Windows)
}

pub fn validate_for(entries: &[Entry], origin: Origin) -> Result<(), &'static str> {
    let mut buttons = Vec::new();
    let mut keys = Vec::new();
    for entry in entries {
        if entry.delay_ms > 86_400_000 {
            return Err("A recording delay must be at most 24 hours.");
        }
        match entry.event {
            Event::Move(_) => {}
            Event::Scroll { delta, .. } if delta == 0 || delta.unsigned_abs() > 32768 => {
                return Err("A recorded scroll amount is invalid.");
            }
            Event::Scroll { delta, .. } if origin != Origin::Windows && delta % 120 != 0 => {
                return Err("This desktop requires scroll amounts in multiples of 120.");
            }
            Event::Scroll { .. } => {}
            Event::Button { button, down, .. } => {
                if down {
                    if buttons.contains(&button) {
                        return Err("A mouse button is pressed twice without a release.");
                    }
                    buttons.push(button);
                } else if let Some(index) = buttons.iter().position(|held| *held == button) {
                    buttons.remove(index);
                } else {
                    return Err("A mouse button release has no matching press.");
                }
            }
            Event::Key {
                code,
                extended,
                down,
            } => {
                if !valid_key_code(origin, code, extended) {
                    return Err("A recorded key is invalid.");
                }
                let key = (code, extended);
                if down {
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                } else if let Some(index) = keys.iter().position(|held| *held == key) {
                    keys.remove(index);
                } else {
                    return Err("A key release has no matching press.");
                }
            }
        }
    }
    if !buttons.is_empty() || !keys.is_empty() {
        return Err("Every recorded press needs a matching release.");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureOptions {
    pub movement: bool,
    pub buttons: bool,
    pub keys: bool,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            movement: true,
            buttons: true,
            keys: false,
        }
    }
}

/// `now` values use a monotonic clock relative to the start of the capture.
pub struct Recorder {
    options: CaptureOptions,
    entries: Vec<Entry>,
    last: Duration,
    remainder: Duration,
    paused_at: Option<Duration>,
    buttons: Vec<Button>,
    keys: Vec<(u32, bool)>,
    last_point: Point,
    stop_requested: Option<(Arc<RecordingStop>, u64, Duration)>,
}

impl Recorder {
    pub fn new(options: CaptureOptions, now: Duration) -> Self {
        Self {
            options,
            entries: Vec::new(),
            last: now,
            remainder: Duration::ZERO,
            paused_at: None,
            buttons: Vec::new(),
            keys: Vec::new(),
            last_point: Point { x: 0, y: 0 },
            stop_requested: None,
        }
    }

    pub fn with_stop_signal(
        options: CaptureOptions,
        now: Duration,
        stop_requested: Arc<RecordingStop>,
    ) -> Self {
        Self::with_stop_at(options, now, stop_requested, crate::hotkeys::tick_count())
    }

    fn with_stop_at(
        options: CaptureOptions,
        now: Duration,
        stop_requested: Arc<RecordingStop>,
        tick: u64,
    ) -> Self {
        let mut recorder = Self::new(options, now);
        recorder.stop_requested = Some((stop_requested, tick, now));
        recorder
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn stop_requested(&self) -> bool {
        self.stop_requested
            .as_ref()
            .is_some_and(|(signal, _, _)| signal.timestamp().is_some())
    }

    fn stop_boundary(&self, now: Duration) -> Duration {
        self.stop_requested
            .as_ref()
            .and_then(|(signal, tick, start)| {
                signal.timestamp().map(|stopped| {
                    start.saturating_add(Duration::from_millis(stopped.saturating_sub(*tick)))
                })
            })
            .map_or(now, |stopped| now.min(stopped))
    }

    pub fn record(&mut self, event: Event, now: Duration) {
        if self.paused_at.is_some() || self.stop_requested() {
            return;
        }
        match &event {
            Event::Move(point) | Event::Button { point, .. } | Event::Scroll { point, .. } => {
                self.last_point = *point
            }
            Event::Key { .. } => {}
        }
        let accept = match &event {
            Event::Move(_) => self.options.movement,
            Event::Scroll { .. } => self.options.buttons,
            Event::Button { button, down, .. } => {
                if !self.options.buttons {
                    false
                } else if *down && !self.buttons.contains(button) {
                    self.buttons.push(*button);
                    true
                } else if !*down && self.buttons.contains(button) {
                    self.buttons.retain(|held| held != button);
                    true
                } else {
                    false
                }
            }
            Event::Key {
                code,
                extended,
                down,
            } => {
                let key = (*code, *extended);
                if !self.options.keys {
                    false
                } else if *down {
                    if !self.keys.contains(&key) {
                        self.keys.push(key);
                    }
                    true
                } else if !*down && self.keys.contains(&key) {
                    self.keys.retain(|held| *held != key);
                    true
                } else {
                    false
                }
            }
        };
        if accept {
            self.push(event, now);
        }
    }

    fn push(&mut self, event: Event, now: Duration) {
        let now = now.max(self.last);
        let elapsed = now.saturating_sub(self.last).saturating_add(self.remainder);
        let millis = elapsed.as_millis().min(u64::MAX as u128) as u64;
        self.remainder = elapsed.saturating_sub(Duration::from_millis(millis));
        self.entries.push(Entry {
            delay_ms: millis,
            event,
        });
        self.last = now;
    }

    /// Close held inputs at the boundary, so a pause or stop cannot leave a
    /// recorded key or button held indefinitely during playback.
    pub fn pause(&mut self, now: Duration) {
        if self.paused_at.is_some() {
            return;
        }
        let now = self.stop_boundary(now);
        self.exclude_buttons(now);
        self.exclude_keys(now);
        self.paused_at = Some(now.max(self.last));
    }

    /// End recorded holds before entering a region excluded from capture.
    pub fn exclude_buttons(&mut self, now: Duration) {
        if self.paused_at.is_some() {
            return;
        }
        let now = self.stop_boundary(now);
        let point = self.last_point;
        for button in std::mem::take(&mut self.buttons) {
            self.push(
                Event::Button {
                    button,
                    down: false,
                    point,
                },
                now,
            );
        }
    }

    pub fn exclude_keys(&mut self, now: Duration) {
        if self.paused_at.is_some() {
            return;
        }
        let now = self.stop_boundary(now);
        for (code, extended) in std::mem::take(&mut self.keys) {
            self.push(
                Event::Key {
                    code,
                    extended,
                    down: false,
                },
                now,
            );
        }
    }

    pub fn resume(&mut self, now: Duration) {
        if let Some(paused_at) = self.paused_at.take() {
            self.last += now.saturating_sub(paused_at);
        }
    }

    pub fn finish(mut self, now: Duration) -> Vec<Entry> {
        self.pause(now);
        self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn scroll_follows_mouse_option_and_paused_time() {
        let point = Point { x: 12, y: 20 };
        let scroll = Event::Scroll {
            point,
            horizontal: false,
            delta: -120,
        };
        let mut recorder = Recorder::new(CaptureOptions::default(), ms(0));
        recorder.record(scroll.clone(), ms(10));
        recorder.pause(ms(20));
        recorder.record(scroll.clone(), ms(30));
        recorder.resume(ms(120));
        recorder.record(scroll.clone(), ms(130));
        let entries = recorder.finish(ms(140));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].delay_ms, 10);
        assert_eq!(entries[1].delay_ms, 20);
        assert_eq!(validate_for(&entries, Origin::X11), Ok(()));
        let mut excluded = Recorder::new(
            CaptureOptions {
                buttons: false,
                ..CaptureOptions::default()
            },
            ms(0),
        );
        excluded.record(scroll, ms(5));
        assert!(excluded.entries().is_empty());
    }

    #[test]
    fn emergency_stop_rejects_new_events_before_ui_finishes_capture() {
        let stop = Arc::new(RecordingStop::default());
        let mut recorder = Recorder::with_stop_at(
            CaptureOptions {
                keys: true,
                ..CaptureOptions::default()
            },
            ms(0),
            stop.clone(),
            1_000,
        );
        recorder.record(
            Event::Key {
                code: 65,
                extended: false,
                down: true,
            },
            ms(10),
        );
        stop.request(1_015);
        stop.request(1_018);
        recorder.record(
            Event::Key {
                code: 66,
                extended: false,
                down: true,
            },
            ms(11),
        );
        recorder.record(Event::Move(Point { x: 10, y: 20 }), ms(12));
        let entries = recorder.finish(ms(20));
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[1].delay_ms, 5,
            "The release uses the first stop time, not the later UI time"
        );
        assert!(matches!(
            entries[1].event,
            Event::Key {
                code: 65,
                down: false,
                ..
            }
        ));
        assert_eq!(validate(&entries), Ok(()));
    }

    #[test]
    fn keys_are_excluded_by_default_and_do_not_reset_timing() {
        let mut recorder = Recorder::new(CaptureOptions::default(), ms(0));
        recorder.record(
            Event::Key {
                code: 65,
                extended: false,
                down: true,
            },
            ms(10),
        );
        recorder.record(Event::Move(Point { x: 2, y: 3 }), ms(30));
        assert_eq!(recorder.entries()[0].delay_ms, 30);
        assert_eq!(recorder.entries().len(), 1);
    }

    #[test]
    fn pause_balances_inputs_and_excludes_paused_time() {
        let mut recorder = Recorder::new(
            CaptureOptions {
                keys: true,
                ..CaptureOptions::default()
            },
            ms(0),
        );
        recorder.record(
            Event::Key {
                code: 65,
                extended: false,
                down: true,
            },
            ms(10),
        );
        recorder.pause(ms(20));
        recorder.record(Event::Move(Point { x: 1, y: 1 }), ms(30));
        recorder.resume(ms(120));
        recorder.record(
            Event::Key {
                code: 65,
                extended: false,
                down: false,
            },
            ms(125),
        );
        recorder.record(Event::Move(Point { x: 2, y: 2 }), ms(130));
        assert_eq!(recorder.entries().len(), 3);
        assert_eq!(
            recorder.entries()[1].event,
            Event::Key {
                code: 65,
                extended: false,
                down: false
            }
        );
        assert_eq!(recorder.entries()[2].delay_ms, 10);
    }

    #[test]
    fn repeats_and_orphan_releases_are_not_recorded() {
        let mut recorder = Recorder::new(CaptureOptions::default(), ms(0));
        let down = Event::Button {
            button: Button::Left,
            down: true,
            point: Point { x: 5, y: 8 },
        };
        let up = Event::Button {
            button: Button::Left,
            down: false,
            point: Point { x: 5, y: 8 },
        };
        recorder.record(up.clone(), ms(1));
        recorder.record(down.clone(), ms(2));
        recorder.record(down, ms(3));
        let entries = recorder.finish(ms(4));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].event, up);
        assert_eq!(validate(&entries), Ok(()));
        assert!(validate(&entries[..1]).is_err());
        assert!(validate(&entries[1..]).is_err());
    }

    #[test]
    fn exclusion_closes_holds_and_does_not_capture_the_excluded_release() {
        let mut recorder = Recorder::new(
            CaptureOptions {
                keys: true,
                ..CaptureOptions::default()
            },
            ms(0),
        );
        let point = Point { x: 50, y: 60 };
        recorder.record(
            Event::Button {
                button: Button::Left,
                down: true,
                point,
            },
            ms(5),
        );
        recorder.record(
            Event::Key {
                code: 65,
                extended: false,
                down: true,
            },
            ms(6),
        );
        recorder.exclude_buttons(ms(7));
        recorder.exclude_keys(ms(8));
        recorder.record(
            Event::Button {
                button: Button::Left,
                down: false,
                point,
            },
            ms(9),
        );
        recorder.record(
            Event::Key {
                code: 65,
                extended: false,
                down: false,
            },
            ms(10),
        );
        assert_eq!(recorder.entries().len(), 4);
        assert_eq!(validate(recorder.entries()), Ok(()));
        let encoded = serde_json::to_string(recorder.entries()).unwrap();
        let restored: Vec<Entry> = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored, recorder.entries());
    }

    #[test]
    fn records_key_repeats_without_creating_extra_held_keys() {
        let mut recorder = Recorder::new(
            CaptureOptions {
                keys: true,
                ..CaptureOptions::default()
            },
            ms(0),
        );
        let key = Event::Key {
            code: 65,
            extended: false,
            down: true,
        };
        recorder.record(key.clone(), ms(1));
        recorder.record(key, ms(5));
        let entries = recorder.finish(ms(10));
        assert_eq!(entries.len(), 3);
        assert_eq!(validate(&entries), Ok(()));
    }

    #[test]
    fn preserves_fractional_milliseconds_across_events() {
        let mut recorder = Recorder::new(CaptureOptions::default(), Duration::ZERO);
        for n in 1..=3 {
            recorder.record(
                Event::Move(Point { x: n, y: 0 }),
                Duration::from_micros(n as u64 * 400),
            );
        }
        assert_eq!(
            recorder
                .entries()
                .iter()
                .map(|entry| entry.delay_ms)
                .sum::<u64>(),
            1
        );
    }

    #[test]
    fn native_key_codes_are_validated_for_the_recording_origin() {
        let pair = |code, extended| {
            vec![
                Entry {
                    delay_ms: 0,
                    event: Event::Key {
                        code,
                        extended,
                        down: true,
                    },
                },
                Entry {
                    delay_ms: 1,
                    event: Event::Key {
                        code,
                        extended,
                        down: false,
                    },
                },
            ]
        };
        assert!(validate_for(&pair(0, false), Origin::Macos).is_ok());
        assert!(validate_for(&pair(0, false), Origin::Windows).is_err());
        assert!(validate_for(&pair(8, false), Origin::X11).is_ok());
        assert!(validate_for(&pair(8, true), Origin::X11).is_err());
        assert!(validate_for(&pair(8, true), Origin::Windows).is_ok());
    }
}
