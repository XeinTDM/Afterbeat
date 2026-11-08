use slint::platform::{
    Key, Platform, PointerEventButton, WindowAdapter, WindowEvent,
    software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
};
use slint::{
    ComponentHandle, LogicalPosition, ModelRc, PhysicalSize, Rgb8Pixel, SharedString, VecModel,
};
use std::{
    cell::Cell,
    io::{BufWriter, Write},
    rc::Rc,
};

slint::include_modules!();

struct Offscreen(Rc<MinimalSoftwareWindow>);
impl Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

fn list(items: &[&str]) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        items
            .iter()
            .map(|item| SharedString::from(*item))
            .collect::<Vec<_>>(),
    ))
}

fn main() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen(window.clone()))).unwrap();
    let app = App::new().unwrap();
    app.show().unwrap();
    app.set_input_ready(true);
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "default".into());
    let size = if mode.starts_with("compact-text") {
        (560, 420)
    } else if mode.starts_with("compact") {
        (640, 530)
    } else {
        (922, 634)
    };
    window.set_size(PhysicalSize::new(size.0, size.1));
    slint::platform::update_timers_and_animations();

    let click = |x, y| {
        let position = LogicalPosition::new(x, y);
        let button = PointerEventButton::Left;
        window.dispatch_event(WindowEvent::PointerMoved { position });
        window.dispatch_event(WindowEvent::PointerPressed { position, button });
        window.dispatch_event(WindowEvent::PointerReleased { position, button });
    };
    match mode.as_str() {
        "running" | "compact-running" => {
            app.set_active(true);
            app.set_run_state("Running".into());
            app.set_session_count("128".into());
            app.set_session_rate("10".into());
            app.set_session_elapsed("12.8 s".into());
            app.set_pad_count("128".into());
        }
        "timing" => {
            app.set_page(1);
            app.set_random_rate(true);
            app.set_press_text("30".into());
            app.set_timed_press(true);
            app.set_time_limit("45".into());
            app.set_pace_value("8–12 /s".into());
        }
        "points" | "compact" => {
            app.set_page(2);
            app.set_route_enabled(true);
            app.set_route_once(true);
            app.set_route_count(3);
            app.set_route_rows(list(&[
                "1. X -850  Y 640",
                "2. X -600  Y 480",
                "3. X -470  Y 370",
            ]));
            app.set_route_index(1);
            app.set_stop_zone_label("X 900  Y 640 to X 1,050  Y 790".into());
            app.set_inside_label("First corner X 200  Y 550. Pick the opposite corner.".into());
            app.set_process_mode(1);
            app.set_process_names("notepad.exe".into());
            app.set_pick_kind(2);
        }
        "presets" => {
            app.set_page(3);
            app.set_profile_rows(list(&["QA smoke test", "Navigation route"]));
            app.set_profile_index(0);
            app.set_selected_preset("QA smoke test".into());
            app.set_library_stats("12 runs  ·  1,240 actions  ·  3 min active".into());
        }
        "compact-error" => {
            app.set_keyboard(true);
            app.set_key_text("F6".into());
            app.set_configuration_field("input".into());
            app.set_configuration_error(
                "Choose an output key different from the activation, stop, and capture keys."
                    .into(),
            );
        }
        "dry-run" | "compact-dry-run" | "compact-dry-blocked" | "compact-dry-fix" => {
            app.set_route_enabled(true);
            if matches!(mode.as_str(), "compact-dry-blocked" | "compact-dry-fix") {
                app.set_configuration_field("inside".into());
                app.set_configuration_error(
                    "Clear the inside-only zone before running a route.".into(),
                );
                app.set_inside_label("X 20  Y 40 to X 980  Y 760".into());
            }
            app.set_dry_aspect(2.25);
            app.set_dry_areas(ModelRc::new(VecModel::from(vec![
                DryArea {
                    x: 0.0,
                    y: 0.12,
                    w: 0.45,
                    h: 0.80,
                    kind: 0,
                },
                DryArea {
                    x: 0.45,
                    y: 0.0,
                    w: 0.55,
                    h: 1.0,
                    kind: 0,
                },
                DryArea {
                    x: 0.68,
                    y: 0.34,
                    w: 0.23,
                    h: 0.45,
                    kind: 2,
                },
            ])));
            app.set_dry_points(ModelRc::new(VecModel::from(
                [
                    (0.12, 0.33),
                    (0.36, 0.66),
                    (0.59, 0.27),
                    (0.79, 0.58),
                    (0.91, 0.19),
                ]
                .into_iter()
                .enumerate()
                .map(|(i, (x, y))| DryPoint {
                    x,
                    y,
                    radius_x: 0.036,
                    radius_y: 0.081,
                    blocked: i == 3,
                    location: format!("X {}  Y {}", (x * 2400.0) as i32, (y * 1080.0) as i32)
                        .into(),
                    result: if i == 3 {
                        "Stopped in the stop zone."
                    } else {
                        "Clear"
                    }
                    .into(),
                })
                .collect::<Vec<_>>(),
            )));
            app.invoke_show_dry_run();
            let step_button = (
                if mode.starts_with("compact") {
                    277.0
                } else {
                    392.0
                },
                if mode.starts_with("compact") {
                    124.0
                } else {
                    132.0
                },
            );
            for _ in 0..5 {
                click(step_button.0, step_button.1);
            }
            assert_eq!(
                app.get_dry_step(),
                3,
                "The dry run must stop at the blocked point."
            );
            if mode == "compact-dry-fix" {
                click(131.0, 183.0);
                assert_eq!(app.get_page(), 2, "Fix should open Targets.");
                std::thread::sleep(std::time::Duration::from_millis(10));
                slint::platform::update_timers_and_animations();
            }
        }
        "compact-text" | "compact-text-shortcuts" | "compact-text-paused" => {
            app.global::<Theme>().set_text_scale(2.25);
            app.set_toggle_key("Ctrl+Shift+Alt+Backspace".into());
            app.set_stop_key("Ctrl+Alt+F7".into());
            app.set_capture_key("Ctrl+Alt+F8".into());
            if mode == "compact-text-shortcuts" {
                click(148.0, 370.0);
            }
            if mode == "compact-text-paused" {
                app.set_active(true);
                app.set_keyboard(true);
                app.set_process_mode(1);
                app.set_run_state("Paused".into());
                app.set_message(
                    "App filter blocked the foreground app. Switch to an allowed app.".into(),
                );
            }
        }
        "compact-contrast" => {
            let theme = app.global::<Theme>();
            theme.set_high_contrast(true);
            theme.set_system_background(slint::Color::from_rgb_u8(0, 0, 0));
            theme.set_system_text(slint::Color::from_rgb_u8(255, 255, 255));
            theme.set_system_highlight(slint::Color::from_rgb_u8(255, 235, 94));
            theme.set_system_highlight_text(slint::Color::from_rgb_u8(0, 0, 0));
            theme.set_system_disabled(slint::Color::from_rgb_u8(156, 156, 156));
            theme.set_text_scale(1.5);
        }
        "keyboard-check" => {
            let pick_kind = app.get_pick_kind();
            app.set_pick_kind(0);
            assert_eq!(app.get_capture_shortcut_kind(), 0);
            app.set_page(2);
            assert_eq!(app.get_capture_shortcut_kind(), 1);
            app.set_pick_kind(3);
            assert_eq!(app.get_capture_shortcut_kind(), 3);
            app.set_pick_kind(pick_kind);
            app.set_page(0);

            let press = |key: Key| {
                let text: SharedString = key.into();
                window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                window.dispatch_event(WindowEvent::KeyReleased { text });
            };
            app.set_page(1);
            click(433.0, 203.0);
            assert_eq!(app.get_press_text(), "30");
            click(585.0, 203.0);
            press(Key::Home);
            press(Key::Delete);
            assert_eq!(app.get_press_text(), "0");
            press(Key::End);
            for character in [".", "5"] {
                let text: SharedString = character.into();
                window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                window.dispatch_event(WindowEvent::KeyReleased { text });
            }
            assert_eq!(app.get_press_text(), "0.5");
            app.set_timed_press(false);
            app.set_press_text("0".into());
            app.set_page(0);

            let weak = app.as_weak();
            app.on_select_unit(move |unit| weak.unwrap().set_interval_unit(unit));
            click(345.0, 170.0);
            press(Key::Tab);
            for _ in 0..5 {
                press(Key::DownArrow);
            }
            assert_eq!(
                app.get_interval_unit(),
                5,
                "Clicker should select the rate unit."
            );
            app.set_interval_unit(0);
            click(345.0, 217.0);
            press(Key::RightArrow);
            assert!(app.get_keyboard());
            press(Key::Tab);
            press(Key::LeftArrow);
            assert!(app.get_keyboard(), "Tab should leave the input group");

            app.set_page(3);
            app.set_profile_rows(ModelRc::new(VecModel::from(
                (1..=18)
                    .map(|i| format!("Preset {i:02}").into())
                    .collect::<Vec<SharedString>>(),
            )));
            click(250.0, 163.0);
            assert_eq!(app.get_profile_index(), 0);
            for _ in 0..7 {
                press(Key::DownArrow);
            }
            assert_eq!(app.get_profile_index(), 7);
            press(Key::Tab);
            press(Key::DownArrow);
            assert_eq!(app.get_profile_index(), 7, "Tab should leave the list");
            println!(
                "Arrow navigation, grouped Tab stops, capture shortcuts, and fractional presses passed"
            );
        }
        _ => {}
    }

    let mut pixels = vec![Rgb8Pixel::default(); (size.0 * size.1) as usize];
    assert!(window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, size.0 as usize);
    }));
    let file = std::fs::File::create(format!("afterbeat-{mode}.ppm")).unwrap();
    let mut file = BufWriter::new(file);
    write!(file, "P6\n{} {}\n255\n", size.0, size.1).unwrap();
    for pixel in pixels {
        file.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
    }
    file.flush().unwrap();

    if mode == "check" {
        let capture = Rc::new(Cell::new(-1));
        let capture_event = capture.clone();
        app.on_capture_point(move |kind| capture_event.set(kind));
        let save = Rc::new(Cell::new(-1));
        let save_event = save.clone();
        app.on_profile_action(move |operation| save_event.set(operation));
        let pressed = Rc::new(Cell::new(0));
        let pressed_event = pressed.clone();
        app.on_test_hit(move || pressed_event.set(pressed_event.get() + 1));
        let mut position = || {
            let x = args.next().unwrap().parse().unwrap();
            let y = args.next().unwrap().parse().unwrap();
            LogicalPosition::new(x, y)
        };
        let click = |position, button| {
            window.dispatch_event(WindowEvent::PointerMoved { position });
            window.dispatch_event(WindowEvent::PointerPressed { position, button });
            window.dispatch_event(WindowEvent::PointerReleased { position, button });
        };
        let pad = position();
        assert!(app.invoke_test_pad_at(pad.x, pad.y));
        assert!(!app.invoke_test_pad_at(450.0, 150.0));
        for button in [
            PointerEventButton::Left,
            PointerEventButton::Right,
            PointerEventButton::Middle,
        ] {
            click(pad, button);
        }
        assert_eq!(pressed.get(), 3);
        click(position(), PointerEventButton::Left);
        assert!(app.get_keyboard());
        click(position(), PointerEventButton::Left);
        assert_eq!(app.get_page(), 1);
        click(position(), PointerEventButton::Left);
        assert!(app.get_hold_activation());
        click(position(), PointerEventButton::Left);
        assert_eq!(app.get_page(), 2);
        click(position(), PointerEventButton::Left);
        assert_eq!(capture.get(), 1);
        click(position(), PointerEventButton::Left);
        assert_eq!(capture.get(), 3);
        click(position(), PointerEventButton::Left);
        assert_eq!(app.get_page(), 3);
        app.set_profile_name("Smoke test".into());
        click(position(), PointerEventButton::Left);
        assert_eq!(save.get(), 0);
        println!("Offscreen page and feature checks passed");
    }
}
