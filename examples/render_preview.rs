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
    app.invoke_preview_color_scheme(mode == "light");
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
    let settle = || {
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            slint::platform::update_timers_and_animations();
            let dimensions = app.window().size();
            let mut pixels =
                vec![Rgb8Pixel::default(); (dimensions.width * dimensions.height) as usize];
            window.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, dimensions.width as usize);
            });
        }
    };
    match mode.as_str() {
        "shortcut-editing-check" => {
            let press = |key: Key| {
                let text: SharedString = key.into();
                window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                window.dispatch_event(WindowEvent::KeyReleased { text });
            };
            app.set_page(1);
            settle();
            assert!(app.get_run_button_enabled());
            click(345.0, 159.0);
            for _ in 0..20 {
                if app.get_shortcut_editing() {
                    break;
                }
                press(Key::Tab);
                settle();
            }
            assert!(
                app.get_shortcut_editing(),
                "Tab must reach the shortcut editor."
            );
            assert!(
                !app.get_run_button_enabled(),
                "Typing a shortcut must disable new starts."
            );
            app.set_trigger_text("Ctrl+Alt+G".into());
            press(Key::Escape);
            assert_eq!(app.get_trigger_text(), "F6");
            settle();
            assert!(
                !app.get_shortcut_editing(),
                "Escape must finish editing, not leave Start blocked."
            );
            assert!(app.get_run_button_enabled());
            app.set_active(true);
            settle();
            assert!(
                app.get_run_button_enabled(),
                "Editing must never disable Stop."
            );
            app.set_active(false);
            app.set_page(0);
            settle();
            assert!(!app.get_shortcut_editing());
            assert!(app.get_run_button_enabled());
            println!(
                "Shortcut focus blocks Start, Escape restores the binding, and Stop remains available"
            );
        }
        "reorder-check"
        | "route-reorder"
        | "compact-route-reorder"
        | "compact-text-route-reorder" => {
            app.set_page(2);
            app.set_route_enabled(true);
            app.set_route_count(64);
            app.set_route_rows(ModelRc::new(VecModel::from(
                (1..=64)
                    .map(|i| format!("{i}. Target {i}").into())
                    .collect::<Vec<SharedString>>(),
            )));
            if mode.starts_with("compact-text") {
                app.global::<Theme>().set_text_scale(2.25);
            }
            settle();
            app.set_route_index(59);
            if mode == "reorder-check" {
                let received = Rc::new(Cell::new(0));
                let operation = received.clone();
                let weak = app.as_weak();
                app.on_edit_route(move |direction| {
                    operation.set(direction);
                    let app = weak.unwrap();
                    app.set_route_index(app.get_route_index() + direction);
                });
                settle();
                click(350.0, 263.0);
                let before = app.get_route_index();
                assert!((0..63).contains(&before));
                window.dispatch_event(WindowEvent::KeyPressed {
                    text: Key::Alt.into(),
                });
                window.dispatch_event(WindowEvent::KeyPressed {
                    text: Key::DownArrow.into(),
                });
                window.dispatch_event(WindowEvent::KeyReleased {
                    text: Key::DownArrow.into(),
                });
                window.dispatch_event(WindowEvent::KeyReleased {
                    text: Key::Alt.into(),
                });
                assert_eq!(
                    received.get(),
                    1,
                    "Alt+Down must request a move instead of selecting another row."
                );
                assert_eq!(app.get_route_index(), before + 1);
                assert_eq!(app.get_route_position(), (before + 2).to_string());
                println!(
                    "Keyboard route reordering preserves selection and updates the one-based position"
                );
            }
        }
        "recording-check"
        | "compact-text-recording-check"
        | "recorded-step-editor"
        | "compact-text-recorded-step-editor"
        | "recorded-key-editor"
        | "compact-text-recorded-key-editor"
        | "compact-text-recorded-key-editor-bottom" => {
            app.set_page(2);
            app.set_smart_targets_available(true);
            app.set_recording_available(true);
            app.set_recording_windows(cfg!(windows));
            app.set_recording_macos(cfg!(target_os = "macos"));
            assert!(!app.get_record_keys(), "Keyboard capture must be opt-in.");
            app.set_route_enabled(true);
            app.set_route_recorded(true);
            app.set_route_count(2);
            app.set_route_rows(list(&[
                "1. Left down · X 10 Y 20",
                "2. Left up · X 30 Y 40 · 300 ms delay",
            ]));
            app.set_route_index(0);
            if mode.ends_with("recorded-step-editor") || mode.contains("recorded-key-editor") {
                if mode.starts_with("compact-text") {
                    app.global::<Theme>().set_text_scale(2.25);
                }
                app.set_step_recorded(true);
                app.set_recorded_kind(if mode.contains("recorded-key-editor") {
                    2
                } else {
                    1
                });
                app.set_recorded_key("65".into());
                app.set_recorded_x("10".into());
                app.set_recorded_y("20".into());
                app.set_recorded_down(true);
                app.set_step_button(0);
                app.set_step_delay("25".into());
                app.set_step_edit_index(0);
                app.set_step_editing(true);
                app.invoke_reveal_step_editor();
                settle();
                if mode.ends_with("-bottom") {
                    for _ in 0..100 {
                        window.dispatch_event(WindowEvent::PointerScrolled {
                            position: LogicalPosition::new(150.0, 200.0),
                            delta_x: 0.0,
                            delta_y: -2200.0,
                        });
                        settle();
                    }
                }
            } else {
                if mode.starts_with("compact-text") {
                    app.global::<Theme>().set_text_scale(2.25);
                }
                settle();
                assert!(
                    app.get_run_button_enabled(),
                    "Playback should be available before recording."
                );
                app.set_recording_state(1);
                settle();
                assert!(
                    !app.get_run_button_enabled(),
                    "Recording must block playback."
                );
                app.set_recording_state(2);
                settle();
                assert!(
                    !app.get_run_button_enabled(),
                    "Paused recording must block playback."
                );
                app.set_recorded_count(12);
                let distance = args.next().map_or_else(
                    || {
                        if mode.starts_with("compact-text") {
                            2200.0
                        } else {
                            900.0
                        }
                    },
                    |value| value.parse().unwrap(),
                );
                let scroll_count = args.next().map_or_else(
                    || {
                        if mode.starts_with("compact-text") {
                            100
                        } else {
                            1
                        }
                    },
                    |value| value.parse().unwrap(),
                );
                for _ in 0..scroll_count {
                    window.dispatch_event(WindowEvent::PointerScrolled {
                        position: LogicalPosition::new(
                            if mode.starts_with("compact-text") {
                                150.0
                            } else {
                                400.0
                            },
                            200.0,
                        ),
                        delta_x: 0.0,
                        delta_y: -distance,
                    });
                    settle();
                }
            }
        }
        "pixel-branch-editor"
        | "compact-pixel-branch-editor"
        | "pixel-check-editor"
        | "compact-pixel-check-editor"
        | "compact-image-editor"
        | "compact-workflow-editor" => {
            app.set_page(2);
            app.set_smart_targets_available(true);
            app.set_route_enabled(true);
            app.set_route_actions(true);
            app.set_route_count(2);
            app.set_route_rows(list(&["1. Wait for pixel", "2. Click target"]));
            app.set_route_index(0);
            app.set_step_edit_index(0);
            app.set_step_has_target(false);
            app.set_step_inherits(false);
            app.set_step_kind(match mode.as_str() {
                "compact-image-editor" => 6,
                "compact-workflow-editor" => 7,
                _ => 5,
            });
            if mode == "compact-image-editor" {
                app.set_step_image_output(2);
                app.set_step_image_scaled(true);
                app.set_step_image_template(
                    format!(
                        "{{\"width\":8,\"height\":8,\"pixels\":\"{}\"}}",
                        "55aa".repeat(32)
                    )
                    .into(),
                );
                app.set_step_image_preview("Match at 120, 200 · 98%. Snapshot only.".into());
                let mut pixels = slint::SharedPixelBuffer::<Rgb8Pixel>::new(8, 8);
                for (index, pixel) in pixels.make_mut_slice().iter_mut().enumerate() {
                    let value = if index % 2 == 0 { 85 } else { 170 };
                    *pixel = Rgb8Pixel {
                        r: value,
                        g: value,
                        b: value,
                    };
                }
                let image = slint::Image::from_rgb8(pixels);
                app.set_step_image_reference_image(image.clone());
                app.set_step_image_match_image(image);
                app.set_step_image_has_match(true);
            }
            app.set_step_key("100, 200, #00FF00".into());
            app.set_step_pixel_tolerance("5".into());
            app.set_step_pixel_retries("2".into());
            app.set_step_pixel_preview(
                "Sampled (100, 200): #00FF00. Condition matches at preview time.".into(),
            );
            let check_once = mode.ends_with("pixel-check-editor");
            app.set_step_pixel_stable(if check_once { "0" } else { "250" }.into());
            app.set_step_pixel_check_once(check_once);
            app.set_step_pixel_skip_next(true);
            app.set_step_editing(true);
            settle();
            window.dispatch_event(WindowEvent::PointerScrolled {
                position: LogicalPosition::new(size.0 as f32 - 150.0, 220.0),
                delta_x: 0.0,
                delta_y: if mode == "compact-image-editor" {
                    -460.0
                } else {
                    -2200.0
                },
            });
            settle();
        }
        "route-details"
        | "compact-route-details"
        | "compact-text-route-details"
        | "step-editor"
        | "compact-step-editor" => {
            app.set_page(2);
            app.set_smart_targets_available(true);
            app.set_route_enabled(true);
            app.set_route_once(true);
            app.set_route_smart(true);
            app.set_route_actions(true);
            app.set_pace_value("Step timing".into());
            app.set_route_count(2);
            app.set_route_rows(list(&[
                "1. Right click ×2 · Apply the selected configuration to all output devices in this workspace · 1200 ms delay · Wait until gone",
                "2. Wait 500 ms",
            ]));
            app.set_route_control_rows(ModelRc::new(VecModel::from(vec![2, 0])));
            app.set_route_index(0);
            app.set_pick_kind(5);
            let editing = mode.ends_with("step-editor");
            app.set_capture_visual(!editing);
            if editing {
                app.set_step_edit_index(0);
                app.set_step_has_target(true);
                app.set_step_control_target(true);
                app.set_step_inherits(false);
                app.set_step_kind(0);
                app.set_step_button(1);
                app.set_step_count("2".into());
                app.set_step_delay("1200".into());
                app.set_step_editing(true);
            }
            if mode.starts_with("compact-text") {
                app.global::<Theme>().set_text_scale(2.25);
                settle();
                let distance = args.next().map_or(900.0, |value| value.parse().unwrap());
                window.dispatch_event(WindowEvent::PointerScrolled {
                    position: LogicalPosition::new(400.0, 200.0),
                    delta_x: 0.0,
                    delta_y: -distance,
                });
                settle();
            }
        }
        "hold-summary"
        | "compact-hold-summary"
        | "compact-text-hold-summary"
        | "compact-text-summary" => {
            app.set_page(1);
            app.set_keyboard(true);
            app.set_key_text("Enter".into());
            app.set_hold_activation(mode.ends_with("hold-summary"));
            if mode.starts_with("compact-text") {
                app.global::<Theme>().set_text_scale(2.25);
            }
        }
        "summary-check" => {
            app.set_keyboard(true);
            app.set_key_text("Enter".into());
            assert!(app.get_setup_summary().contains("No action limit"));
            assert!(app.get_setup_summary().contains("No time limit"));
            app.set_route_enabled(true);
            app.set_route_count(3);
            app.set_route_once(true);
            assert!(app.get_setup_summary().contains("Once"));
            app.set_route_once(false);
            assert!(app.get_setup_summary().contains("Loop"));
            app.set_route_enabled(false);
            for (width, height, scale) in [(922, 634, 1.0), (640, 530, 1.0), (560, 420, 2.25)] {
                window.set_size(PhysicalSize::new(width, height));
                app.global::<Theme>().set_text_scale(scale);
                for hold in [false, true] {
                    app.set_hold_activation(hold);
                    app.set_toggle_key("Ctrl+Shift+Alt+Backspace".into());
                    for page in 0..4 {
                        app.set_page(page);
                        settle();
                        assert!(
                            app.invoke_setup_summary_on_screen(),
                            "Summary must remain visible on page {page} at {scale}x with Hold={hold}."
                        );
                        if scale > 1.25 {
                            click(230.0, 370.0);
                            settle();
                            assert!(
                                app.get_full_setup_summary_visible(),
                                "Run details must reveal the full setup on page {page} in Hold={hold}."
                            );
                        }
                    }
                }
            }
            app.set_run_state("Last run".into());
            settle();
            assert!(
                app.invoke_setup_summary_on_screen(),
                "The next setup must stay visible beside last-run metrics."
            );
            app.set_active(true);
            app.set_stop_key("Ctrl+Alt+F7".into());
            app.set_session_count("100,000,000".into());
            app.set_session_rate("1,000".into());
            app.set_session_elapsed("59:59".into());
            settle();
            assert!(
                !app.invoke_setup_summary_on_screen(),
                "Active runs should prioritize live status and Stop."
            );
            if !app.invoke_stop_button_on_screen() {
                let dimensions = app.window().size();
                let mut pixels =
                    vec![Rgb8Pixel::default(); (dimensions.width * dimensions.height) as usize];
                app.window().request_redraw();
                window.draw_if_needed(|renderer| {
                    renderer.render(&mut pixels, dimensions.width as usize);
                });
                let mut output =
                    BufWriter::new(std::fs::File::create("afterbeat-stop-overflow.ppm").unwrap());
                write!(
                    output,
                    "P6\n{} {}\n255\n",
                    dimensions.width, dimensions.height
                )
                .unwrap();
                for pixel in pixels {
                    output.write_all(&[pixel.r, pixel.g, pixel.b]).unwrap();
                }
            }
            assert!(
                app.invoke_stop_button_on_screen(),
                "Stop must remain fully visible when enlarged live metrics wrap."
            );
            app.set_active(false);
            app.set_run_state("".into());
            app.set_hold_activation(false);
            app.global::<Theme>().set_text_scale(1.0);
            window.set_size(PhysicalSize::new(size.0, size.1));
            println!(
                "Summary visibility passed on every setup page, in Hold and Toggle, at normal, compact, and 225% sizes"
            );
        }
        "preset-load"
        | "preset-load-error"
        | "compact-preset-load"
        | "compact-text-preset-load"
        | "compact-text-save-name"
        | "compact-text-save-error" => {
            app.set_page(3);
            app.set_active_preset("Navigation route".into());
            app.set_preset_modified(true);
            app.set_pending_preset("QA smoke test".into());
            // Match the real controller: new names start empty.
            app.set_load_save_name("".into());
            if mode.starts_with("compact-text") {
                app.global::<Theme>().set_text_scale(2.25);
            }
            app.invoke_confirm_preset_load();
            if mode == "preset-load-error"
                || mode.ends_with("save-name")
                || mode.ends_with("save-error")
            {
                app.invoke_begin_preset_save();
                settle();
                assert!(
                    app.get_preset_name_visible(),
                    "The empty name field must be visible before saving."
                );
                if mode.ends_with("save-error") || mode == "preset-load-error" {
                    app.set_load_save_name("Kept setup".into());
                    app.set_load_error(
                        "Could not save the current setup. Try again or cancel to keep it.".into(),
                    );
                    settle();
                    assert!(
                        app.get_preset_error_visible(),
                        "Save errors must scroll into view."
                    );
                }
            }
        }
        "preset-form-check" => {
            let key = |value: Key| {
                let text: SharedString = value.into();
                window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                window.dispatch_event(WindowEvent::KeyReleased { text });
            };
            let action = Rc::new(Cell::new(-1));
            let received = action.clone();
            let weak = app.as_weak();
            app.on_resolve_preset_load(move |operation| {
                received.set(operation);
                let app = weak.unwrap();
                if operation == 2 {
                    app.set_load_error("That name is taken. Choose another preset name.".into());
                } else {
                    app.invoke_close_preset_load();
                }
            });
            for (width, height, scale) in [(922, 634, 1.0), (560, 420, 2.25)] {
                window.set_size(PhysicalSize::new(width, height));
                app.global::<Theme>().set_text_scale(scale);
                app.set_load_save_name("".into());
                app.set_load_error("".into());
                app.set_pending_preset("QA smoke test".into());
                app.invoke_confirm_preset_load();
                key(Key::Return);
                assert_eq!(action.get(), 0, "Enter must still default to Cancel.");
                action.set(-1);
                app.invoke_confirm_preset_load();
                key(Key::Tab);
                key(Key::Tab);
                key(Key::Return);
                assert!(
                    app.get_naming_preset(),
                    "Save as new must open the naming form even with an empty name."
                );
                assert_eq!(
                    action.get(),
                    -1,
                    "Entering the naming form must not save or load anything."
                );
                settle();
                assert!(app.get_preset_name_visible());
                key(Key::Return);
                assert_eq!(action.get(), -1, "An empty name must not submit.");
                let text: SharedString = "R".into();
                window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                window.dispatch_event(WindowEvent::KeyReleased { text });
                assert_eq!(
                    app.get_load_save_name(),
                    "R",
                    "The visible name field must receive keyboard focus."
                );
                key(Key::Return);
                assert_eq!(action.get(), 2);
                settle();
                assert!(
                    app.get_preset_error_visible(),
                    "Validation errors must reveal themselves."
                );
                assert!(
                    app.get_preset_load_open(),
                    "Validation errors must preserve the pending load."
                );
                key(Key::Tab);
                key(Key::Tab);
                key(Key::Return);
                assert!(
                    !app.get_naming_preset(),
                    "Back should return to the safe choice screen."
                );
                assert_eq!(
                    app.get_load_save_name(),
                    "R",
                    "Back must preserve the entered name."
                );
                key(Key::Escape);
                assert!(!app.get_preset_load_open());
            }
            app.global::<Theme>().set_text_scale(1.0);
            window.set_size(PhysicalSize::new(size.0, size.1));
            println!(
                "Empty-name entry, keyboard focus, validation reveal, Back, and safe cancellation passed at normal and 225% text sizes"
            );
        }
        "zones" | "compact-zones" | "compact-text-zones" => {
            app.set_page(2);
            if mode.starts_with("compact-text") {
                app.global::<Theme>().set_text_scale(2.25);
            }
            app.set_edge_mode(1);
            app.set_stop_zone_label("X 900  Y 640 to X 1,050  Y 790".into());
            app.set_process_mode(1);
            app.set_process_names("notepad.exe".into());
            app.invoke_show_target_panel(1);
            assert_eq!(
                app.get_capture_shortcut_kind(),
                -1,
                "Zones must not capture into a hidden route."
            );
        }
        "undo-route" | "compact-undo-route" => {
            app.set_page(2);
            app.set_smart_targets_available(true);
            app.set_route_enabled(true);
            app.set_route_count(1);
            app.set_route_rows(list(&["1. Export"]));
            app.set_route_index(0);
            app.set_can_undo_route(true);
        }
        "ux-check" => {
            let key = |value: Key| {
                let text: SharedString = value.into();
                window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                window.dispatch_event(WindowEvent::KeyReleased { text });
            };
            assert!(app.get_setup_summary().starts_with("Left click"));
            app.set_button_mode(1);
            app.set_double_click(true);
            assert!(app.get_setup_summary().starts_with("Right double click"));
            app.set_keyboard(true);
            app.set_key_text("Enter".into());
            assert!(app.get_setup_summary().starts_with("Press Enter"));
            app.set_text_output(true);
            app.set_text_character("å".into());
            assert!(app.get_setup_summary().starts_with("Type “å”"));
            app.set_keyboard(false);
            app.set_scroll_output(true);
            app.set_scroll_direction(2);
            app.set_scroll_ticks("3".into());
            assert!(app.get_setup_summary().starts_with("Scroll left · 3 ticks"));
            app.set_route_enabled(true);
            app.set_route_actions(true);
            assert!(app.get_setup_summary().starts_with("Custom route actions"));
            app.set_page(2);
            app.invoke_show_target_panel(1);
            assert_eq!(app.get_targets_panel(), 1);
            app.invoke_show_target_panel(0);
            app.set_step_editing(true);
            app.invoke_show_target_panel(1);
            assert_eq!(
                app.get_targets_panel(),
                0,
                "Switching panels must preserve step edits."
            );
            app.set_step_editing(false);
            app.set_page(3);
            app.set_pending_preset("QA smoke test".into());
            let action = Rc::new(Cell::new(-1));
            let received = action.clone();
            let weak = app.as_weak();
            app.on_resolve_preset_load(move |operation| {
                received.set(operation);
                weak.unwrap().invoke_close_preset_load();
            });
            app.invoke_confirm_preset_load();
            assert!(app.get_preset_load_open());
            key(Key::Return);
            assert_eq!(
                action.get(),
                0,
                "Enter must default to Cancel, preserving the setup."
            );
            assert!(!app.get_preset_load_open());
            action.set(-1);
            app.set_load_save_name("Kept setup".into());
            app.invoke_confirm_preset_load();
            key(Key::Tab);
            key(Key::Tab);
            key(Key::Return);
            assert!(app.get_naming_preset());
            settle();
            key(Key::Return);
            assert_eq!(
                action.get(),
                2,
                "Save as new must request saving before loading."
            );
            assert!(!app.get_preset_load_open());
            action.set(-1);
            app.invoke_confirm_preset_load();
            key(Key::Tab);
            key(Key::Return);
            assert_eq!(
                action.get(),
                1,
                "Discard & load must be an explicit choice."
            );
            assert!(!app.get_preset_load_open());
            action.set(-1);
            app.invoke_confirm_preset_load();
            key(Key::Escape);
            assert_eq!(action.get(), 0, "Escape must cancel the pending load.");
            assert!(!app.get_preset_load_open());
            println!(
                "Output summaries, Targets panels, and safe preset dialog keyboard checks passed"
            );
        }
        "wheel" | "compact-wheel" => {
            app.set_scroll_output(true);
            app.set_scroll_direction(2);
            app.set_scroll_ticks("3".into());
        }
        "triple" | "compact-triple" => {
            app.set_button_mode(4);
            app.set_triple_click(true);
        }
        "running" | "compact-running" => {
            app.set_active(true);
            app.set_run_state("Running".into());
            app.set_session_count("128".into());
            app.set_session_rate("10".into());
            app.set_session_elapsed("12.8 s".into());
            app.set_pad_count("128".into());
        }
        "timing" | "compact-timing" | "compact-text-timing" => {
            app.set_page(1);
            if mode.starts_with("compact-text") {
                app.global::<Theme>().set_text_scale(2.25);
            }
            app.set_random_rate(true);
            app.set_press_text("30".into());
            app.set_timed_press(true);
            app.set_time_limit("45".into());
            app.set_limit_summary("  ·  No action limit  ·  45 s max".into());
            app.set_pace_value("8–12 /s".into());
            app.set_low_cpu_reason("Low CPU requires Tap instead of Timed or Hold output.".into());
        }
        "keyboard" | "compact-keyboard" | "compact-text-keyboard" => {
            app.set_keyboard(true);
            app.set_key_text("Enter".into());
            if mode.starts_with("compact-text") {
                app.global::<Theme>().set_text_scale(2.25);
            }
        }
        "points" | "compact" | "smart-targets" | "compact-smart-targets" => {
            app.set_page(2);
            app.set_route_enabled(true);
            app.set_route_once(true);
            app.set_route_count(3);
            app.set_limit_summary("  ·  3 actions max  ·  No time limit".into());
            app.set_route_rows(list(&[
                "1. X -850  Y 640",
                "2. X -600  Y 480",
                "3. X -470  Y 370",
            ]));
            app.set_route_index(1);
            if mode.ends_with("smart-targets") {
                app.set_smart_targets_available(true);
                app.set_route_smart(true);
                app.set_capture_visual(true);
                app.set_route_rows(list(&[
                    "1. Export",
                    "2. Save · Wait until gone",
                    "3. X 470  Y 370",
                ]));
                app.set_route_control_rows(ModelRc::new(VecModel::from(vec![1, 2, 0])));
                app.set_pick_kind(5);
            } else {
                app.set_pick_kind(2);
            }
            app.set_stop_zone_label("X 900  Y 640 to X 1,050  Y 790".into());
            app.set_inside_pending("First corner X 200  Y 550. Pick the opposite corner.".into());
            app.set_process_mode(1);
            app.set_process_names("notepad.exe".into());
        }
        "sharing-review" | "compact-sharing-review" => {
            app.set_page(3);
            app.set_share_review_kind(2);
            app.set_share_review_name("A route with a long shared name".into());
            app.set_share_review_path("C:\\Users\\Example\\Documents\\Shared\\preset.json".into());
            app.set_share_review_metadata("Transfer format: afterbeat-preset, version 3".into());
            let mut buffer = slint::SharedPixelBuffer::<Rgb8Pixel>::new(20, 12);
            for (index, pixel) in buffer.make_mut_slice().iter_mut().enumerate() {
                let gray = ((index * 5) % 256) as u8;
                *pixel = Rgb8Pixel {
                    r: gray,
                    g: gray,
                    b: gray,
                };
            }
            let preview = slint::Image::from_rgb8(buffer);
            app.set_share_review_items(ModelRc::new(VecModel::from(vec![ShareReviewItem {
                title: "Route step 1 of 1".into(),
                details: "{\n  \"executable\": \"C:\\\\Example\\\\App.exe\",\n  \"title\": \"Private document\"\n}".into(),
                has_visual: true,
                visual_status: "Two saved 20 × 12 grayscale fingerprints.".into(),
                target_image: preview.clone(),
                landmark_image: preview,
                ..Default::default()
            }])));
            settle();
            window.dispatch_event(WindowEvent::PointerScrolled {
                position: LogicalPosition::new(size.0 as f32 - 150.0, 220.0),
                delta_x: 0.0,
                delta_y: -2000.0,
            });
            settle();
        }
        "presets" | "compact-presets" | "compact-text-presets" | "compact-text-preset-details" => {
            app.set_page(3);
            if mode.starts_with("compact-text") {
                app.global::<Theme>().set_text_scale(2.25);
            }
            app.set_profile_rows(list(&["QA smoke test", "Navigation route"]));
            app.set_profile_summaries(list(&[
                "Left click ×1 · 10 /s\nFollow cursor · 50 actions · 30 s limit\nToggle F6 · 3 s start delay\nGuards · Screen corners · Allow apps: notepad.exe",
                "Left click ×1 · 5 /s\n3-step route · Once · No action limit · No time limit\nToggle F6 · 3 s start delay\nGuards · Stop zone",
            ]));
            app.set_profile_index(0);
            app.set_selected_preset("QA smoke test".into());
            app.set_active_preset("Navigation route with a long preset name".into());
            app.set_preset_modified(true);
            app.set_library_stats("12 runs  ·  1,240 actions  ·  3 min active".into());
            if mode == "compact-text-preset-details" {
                settle();
                click(230.0, 370.0);
                settle();
            }
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
        "dry-run"
        | "dry-results-check"
        | "dry-count-check"
        | "dry-wait-check"
        | "compact-dry-run"
        | "compact-dry-blocked"
        | "compact-dry-fix" => {
            app.set_route_enabled(true);
            let counted = mode == "dry-count-check";
            let waiting = mode == "dry-wait-check";
            if counted || waiting || mode == "dry-results-check" {
                app.set_route_actions(true);
            }
            if waiting || mode == "dry-results-check" {
                app.set_route_smart(true);
            }
            if counted {
                app.set_dry_action_limit(2);
            }
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
                    blocked: i == 3 || (waiting && i == 1),
                    terminal: i == 3,
                    counts_as_action: !(counted && i == 1),
                    action_count: if counted && i == 1 { 0 } else { 1 },
                    hidden: (counted || waiting) && i == 1,
                    repair: mode == "dry-results-check" && i == 4,
                    location: format!("X {}  Y {}", (x * 2400.0) as i32, (y * 1080.0) as i32)
                        .into(),
                    inherited_output: "Left click ×1".into(),
                    result: if counted && i == 1 {
                        "Waits without sending input."
                    } else if waiting && i == 1 {
                        "Control or its saved window is missing."
                    } else if i == 3 {
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
                if counted {
                    2
                } else if waiting {
                    1
                } else {
                    3
                },
                "Playback should respect terminal stops, target waits, and action limits."
            );
            if mode == "dry-results-check" {
                click(650.0, 291.0);
                assert_eq!(app.get_dry_step(), 0, "Clicking a result should select it.");
                let key = |value: Key| {
                    let text: SharedString = value.into();
                    window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                    window.dispatch_event(WindowEvent::KeyReleased { text });
                };
                key(Key::End);
                assert_eq!(
                    app.get_dry_step(),
                    4,
                    "Results after a blocked step should be accessible."
                );
                click(step_button.0, step_button.1);
                assert_eq!(
                    app.get_dry_step(),
                    4,
                    "Inspecting later results must not resume blocked playback."
                );
                click(450.0, 132.0);
                click(step_button.0, step_button.1);
                assert_eq!(
                    app.get_dry_step(),
                    0,
                    "Reset should restart playback at the first step."
                );
                click(650.0, 264.0);
                key(Key::End);
                let repaired = Rc::new(Cell::new(-1));
                let requested = repaired.clone();
                let weak = app.as_weak();
                app.on_repair_target(move || requested.set(weak.unwrap().get_dry_step()));
                key(Key::Tab);
                key(Key::Return);
                assert_eq!(
                    repaired.get(),
                    4,
                    "Repair should act on the selected result."
                );
            }
            if mode == "compact-dry-fix" {
                click(131.0, 183.0);
                assert_eq!(app.get_page(), 2, "Fix should open Targets.");
                assert_eq!(
                    app.get_targets_panel(),
                    1,
                    "Fix must open Zones & filters for an inside-only error."
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
                slint::platform::update_timers_and_animations();
            }
        }
        "compact-text"
        | "compact-text-shortcuts"
        | "compact-text-running"
        | "compact-text-paused"
        | "compact-text-blocked" => {
            app.global::<Theme>().set_text_scale(2.25);
            app.set_toggle_key("Ctrl+Shift+Alt+Backspace".into());
            app.set_stop_key("Ctrl+Alt+F7".into());
            app.set_capture_key("Ctrl+Alt+F8".into());
            if mode == "compact-text" {
                app.set_pad_count("12,345".into());
                click(105.0, 68.0);
                click(105.0, 114.0);
                assert_eq!(
                    app.get_page(),
                    1,
                    "The labeled Timing button should be clickable."
                );
                click(105.0, 164.0);
                assert_eq!(app.get_page(), 2);
                click(105.0, 215.0);
                assert_eq!(app.get_page(), 3);
                click(105.0, 68.0);
                assert_eq!(app.get_page(), 0);
                assert!(
                    app.invoke_test_pad_at(105.0, 310.0),
                    "The expanded Test pad should count hits."
                );
                assert!(!app.invoke_test_pad_at(165.0, 310.0));
                let reset = Rc::new(Cell::new(false));
                let invoked = reset.clone();
                app.on_reset_pad(move || invoked.set(true));
                click(116.0, 388.0);
                assert!(reset.get(), "Reset should be reachable at 225% text size.");
            }
            if mode == "compact-text-shortcuts" {
                click(230.0, 370.0);
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
            if mode == "compact-text-running" {
                app.set_active(true);
                app.set_run_state("Running".into());
                app.set_session_count("128".into());
                app.set_session_rate("10".into());
                app.set_session_elapsed("12.8 s".into());
                settle();
                assert!(
                    app.invoke_stop_button_on_screen(),
                    "The emergency Stop control must fit the minimum window."
                );
            }
            if mode == "compact-text-blocked" {
                app.set_configuration_field("inside".into());
                app.set_configuration_error(
                    "Clear the inside-only zone before running a route.".into(),
                );
                app.set_inside_label("X 20  Y 40 to X 980  Y 760".into());
                app.set_route_enabled(true);
                click(225.0, 370.0);
                std::thread::sleep(std::time::Duration::from_millis(10));
                slint::platform::update_timers_and_animations();
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
        "workflow-check" => {
            let selected = Rc::new(Cell::new(-1));
            let output = selected.clone();
            app.on_preset_interval(move |millis| output.set(millis));
            app.set_hold_down(true);
            click(545.0, 171.0);
            assert_eq!(
                selected.get(),
                -1,
                "Hold output should disable interval presets."
            );
            app.set_hold_down(false);
            app.set_random_rate(true);
            click(570.0, 171.0);
            assert!(
                !app.get_random_rate(),
                "Using a fixed interval should be explicit."
            );
            assert_eq!(
                selected.get(),
                -1,
                "The mode switch should leave the interval intact."
            );
            click(545.0, 171.0);
            assert_eq!(selected.get(), 10);

            app.set_page(2);
            app.set_step_editing(true);
            app.set_step_edited(true);
            app.set_step_delay("725".into());
            click(62.0, 148.0);
            assert_eq!(
                app.get_page(),
                2,
                "Opening another page should prompt before discarding edits."
            );
            let text: SharedString = Key::Return.into();
            window.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            window.dispatch_event(WindowEvent::KeyReleased { text });
            assert!(
                app.get_step_editing(),
                "Keep editing should retain the draft."
            );
            assert_eq!(app.get_step_delay(), "725");
            println!("Interval mode and route draft checks passed");
        }
        "wayland-recovery" => {
            app.set_native_wayland(true);
            app.set_can_start(false);
            app.set_has_toggle(false);
            app.set_stop_key("—".into());
            app.set_capture_key("—".into());
            let command = Rc::new(Cell::new(-1));
            let received = command.clone();
            app.on_wayland_action(move |operation| received.set(operation));
            let recover = || click(248.0, 528.0);
            recover();
            assert_eq!(command.get(), 0, "Disconnected input should offer setup.");
            app.set_fixed_target(true);
            recover();
            assert_eq!(command.get(), 1, "Fixed points should request displays.");
            command.set(-1);
            app.set_wayland_pending(true);
            recover();
            assert_eq!(
                command.get(),
                -1,
                "A pending request should not restart setup."
            );
            app.set_wayland_pending(false);
            app.set_wayland_ready(true);
            app.set_wayland_configure(true);
            app.set_wayland_status("Bind an emergency stop shortcut in Desktop shortcuts.".into());
            recover();
            assert_eq!(
                command.get(),
                3,
                "A missing Stop shortcut should open desktop shortcuts."
            );
            println!("Wayland setup and shortcut recovery actions passed");
        }
        "keyboard-check" => {
            let pick_kind = app.get_pick_kind();
            app.set_pick_kind(0);
            assert_eq!(app.get_capture_shortcut_kind(), 0);
            app.set_page(2);
            assert_eq!(app.get_capture_shortcut_kind(), 1);
            app.set_pick_kind(3);
            assert_eq!(
                app.get_capture_shortcut_kind(),
                1,
                "The Route tab must not capture an unseen zone."
            );
            app.invoke_show_target_panel(1);
            assert_eq!(
                app.get_capture_shortcut_kind(),
                3,
                "An explicitly selected stop zone stays capturable in Zones."
            );
            app.invoke_show_target_panel(0);
            app.set_pick_kind(5);
            app.invoke_show_target_panel(1);
            assert_eq!(
                app.get_capture_shortcut_kind(),
                -1,
                "Control capture must not add a hidden route step from Zones."
            );
            app.invoke_show_target_panel(0);
            assert_eq!(
                app.get_capture_shortcut_kind(),
                5,
                "Route control capture must still work."
            );
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
            // Two Left presses reach the start of "30" regardless of click placement.
            press(Key::LeftArrow);
            press(Key::LeftArrow);
            press(Key::Delete);
            assert_eq!(app.get_press_text(), "0");
            press(Key::RightArrow);
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
            // Use the same focus path as Fix in Clicker. Font fallback can
            // change field geometry across native Windows, Linux, and macOS.
            app.set_configuration_field("input-mode".into());
            app.set_configuration_error("Choose an input.".into());
            app.invoke_recover_start();
            settle();
            app.set_configuration_error("".into());
            app.set_configuration_field("".into());
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

            app.set_page(0);
            for (width, height) in [size, (640, 530)] {
                window.set_size(PhysicalSize::new(width, height));
                let x = width as f32 - 90.0;
                let y = height as f32 - 40.0;
                assert!(!app.invoke_stop_run_at(x, y));
                app.set_active(true);
                assert!(app.invoke_stop_run_at(x, y));
                assert!(!app.invoke_stop_run_at(20.0, y));
                app.set_stopping(true);
                assert!(!app.invoke_stop_run_at(x, y));
                app.set_stopping(false);
                app.set_active(false);
            }
            window.set_size(PhysicalSize::new(size.0, size.1));
            println!(
                "Arrow navigation, grouped Tab stops, capture shortcuts, fractional presses, and Stop hit testing passed"
            );
        }
        _ => {}
    }

    if matches!(
        mode.as_str(),
        "compact-text-presets"
            | "compact-text-preset-details"
            | "compact-text-timing"
            | "compact-text-zones"
            | "compact-text-keyboard"
            | "compact-text-route-reorder"
    ) && let Some(distance) = args.next()
    {
        settle();
        window.dispatch_event(WindowEvent::PointerScrolled {
            position: LogicalPosition::new(400.0, 200.0),
            delta_x: 0.0,
            delta_y: -distance.parse::<f32>().unwrap(),
        });
        settle();
    }

    let mut pixels = vec![Rgb8Pixel::default(); (size.0 * size.1) as usize];
    app.window().request_redraw();
    assert!(window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, size.0 as usize);
    }));
    // Let nested layouts and focus-reveal timers settle before exporting the frame.
    for _ in 0..2 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        slint::platform::update_timers_and_animations();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, size.0 as usize);
        });
    }
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
