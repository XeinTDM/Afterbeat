#[path = "../x11/hotkeys.rs"]
mod x11;

use crate::wayland;
use std::io;
pub use x11::{Action, Binding, tick_count};

pub enum Hotkeys {
    X11(x11::Hotkeys),
    Wayland(wayland::Hotkeys),
}

impl Hotkeys {
    pub fn start(
        preferred: &str,
        callback: impl FnMut(Action) + Send + 'static,
    ) -> io::Result<Self> {
        if wayland::active() {
            wayland::Hotkeys::start(preferred, callback).map(Self::Wayland)
        } else {
            x11::Hotkeys::start(preferred, callback).map(Self::X11)
        }
    }

    pub fn label(&self) -> Option<String> {
        match self {
            Self::X11(hotkeys) => hotkeys.label(),
            Self::Wayland(_) => wayland::status().toggle,
        }
    }

    pub fn stop_label(&self) -> Option<String> {
        match self {
            Self::X11(hotkeys) => hotkeys.shortcuts.stop.map(str::to_owned),
            Self::Wayland(_) => wayland::status().stop,
        }
    }

    pub fn capture_label(&self) -> Option<&'static str> {
        match self {
            Self::X11(hotkeys) => hotkeys.shortcuts.capture,
            Self::Wayland(_) => None,
        }
    }

    pub fn toggle_binding(&self) -> Option<Binding> {
        match self {
            Self::X11(hotkeys) => hotkeys.toggle_binding(),
            Self::Wayland(_) => wayland::toggle_binding(),
        }
    }

    pub fn listening(&self) -> bool {
        match self {
            Self::X11(hotkeys) => hotkeys.listening(),
            Self::Wayland(_) => wayland::status().stop.is_some(),
        }
    }

    pub fn set_paced_stop_priority(&self, enabled: bool) {
        if let Self::X11(hotkeys) = self {
            hotkeys.set_paced_stop_priority(enabled);
        }
    }

    pub fn stop_probe(&self, invoked: Option<Binding>) -> StopProbe {
        StopProbe(match self {
            Self::X11(hotkeys) => Some(hotkeys.stop_probe(invoked)),
            Self::Wayland(_) => None,
        })
    }

    pub fn rebind(&self, text: &str) -> Result<(), &'static str> {
        match self {
            Self::X11(hotkeys) => hotkeys.rebind(text),
            // Loading a preset leaves the desktop's shortcut assignment in place.
            Self::Wayland(_) => Ok(()),
        }
    }
}

pub struct StopProbe(Option<x11::StopProbe>);

impl StopProbe {
    pub fn pending(&mut self) -> bool {
        self.0.as_mut().is_some_and(x11::StopProbe::pending)
    }
}
