use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;

pub const F7: u16 = VK_F7;
pub const F8: u16 = VK_F8;

#[derive(Clone, Copy)]
pub struct Key {
    pub vk: u16,
    pub shift: bool,
    pub extended: bool,
}

impl Key {
    pub fn is_safety_key(self) -> bool {
        matches!(self.vk, F7 | F8)
    }
}

pub fn parse(text: &str) -> Option<Key> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let vk = match lower.as_str() {
        "space" => VK_SPACE,
        "enter" | "return" => VK_RETURN,
        "tab" => VK_TAB,
        "esc" | "escape" => VK_ESCAPE,
        "backspace" => VK_BACK,
        "delete" => VK_DELETE,
        "insert" => VK_INSERT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "page up" => VK_PRIOR,
        "pagedown" | "page down" => VK_NEXT,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        "up" => VK_UP,
        "down" => VK_DOWN,
        _ => {
            if let Some(function) = lower.strip_prefix('f').and_then(|n| n.parse::<u16>().ok())
                && (1..=24).contains(&function)
            {
                return Some(Key {
                    vk: VK_F1 + function - 1,
                    shift: false,
                    extended: false,
                });
            }
            let mut chars = text.chars();
            let ch = chars.next()?;
            if chars.next().is_some() || u32::from(ch) > u16::MAX.into() {
                return None;
            }
            let code = unsafe { VkKeyScanW(ch as u16) };
            let modifiers = (code >> 8) & 0xff;
            return (code >= 0 && modifiers <= 1).then_some(Key {
                vk: code as u16 & 0xff,
                shift: modifiers == 1,
                extended: false,
            });
        }
    };
    Some(Key {
        vk,
        shift: false,
        extended: matches!(vk, VK_PRIOR..=VK_DOWN | VK_INSERT | VK_DELETE),
    })
}

pub fn down(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(vk.into()) < 0 }
}
