use crate::keys::Key;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod route;

#[cfg(target_os = "linux")]
#[path = "mouse/linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "mouse/macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "mouse/windows.rs"]
mod platform;
#[cfg(target_os = "linux")]
mod wayland;

pub use platform::*;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Default, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Button {
    #[default]
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Zone {
    pub from: Point,
    pub to: Point,
}

impl Zone {
    pub fn contains(&self, point: Point) -> bool {
        (self.from.x.min(self.to.x)..=self.from.x.max(self.to.x)).contains(&point.x)
            && (self.from.y.min(self.to.y)..=self.from.y.max(self.to.y)).contains(&point.y)
    }
}

#[derive(Clone, Copy)]
pub enum Input {
    Mouse(Button),
    Key(Key),
    Text(char),
}

#[derive(Clone, Copy)]
pub enum Press {
    Tap,
    Timed(Duration),
    Held,
}
