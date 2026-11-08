use objc2::MainThreadMarker;
use objc2_core_foundation::{CFData, CFRetained, CFString, CFType};
use objc2_core_graphics::{CGEventSource, CGEventSourceStateID};
use slint::{Timer, TimerMode};
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

pub const F7: u16 = 98;
pub const F8: u16 = 100;
pub const F12: u16 = 111;
const FUNCTION_KEYS: [u16; 20] = [
    122, 120, 99, 118, 96, 97, F7, F8, 101, 109, 103, F12, 105, 107, 113, 106, 64, 79, 80, 90,
];
static LAYOUT_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    pub code: u16,
    pub shift: bool,
    layout: Option<u64>,
}

impl Key {
    pub fn is_safety_key(self) -> bool {
        matches!(self.code, F7 | F8)
    }

    pub fn layout_unchanged(self) -> bool {
        self.layout
            .is_none_or(|layout| layout == LAYOUT_ID.load(Ordering::Acquire))
    }
}

pub fn parse(text: &str) -> Option<Key> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    // Virtual key codes from the macOS HIToolbox Events.h header.
    let code = match lower.as_str() {
        "space" => 49,
        "enter" | "return" => 36,
        "tab" => 48,
        "esc" | "escape" => 53,
        "backspace" => 51,
        "delete" => 117,
        "insert" | "help" => 114,
        "home" => 115,
        "end" => 119,
        "pageup" | "page up" => 116,
        "pagedown" | "page down" => 121,
        "left" => 123,
        "right" => 124,
        "up" => 126,
        "down" => 125,
        _ => {
            if let Some(index) = lower
                .strip_prefix('f')
                .and_then(|n| n.parse::<usize>().ok())
                .and_then(|n| n.checked_sub(1))
            {
                *FUNCTION_KEYS.get(index)?
            } else {
                let mut chars = text.chars();
                let character = chars.next()?;
                return (chars.next().is_none() && !character.is_control())
                    .then(|| character_key(character))
                    .flatten();
            }
        }
    };
    Some(Key {
        code,
        shift: false,
        layout: None,
    })
}

pub fn watch_layout() -> Timer {
    refresh_layout();
    let timer = Timer::default();
    timer.start(
        TimerMode::Repeated,
        Duration::from_millis(250),
        refresh_layout,
    );
    timer
}

fn refresh_layout() {
    let id = with_layout(|_, _, id| id).unwrap_or_default();
    LAYOUT_ID.store(id, Ordering::Release);
}

fn with_layout<T>(read: impl FnOnce(*const u8, u32, u64) -> T) -> Option<T> {
    // HIToolbox requires TIS calls on the main thread in GUI applications.
    let _main = MainThreadMarker::new()?;
    // The layout bytes are borrowed from the input source, which stays retained
    // for the duration of the lookup. UCKeyTranslate handles physical layouts.
    let source = unsafe { CFRetained::from_raw(TISCopyCurrentKeyboardLayoutInputSource()?) };
    let data = unsafe {
        TISGetInputSourceProperty(&source, kTISPropertyUnicodeKeyLayoutData)?
            .as_ref()
            .downcast_ref::<CFData>()?
    };
    if data.is_empty() {
        return None;
    }
    let layout = data.byte_ptr();
    let keyboard_type = u32::from(unsafe { LMGetKbdType() });
    let mut hasher = DefaultHasher::new();
    keyboard_type.hash(&mut hasher);
    unsafe { data.as_bytes_unchecked() }.hash(&mut hasher);
    Some(read(layout, keyboard_type, hasher.finish().max(1)))
}

fn character_key(character: char) -> Option<Key> {
    with_layout(|layout, keyboard_type, id| {
        LAYOUT_ID.store(id, Ordering::Release);
        translate_character(character, layout, keyboard_type, id)
    })
    .flatten()
}

fn translate_character(
    character: char,
    layout: *const u8,
    keyboard_type: u32,
    id: u64,
) -> Option<Key> {
    let mut utf16 = [0; 2];
    let wanted = character.encode_utf16(&mut utf16);
    for shift in [false, true] {
        for code in 0..128 {
            let mut dead = 0;
            let mut len = 0;
            let mut output = [0; 4];
            let result = unsafe {
                UCKeyTranslate(
                    layout,
                    code,
                    3,                         // kUCKeyActionDisplay
                    if shift { 2 } else { 0 }, // shiftKey >> 8
                    keyboard_type,
                    1, // kUCKeyTranslateNoDeadKeysMask
                    &mut dead,
                    output.len() as u32,
                    &mut len,
                    output.as_mut_ptr(),
                )
            };
            if result == 0 && dead == 0 && output.get(..len as usize) == Some(wanted) {
                return Some(Key {
                    code,
                    shift,
                    layout: Some(id),
                });
            }
        }
    }
    None
}

pub fn down(code: u16) -> bool {
    CGEventSource::key_state(CGEventSourceStateID::HIDSystemState, code)
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    static kTISPropertyUnicodeKeyLayoutData: &'static CFString;
    fn TISCopyCurrentKeyboardLayoutInputSource() -> Option<NonNull<CFType>>;
    fn TISGetInputSourceProperty(source: &CFType, property: &CFString) -> Option<NonNull<CFType>>;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        layout: *const u8,
        code: u16,
        action: u16,
        modifiers: u32,
        keyboard_type: u32,
        options: u32,
        dead_key_state: &mut u32,
        max_length: u32,
        actual_length: &mut u32,
        output: *mut u16,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_macos_virtual_codes_for_named_and_safety_keys() {
        assert_eq!(parse("F1").unwrap().code, 122);
        assert_eq!(parse("F20").unwrap().code, 90);
        assert!(parse("F21").is_none());
        assert!(parse("F7").unwrap().is_safety_key());
        assert!(parse("f8").unwrap().is_safety_key());
        assert_eq!(parse("Return"), parse("enter"));
        assert_ne!(parse("Delete"), parse("Backspace"));
    }
}
