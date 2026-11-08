use crate::{App, Theme, wide};
use slint::{Color, ComponentHandle, Timer, TimerMode};
use std::{ptr::null_mut, time::Duration};
use windows_sys::Win32::{
    Foundation::ERROR_SUCCESS,
    Graphics::Gdi::{
        COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT,
        GetSysColor, SYS_COLOR_INDEX,
    },
    System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    UI::{
        Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW},
        WindowsAndMessaging::{SPI_GETHIGHCONTRAST, SystemParametersInfoW},
    },
};

pub fn watch(app: &App) -> Timer {
    apply(app);
    let app = app.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
        if let Some(app) = app.upgrade() {
            apply(&app);
        }
    });
    timer
}

fn apply(app: &App) {
    let theme = app.global::<Theme>();
    theme.set_text_scale(text_scale());
    let mut contrast = HIGHCONTRASTW {
        cbSize: size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    let available = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            contrast.cbSize,
            (&mut contrast as *mut HIGHCONTRASTW).cast(),
            0,
        )
    };
    let enabled = available != 0 && contrast.dwFlags & HCF_HIGHCONTRASTON != 0;
    if enabled {
        theme.set_system_background(system_color(COLOR_WINDOW));
        theme.set_system_text(system_color(COLOR_WINDOWTEXT));
        theme.set_system_highlight(system_color(COLOR_HIGHLIGHT));
        theme.set_system_highlight_text(system_color(COLOR_HIGHLIGHTTEXT));
        theme.set_system_disabled(system_color(COLOR_GRAYTEXT));
    }
    theme.set_high_contrast(enabled);
}

fn system_color(index: SYS_COLOR_INDEX) -> Color {
    let [red, green, blue, _] = unsafe { GetSysColor(index) }.to_le_bytes();
    Color::from_rgb_u8(red, green, blue)
}

fn text_scale() -> f32 {
    let mut percent = 100u32;
    let mut size = size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            wide(r"Software\Microsoft\Accessibility").as_ptr(),
            wide("TextScaleFactor").as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            (&mut percent as *mut u32).cast(),
            &mut size,
        )
    };
    if result == ERROR_SUCCESS {
        percent.clamp(100, 225) as f32 / 100.0
    } else {
        1.0
    }
}
