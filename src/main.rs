#![cfg_attr(windows, windows_subsystem = "windows")]

mod automation;
#[cfg(windows)]
mod desktop_shell;
mod diagnostics;
mod dry_run;
mod engine;
#[cfg_attr(target_os = "linux", path = "linux/hotkeys.rs")]
#[cfg_attr(target_os = "macos", path = "macos/hotkeys.rs")]
mod hotkeys;
#[cfg_attr(target_os = "linux", path = "x11/input_filter.rs")]
#[cfg_attr(target_os = "macos", path = "macos/input_filter.rs")]
mod input_filter;
#[cfg_attr(target_os = "linux", path = "x11/keys.rs")]
#[cfg_attr(target_os = "macos", path = "macos/keys.rs")]
mod keys;
#[cfg(target_os = "macos")]
mod macos;
mod mouse;
mod settings;
#[cfg(windows)]
mod smart_targets;
#[cfg(windows)]
mod system_appearance;
#[cfg(windows)]
mod target_pixels;
mod targets;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "linux")]
mod x11;

#[cfg(windows)]
use desktop_shell::{DesktopShell, RunState};
use engine::{Checks, Outcome, PauseReason, Progress, Schedule, StopSignal};
use hotkeys::{Action, Binding, Hotkeys};
use mouse::{Button, Point, Zone};
use settings::{Library, Settings};
use slint::{CloseRequestResponse, ComponentHandle, Timer, TimerMode, Weak};
use std::{
    cell::RefCell,
    error::Error,
    io,
    path::{Path, PathBuf},
    rc::{Rc, Weak as RcWeak},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
#[cfg(windows)]
use std::{
    os::windows::io::{FromRawHandle, OwnedHandle},
    ptr::{null, null_mut},
};
use targets::Step;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_ALREADY_EXISTS, GetLastError},
    System::Threading::CreateMutexW,
    UI::WindowsAndMessaging::{MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW},
};

slint::include_modules!();

const CAPTURE_DELAY: Duration = Duration::from_secs(3);
const DRY_SAMPLE: i32 = 4;
#[cfg(windows)]
const CONTROL_CAPTURE: i32 = 5;
const UI_INTERVAL: Duration = Duration::from_millis(80);
type StopSlot = Arc<Mutex<StopState>>;
type RunResult = (Instant, Result<Outcome, String>);

#[derive(Default)]
struct StopState {
    run: Option<Arc<StopSignal>>,
    last_emergency: Option<u64>,
    capture: Option<Capture>,
    last_capture_finished: Option<u64>,
}

impl StopState {
    fn cancelled(&self, requested_at: u64) -> bool {
        self.last_emergency.is_some_and(|stop| requested_at <= stop)
    }

    fn signal(&self) -> bool {
        if let Some(stop) = &self.run {
            stop.request();
            return true;
        }
        false
    }

    fn receive_capture(&mut self, point: Point, requested_at: u64) -> bool {
        if self.run.is_some()
            || self.cancelled(requested_at)
            || self
                .last_capture_finished
                .is_some_and(|finished| requested_at <= finished)
        {
            return true;
        }
        let Some(capture) = &mut self.capture else {
            return false;
        };
        let expires = capture.requested_at + CAPTURE_DELAY.as_millis() as u64;
        if (capture.requested_at..expires).contains(&requested_at) && capture.point.is_none() {
            capture.point = Some(point);
        }
        true
    }

    fn finish_capture(&mut self) {
        if self.capture.take().is_some() {
            self.last_capture_finished = Some(hotkeys::tick_count());
        }
    }
}

struct Run {
    stop: Arc<StopSignal>,
    progress: Arc<Progress>,
    worker: JoinHandle<()>,
    completion: mpsc::Receiver<RunResult>,
    countdown: Instant,
    burst: bool,
}

struct State {
    settings: Settings,
    saved: Settings,
    settings_dirty: bool,
    library: Library,
    library_dirty: bool,
    run: Option<Run>,
    zone_origin: Option<(i32, Point)>,
    pad_hits: u64,
}

#[derive(Clone, Copy)]
struct Capture {
    deadline: Instant,
    kind: i32,
    requested_at: u64,
    point: Option<Point>,
}

struct Controller {
    app: Weak<App>,
    own: RcWeak<Self>,
    state: RefCell<State>,
    stop_slot: StopSlot,
    path: Option<PathBuf>,
    tick: Timer,
    save_timer: Timer,
    hotkeys: Hotkeys,
    #[cfg(windows)]
    target_service: OnceCell<smart_targets::Service>,
    #[cfg(windows)]
    target_job: RefCell<Option<TargetJob>>,
    #[cfg(windows)]
    target_proposals: RefCell<Vec<Option<smart_targets::Target>>>,
    #[cfg(windows)]
    shell: RefCell<Option<DesktopShell>>,
    #[cfg(not(target_os = "macos"))]
    input_filter: RefCell<Option<input_filter::InputFilter>>,
}

#[cfg(windows)]
use std::cell::OnceCell;

#[cfg(windows)]
enum TargetJob {
    Capture {
        requested_at: u64,
        deadline: Instant,
        reply: smart_targets::Request<targets::ControlStep>,
    },
    Preview {
        deadline: Instant,
        reply: smart_targets::Request<Vec<smart_targets::Resolution>>,
    },
    Repair {
        index: usize,
        original: Box<targets::ControlStep>,
        deadline: Instant,
        reply: smart_targets::Request<targets::ControlStep>,
    },
}

