use super::{Point, Zone, display_areas};
use crate::engine::Random;
use std::time::{Duration, Instant};

const LAYOUT_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const LAYOUT_CHANGED: &str = "The display layout changed. Capture your points again.";

pub(super) struct Route {
    points: Vec<Point>,
    displays: Vec<Zone>,
    next_check: Instant,
    pub(super) radius: i32,
    random: Random,
    index: usize,
    pending: Option<Point>,
}

impl Route {
    pub(super) fn new(points: Vec<Point>, radius: i32) -> Result<Option<Self>, String> {
        if points.is_empty() {
            return Ok(None);
        }
        let displays = display_areas();
        if displays.is_empty() {
            return Err("The desktop did not report any displays for the saved points.".into());
        }
        if !points.iter().all(|point| {
            supported(*point) && displays.iter().any(|display| display.contains(*point))
        }) {
            return Err("A saved point is outside your displays. Capture it again.".into());
        }
        Ok(Some(Self {
            points,
            displays,
            next_check: Instant::now(),
            radius,
            random: Random::new(),
            index: 0,
            pending: None,
        }))
    }

    pub(super) fn validate(&mut self) -> Result<(), &'static str> {
        let now = Instant::now();
        if now >= self.next_check {
            if self.displays != display_areas() {
                return Err(LAYOUT_CHANGED);
            }
            self.next_check = now + LAYOUT_CHECK_INTERVAL;
        }
        Ok(())
    }

    pub(super) fn changing(&self) -> bool {
        self.points.len() > 1 || self.radius > 0
    }

    pub(super) fn destination(&mut self) -> Point {
        *self.pending.get_or_insert_with(|| {
            let point = self.points[self.index];
            if self.radius == 0 {
                return point;
            }
            let angle = self.random.unit() * std::f64::consts::TAU;
            let distance = self.random.unit().sqrt() * f64::from(self.radius);
            let shifted = Point {
                x: point
                    .x
                    .saturating_add((angle.cos() * distance).round() as i32),
                y: point
                    .y
                    .saturating_add((angle.sin() * distance).round() as i32),
            };
            if supported(shifted)
                && self
                    .displays
                    .iter()
                    .any(|display| display.contains(shifted))
            {
                shifted
            } else {
                point
            }
        })
    }

    pub(super) fn advance(&mut self) {
        self.pending = None;
        self.index = (self.index + 1) % self.points.len();
    }
}

#[cfg(target_os = "linux")]
fn supported(point: Point) -> bool {
    crate::wayland::active() || (i16::try_from(point.x).is_ok() && i16::try_from(point.y).is_ok())
}

#[cfg(target_os = "macos")]
fn supported(_: Point) -> bool {
    true
}
