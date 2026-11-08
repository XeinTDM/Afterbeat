use crate::x11::{Desktop, XResult};
use std::ptr::{NonNull, null_mut};
use x11rb::{
    connection::Connection,
    protocol::{
        xkb::{self, ConnectionExt as _},
        xproto::ConnectionExt as _,
    },
};
use xkbcommon_dl as xkb_dl;
use xkeysym::{Keysym, key};

pub const F7: u32 = key::F7;
pub const F8: u32 = key::F8;

#[derive(Clone, Copy)]
pub struct Key(pub u32);

impl Key {
    pub fn is_safety_key(self) -> bool {
        matches!(self.0, F7 | F8)
    }
}

pub fn parse(text: &str) -> Option<Key> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let symbol = match lower.as_str() {
        "space" => key::space,
        "enter" | "return" => key::Return,
        "tab" => key::Tab,
        "esc" | "escape" => key::Escape,
        "backspace" => key::BackSpace,
        "delete" => key::Delete,
        "insert" => key::Insert,
        "home" => key::Home,
        "end" => key::End,
        "pageup" | "page up" => key::Page_Up,
        "pagedown" | "page down" => key::Page_Down,
        "left" => key::Left,
        "right" => key::Right,
        "up" => key::Up,
        "down" => key::Down,
        _ => {
            if let Some(number) = lower.strip_prefix('f').and_then(|n| n.parse::<u32>().ok())
                && (1..=24).contains(&number)
            {
                return Some(Key(key::F1 + number - 1));
            }
            let mut chars = text.chars();
            let character = chars
                .next()
                .filter(|ch| !ch.is_control() && chars.next().is_none())?;
            Keysym::from_char(character).raw()
        }
    };
    Some(Key(symbol))
}

pub struct Keymap {
    map: NonNull<xkb_dl::xkb_keymap>,
    xkb: &'static xkb_dl::XkbCommon,
    group: u32,
    min: u8,
    max: u8,
}

impl Keymap {
    pub fn read(desktop: &Desktop) -> XResult<Self> {
        let xkb = xkb_dl::xkbcommon_option().ok_or("libxkbcommon is required.")?;
        let x11 = xkb_dl::x11::xkbcommon_x11_option().ok_or("libxkbcommon-x11 is required.")?;
        let conn = desktop.conn.get_raw_xcb_connection();
        // The raw XCB connection is borrowed for these synchronous XKB calls.
        let (map, device) = unsafe {
            if (x11.xkb_x11_setup_xkb_extension)(
                conn, 1, 0,
                xkb_dl::x11::xkb_x11_setup_xkb_extension_flags::XKB_X11_SETUP_XKB_EXTENSION_NO_FLAGS,
                null_mut(), null_mut(), null_mut(), null_mut(),
            ) == 0 {
                return Err("The X11 server does not provide XKB.".into());
            }
            let device = (x11.xkb_x11_get_core_keyboard_device_id)(conn);
            if device < 0 {
                return Err("No X11 keyboard is available.".into());
            }
            let context = NonNull::new((xkb.xkb_context_new)(
                xkb_dl::xkb_context_flags::XKB_CONTEXT_NO_FLAGS,
            ))
            .ok_or("Could not initialize XKB.")?;
            let map = (x11.xkb_x11_keymap_new_from_device)(
                context.as_ptr(),
                conn,
                device,
                xkb_dl::xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            );
            (xkb.xkb_context_unref)(context.as_ptr());
            (
                NonNull::new(map).ok_or("Could not read the X11 keyboard layout.")?,
                device as u16,
            )
        };
        let setup = desktop.conn.setup();
        let mut keymap = Self {
            map,
            xkb,
            group: 0,
            min: setup.min_keycode,
            max: setup.max_keycode,
        };
        keymap.group = desktop.conn.xkb_get_state(device)?.reply()?.group.into();
        Ok(keymap)
    }

    fn matches(&self, code: u8, group: u32, level: u32, symbol: u32) -> bool {
        let code = u32::from(code);
        // The keymap owns the symbol array for the duration of this call.
        unsafe {
            let layouts = (self.xkb.xkb_keymap_num_layouts_for_key)(self.map.as_ptr(), code);
            let mut symbols = std::ptr::null();
            let count = (self.xkb.xkb_keymap_key_get_syms_by_level)(
                self.map.as_ptr(),
                code,
                group.min(layouts.saturating_sub(1)),
                level,
                &mut symbols,
            );
            count == 1 && *symbols == symbol
        }
    }

    pub fn resolve(&self, symbol: u32, allow_shift: bool) -> Option<(u8, bool)> {
        for level in 0..=u32::from(allow_shift) {
            if let Some(code) =
                (self.min..=self.max).find(|&code| self.matches(code, self.group, level, symbol))
            {
                return Some((code, level == 1));
            }
        }
        None
    }

    pub fn modifier_masks(&self, desktop: &Desktop) -> XResult<(u16, u16)> {
        let mapping = desktop.conn.get_modifier_mapping()?.reply()?;
        let mut alt = 0;
        let mut locks = 0;
        let width = usize::from(mapping.keycodes_per_modifier());
        if width == 0 {
            return Ok((0, 0));
        }
        for (index, codes) in mapping.keycodes.chunks_exact(width).enumerate() {
            for &code in codes.iter().filter(|&&code| code != 0) {
                let mask = 1 << index;
                if [key::Alt_L, key::Alt_R]
                    .into_iter()
                    .any(|symbol| self.matches(code, 0, 0, symbol))
                {
                    alt |= mask;
                }
                if [key::Caps_Lock, key::Num_Lock, key::Scroll_Lock]
                    .into_iter()
                    .any(|symbol| self.matches(code, 0, 0, symbol))
                {
                    locks |= mask;
                }
            }
        }
        Ok((alt, locks))
    }
}

impl Drop for Keymap {
    fn drop(&mut self) {
        unsafe { (self.xkb.xkb_keymap_unref)(self.map.as_ptr()) };
    }
}

pub fn enable_detectable_repeat(desktop: &Desktop) -> XResult<()> {
    let flag = xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT;
    let reply = desktop
        .conn
        .xkb_per_client_flags(
            u16::from(xkb::ID::USE_CORE_KBD),
            flag,
            flag,
            0u32.into(),
            0u32.into(),
            0u32.into(),
        )?
        .reply()?;
    if u32::from(reply.value & flag) == 0 {
        return Err("The X11 server could not enable detectable key repeat.".into());
    }
    Ok(())
}