fn main() {
    #[cfg(windows)]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--licenses")
    {
        print!(
            "{}\n{}",
            include_str!("../LICENSE"),
            include_str!("../THIRD-PARTY-NOTICES.txt")
        );
        return;
    }
    #[cfg(not(windows))]
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--version")
    {
        println!("afterbeat {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    diagnostics::install_panic_hook();
    if let Err(error) = launch() {
        diagnostics::record(&format!("Startup failed: {error}"));
        let message = format!("Afterbeat could not start.\n\n{error}");
        #[cfg(windows)]
        alert(&message, MB_ICONERROR);
        #[cfg(not(windows))]
        {
            eprintln!("{message}");
            if let Ok(dialog) = StartupError::new() {
                dialog.set_message(message.into());
                let _ = dialog.run();
            }
            std::process::exit(1);
        }
    }
}

fn launch() -> Result<(), Box<dyn Error>> {
    #[cfg(windows)]
    let Some(_instance) = acquire_windows_instance()? else {
        return Ok(());
    };
    #[cfg(target_os = "linux")]
    let _instance = {
        wayland::select(wayland::detect());
        if wayland::active() {
            let instance = wayland::acquire_instance()?;
            wayland::select_backend()?;
            (None, Some(instance))
        } else {
            let Some(instance) = x11::shell::Instance::acquire()? else {
                return Ok(());
            };
            x11::shell::select_backend()?;
            (Some(instance), None)
        }
    };
    #[cfg(target_os = "macos")]
    let Some(_instance) = macos::acquire_instance()? else {
        return Ok(());
    };

    let app = App::new()?;
    #[cfg(windows)]
    let _appearance = system_appearance::watch(&app);
    #[cfg(target_os = "macos")]
    let _keyboard_layout = keys::watch_layout();
    #[cfg(target_os = "linux")]
    {
        app.set_process_example("firefox".into());
        app.set_native_wayland(wayland::active());
    }
    #[cfg(target_os = "macos")]
    app.set_process_example("Safari, Finder".into());
    run_app(app)
}

#[cfg(windows)]
fn acquire_windows_instance() -> io::Result<Option<OwnedHandle>> {
    // Share the single-instance guard with older releases.
    let name = wide(r"Local\PulseClick.Desktop.SingleInstance");
    let instance = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
    let last_error = unsafe { GetLastError() };
    if instance.is_null() {
        return Err(io::Error::from_raw_os_error(last_error as i32));
    }
    let _instance = unsafe { OwnedHandle::from_raw_handle(instance) };
    if last_error == ERROR_ALREADY_EXISTS {
        if !desktop_shell::activate_existing() {
            alert("Afterbeat is already open.", MB_ICONINFORMATION);
        }
        return Ok(None);
    }
    Ok(Some(_instance))
}

fn run_app(app: App) -> Result<(), Box<dyn Error>> {
    let stop_slot = StopSlot::default();
    let path = settings::path();
    let (mut settings, recovered_settings, settings_error) =
        load_store::<Settings>(path.as_deref());
    let library_path = path
        .as_ref()
        .map(|file| file.with_file_name("library.json"));
    let (library, recovered_library, library_error) =
        load_store::<Library>(library_path.as_deref());
    let load_notice = if settings_error || library_error {
        "Some local data could not be read. See Help → Open data folder."
    } else if recovered_settings || recovered_library {
        "Local data recovered from its previous good copy."
    } else {
        ""
    };
    settings.normalize();
    let hotkeys = connect_shortcuts(stop_slot.clone(), &settings.trigger)?;

    let controller = Rc::new_cyclic(|own| Controller {
        app: app.as_weak(),
        own: own.clone(),
        state: RefCell::new(State {
            saved: settings.clone(),
            settings_dirty: recovered_settings,
            settings,
            library,
            library_dirty: recovered_library,
            run: None,
            zone_origin: None,
            pad_hits: 0,
        }),
        stop_slot,
        path,
        tick: Timer::default(),
        save_timer: Timer::default(),
        hotkeys,
        #[cfg(windows)]
        target_service: OnceCell::new(),
        #[cfg(windows)]
        target_job: RefCell::new(None),
        #[cfg(windows)]
        target_proposals: RefCell::new(Vec::new()),
        #[cfg(windows)]
        shell: RefCell::new(None),
        #[cfg(not(target_os = "macos"))]
        input_filter: RefCell::new(None),
    });
    ACTIVE_CONTROLLER.with(|local| *local.borrow_mut() = Rc::downgrade(&controller));
    controller.wire(&app, load_notice);
    #[cfg(not(target_os = "macos"))]
    controller.attach_input_filter(8);
    #[cfg(target_os = "macos")]
    {
        let ready = controller.hotkeys.listening();
        let permissions_needed = !ready && !macos::has_permissions();
        app.set_input_ready(ready);
        app.set_input_permissions_help(macos::PERMISSIONS_HELP.into());
        app.set_input_permissions_needed(permissions_needed);
        app.set_shortcut_failure(!ready && !permissions_needed);
        if ready {
            diagnostics::record("Input protection active.");
        }
        app.on_request_input_permissions(macos::request_permissions);
    }
    #[cfg(target_os = "macos")]
    let _quit_observer = macos::QuitObserver::install();
    let result = app.run();
    controller.shutdown();
    result?;
    Ok(())
}

fn load_store<T: serde::de::DeserializeOwned + Default>(path: Option<&Path>) -> (T, bool, bool) {
    let Some(path) = path else {
        return (T::default(), false, true);
    };
    match settings::load(path) {
        Ok(data) => (data.value, data.recovered, false),
        Err(error) => {
            diagnostics::record(&format!(
                "Read {}: {error}",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            (T::default(), false, true)
        }
    }
}

thread_local! {
    static ACTIVE_CONTROLLER: RefCell<RcWeak<Controller>> = const { RefCell::new(RcWeak::new()) };
}

fn connect_shortcuts(stop_slot: StopSlot, preferred: &str) -> io::Result<Hotkeys> {
    Hotkeys::start(preferred, move |action| {
        let interrupted = match action {
            Action::Toggle(_, requested_at) => {
                let slot = stop_slot.lock().unwrap();
                slot.cancelled(requested_at) || slot.signal()
            }
            Action::Stop(stopped_at) => {
                let mut slot = stop_slot.lock().unwrap();
                slot.last_emergency = slot.last_emergency.max(Some(stopped_at));
                slot.signal()
            }
            Action::Unavailable => signal_stop(&stop_slot),
            #[cfg(target_os = "linux")]
            Action::PortalChanged => signal_stop(&stop_slot),
            #[cfg(target_os = "linux")]
            Action::PortalStatus => false,
            Action::Capture(point, requested_at) => stop_slot
                .lock()
                .unwrap()
                .receive_capture(point, requested_at),
        };
        let _ = slint::invoke_from_event_loop(move || {
            let controller = ACTIVE_CONTROLLER.with(|local| local.borrow().upgrade());
            if let Some(controller) = controller
                && let Some(ui) = controller.app.upgrade()
            {
                match action {
                    Action::Toggle(binding, requested_at)
                        if !interrupted && controller.hotkeys.toggle_binding() == Some(binding) =>
                    {
                        controller.toggle_with(&ui, Some(binding), requested_at)
                    }
                    Action::Toggle(..) => controller.refresh(&ui),
                    Action::Stop(_) => controller.refresh(&ui),
                    Action::Capture(point, requested_at)
                        if !interrupted && !ui.get_active() && ui.get_page() != 4 =>
                    {
                        controller.capture_shortcut(&ui, point, requested_at)
                    }
                    Action::Capture(..) => controller.refresh(&ui),
                    #[cfg(target_os = "linux")]
                    Action::PortalChanged | Action::PortalStatus => {
                        controller.display_shortcuts(&ui);
                        controller.update_configuration(&ui, &controller.state.borrow().settings);
                        controller.refresh(&ui);
                    }
                    Action::Unavailable => {
                        ui.set_can_start(false);
                        ui.set_has_toggle(false);
                        ui.set_toggle_key("—".into());
                        ui.set_stop_key("—".into());
                        ui.set_capture_key("—".into());
                        ui.set_shortcut_failure(true);
                        #[cfg(target_os = "macos")]
                        ui.set_input_permissions_needed(!macos::has_permissions());
                        diagnostics::record("Global shortcut listener stopped.");
                        controller.refresh(&ui);
                    }
                }
            }
        });
    })
}

fn signal_stop(slot: &StopSlot) -> bool {
    slot.lock().unwrap().signal()
}

impl Controller {
    fn wire(self: &Rc<Self>, ui: &App, load_notice: &str) {
        ui.set_smart_targets_available(cfg!(windows));
        {
            let state = self.state.borrow();
            self.display_settings(ui, &state.settings);
            Self::display_library(ui, &state.library);
        }
        self.display_shortcuts(ui);
        if !load_notice.is_empty() {
            status(ui, load_notice);
        }

        macro_rules! bind {
            ($signal:ident, $handler:ident $(, $arg:ident)*) => {{
                let controller = self.own.clone();
                ui.$signal(move |$($arg),*| {
                    if let Some(controller) = controller.upgrade()
                        && let Some(ui) = controller.app.upgrade()
                    {
                        controller.$handler(&ui $(, $arg)*);
                    }
                });
            }};
        }
        bind!(on_settings_edited, settings_edited);
        bind!(on_toggle_run, toggle);
        bind!(on_capture_point, capture, kind);
        bind!(on_test_hit, test_hit);
        bind!(on_test_hits, test_hits, count);
        bind!(on_reset_pad, reset_pad);
        bind!(on_apply_shortcut, apply_shortcut);
        bind!(on_edit_route, edit_route, operation);
        #[cfg(windows)]
        bind!(on_toggle_target_verification, toggle_target_verification);
        #[cfg(windows)]
        bind!(on_edit_step, edit_step, index);
        #[cfg(windows)]
        bind!(on_save_step, save_step);
        #[cfg(windows)]
        bind!(on_repair_target, repair_target);
        bind!(on_clear_zone, clear_zone, kind);
        bind!(on_open_dry_run, open_dry_run);
        bind!(on_sample_dry_run, sample_dry_run);
        bind!(on_profile_action, profile_action, operation);
        bind!(on_preset_interval, preset_interval, millis);
        bind!(on_select_unit, select_unit, unit);
        bind!(on_open_data_folder, open_data_folder);
        #[cfg(target_os = "linux")]
        bind!(on_wayland_action, wayland_action, operation);

        let slot = self.stop_slot.clone();
        ui.window().on_close_requested(move || {
            signal_stop(&slot);
            CloseRequestResponse::HideWindow
        });
    }

    #[cfg(not(target_os = "macos"))]
    fn attach_input_filter(&self, attempts: u8) {
        #[cfg(target_os = "linux")]
        if wayland::active() {
            if let Some(ui) = self.app.upgrade() {
                wayland::attach(&ui, self.stop_slot.clone());
                ui.set_input_ready(true);
            }
            return;
        }
        let weak = self.own.clone();
        Timer::single_shot(Duration::from_millis(50), move || {
            let Some(controller) = weak.upgrade() else {
                return;
            };
            let Some(ui) = controller.app.upgrade() else {
                return;
            };
            #[cfg(windows)]
            let attached = desktop_shell::window_handle(&ui).and_then(|hwnd| {
                let mut shell = controller.shell.borrow_mut();
                if shell.is_none() {
                    match DesktopShell::attach(hwnd, controller.stop_slot.clone()) {
                        Ok(desktop) => *shell = Some(desktop),
                        Err(error) => {
                            diagnostics::record(&format!("Windows shell integration: {error}"))
                        }
                    }
                }
                input_filter::InputFilter::attach(&ui, hwnd)
            });
            #[cfg(target_os = "linux")]
            let attached =
                x11::shell::window_handle(&ui).and_then(input_filter::InputFilter::attach);
            match attached {
                Ok(filter) => {
                    *controller.input_filter.borrow_mut() = Some(filter);
                    ui.set_input_ready(true);
                    diagnostics::record("Input protection active.");
                }
                Err(_) if attempts > 1 => controller.attach_input_filter(attempts - 1),
                Err(error) => {
                    ui.set_input_failed(true);
                    diagnostics::record(&format!("Could not attach the input filter: {error}"));
                }
            }
        });
    }

    fn open_data_folder(&self, ui: &App) {
        if let Err(error) = diagnostics::open_folder() {
            failure(ui, &error.to_string());
        }
    }

    fn open_dry_run(&self, ui: &App) {
        if ui.get_active() || ui.get_capturing() {
            return;
        }
        #[cfg(windows)]
        if ui.get_dry_checking() {
            return;
        }
        #[cfg(windows)]
        self.target_proposals.borrow_mut().clear();
        let settings = self.state.borrow().settings.clone();
        if let Err(error) = dry_run::show(ui, &settings, None) {
            failure(ui, error);
        } else {
            ui.set_dry_message("".into());
            ui.set_dry_error(false);
            #[cfg(windows)]
            if settings.has_smart_route() {
                match self
                    .target_service()
                    .and_then(|service| service.preview(settings.route))
                {
                    Ok(reply) => {
                        *self.target_job.borrow_mut() = Some(TargetJob::Preview {
                            deadline: Instant::now() + Duration::from_secs(15),
                            reply,
                        });
                        ui.set_dry_checking(true);
                        ui.set_dry_message("Checking the route’s controls on the desktop…".into());
                        self.start_tick();
                    }
                    Err(error) => {
                        ui.set_dry_message(error.into());
                        ui.set_dry_error(true);
                    }
                }
            }
        }
    }

    fn sample_dry_run(&self, ui: &App) {
        if ui.get_page() == 4 && dry_run::can_sample(&self.state.borrow().settings) {
            self.capture(ui, DRY_SAMPLE);
        }
    }

    fn select_unit(&self, ui: &App, unit: i32) {
        if ui.get_active() || ui.get_capturing() {
            return;
        }
        let Ok(unit) = u8::try_from(unit) else {
            return;
        };
        let mut state = self.state.borrow_mut();
        state.settings.change_unit(unit);
        ui.set_interval_unit(state.settings.unit.into());
        ui.set_interval_text(state.settings.interval.as_str().into());
        drop(state);
        self.settings_edited(ui);
    }

    fn display_shortcuts(&self, ui: &App) {
        ui.set_toggle_key(self.hotkeys.label().unwrap_or_else(|| "—".into()).into());
        ui.set_has_toggle(self.hotkeys.toggle_binding().is_some());
        #[cfg(target_os = "linux")]
        {
            let stop = self.hotkeys.stop_label();
            ui.set_stop_key(stop.as_deref().unwrap_or("—").into());
            ui.set_capture_key(self.hotkeys.capture_label().unwrap_or("—").into());
            ui.set_can_start(stop.is_some());
            if wayland::active() {
                let state = wayland::status();
                ui.set_can_start(state.ready && state.stop.is_some());
                ui.set_wayland_ready(state.ready);
                ui.set_wayland_pending(state.pending);
                ui.set_wayland_configure(state.configure);
                ui.set_wayland_displays(state.displays.into());
                ui.set_wayland_status(state.message.into());
                let _ = wayland::layout_unchanged(ui);
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            ui.set_stop_key(self.hotkeys.shortcuts.stop.unwrap_or("—").into());
            ui.set_capture_key(self.hotkeys.shortcuts.capture.unwrap_or("—").into());
            ui.set_can_start(self.hotkeys.shortcuts.stop.is_some());
        }
    }

    #[cfg(target_os = "linux")]
    fn wayland_action(&self, ui: &App, operation: i32) {
        if let Hotkeys::Wayland(hotkeys) = &self.hotkeys {
            match operation {
                0 | 1
                    if !ui.get_active() && !ui.get_wayland_pending() && !ui.get_wayland_ready() =>
                {
                    if hotkeys.enable(operation == 1) {
                        ui.set_wayland_pending(true);
                    } else {
                        failure(ui, "Could not start the Wayland permission request.");
                    }
                }
                2 => {
                    signal_stop(&self.stop_slot);
                    ui.set_can_start(false);
                    hotkeys.disable();
                }
                3 if !ui.get_active() => hotkeys.configure(),
                _ => {}
            }
        }
    }

    fn apply_shortcut(&self, ui: &App) {
        if ui.get_active() || ui.get_capturing() {
            return;
        }
        let text = ui.get_trigger_text();
        match self.hotkeys.rebind(text.as_str()) {
            Ok(()) => {
                let mut state = self.state.borrow_mut();
                state.settings.trigger = text.trim().into();
                self.update_configuration(ui, &state.settings);
                self.display_shortcuts(ui);
                ui.set_shortcut_error("".into());
                status(ui, "");
                self.queue_save();
            }
            Err(error) => ui.set_shortcut_error(error.into()),
        }
    }

    fn preset_interval(&self, ui: &App, millis: i32) {
        if ui.get_active() || ui.get_capturing() {
            return;
        }
        ui.set_interval_text(millis.to_string().into());
        ui.set_interval_unit(0);
        ui.set_random_rate(false);
        ui.set_hold_down(false);
        self.settings_edited(ui);
    }

    fn display_settings(&self, ui: &App, settings: &Settings) {
        ui.set_interval_text(settings.interval.as_str().into());
        ui.set_count_text(settings.count.as_str().into());
        ui.set_key_text(settings.key.as_str().into());
        ui.set_text_output(settings.text_output);
        ui.set_text_character(settings.text_character.as_str().into());
        ui.set_time_limit(settings.time_limit.as_str().into());
        ui.set_press_text(settings.press.as_str().into());
        ui.set_timed_press(settings.press_duration() != Ok(Duration::ZERO));
        ui.set_min_rate(settings.min_rate.as_str().into());
        ui.set_max_rate(settings.max_rate.as_str().into());
        ui.set_route_radius(settings.route_radius.as_str().into());
        ui.set_target_timeout(settings.target_timeout.as_str().into());
        ui.set_capture_visual(settings.capture_visual);
        ui.set_double_click(settings.double);
        ui.set_triple_click(settings.triple);
        ui.set_scroll_output(settings.scroll && !settings.keyboard);
        ui.set_scroll_direction(settings.scroll_direction.index());
        ui.set_scroll_ticks(settings.scroll_ticks.as_str().into());
        ui.set_limited(settings.limited);
        ui.set_fixed_target(settings.fixed);
        ui.set_keyboard(settings.keyboard);
        ui.set_hold_activation(settings.hold_activation);
        ui.set_armed(settings.armed);
        ui.set_hold_down(settings.hold_down);
        ui.set_random_rate(settings.random_rate);
        ui.set_low_cpu(settings.low_cpu);
        ui.set_route_enabled(settings.route_enabled);
        ui.set_route_once(settings.route_once);
        ui.set_delay(settings.delay_seconds.into());
        ui.set_interval_unit(settings.unit.into());
        ui.set_edge_mode(settings.edge_mode.into());
        ui.set_process_mode(settings.process_mode.into());
        ui.set_process_names(settings.process_names.as_str().into());
        ui.set_trigger_text(settings.trigger.as_str().into());
        ui.set_button_mode(settings.button.index());
        self.update_configuration(ui, settings);
        Self::display_points(ui, settings, None);
    }

    fn settings_edited(&self, ui: &App) {
        if ui.get_active() || ui.get_capturing() {
            return;
        }
        let mut state = self.state.borrow_mut();
        let settings = &mut state.settings;
        settings.interval = ui.get_interval_text().to_string();
        settings.count = ui.get_count_text().to_string();
        settings.key = ui.get_key_text().to_string();
        settings.text_output = ui.get_text_output();
        settings.text_character = ui.get_text_character().to_string();
        settings.time_limit = ui.get_time_limit().to_string();
        settings.press = ui.get_press_text().to_string();
        settings.min_rate = ui.get_min_rate().to_string();
        settings.max_rate = ui.get_max_rate().to_string();
        settings.route_radius = ui.get_route_radius().to_string();
        settings.target_timeout = ui.get_target_timeout().to_string();
        settings.capture_visual = ui.get_capture_visual();
        settings.double = ui.get_double_click();
        settings.triple = ui.get_triple_click();
        settings.scroll = ui.get_scroll_output() && !ui.get_keyboard();
        settings.scroll_direction = targets::Direction::from_index(ui.get_scroll_direction());
        settings.scroll_ticks = ui.get_scroll_ticks().to_string();
        settings.limited = ui.get_limited();
        settings.fixed = ui.get_fixed_target();
        settings.keyboard = ui.get_keyboard();
        settings.hold_activation = ui.get_hold_activation();
        settings.armed = ui.get_armed();
        settings.hold_down = ui.get_hold_down();
        settings.random_rate = ui.get_random_rate();
        settings.low_cpu = ui.get_low_cpu();
        settings.route_enabled = ui.get_route_enabled();
        settings.route_once = ui.get_route_once();
        settings.delay_seconds = ui.get_delay() as u8;
        settings.unit = ui.get_interval_unit() as u8;
        settings.edge_mode = ui.get_edge_mode() as u8;
        settings.process_mode = ui.get_process_mode() as u8;
        settings.process_names = ui.get_process_names().to_string();
        settings.button = Button::from_index(ui.get_button_mode());
        self.update_configuration(ui, settings);
        status(ui, "");
        drop(state);
        self.queue_save();
    }

    fn update_configuration(&self, ui: &App, settings: &Settings) {
        ui.set_route_actions(settings.has_action_route());
        #[cfg(target_os = "linux")]
        if wayland::active() {
            wayland::set_hold_mode(settings.hold_activation);
        }
        let binding = self.hotkeys.toggle_binding();
        let schedule = automation::validated_schedule(settings, binding);
        let issue = schedule.as_ref().err();
        ui.set_low_cpu_available(automation::low_cpu_available(settings, binding));
        ui.set_configuration_error(issue.map_or("", |issue| issue.message).into());
        ui.set_configuration_field(issue.map_or("", |issue| issue.field).into());
        let mut limits = String::new();
        if let Ok(schedule) = schedule {
            if let Some(limit) = schedule.limit {
                limits.push_str(&format!("  ·  {} actions", grouped(limit)));
            }
            if schedule.duration.is_some() {
                limits.push_str(&format!("  ·  {} s limit", settings.time_limit.trim()));
            }
        }
        ui.set_limit_summary(limits.into());
        if ui.get_run_state() == "Last run" {
            ui.set_run_state("".into());
        }
        let guards = [
            (settings.edge_mode != 0, "Screen stop"),
            (settings.inside_zone.is_some(), "Inside only"),
            (settings.stop_zone.is_some(), "Stop zone"),
            (settings.process_mode != 0, "App filter"),
        ];
        ui.set_guard_summary(
            guards
                .into_iter()
                .filter_map(|(enabled, label)| enabled.then_some(label))
                .collect::<Vec<_>>()
                .join(", ")
                .into(),
        );
        ui.set_point_label(
            settings
                .point
                .map_or_else(|| "No point captured".into(), point_label)
                .into(),
        );
        let pace = if settings.has_action_route() {
            "Step timing".into()
        } else if settings.hold_down {
            "Held down".into()
        } else {
            match settings.rates() {
                Ok(Some((low, high))) => format!("{}–{} /s", rate_label(low), rate_label(high)),
                Ok(None) => match settings.interval() {
                    Ok(interval) if interval.is_zero() => "Max speed".into(),
                    Ok(interval) => format!("{} /s", rate_label(interval.as_secs_f64().recip())),
                    Err(_) => "Check timing".into(),
                },
                Err(_) => "Check rate range".into(),
            }
        };
        ui.set_pace_value(pace.into());
    }

    fn display_points(ui: &App, settings: &Settings, origin: Option<(i32, Point)>) {
        let model: Vec<slint::SharedString> = settings
            .route
            .iter()
            .enumerate()
            .map(|(i, step)| format!("{}. {}", i + 1, step.label()).into())
            .collect();
        ui.set_route_smart(settings.route.iter().any(Step::is_smart));
        ui.set_route_control_rows(slint::ModelRc::new(slint::VecModel::from(
            settings
                .route
                .iter()
                .map(|step| {
                    step.control()
                        .map_or(0, |step| if step.wait_for_disappearance { 2 } else { 1 })
                })
                .collect::<Vec<i32>>(),
        )));
        ui.set_route_count(model.len() as i32);
        let index = ui.get_route_index().clamp(-1, model.len() as i32 - 1);
        ui.set_route_rows(slint::ModelRc::new(slint::VecModel::from(model)));
        ui.set_route_index(index);
        for (kind, zone) in [(2, settings.inside_zone), (3, settings.stop_zone)] {
            let text = if let Some((_, point)) = origin.filter(|(pending, _)| *pending == kind) {
                format!(
                    "First corner {}. Pick the opposite corner.",
                    point_label(point)
                )
            } else if let Some(zone) = zone {
                format!("{} to {}", point_label(zone.from), point_label(zone.to))
            } else {
                "Unset".into()
            };
            if kind == 2 {
                ui.set_inside_label(text.into());
            } else {
                ui.set_stop_zone_label(text.into());
            }
        }
    }

    fn edit_route(&self, ui: &App, operation: i32) {
        if ui.get_active() || ui.get_capturing() {
            return;
        }
        let Ok(index) = usize::try_from(ui.get_route_index()) else {
            return;
        };
        let mut state = self.state.borrow_mut();
        let route = &mut state.settings.route;
        if index >= route.len() {
            return;
        }
        let selected = match operation {
            -1 if index > 0 => {
                route.swap(index, index - 1);
                index as i32 - 1
            }
            1 if index + 1 < route.len() => {
                route.swap(index, index + 1);
                index as i32 + 1
            }
            0 => {
                route.remove(index);
                (index as i32).min(route.len() as i32 - 1)
            }
            _ => return,
        };
        if route.is_empty() {
            state.settings.route_enabled = false;
            ui.set_route_enabled(false);
        }
        ui.set_route_index(selected);
        Self::display_points(ui, &state.settings, state.zone_origin);
        self.update_configuration(ui, &state.settings);
        drop(state);
        self.queue_save();
    }

    #[cfg(windows)]
    fn toggle_target_verification(&self, ui: &App) {
        if ui.get_active() || ui.get_capturing() {
            return;
        }
        let Ok(index) = usize::try_from(ui.get_route_index()) else {
            return;
        };
        let mut state = self.state.borrow_mut();
        let Some(step) = state
            .settings
            .route
            .get_mut(index)
            .and_then(Step::control_mut)
        else {
            return;
        };
        step.wait_for_disappearance = !step.wait_for_disappearance;
        Self::display_points(ui, &state.settings, state.zone_origin);
        drop(state);
        self.queue_save();
    }

    #[cfg(windows)]
    fn edit_step(&self, ui: &App, index: i32) {
        use targets::{Action, Direction};
        if ui.get_active() || ui.get_capturing() {
            return;
        }
        let state = self.state.borrow();
        let step = usize::try_from(index)
            .ok()
            .and_then(|index| state.settings.route.get(index));
        if index >= 0 && step.is_none() {
            return;
        }
        let mut delay = 0;
        let mut count = 1;
        let mut button = 0;
        let mut direction = 0;
        let mut key = "Enter";
        let kind = match step {
            Some(Step::Action(step)) => {
                delay = step.delay_ms;
                match &step.action {
                    Action::Click {
                        button: selected,
                        count: selected_count,
                    } => {
                        count = *selected_count;
                        button = selected.index();
                        0
                    }
                    Action::Wait => 1,
                    Action::Scroll {
                        direction: selected,
                        ticks,
                    } => {
                        direction = match selected {
                            Direction::Up => 0,
                            Direction::Down => 1,
                            Direction::Left => 2,
                            Direction::Right => 3,
                        };
                        count = *ticks;
                        2
                    }
                    Action::Key { key: selected } => {
                        key = selected;
                        3
                    }
                }
            }
            Some(_) => {
                button = state.settings.button.index();
                count = state.settings.click_count();
                if state.settings.scroll && !state.settings.keyboard {
                    direction = state.settings.scroll_direction.index();
                    count = state.settings.scroll_ticks().unwrap_or(1);
                    2
                } else {
                    0
                }
            }
            None => {
                delay = 250;
                1
            }
        };
        ui.set_step_edit_index(index);
        ui.set_step_has_target(step.is_some_and(|step| step.point().is_some() || step.is_smart()));
        ui.set_step_control_target(step.is_some_and(Step::is_smart));
        ui.set_step_kind(kind);
        ui.set_step_delay(delay.to_string().into());
        ui.set_step_count(count.to_string().into());
        ui.set_step_button(button);
        ui.set_step_direction(direction);
        ui.set_step_key(key.into());
        ui.set_step_error("".into());
        ui.set_step_editing(true);
        ui.invoke_reveal_step_editor();
    }

    #[cfg(windows)]
    fn save_step(&self, ui: &App) {
        use targets::{Action, ActionStep, Direction};
        if !ui.get_step_editing() || ui.get_active() || ui.get_capturing() {
            return;
        }
        let mut state = self.state.borrow_mut();
        let settings = &mut state.settings;
        let index = usize::try_from(ui.get_step_edit_index()).ok();
        let result = (|| {
            let target = index
                .and_then(|index| settings.route.get(index))
                .and_then(Step::destination);
            let delay_ms = ui
                .get_step_delay()
                .trim()
                .parse()
                .map_err(|_| "Enter a whole delay in milliseconds.")?;
            let count = || {
                ui.get_step_count()
                    .trim()
                    .parse::<u8>()
                    .map_err(|_| "Enter a whole click or wheel tick count.")
            };
            let action = match ui.get_step_kind() {
                0 => Action::Click {
                    button: Button::from_index(ui.get_step_button()),
                    count: count()?,
                },
                1 => Action::Wait,
                2 => Action::Scroll {
                    direction: Direction::from_index(ui.get_step_direction()),
                    ticks: count()?,
                },
                _ => Action::Key {
                    key: ui.get_step_key().trim().to_owned(),
                },
            };
            if action == Action::Wait && target.is_some() {
                return Err("Use Add step to insert a wait alongside this target.");
            }
            let step = ActionStep {
                target,
                action,
                delay_ms,
            };
            automation::validate_action(&step, self.hotkeys.toggle_binding())?;
            Ok::<_, &'static str>(Step::Action(Box::new(step)))
        })();
        let step = match result {
            Ok(step) => step,
            Err(error) => {
                ui.set_step_error(error.into());
                return;
            }
        };
        let selected = if let Some(index) = index {
            let Some(existing) = settings.route.get_mut(index) else {
                return;
            };
            *existing = step;
            index
        } else {
            if settings.route.len() >= settings::MAX_POINTS {
                ui.set_step_error("The route has 64 steps. Remove a step to add another.".into());
                return;
            }
            let selected = usize::try_from(ui.get_route_index())
                .ok()
                .filter(|index| *index < settings.route.len())
                .map_or(settings.route.len(), |index| index + 1);
            settings.route.insert(selected, step);
            selected
        };
        settings.route_enabled = true;
        ui.set_route_enabled(true);
        ui.set_route_index(selected as i32);
        ui.set_step_editing(false);
        Self::display_points(ui, &state.settings, state.zone_origin);
        self.update_configuration(ui, &state.settings);
        drop(state);
        self.queue_save();
    }

    fn clear_zone(&self, ui: &App, kind: i32) {
        if ui.get_active() || ui.get_capturing() {
            return;
        }
        let mut state = self.state.borrow_mut();
        match kind {
            2 => state.settings.inside_zone = None,
            3 => state.settings.stop_zone = None,
            _ => return,
        }
        if state
            .zone_origin
            .is_some_and(|(pending, _)| pending == kind)
        {
            state.zone_origin = None;
        }
        Self::display_points(ui, &state.settings, state.zone_origin);
        self.update_configuration(ui, &state.settings);
        status(ui, "");
        drop(state);
        self.queue_save();
    }

    fn display_library(ui: &App, library: &Library) {
        let name = ui.get_selected_preset();
        let selected = library
            .profiles
            .keys()
            .position(|saved| saved == name.as_str());
        let names: Vec<slint::SharedString> = library
            .profiles
            .keys()
            .map(|name| name.as_str().into())
            .collect();
        ui.set_profile_rows(slint::ModelRc::new(slint::VecModel::from(names)));
        ui.set_profile_index(selected.map_or(-1, |index| index as i32));
        Self::display_stats(ui, library);
    }

    fn display_stats(ui: &App, library: &Library) {
        ui.set_library_stats(
            format!(
                "{} runs  ·  {} actions  ·  {} min active",
                grouped(library.sessions),
                grouped(library.actions),
                grouped(library.active_millis / 60_000)
            )
            .into(),
        );
    }

    fn profile_action(&self, ui: &App, operation: i32) {
        if ui.get_active() || ui.get_capturing() {
            return;
        }
        let name = if operation == 0 {
            ui.get_profile_name()
        } else {
            ui.get_selected_preset()
        };
        let name = if operation == 0 {
            name.trim()
        } else {
            name.as_str()
        };
        let mut state = self.state.borrow_mut();
        let size_error = "Presets are too large to save. Shorten the settings or delete a preset.";
        let notice = match operation {
            0 => {
                if name.is_empty() || name.chars().count() > 40 {
                    ui.set_preset_error("Enter a preset name from 1 to 40 characters.".into());
                    return;
                }
                if state.library.profiles.contains_key(name) {
                    ui.set_preset_error("That name is taken. Choose another name or select the preset to update it.".into());
                    return;
                }
                if state.library.profiles.len() >= 100 {
                    ui.set_preset_error(
                        "Save up to 100 presets. Delete one to add another.".into(),
                    );
                    return;
                }
                let settings = state.settings.clone();
                if state.library.set_profile(name, settings).is_err() {
                    ui.set_preset_error(size_error.into());
                    return;
                }
                state.library_dirty = true;
                ui.set_selected_preset(name.into());
                ui.set_profile_name("".into());
                ui.set_preset_error("".into());
                "Preset saved."
            }
            1 => {
                let Some(mut settings) = state.library.profiles.get(name).cloned() else {
                    failure(ui, "Choose a saved preset to load.");
                    return;
                };
                if let Err(error) = self.hotkeys.rebind(&settings.trigger) {
                    failure(ui, error);
                    return;
                }
                settings.normalize();
                state.settings = settings;
                state.zone_origin = None;
                self.display_settings(ui, &state.settings);
                self.display_shortcuts(ui);
                ui.set_shortcut_error("".into());
                "Preset loaded."
            }
            2 => {
                if state.library.profiles.remove(name).is_none() {
                    failure(ui, "Choose a saved preset to delete.");
                    return;
                }
                state.library_dirty = true;
                ui.set_selected_preset("".into());
                "Preset deleted."
            }
            3 => {
                if !state.library.profiles.contains_key(name) {
                    failure(ui, "Choose a saved preset to update.");
                    return;
                }
                let settings = state.settings.clone();
                if state.library.set_profile(name, settings).is_err() {
                    failure(ui, size_error);
                    return;
                }
                state.library_dirty = true;
                "Preset updated."
            }
            _ => return,
        };
        Self::display_library(ui, &state.library);
        drop(state);
        let saved = self.save();
        status(ui, if saved || operation == 1 { notice } else { "" });
    }

    fn queue_save(&self) {
        let controller = self.own.clone();
        self.save_timer.start(
            TimerMode::SingleShot,
            Duration::from_millis(400),
            move || {
                if let Some(controller) = controller.upgrade() {
                    controller.save();
                }
            },
        );
    }

    fn save(&self) -> bool {
        let mut state = self.state.borrow_mut();
        let settings_pending = state.settings_dirty || state.settings != state.saved;
        let mut failed = false;
        if let Some(path) = self.path.as_deref() {
            if settings_pending {
                match settings::save(path, &state.settings) {
                    Ok(()) => {
                        state.saved = state.settings.clone();
                        state.settings_dirty = false;
                    }
                    Err(error) => {
                        diagnostics::record(&format!("Save settings: {error}"));
                        failed = true;
                    }
                }
            }
            if state.library_dirty {
                match settings::save(&path.with_file_name("library.json"), &state.library) {
                    Ok(()) => state.library_dirty = false,
                    Err(error) => {
                        diagnostics::record(&format!("Save library: {error}"));
                        failed = true;
                    }
                }
            }
        } else {
            failed = settings_pending || state.library_dirty;
        }
        drop(state);
        if let Some(ui) = self.app.upgrade() {
            ui.set_save_error(
                if failed {
                    "Afterbeat could not save your data."
                } else {
                    ""
                }
                .into(),
            );
        }
        !failed
    }

    fn capture(&self, ui: &App, kind: i32) {
        if ui.get_active() || ui.get_step_editing() {
            return;
        }
        #[cfg(target_os = "linux")]
        if wayland::active() {
            if !matches!(kind, 0 | 1) {
                failure(
                    ui,
                    "Wayland does not expose the live pointer for this capture.",
                );
                return;
            }
            match (
                ui.get_wayland_x().trim().parse(),
                ui.get_wayland_y().trim().parse(),
            ) {
                (Ok(x), Ok(y)) if mouse::on_monitor(Point { x, y }) => {
                    self.set_point(ui, Some(Point { x, y }), kind)
                }
                _ => failure(
                    ui,
                    "Enter whole X and Y coordinates inside a shared Wayland display.",
                ),
            }
            return;
        }
        if ui.get_capturing() {
            self.cancel_capture(ui);
            return;
        }
        if kind != DRY_SAMPLE {
            ui.set_pick_kind(kind);
        }
        self.stop_slot.lock().unwrap().capture = Some(Capture {
            deadline: Instant::now() + CAPTURE_DELAY,
            kind,
            requested_at: hotkeys::tick_count(),
            point: None,
        });
        ui.set_capturing(true);
        self.start_tick();
        self.refresh(ui);
    }

    fn cancel_capture(&self, ui: &App) {
        #[cfg(windows)]
        self.target_job.borrow_mut().take();
        self.stop_slot.lock().unwrap().finish_capture();
        self.end_capture(ui);
        clear_capture_status(ui);
    }

    fn end_capture(&self, ui: &App) {
        ui.set_capturing(false);
        self.tick.stop();
    }

    fn capture_shortcut(&self, ui: &App, point: Point, requested_at: u64) {
        if ui.get_step_editing() {
            return;
        }
        #[cfg(windows)]
        if matches!(*self.target_job.borrow(), Some(TargetJob::Capture { .. })) {
            return;
        }
        if self
            .stop_slot
            .lock()
            .unwrap()
            .receive_capture(point, requested_at)
        {
            self.refresh(ui);
        } else {
            self.complete_capture(
                ui,
                Some(point),
                ui.get_capture_shortcut_kind(),
                requested_at,
            );
        }
    }

    fn complete_capture(&self, ui: &App, point: Option<Point>, kind: i32, _requested_at: u64) {
        #[cfg(windows)]
        if kind == CONTROL_CAPTURE {
            ui.set_dry_checking(false);
            match point
                .ok_or_else(|| "Could not read the cursor position.".to_owned())
                .and_then(|point| {
                    self.target_service()?
                        .capture(point, ui.get_capture_visual())
                }) {
                Ok(reply) => {
                    *self.target_job.borrow_mut() = Some(TargetJob::Capture {
                        requested_at: _requested_at,
                        deadline: Instant::now() + Duration::from_secs(5),
                        reply,
                    });
                    ui.set_capturing(true);
                    status(ui, "Reading the control under the pointer…");
                    self.start_tick();
                }
                Err(error) => {
                    self.end_capture(ui);
                    failure(ui, &error);
                }
            }
            return;
        }
        self.set_point(ui, point, kind);
    }

    #[cfg(windows)]
    fn target_service(&self) -> Result<smart_targets::Service, String> {
        if self.target_service.get().is_none() {
            let service = smart_targets::Service::new()
                .map_err(|err| format!("Could not start Windows UI Automation ({err})."))?;
            let _ = self.target_service.set(service);
        }
        Ok(self.target_service.get().unwrap().clone())
    }

    #[cfg(windows)]
    fn refresh_target_job(&self, ui: &App, now: Instant) {
        let mut pending = self.target_job.borrow_mut();
        match pending.as_ref() {
            Some(TargetJob::Capture {
                requested_at,
                deadline,
                reply,
            }) => {
                if self.stop_slot.lock().unwrap().cancelled(*requested_at) {
                    pending.take();
                    self.end_capture(ui);
                    clear_capture_status(ui);
                    return;
                }
                let Some(result) = target_reply(reply, now >= *deadline) else {
                    return;
                };
                pending.take();
                self.end_capture(ui);
                match result {
                    Ok(step) => self.add_control(ui, step),
                    Err(error) => failure(ui, &error),
                }
            }
            Some(TargetJob::Preview { deadline, reply }) => {
                if ui.get_page() != 4 || ui.get_active() {
                    pending.take();
                    ui.set_dry_checking(false);
                    if !ui.get_active() && !ui.get_capturing() {
                        self.tick.stop();
                    }
                    return;
                }
                let Some(result) = target_reply(reply, now >= *deadline) else {
                    return;
                };
                pending.take();
                ui.set_dry_checking(false);
                self.tick.stop();
                let result = result.and_then(|results| {
                    *self.target_proposals.borrow_mut() = results
                        .iter()
                        .map(|result| match result {
                            smart_targets::Resolution::Recovered(target)
                            | smart_targets::Resolution::Conflict(target) => Some(target.clone()),
                            _ => None,
                        })
                        .collect();
                    dry_run::show_smart(ui, &self.state.borrow().settings, &results)
                        .map_err(str::to_owned)
                });
                ui.set_dry_error(result.is_err());
                ui.set_dry_message(result.err().unwrap_or_default().into());
            }
            Some(TargetJob::Repair {
                index,
                original,
                deadline,
                reply,
            }) => {
                if ui.get_page() != 4 || ui.get_active() {
                    pending.take();
                    ui.set_dry_checking(false);
                    if !ui.get_active() && !ui.get_capturing() {
                        self.tick.stop();
                    }
                    return;
                }
                let Some(result) = target_reply(reply, now >= *deadline) else {
                    return;
                };
                let (index, original) = (*index, original.clone());
                pending.take();
                drop(pending);
                ui.set_dry_checking(false);
                self.tick.stop();
                let repaired = result.and_then(|repaired| {
                    let mut state = self.state.borrow_mut();
                    let Some(step) = state
                        .settings
                        .route
                        .get_mut(index)
                        .and_then(Step::control_mut)
                    else {
                        return Err("The route changed. Check the step again.".into());
                    };
                    if *step != *original {
                        return Err("The route changed. Check the step again.".into());
                    }
                    *step = repaired;
                    Self::display_points(ui, &state.settings, state.zone_origin);
                    Ok(())
                });
                match repaired {
                    Ok(()) => {
                        self.queue_save();
                        self.open_dry_run(ui);
                        ui.set_dry_step(index as i32);
                        ui.set_dry_message(
                            format!("Repaired step {}. Checking the updated target…", index + 1)
                                .into(),
                        );
                    }
                    Err(error) => {
                        ui.set_dry_error(true);
                        ui.set_dry_message(error.into());
                    }
                }
            }
            None => {}
        }
    }

    #[cfg(windows)]
    fn repair_target(&self, ui: &App) {
        if ui.get_page() != 4 || ui.get_active() || ui.get_capturing() || ui.get_dry_checking() {
            return;
        }
        let Ok(index) = usize::try_from(ui.get_dry_step()) else {
            return;
        };
        let Some(shown) = self.target_proposals.borrow().get(index).cloned().flatten() else {
            return;
        };
        let Some(step) = self
            .state
            .borrow()
            .settings
            .route
            .get(index)
            .and_then(Step::control)
            .cloned()
            .map(Box::new)
        else {
            return;
        };
        match self
            .target_service()
            .and_then(|service| service.repair((*step).clone(), shown))
        {
            Ok(reply) => {
                *self.target_job.borrow_mut() = Some(TargetJob::Repair {
                    index,
                    original: step,
                    deadline: Instant::now() + Duration::from_secs(8),
                    reply,
                });
                ui.set_dry_checking(true);
                ui.set_dry_error(false);
                ui.set_dry_message("Confirming that the proposed target is still present…".into());
                self.start_tick();
            }
            Err(error) => {
                ui.set_dry_error(true);
                ui.set_dry_message(error.into());
            }
        }
    }

    #[cfg(windows)]
    fn add_control(&self, ui: &App, step: targets::ControlStep) {
        let mut state = self.state.borrow_mut();
        if state.settings.route_wayland && !state.settings.route.is_empty() {
            failure(
                ui,
                "Remove the route points from the other desktop coordinate system first.",
            );
            return;
        }
        if state.settings.route.len() >= settings::MAX_POINTS {
            failure(ui, "The route has 64 steps. Remove a step to add another.");
            return;
        }
        ui.set_route_index(state.settings.route.len() as i32);
        let step = Step::Control(Box::new(step));
        let message = format!("Captured {}.", step.label());
        state.settings.route.push(step);
        state.settings.route_wayland = false;
        state.settings.route_enabled = true;
        ui.set_route_enabled(true);
        self.update_configuration(ui, &state.settings);
        Self::display_points(ui, &state.settings, state.zone_origin);
        status(ui, &message);
        drop(state);
        self.queue_save();
    }

    fn set_point(&self, ui: &App, point: Option<Point>, kind: i32) {
        self.end_capture(ui);
        let mut state = self.state.borrow_mut();
        if kind == DRY_SAMPLE {
            let result = point
                .ok_or("Could not read the cursor position.")
                .and_then(|point| dry_run::show(ui, &state.settings, Some(point)));
            ui.set_dry_message(result.as_ref().err().copied().unwrap_or("").into());
            ui.set_dry_error(result.is_err());
            return;
        }
        let Some(point) = point else {
            failure(ui, "Could not read the cursor position.");
            return;
        };
        #[cfg(target_os = "linux")]
        let native_wayland = wayland::active();
        #[cfg(not(target_os = "linux"))]
        let native_wayland = false;
        match kind {
            1 => {
                if !state.settings.route.is_empty()
                    && state.settings.route_wayland != native_wayland
                {
                    failure(
                        ui,
                        "Remove the route points from the other desktop coordinate system first.",
                    );
                    return;
                }
                if state.settings.route.len() == settings::MAX_POINTS {
                    failure(
                        ui,
                        "The route has 64 points. Remove a point to add another.",
                    );
                    return;
                }
                ui.set_route_index(state.settings.route.len() as i32);
                state.settings.route.push(Step::Point(point));
                state.settings.route_wayland = native_wayland;
                state.settings.route_enabled = true;
                ui.set_route_enabled(true);
            }
            2 | 3 => {
                if let Some((_, from)) = state.zone_origin.filter(|(pending, _)| *pending == kind) {
                    if from == point {
                        failure(ui, "Pick a different point for the opposite corner.");
                        return;
                    }
                    let zone = Some(Zone { from, to: point });
                    if kind == 2 {
                        state.settings.inside_zone = zone;
                    } else {
                        state.settings.stop_zone = zone;
                    }
                    state.zone_origin = None;
                } else {
                    state.zone_origin = Some((kind, point));
                }
            }
            _ => {
                state.settings.point = Some(point);
                state.settings.point_wayland = native_wayland;
                state.settings.fixed = true;
                ui.set_fixed_target(true);
            }
        }
        self.update_configuration(ui, &state.settings);
        Self::display_points(ui, &state.settings, state.zone_origin);
        status(ui, "");
        drop(state);
        self.queue_save();
    }

    fn toggle(&self, ui: &App) {
        self.toggle_with(ui, None, hotkeys::tick_count());
    }

    fn toggle_with(&self, ui: &App, binding: Option<Binding>, requested_at: u64) {
        let interrupted = {
            let slot = self.stop_slot.lock().unwrap();
            if slot.cancelled(requested_at) {
                return;
            }
            slot.signal()
        };
        if interrupted {
            self.refresh(ui);
            return;
        }
        if ui.get_page() == 4
            || !ui.get_can_start()
            || !ui.get_input_ready()
            || ui.get_capturing()
            || ui.get_step_editing()
        {
            return;
        }
        if let Err(error) = self.start(ui, binding, requested_at) {
            failure(ui, &error);
        }
    }

    fn start(&self, ui: &App, binding: Option<Binding>, requested_at: u64) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        if wayland::active() && !wayland::layout_unchanged(ui) {
            self.wayland_action(ui, 2);
            return Err(
                "The display layout changed. Reconnect Wayland input and verify your coordinates."
                    .into(),
            );
        }
        let settings = self.state.borrow().settings.clone();
        let (schedule, injector, guard) =
            automation::prepare(&settings, self.hotkeys.toggle_binding(), binding)?;
        #[cfg(windows)]
        let smart = if settings.has_smart_route() || settings.has_action_route() {
            Some(smart_targets::Route::new(
                settings
                    .has_smart_route()
                    .then(|| self.target_service())
                    .transpose()?,
                settings.route.clone(),
                settings.target_timeout()?,
            ))
        } else {
            None
        };
        #[cfg(not(windows))]
        let smart_run = false;
        #[cfg(windows)]
        let smart_run = smart.is_some();
        let stop = Arc::new(
            StopSignal::new().map_err(|err| format!("Could not create a stop signal ({err})."))?,
        );
        let progress = Arc::new(Progress::default());
        let sequence =
            settings.has_action_route() || (settings.has_smart_route() && settings.scroll);
        let worker_stop = stop.clone();
        let worker_progress = progress.clone();
        let countdown = Instant::now() + schedule.delay;
        let burst = !sequence && schedule.interval.is_zero() && schedule.rates.is_none();
        let guarded = guard.enabled();
        let needs_destination_check = guarded || injector.has_targets();
        let changing_targets = injector.has_changing_targets();
        let batchable = injector.batchable() && !smart_run;
        let load_pacing = schedule.uses_load_pacing(batchable, guarded || changing_targets);
        let priority_pacing = schedule.uses_priority_pacing(load_pacing);
        let stop_probe = (priority_pacing || smart_run).then(|| self.hotkeys.stop_probe(binding));
        {
            let mut slot = self.stop_slot.lock().unwrap();
            if slot.cancelled(requested_at) {
                return Ok(());
            }
            if !self.hotkeys.listening() {
                return Err("The shortcut listener stopped. Reopen Afterbeat to run again.".into());
            }
            // Publish before spawning so the shortcut listener can stop the worker from its first action.
            slot.run = Some(stop.clone());
        }
        let (complete, completion) = mpsc::channel();
        #[cfg(target_os = "linux")]
        if wayland::active() {
            wayland::protect(Some(settings.keyboard));
        }
        self.hotkeys.set_paced_stop_priority(load_pacing);
        let worker = thread::Builder::new()
            .name("afterbeat-worker".into())
            .spawn(move || {
                if priority_pacing {
                    Schedule::prioritize_paced_worker();
                }
                let guard = RefCell::new(guard);
                let injector = RefCell::new(injector);
                let mut poll = || guard.borrow_mut().check();
                let mut destination = || {
                    let point = injector.borrow_mut().destination()?;
                    Ok(guard.borrow_mut().check_destination(point))
                };
                let checks = Checks {
                    poll: guarded.then_some(&mut poll),
                    destination: (needs_destination_check && !smart_run)
                        .then_some(&mut destination),
                    batch_destination: !changing_targets,
                };
                #[cfg(windows)]
                let mut smart = smart;
                #[cfg(windows)]
                let mut dispatch_probe = stop_probe.clone();
                let mut stop_probe = stop_probe.map(|mut probe| move || probe.pending());
                let run = || {
                    engine::run_controlled(
                        schedule,
                        &worker_stop,
                        &worker_progress,
                        batchable,
                        checks,
                        stop_probe
                            .as_mut()
                            .map(|probe| probe as &mut dyn FnMut() -> bool),
                        |count, control| {
                            #[cfg(windows)]
                            if let Some(route) = &mut smart {
                                return route.dispatch(
                                    &injector.borrow(),
                                    &mut guard.borrow_mut(),
                                    control,
                                    || dispatch_probe.as_mut().is_some_and(|probe| probe.pending()),
                                );
                            }
                            #[cfg(target_os = "macos")]
                            let result = injector.borrow_mut().send(
                                count,
                                control,
                                &mut |target, cursor| {
                                    guard.borrow_mut().dispatch_target(target, cursor)
                                },
                            );
                            #[cfg(not(target_os = "macos"))]
                            let result = {
                                let mut check =
                                    |target| guard.borrow_mut().check_destination(target);
                                injector.borrow_mut().send(
                                    count,
                                    control,
                                    needs_destination_check.then_some(&mut check),
                                )
                            };
                            result
                        },
                    )
                };
                #[cfg(windows)]
                let result = if sequence {
                    engine::run_sequence(schedule, &worker_stop, &worker_progress, |control| {
                        smart.as_mut().unwrap().sequence(
                            &settings,
                            schedule,
                            &mut guard.borrow_mut(),
                            control,
                            || dispatch_probe.as_mut().is_some_and(|probe| probe.pending()),
                        )
                    })
                } else {
                    run()
                };
                #[cfg(not(windows))]
                let result = run();
                let ended = Instant::now();
                // X11 text output may still need to restore the keyboard mapping.
                drop(injector);
                if complete.send((ended, result)).is_ok() {
                    let _ = slint::invoke_from_event_loop(|| {
                        let controller = ACTIVE_CONTROLLER.with(|local| local.borrow().upgrade());
                        if let Some(controller) = controller
                            && let Some(ui) = controller.app.upgrade()
                        {
                            controller.refresh(&ui);
                        }
                    });
                }
            })
            .map_err(|err| {
                #[cfg(target_os = "linux")]
                wayland::protect(None);
                self.stop_slot.lock().unwrap().run = None;
                self.hotkeys.set_paced_stop_priority(false);
                format!("Could not start the click worker ({err}).")
            })?;
        self.state.borrow_mut().run = Some(Run {
            stop,
            progress,
            worker,
            completion,
            countdown,
            burst,
        });
        ui.set_active(true);
        #[cfg(windows)]
        self.set_shell_run_state(RunState::Active);
        ui.set_stopping(false);
        ui.set_run_state("Starting".into());
        self.start_tick();
        self.refresh(ui);
        Ok(())
    }

    fn start_tick(&self) {
        let controller = self.own.clone();
        self.tick.start(TimerMode::Repeated, UI_INTERVAL, move || {
            if let Some(controller) = controller.upgrade()
                && let Some(ui) = controller.app.upgrade()
            {
                controller.refresh(&ui);
            }
        });
    }

    fn refresh(&self, ui: &App) {
        let now = Instant::now();
        #[cfg(windows)]
        self.refresh_target_job(ui, now);
        #[cfg(target_os = "linux")]
        if ui.get_active() && wayland::active() && !wayland::layout_unchanged(ui) {
            self.wayland_action(ui, 2);
        }
        if self.refresh_capture(ui, now) {
            return;
        }

        let mut state = self.state.borrow_mut();
        let Some(run) = state.run.as_ref() else {
            return;
        };
        let completed = match run.completion.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Disconnected) => {
                Some((now, Err("The click worker stopped unexpectedly.".into())))
            }
            Err(mpsc::TryRecvError::Empty) => None,
        };
        if let Some((ended, result)) = completed {
            #[cfg(target_os = "linux")]
            wayland::protect(None);
            let run = state.run.take().unwrap();
            self.stop_slot.lock().unwrap().run = None;
            self.hotkeys.set_paced_stop_priority(false);
            render_metrics(ui, &run.progress, ended, run.burst);
            record_run(&mut state, &run.progress, ended);
            Self::display_stats(ui, &state.library);
            ui.set_active(false);
            ui.set_stopping(false);
            ui.set_run_state("Last run".into());
            self.tick.stop();
            match result {
                Ok(Outcome::Completed) => status(ui, "Completed."),
                Ok(Outcome::Stopped) => status(ui, "Stopped."),
                Ok(Outcome::Guarded(reason)) => status(ui, reason),
                Err(error) => {
                    diagnostics::record(&format!("Run stopped with an error: {error}"));
                    failure(ui, &error);
                }
            }
            drop(state);
            #[cfg(windows)]
            self.set_shell_run_state(RunState::Idle);
            self.queue_save();
            return;
        }

        render_metrics(ui, &run.progress, now, run.burst);
        if run.stop.requested() {
            drop(state);
            ui.set_stopping(true);
            ui.set_run_state("Stopping".into());
            status(ui, "");
            #[cfg(windows)]
            self.set_shell_run_state(RunState::Stopping);
        } else if let Some(reason) = run.progress.pause_reason() {
            ui.set_run_state("Paused".into());
            status(ui, pause_hint(reason));
        } else if run.progress.started.get().is_some() {
            ui.set_run_state("Running".into());
            status(ui, "");
        } else if now >= run.countdown {
            ui.set_run_state("Starting".into());
            status(ui, "Waiting to send the first action.");
        } else {
            ui.set_run_state("Countdown".into());
            let left = seconds_left(run.countdown, now);
            status(
                ui,
                &format!("Starts in {left} s. {}", countdown_target(&state.settings)),
            );
        }
    }

    fn refresh_capture(&self, ui: &App, now: Instant) -> bool {
        let mut slot = self.stop_slot.lock().unwrap();
        let Some(capture) = slot.capture else {
            return false;
        };
        let cancelled = slot.cancelled(capture.requested_at);
        if cancelled || capture.point.is_some() || capture.deadline <= now {
            slot.finish_capture();
            drop(slot);
            if cancelled {
                self.end_capture(ui);
                clear_capture_status(ui);
            } else {
                let point = capture.point.or_else(|| mouse::cursor().ok());
                self.complete_capture(ui, point, capture.kind, capture.requested_at);
            }
        } else {
            drop(slot);
            let left = seconds_left(capture.deadline, now);
            if capture.kind == DRY_SAMPLE {
                ui.set_dry_error(false);
                ui.set_dry_message(
                    format!("Sampling in {left} s. Move the pointer to the target.").into(),
                );
            } else {
                status(
                    ui,
                    &format!("Picking in {left} s. Move the pointer to the target."),
                );
            }
        }
        true
    }

    #[cfg(windows)]
    fn set_shell_run_state(&self, state: RunState) {
        if let Some(shell) = self.shell.borrow().as_ref() {
            shell.set_run_state(state);
        }
    }

    fn test_hit(&self, ui: &App) {
        self.test_hits(ui, 1);
    }

    fn test_hits(&self, ui: &App, count: i32) {
        let mut state = self.state.borrow_mut();
        state.pad_hits = state.pad_hits.saturating_add(count.max(0) as u64);
        ui.set_pad_count(grouped(state.pad_hits).into());
    }

    fn reset_pad(&self, ui: &App) {
        input_filter::clear_hits();
        self.state.borrow_mut().pad_hits = 0;
        ui.set_pad_count("0".into());
    }

    fn shutdown(&self) {
        signal_stop(&self.stop_slot);
        let run = self.state.borrow_mut().run.take();
        if let Some(run) = run
            && run.worker.join().is_ok()
            && let Ok((ended, _)) = run.completion.try_recv()
        {
            record_run(&mut self.state.borrow_mut(), &run.progress, ended);
        }
        self.save();
        #[cfg(target_os = "linux")]
        wayland::protect(None);
    }
}

fn point_label(point: Point) -> String {
    format!("X {}  Y {}", point.x, point.y)
}

fn pause_hint(reason: PauseReason) -> &'static str {
    match reason {
        PauseReason::OutsideZone => "Outside the inside-only zone. Move the pointer inside.",
        PauseReason::Modifiers => "Release the activation modifiers to resume.",
        PauseReason::KeyboardFocus => "Focus the app that should receive the keys.",
        PauseReason::KeyboardFilter => {
            "App filter blocked the foreground app. Switch to an allowed app."
        }
        PauseReason::PointerFilter => {
            "App filter blocked clicks here. Release held mouse buttons and move over an allowed app."
        }
        PauseReason::TargetFilter => {
            "App filter blocked the target. Release held mouse buttons and bring an allowed app to the target."
        }
        PauseReason::CursorConfined => {
            "The cursor is confined away from the target. Release cursor confinement to resume."
        }
        PauseReason::InputGrabbed => {
            #[cfg(target_os = "linux")]
            let message = "Input is grabbed. Release held buttons or close the grab to resume.";
            #[cfg(not(target_os = "linux"))]
            let message = "Release the output key or mouse button to resume.";
            message
        }
        PauseReason::TargetMissing => "Waiting for the saved control to appear.",
        PauseReason::TargetAmbiguous => {
            "Multiple controls or windows match. Leave one match visible."
        }
        PauseReason::TargetUnavailable => "Waiting for the control to be enabled and unobstructed.",
        PauseReason::VerifyingTarget => "Click sent. Waiting for the control to disappear.",
        PauseReason::TargetEvidence => {
            "Waiting for matching visual evidence. Check the target in Dry run."
        }
        PauseReason::WheelFocus => "Focus the window at the wheel target to resume.",
    }
}

#[cfg(windows)]
fn target_reply<T>(reply: &smart_targets::Request<T>, expired: bool) -> Option<Result<T, String>> {
    match reply.try_recv() {
        Ok(result) => Some(result),
        Err(mpsc::TryRecvError::Empty) if !expired => None,
        Err(_) => Some(Err("Windows UI Automation did not respond in time.".into())),
    }
}

fn countdown_target(settings: &Settings) -> String {
    if settings.has_action_route() {
        "Running the saved action route.".into()
    } else if settings.has_smart_route() {
        "Waiting to start the saved control route.".into()
    } else if settings.route_enabled {
        if settings.keyboard {
            "Using the saved route. Focus the app that should receive the keys."
        } else {
            "Clicking along the saved route."
        }
        .into()
    } else if settings.keyboard {
        "Focus the app that should receive the keys.".into()
    } else if let Some(point) = settings.point.filter(|_| settings.fixed) {
        format!("Clicking the fixed point at {}.", point_label(point))
    } else {
        "Move the pointer to your target.".into()
    }
}

fn record_run(state: &mut State, progress: &Progress, ended: Instant) {
    if let Some(started) = progress.started.get() {
        let millis = ended.saturating_duration_since(*started).as_millis();
        state.library.sessions = state.library.sessions.saturating_add(1);
        state.library.actions = state.library.actions.saturating_add(progress.actions());
        state.library.active_millis = state.library.active_millis.saturating_add(millis as u64);
        state.library_dirty = true;
    }
}

fn status(ui: &App, message: &str) {
    ui.set_message_error(false);
    ui.set_message(message.into());
}

fn failure(ui: &App, message: &str) {
    ui.set_message_error(true);
    ui.set_message(message.into());
}

fn clear_capture_status(ui: &App) {
    if ui.get_page() == 4 {
        ui.set_dry_message("".into());
        ui.set_dry_error(false);
    } else {
        status(ui, "");
    }
}

fn seconds_left(deadline: Instant, now: Instant) -> u128 {
    deadline
        .saturating_duration_since(now)
        .as_millis()
        .div_ceil(1000)
        .max(1)
}

fn render_metrics(ui: &App, progress: &Progress, now: Instant, burst: bool) {
    let count = progress.actions();
    let elapsed = progress
        .started
        .get()
        .map(|start| now.saturating_duration_since(*start))
        .unwrap_or_default();
    let seconds = elapsed.as_secs_f64();
    let rate = if count > 1 && seconds > 0.0 {
        let measured = if burst { count } else { count - 1 };
        rate_label(measured as f64 / seconds)
    } else {
        "—".into()
    };
    let time = if seconds >= 60.0 {
        format!("{}:{:02}", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else if seconds > 0.0 && seconds < 1.0 {
        format!("{seconds:.3} s")
    } else {
        format!("{seconds:.1} s")
    };
    ui.set_session_count(grouped(count).into());
    ui.set_session_rate(rate.into());
    ui.set_session_elapsed(time.into());
}

fn rate_label(rate: f64) -> String {
    if rate >= 1_000.0 {
        grouped(rate.round() as u64)
    } else if rate >= 1.0 {
        format!("{rate:.1}").trim_end_matches(".0").into()
    } else {
        let digits = if rate < 0.01 { 5 } else { 3 };
        format!("{rate:.digits$}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .into()
    }
}

fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut text = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, digit) in digits.chars().enumerate() {
        if i != 0 && (digits.len() - i).is_multiple_of(3) {
            text.push(',');
        }
        text.push(digit);
    }
    text
}

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

#[cfg(windows)]
fn alert(text: &str, icon: u32) {
    let caption = wide("Afterbeat");
    let text = wide(text);
    unsafe { MessageBoxW(null_mut(), text.as_ptr(), caption.as_ptr(), MB_OK | icon) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countdown_describes_the_effective_destination() {
        let mut settings = Settings::default();
        assert_eq!(
            countdown_target(&settings),
            "Move the pointer to your target."
        );

        settings.fixed = true;
        settings.point = Some(Point { x: -20, y: 30 });
        assert_eq!(
            countdown_target(&settings),
            "Clicking the fixed point at X -20  Y 30."
        );

        settings.keyboard = true;
        assert_eq!(
            countdown_target(&settings),
            "Focus the app that should receive the keys."
        );

        settings.route_enabled = true;
        assert_eq!(
            countdown_target(&settings),
            "Using the saved route. Focus the app that should receive the keys."
        );

        settings.keyboard = false;
        assert_eq!(
            countdown_target(&settings),
            "Clicking along the saved route."
        );
    }

    #[test]
    fn timed_capture_ignores_shortcuts_outside_its_countdown() {
        let start = 10_000;
        let end = start + CAPTURE_DELAY.as_millis() as u64;
        let mut state = StopState {
            capture: Some(Capture {
                deadline: Instant::now() + CAPTURE_DELAY,
                kind: 0,
                requested_at: start,
                point: None,
            }),
            ..StopState::default()
        };
        let point = Point { x: 20, y: 30 };
        for stale in [start - 1, end] {
            assert!(state.receive_capture(point, stale));
            assert_eq!(state.capture.unwrap().point, None);
        }
        assert!(state.receive_capture(point, end - 1));
        assert_eq!(state.capture.unwrap().point, Some(point));
    }
}
