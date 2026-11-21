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
#[cfg_attr(not(windows), allow(dead_code))]
mod recording;
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
mod workflow;
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
const CONTROL_CAPTURE: i32 = 5;
const PIXEL_CAPTURE: i32 = 6;
const PIXEL_PREVIEW: i32 = 7;
const IMAGE_CAPTURE: i32 = 8;
const IMAGE_PREVIEW: i32 = 9;
const IMAGE_REFERENCE_FIRST: i32 = 10;
#[cfg(windows)]
const IMAGE_REFERENCE_SECOND: i32 = 11;
const IMAGE_SEARCH_FIRST: i32 = 12;
const IMAGE_SEARCH_SECOND: i32 = 13;

fn image_capture_kind(kind: i32) -> bool {
    (IMAGE_CAPTURE..=IMAGE_SEARCH_SECOND).contains(&kind)
}
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
    active_preset: Option<String>,
    pending_preset: Option<String>,
    pending_import: Option<Settings>,
    pending_export: Option<(PathBuf, String, Settings)>,
    route_undo: Option<RouteUndo>,
    library_dirty: bool,
    run: Option<Run>,
    zone_origin: Option<(i32, Point)>,
    pad_hits: u64,
}

#[derive(Clone, PartialEq)]
struct RouteSnapshot {
    steps: Vec<Step>,
    enabled: bool,
    once: bool,
    wayland: bool,
    recording_origin: recording::Origin,
}

impl RouteSnapshot {
    fn capture(settings: &Settings) -> Self {
        Self {
            steps: settings.route.clone(),
            enabled: settings.route_enabled,
            once: settings.route_once,
            wayland: settings.route_wayland,
            recording_origin: settings.recording_origin,
        }
    }

    fn restore(self, settings: &mut Settings) {
        settings.route = self.steps;
        settings.route_enabled = self.enabled;
        settings.route_once = self.once;
        settings.route_wayland = self.wayland;
        settings.recording_origin = self.recording_origin;
    }

    fn matches(&self, settings: &Settings) -> bool {
        self.steps == settings.route
            && self.enabled == settings.route_enabled
            && self.once == settings.route_once
            && self.wayland == settings.route_wayland
            && self.recording_origin == settings.recording_origin
    }
}

struct RouteUndo {
    before: RouteSnapshot,
    after: RouteSnapshot,
    index: usize,
}

impl RouteUndo {
    fn remove(settings: &mut Settings, index: usize) -> Self {
        let before = RouteSnapshot::capture(settings);
        settings.route.remove(index);
        if settings.route.is_empty() {
            settings.route_enabled = false;
        }
        Self {
            before,
            after: RouteSnapshot::capture(settings),
            index,
        }
    }

    fn restore(self, settings: &mut Settings) -> Option<usize> {
        if !self.after.matches(settings) {
            return None;
        }
        self.before.restore(settings);
        Some(self.index)
    }
}

fn setup_needs_preserving(settings: &Settings, library: &Library, active: bool) -> bool {
    let saved_as_preset = library.profiles.values().any(|preset| {
        let mut preset = preset.clone();
        preset.normalize();
        settings == &preset
    });
    !saved_as_preset && (active || settings != &Settings::default())
}

fn move_route_step(route: &mut Vec<Step>, from: usize, to: usize) -> bool {
    if from >= route.len() || to >= route.len() || from == to {
        return false;
    }
    let step = route.remove(from);
    route.insert(to, step);
    true
}

fn run_limit_summary(schedule: &Schedule) -> String {
    let actions = schedule.limit.map_or_else(
        || "No action limit".to_owned(),
        |limit| format!("{} actions max", grouped(limit)),
    );
    let time = schedule.duration.map_or_else(
        || "No time limit".to_owned(),
        |duration| format!("{} s max", duration.as_secs_f64()),
    );
    format!("  ·  {actions}  ·  {time}")
}

fn new_preset(library: &Library, name: &str, settings: &Settings) -> Result<Library, &'static str> {
    if name.is_empty() || name.chars().count() > 40 {
        return Err("Enter a preset name from 1 to 40 characters.");
    }
    if library.profiles.contains_key(name) {
        return Err("That name is taken. Choose another name or select the preset to update it.");
    }
    if library.profiles.len() >= 100 {
        return Err("Save up to 100 presets. Delete one to add another.");
    }
    let mut library = library.clone();
    library
        .set_profile(name, settings.clone())
        .map_err(|_| "Presets are too large to save. Shorten the settings or delete a preset.")?;
    Ok(library)
}

#[cfg(windows)]
fn capture_pixel_value(
    point: Option<Point>,
    on_monitor: impl FnOnce(Point) -> bool,
    read_pixel: impl FnOnce(Point) -> Option<u32>,
) -> Option<String> {
    let point = point.filter(|point| on_monitor(*point))?;
    let rgb = read_pixel(point)?;
    Some(format!("{}, {}, #{:06X}", point.x, point.y, rgb))
}

#[cfg(windows)]
fn preview_pixel_condition(
    value: &str,
    tolerance: &str,
    different: bool,
    on_monitor: impl FnOnce(Point) -> bool,
    read_pixel: impl FnOnce(Point) -> Option<u32>,
) -> Result<String, &'static str> {
    let (point, expected) = targets::parse_pixel(value).ok_or("Enter a pixel as x, y, #RRGGBB.")?;
    let tolerance = tolerance
        .trim()
        .parse::<u8>()
        .map_err(|_| "Set pixel tolerance from 0 to 255.")?;
    if different && tolerance == 255 {
        return Err("Different requires a tolerance below 255.");
    }
    if !on_monitor(point) {
        return Err("The pixel is outside your displays.");
    }
    let actual = read_pixel(point).ok_or("The screen pixel could not be read.")?;
    let matched = target_pixels::pixel_condition_matches(actual, expected, tolerance, different);
    Ok(format!(
        "Sampled ({}, {}): #{actual:06X}. Condition {} at preview time.",
        point.x,
        point.y,
        if matched { "matches" } else { "does not match" }
    ))
}

fn import_preview(preset: &Settings) -> String {
    let mut normalized = preset.clone();
    normalized.normalize();
    let mut summary = preset_preview(preset);
    if normalized != *preset {
        summary
            .push_str("\nSome settings will be adjusted when loaded. Review them before starting.");
    }
    match Binding::parse(&normalized.trigger) {
        None => summary.push_str("\nCheck the activation shortcut on this computer."),
        Some(binding) => {
            if let Err(issue) = automation::validated_schedule(&normalized, Some(binding)) {
                summary.push_str(&format!("\nCheck before starting: {}", issue.message));
            }
        }
    }
    summary.push_str("\nTargets and shortcut availability may differ on this computer.");
    summary
}

fn review_fingerprint(patch: &targets::Patch) -> Option<slint::Image> {
    let bytes = patch.0.as_bytes();
    if bytes.len() != 20 * 12 * 2 {
        return None;
    }
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(20, 12);
    for (pixel, pair) in buffer
        .make_mut_slice()
        .iter_mut()
        .zip(bytes.as_chunks::<2>().0)
    {
        let high = char::from(pair[0]).to_digit(16)? as u8;
        let low = char::from(pair[1]).to_digit(16)? as u8;
        let value = high * 16 + low;
        *pixel = slint::Rgb8Pixel {
            r: value,
            g: value,
            b: value,
        };
    }
    Some(slint::Image::from_rgb8(buffer))
}

fn review_image_template(template: &targets::ImageTemplate) -> Option<slint::Image> {
    if !template.valid() {
        return None;
    }
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(
        template.width.into(),
        template.height.into(),
    );
    let channels = if template.is_color() { 3 } else { 1 };
    for (pixel, bytes) in buffer
        .make_mut_slice()
        .iter_mut()
        .zip(template.pixels.as_bytes().chunks_exact(2 * channels))
    {
        let decode = |offset: usize| -> Option<u8> {
            Some(
                (char::from(bytes[offset]).to_digit(16)? * 16
                    + char::from(bytes[offset + 1]).to_digit(16)?) as u8,
            )
        };
        let r = decode(0)?;
        let (g, b) = if channels == 3 {
            (decode(2)?, decode(4)?)
        } else {
            (r, r)
        };
        *pixel = slint::Rgb8Pixel { r, g, b };
    }
    Some(slint::Image::from_rgb8(buffer))
}

fn share_review_items(preset: &Settings) -> Result<Vec<ShareReviewItem>, String> {
    let mut value = serde_json::to_value(preset).map_err(|error| error.to_string())?;
    let route = value
        .as_object_mut()
        .and_then(|object| object.remove("route"))
        .and_then(|value| value.as_array().cloned())
        .ok_or("Could not inspect the preset route.")?;
    let mut items = Vec::with_capacity(route.len() + 1);
    items.push(ShareReviewItem {
        title: "All settings except route".into(),
        details: serde_json::to_string_pretty(&value)
            .map_err(|error| error.to_string())?
            .into(),
        ..Default::default()
    });
    for (index, (step, data)) in preset.route.iter().zip(route.iter()).enumerate() {
        let visual = step
            .control()
            .and_then(|control| control.selector.visual.as_ref());
        let image_template = match step {
            Step::Action(action) => match &action.action {
                targets::Action::WaitImage { template, .. } => Some(template),
                _ => None,
            },
            _ => None,
        };
        let (target, landmark, status) = match visual {
            Some(visual) => {
                let target = review_fingerprint(&visual.target);
                let landmark = review_fingerprint(&visual.landmark);
                let status = if target.is_some() && landmark.is_some() {
                    "Two saved 20 × 12 grayscale fingerprints. Their full data is shown above."
                } else {
                    "A fingerprint is malformed and cannot be displayed. Its full data remains in the JSON."
                };
                (target, landmark, status)
            }
            None => (None, None, ""),
        };
        items.push(ShareReviewItem {
            title: format!("Route step {} of {}", index + 1, route.len()).into(),
            details: serde_json::to_string_pretty(data)
                .map_err(|error| error.to_string())?
                .into(),
            has_visual: visual.is_some(),
            visual_status: status.into(),
            target_image: target.unwrap_or_default(),
            landmark_image: landmark.unwrap_or_default(),
            has_image_template: image_template.is_some(),
            image_template: image_template
                .and_then(review_image_template)
                .unwrap_or_default(),
        });
    }
    Ok(items)
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
    recording_stop: Arc<recording::RecordingStop>,
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
    #[cfg(windows)]
    recorder: RefCell<Option<recording::windows::Service>>,
    #[cfg(target_os = "linux")]
    recorder: RefCell<Option<recording::x11::Service>>,
    #[cfg(target_os = "macos")]
    recorder: RefCell<Option<recording::macos::Service>>,
    recorded: RefCell<Vec<recording::Entry>>,
    recording_timer: Timer,
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
        app.set_stop_fallback("Ctrl+Shift+F7".into());
    }
    #[cfg(target_os = "macos")]
    {
        app.set_process_example("Safari, Finder".into());
        app.set_stop_fallback("".into());
    }
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
    let recording_stop = Arc::new(recording::RecordingStop::default());
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
    let hotkeys = connect_shortcuts(stop_slot.clone(), recording_stop.clone(), &settings.trigger)?;

    let controller = Rc::new_cyclic(|own| Controller {
        app: app.as_weak(),
        own: own.clone(),
        state: RefCell::new(State {
            saved: settings.clone(),
            settings_dirty: recovered_settings,
            settings,
            library,
            active_preset: None,
            pending_preset: None,
            pending_import: None,
            pending_export: None,
            route_undo: None,
            library_dirty: recovered_library,
            run: None,
            zone_origin: None,
            pad_hits: 0,
        }),
        stop_slot,
        recording_stop,
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
        recorder: RefCell::new(None),
        recorded: RefCell::new(Vec::new()),
        recording_timer: Timer::default(),
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

fn connect_shortcuts(
    stop_slot: StopSlot,
    recording_stop: Arc<recording::RecordingStop>,
    preferred: &str,
) -> io::Result<Hotkeys> {
    Hotkeys::start(preferred, move |action| {
        let interrupted = match action {
            Action::Toggle(_, requested_at) => {
                let slot = stop_slot.lock().unwrap();
                slot.cancelled(requested_at) || slot.signal()
            }
            Action::Stop(stopped_at) => {
                recording_stop.request(stopped_at);
                let mut slot = stop_slot.lock().unwrap();
                slot.last_emergency = slot.last_emergency.max(Some(stopped_at));
                slot.signal()
            }
            Action::Unavailable => {
                recording_stop.request(hotkeys::tick_count());
                signal_stop(&stop_slot)
            }
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
                    Action::Stop(_) => {
                        if matches!(ui.get_recording_state(), 1 | 2) {
                            controller.recording_action(&ui, 3);
                        }
                        controller.refresh(&ui)
                    }
                    Action::Capture(point, requested_at)
                        if !interrupted
                            && !ui.get_active()
                            && ui.get_page() != 4
                            && ui.get_recording_state() != 1
                            && ui.get_recording_state() != 2 =>
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
                        if matches!(ui.get_recording_state(), 1 | 2) {
                            controller.recording_action(&ui, 3);
                        }
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
        ui.set_recording_available(recording::current_origin().is_some());
        ui.set_recording_windows(cfg!(windows));
        ui.set_recording_macos(cfg!(target_os = "macos"));
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
        bind!(on_recording_action, recording_action, operation);
        bind!(on_move_route_to, move_route_to);
        bind!(on_undo_route_removal, undo_route_removal);
        #[cfg(windows)]
        bind!(on_toggle_target_verification, toggle_target_verification);
        bind!(on_edit_step, edit_step, index);
        bind!(on_save_step, save_step);
        #[cfg(windows)]
        bind!(on_repair_target, repair_target);
        bind!(on_clear_zone, clear_zone, kind);
        bind!(on_cancel_zone_replacement, cancel_zone_replacement, kind);
        bind!(on_open_dry_run, open_dry_run);
        bind!(on_sample_dry_run, sample_dry_run);
        bind!(on_profile_action, profile_action, operation);
        bind!(on_resolve_preset_load, resolve_preset_load, operation);
        bind!(on_preset_interval, preset_interval, millis);
        bind!(on_select_unit, select_unit, unit);
        bind!(on_open_data_folder, open_data_folder);
        #[cfg(target_os = "linux")]
        bind!(on_wayland_action, wayland_action, operation);

        let close_ui = ui.as_weak();
        let slot = self.stop_slot.clone();
        ui.window().on_close_requested(move || {
            signal_stop(&slot);
            if let Some(ui) = close_ui.upgrade()
                && ui.get_step_editing()
                && ui.get_step_edited()
                && !ui.get_active()
            {
                ui.invoke_confirm_close();
                return CloseRequestResponse::KeepWindowShown;
            }
            CloseRequestResponse::HideWindow
        });
        let close_ui = ui.as_weak();
        ui.on_discard_and_close(move || {
            if let Some(ui) = close_ui.upgrade() {
                let _ = ui.hide();
            }
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
            if wayland::active() {
                let state = wayland::status();
                ui.set_stop_key(state.stop.as_deref().unwrap_or("—").into());
                ui.set_capture_key("—".into());
                ui.set_can_start(state.ready && state.stop.is_some());
                ui.set_wayland_ready(state.ready);
                ui.set_wayland_pending(state.pending);
                ui.set_wayland_configure(state.configure);
                ui.set_wayland_displays(state.displays.into());
                ui.set_wayland_status(state.message.into());
                let _ = wayland::layout_unchanged(ui);
            } else {
                let stop = self.hotkeys.stop_label();
                ui.set_stop_key(stop.as_deref().unwrap_or("—").into());
                ui.set_capture_key(self.hotkeys.capture_label().unwrap_or("—").into());
                ui.set_can_start(stop.is_some());
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
                        status(ui, "");
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
                3 if !ui.get_active() && ui.get_wayland_ready() && ui.get_wayland_configure() => {
                    if hotkeys.configure() {
                        status(ui, "");
                    } else {
                        failure(
                            ui,
                            "Could not request desktop shortcut settings. Try again.",
                        );
                    }
                }
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
                ui.set_saved_trigger(state.settings.trigger.as_str().into());
                ui.set_trigger_text(state.settings.trigger.as_str().into());
                self.update_configuration(ui, &state.settings);
                self.display_shortcuts(ui);
                ui.set_shortcut_error("".into());
                status(ui, "");
                drop(state);
                self.queue_save();
            }
            Err(error) => ui.set_shortcut_error(error.into()),
        }
    }

    fn preset_interval(&self, ui: &App, millis: i32) {
        if ui.get_active() || ui.get_capturing() || ui.get_step_editing() || ui.get_hold_down() {
            return;
        }
        ui.set_interval_text(millis.to_string().into());
        ui.set_interval_unit(0);
        ui.set_random_rate(false);
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
        ui.set_route_iterations(settings.route_iterations.as_str().into());
        ui.set_delay(settings.delay_seconds as i32);
        ui.set_interval_unit(settings.unit.into());
        ui.set_edge_mode(settings.edge_mode.into());
        ui.set_process_mode(settings.process_mode.into());
        ui.set_process_names(settings.process_names.as_str().into());
        ui.set_trigger_text(settings.trigger.as_str().into());
        ui.set_saved_trigger(settings.trigger.as_str().into());
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
        settings.route_iterations = ui.get_route_iterations().to_string();
        settings.delay_seconds = ui.get_delay().max(0) as u32;
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
        ui.set_low_cpu_reason(
            automation::low_cpu_unavailable_reason(settings, binding)
                .unwrap_or_default()
                .into(),
        );
        ui.set_configuration_error(issue.map_or("", |issue| issue.message).into());
        ui.set_configuration_field(issue.map_or("", |issue| issue.field).into());
        ui.set_limit_summary(schedule.as_ref().map_or_else(
            |_| "  ·  Check run settings".into(),
            |schedule| run_limit_summary(schedule).into(),
        ));
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
            cadence_label(settings)
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
        ui.set_route_recorded(settings.has_recording());
        ui.set_recording_windows(if settings.has_recording() {
            settings.recording_origin == recording::Origin::Windows
        } else {
            cfg!(windows)
        });
        ui.set_recording_macos(if settings.has_recording() {
            settings.recording_origin == recording::Origin::Macos
        } else {
            cfg!(target_os = "macos")
        });
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
        ui.set_route_position((index.max(0) + 1).to_string().into());
        ui.set_route_move_error("".into());
        for (kind, zone) in [(2, settings.inside_zone), (3, settings.stop_zone)] {
            let text = zone.map_or_else(
                || "Unset".into(),
                |zone| format!("{} to {}", point_label(zone.from), point_label(zone.to)),
            );
            let pending = origin.filter(|(pending, _)| *pending == kind).map_or_else(
                String::new,
                |(_, point)| {
                    format!(
                        "First corner {}. Pick the opposite corner.",
                        point_label(point)
                    )
                },
            );
            if kind == 2 {
                ui.set_inside_label(text.into());
                ui.set_inside_pending(pending.into());
            } else {
                ui.set_stop_zone_label(text.into());
                ui.set_stop_zone_pending(pending.into());
            }
        }
    }

    fn edit_route(&self, ui: &App, operation: i32) {
        if ui.get_active()
            || ui.get_capturing()
            || ui.get_step_editing()
            || ui.get_preset_load_open()
        {
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
                state.route_undo = Some(RouteUndo::remove(&mut state.settings, index));
                status(ui, "Route step removed. Undo removal is available.");
                (index as i32).min(state.settings.route.len() as i32 - 1)
            }
            _ => return,
        };
        ui.set_route_enabled(state.settings.route_enabled);
        ui.set_route_move_error("".into());
        ui.set_route_index(selected);
        Self::display_points(ui, &state.settings, state.zone_origin);
        self.update_configuration(ui, &state.settings);
        drop(state);
        self.queue_save();
    }

    fn move_route_to(&self, ui: &App) {
        if ui.get_active()
            || ui.get_capturing()
            || ui.get_step_editing()
            || ui.get_preset_load_open()
        {
            return;
        }
        let Ok(index) = usize::try_from(ui.get_route_index()) else {
            return;
        };
        let mut state = self.state.borrow_mut();
        let count = state.settings.route.len();
        if index >= count {
            return;
        }
        let destination = ui
            .get_route_position()
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|position| (1..=count).contains(position));
        let Some(destination) = destination else {
            ui.set_route_move_error(format!("Enter a position from 1 to {count}.").into());
            return;
        };
        ui.set_route_move_error("".into());
        ui.set_route_position(destination.to_string().into());
        if !move_route_step(&mut state.settings.route, index, destination - 1) {
            return;
        }
        ui.set_route_index(destination as i32 - 1);
        Self::display_points(ui, &state.settings, state.zone_origin);
        self.update_configuration(ui, &state.settings);
        status(
            ui,
            &format!("Moved step {} to position {destination}.", index + 1),
        );
        drop(state);
        self.queue_save();
    }

    fn undo_route_removal(&self, ui: &App) {
        if ui.get_active()
            || ui.get_capturing()
            || ui.get_step_editing()
            || ui.get_preset_load_open()
        {
            return;
        }
        let mut state = self.state.borrow_mut();
        let Some(undo) = state.route_undo.take() else {
            return;
        };
        let Some(index) = undo.restore(&mut state.settings) else {
            ui.set_can_undo_route(false);
            return;
        };
        ui.set_route_enabled(state.settings.route_enabled);
        ui.set_route_once(state.settings.route_once);
        ui.set_route_index(index as i32);
        Self::display_points(ui, &state.settings, state.zone_origin);
        self.update_configuration(ui, &state.settings);
        status(ui, "Route step restored.");
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

    fn recording_action(&self, ui: &App, operation: i32) {
        use recording::CaptureOptions;
        use targets::{Action, ActionStep};
        match operation {
            0 => {
                if ui.get_active()
                    || ui.get_capturing()
                    || ui.get_step_editing()
                    || ui.get_preset_load_open()
                    || self.recorder.borrow().is_some()
                {
                    return;
                }
                if !ui.get_can_start() || !self.hotkeys.listening() {
                    failure(
                        ui,
                        "The emergency stop shortcut is unavailable. Restore it before recording.",
                    );
                    return;
                }
                let options = CaptureOptions {
                    movement: ui.get_record_movement(),
                    buttons: ui.get_record_buttons(),
                    keys: ui.get_record_keys(),
                };
                if !options.movement && !options.buttons && !options.keys {
                    failure(ui, "Choose at least one input type to record.");
                    return;
                }
                let binding = self.hotkeys.toggle_binding();
                self.recording_stop.reset();
                #[cfg(windows)]
                let mut reserved_keys = vec![u32::from(keys::F7), u32::from(keys::F8)];
                #[cfg(windows)]
                if let Some(binding) = binding {
                    reserved_keys.push(u32::from(binding.vk));
                }
                #[cfg(windows)]
                let reserved_buttons = Button::ALL
                    .into_iter()
                    .filter(|button| binding.is_some_and(|binding| binding.matches_button(*button)))
                    .collect();
                #[cfg(windows)]
                let started = recording::windows::Service::start(
                    options,
                    reserved_keys,
                    reserved_buttons,
                    self.recording_stop.clone(),
                );
                #[cfg(target_os = "linux")]
                let started =
                    recording::x11::Service::start(options, binding, self.recording_stop.clone());
                #[cfg(target_os = "macos")]
                let started =
                    recording::macos::Service::start(options, binding, self.recording_stop.clone());
                match started {
                    Ok(service) => {
                        if self.recording_stop.timestamp().is_some() {
                            drop(service);
                            status(ui, "Recording cancelled by the emergency stop.");
                            return;
                        }
                        *self.recorder.borrow_mut() = Some(service);
                        self.recorded.borrow_mut().clear();
                        ui.set_recorded_count(0);
                        ui.set_recording_state(1);
                        let controller = self.own.clone();
                        self.recording_timer.start(TimerMode::Repeated, UI_INTERVAL, move || {
                            if let Some(controller) = controller.upgrade() && let Some(ui) = controller.app.upgrade() {
                                let progress = controller.recorder.borrow().as_ref().map(|recorder| recorder.count());
                                #[cfg(any(windows, target_os = "linux"))]
                                let progress = progress.map(|(count, limit, stopped)| (count, limit, stopped, false));
                                if let Some((count, limit, stopped, secure_pause)) = progress {
                                    ui.set_recorded_count(count as i32);
                                    if limit || stopped {
                                        controller.recording_action(&ui, 3);
                                        status(&ui, if limit { "Recording stopped at the event limit. Review the timeline before using it." } else { "The recording listener stopped. Review the timeline." });
                                    } else if secure_pause && ui.get_recording_state() == 1 {
                                        ui.set_recording_state(2);
                                        status(&ui, "Recording paused while macOS Secure Input is active.");
                                    }
                                }
                            }
                        });
                        status(
                            ui,
                            "Recording. Pause before entering sensitive information.",
                        );
                    }
                    Err(error) => failure(ui, &format!("Could not start recording ({error}).")),
                }
            }
            1 if ui.get_recording_state() == 1 => {
                if let Some(recorder) = self.recorder.borrow().as_ref() {
                    recorder.pause();
                }
                ui.set_recording_state(2);
                status(ui, "Recording paused.");
            }
            2 if ui.get_recording_state() == 2 => {
                if let Some(recorder) = self.recorder.borrow().as_ref()
                    && let Err(error) = recorder.resume()
                {
                    failure(ui, &format!("Could not resume recording ({error})."));
                    return;
                }
                ui.set_recording_state(1);
                status(ui, "Recording resumed.");
            }
            3 => {
                self.recording_stop.request(hotkeys::tick_count());
                let service = self.recorder.borrow_mut().take();
                if let Some(service) = service {
                    self.recording_timer.stop();
                    let entries = service.finish();
                    ui.set_recorded_count(entries.len() as i32);
                    *self.recorded.borrow_mut() = entries;
                    ui.set_recording_state(3);
                    status(
                        ui,
                        "Recording stopped. Replace the route to edit and play it.",
                    );
                }
            }
            4 if ui.get_recording_state() == 3
                && !ui.get_active()
                && !ui.get_capturing()
                && !ui.get_step_editing() =>
            {
                let recorded = self.recorded.borrow();
                if recorded.is_empty() {
                    failure(ui, "The recording has no events.");
                    return;
                }
                let Some(origin) = recording::current_origin() else {
                    failure(ui, "Recording is unavailable on this desktop.");
                    return;
                };
                if let Err(error) = recording::validate_for(&recorded, origin) {
                    failure(ui, error);
                    return;
                }
                let mut steps = Vec::with_capacity(recorded.len());
                for entry in recorded.iter() {
                    let Ok(delay_ms) = u32::try_from(entry.delay_ms) else {
                        failure(ui, "A recorded delay is too long.");
                        return;
                    };
                    let step = ActionStep {
                        action: Action::Recorded {
                            event: entry.event.clone(),
                        },
                        target: None,
                        delay_ms,
                    };
                    if let Err(error) =
                        automation::validate_action(&step, self.hotkeys.toggle_binding())
                    {
                        failure(ui, error);
                        return;
                    }
                    steps.push(Step::Action(Box::new(step)));
                }
                let mut state = self.state.borrow_mut();
                state.route_undo = None;
                state.settings.route = steps;
                state.settings.route_enabled = true;
                state.settings.route_once = true;
                state.settings.route_radius = "0".into();
                state.settings.route_wayland = false;
                state.settings.recording_origin = origin;
                ui.set_route_enabled(true);
                ui.set_route_once(true);
                ui.set_route_radius("0".into());
                ui.set_route_index(0);
                Self::display_points(ui, &state.settings, state.zone_origin);
                self.update_configuration(ui, &state.settings);
                drop(state);
                drop(recorded);
                self.recorded.borrow_mut().clear();
                ui.set_recording_state(0);
                ui.set_recorded_count(0);
                status(
                    ui,
                    "Recording added to the route. Review it before playback.",
                );
                self.queue_save();
            }
            5 if ui.get_recording_state() == 3 => {
                self.recorded.borrow_mut().clear();
                ui.set_recorded_count(0);
                ui.set_recording_state(0);
                status(ui, "Recording discarded.");
            }
            _ => {}
        }
    }

    fn edit_step(&self, ui: &App, index: i32) {
        use targets::Action;
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
        ui.set_step_recorded(false);
        if let Some(Step::Action(step)) = step
            && let targets::Action::Recorded { event } = &step.action
        {
            use recording::Event;
            ui.set_step_recorded(true);
            ui.set_step_edit_index(index);
            ui.set_step_has_target(false);
            ui.set_step_control_target(false);
            ui.set_step_inherits(false);
            ui.set_step_kind(0);
            ui.set_step_delay(step.delay_ms.to_string().into());
            match event {
                Event::Move(point) => {
                    ui.set_recorded_kind(0);
                    ui.set_recorded_x(point.x.to_string().into());
                    ui.set_recorded_y(point.y.to_string().into());
                }
                Event::Scroll {
                    point,
                    horizontal,
                    delta,
                } => {
                    ui.set_recorded_kind(3);
                    ui.set_recorded_x(point.x.to_string().into());
                    ui.set_recorded_y(point.y.to_string().into());
                    ui.set_step_direction(i32::from(*horizontal));
                    ui.set_step_count(delta.to_string().into());
                }
                Event::Button {
                    button,
                    down,
                    point,
                } => {
                    ui.set_recorded_kind(1);
                    ui.set_step_button(button.index());
                    ui.set_recorded_down(*down);
                    ui.set_recorded_x(point.x.to_string().into());
                    ui.set_recorded_y(point.y.to_string().into());
                }
                Event::Key {
                    code,
                    extended,
                    down,
                } => {
                    ui.set_recorded_kind(2);
                    ui.set_recorded_key(code.to_string().into());
                    ui.set_recorded_extended(*extended);
                    ui.set_recorded_down(*down);
                }
            }
            ui.set_step_error("".into());
            ui.set_step_edited(false);
            ui.set_step_editing(true);
            ui.invoke_reveal_step_editor();
            return;
        }
        let mut delay = 0;
        let mut count = 1;
        let mut button = 0;
        let mut direction = 0;
        let mut key = "Enter".to_owned();
        let mut pixel_tolerance = 0u8;
        let mut pixel_different = false;
        let mut pixel_stable_ms = 0u32;
        let mut pixel_skip_next = false;
        let mut pixel_check_once = false;
        let mut pixel_retry_clicks = 0u8;
        let mut image_search = "0, 0, 320, 240".to_owned();
        let mut image_template = String::new();
        let mut image_threshold = 90u8;
        let mut image_timeout = 10_000u32;
        let mut image_output = targets::ImageOutput::Wait;
        let mut image_scaled = false;
        let mut flow_kind = 0;
        let mut flow_count = 2;
        let mut flow_skip = false;
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
                        direction = selected.index();
                        count = *ticks;
                        2
                    }
                    Action::Key { key: selected } => {
                        key = selected.clone();
                        3
                    }
                    Action::Text { text } => {
                        key = text.clone();
                        4
                    }
                    Action::WaitImage {
                        search,
                        template,
                        threshold,
                        timeout_ms,
                        output,
                        scaled,
                    } => {
                        image_search = format!(
                            "{}, {}, {}, {}",
                            search.x, search.y, search.width, search.height
                        );
                        image_template = serde_json::to_string(template).unwrap_or_default();
                        image_threshold = *threshold;
                        image_timeout = *timeout_ms;
                        image_output = *output;
                        image_scaled = *scaled;
                        6
                    }
                    Action::WaitColor {
                        point,
                        rgb,
                        tolerance,
                        different,
                        stable_ms,
                        skip_next_on_timeout,
                        check_once,
                        skip_next_on_mismatch,
                        retry_clicks,
                    } => {
                        key = format!("{},{},#{:06X}", point.x, point.y, rgb);
                        pixel_tolerance = *tolerance;
                        pixel_different = *different;
                        pixel_stable_ms = *stable_ms;
                        pixel_check_once = *check_once;
                        pixel_retry_clicks = *retry_clicks;
                        pixel_skip_next = if *check_once {
                            *skip_next_on_mismatch
                        } else {
                            *skip_next_on_timeout
                        };
                        5
                    }
                    Action::Flow { operation } => {
                        use workflow::Operation;
                        if let Operation::If { test } | Operation::BreakIf { test } = operation {
                            key = format!("{}, {}, #{:06X}", test.point.x, test.point.y, test.rgb);
                            pixel_tolerance = test.tolerance;
                            pixel_different = test.different;
                        }
                        flow_kind = match operation {
                            Operation::If { .. } => 0,
                            Operation::Else => 1,
                            Operation::EndIf => 2,
                            Operation::Repeat { count } => {
                                flow_count = *count;
                                3
                            }
                            Operation::EndRepeat => 4,
                            Operation::BreakIf { .. } => 5,
                            Operation::Recover { retries, skip } => {
                                flow_count = (*retries).into();
                                flow_skip = *skip;
                                6
                            }
                        };
                        7
                    }
                    Action::Recorded { .. } => unreachable!(),
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
        let has_target = step.is_some_and(|step| step.point().is_some() || step.is_smart());
        ui.set_step_has_target(has_target);
        ui.set_step_control_target(step.is_some_and(Step::is_smart));
        ui.set_step_inherits(step.is_some_and(|step| !matches!(step, Step::Action(_))));
        if has_target {
            let settings = &state.settings;
            let output = shared_output_label(settings);
            ui.set_step_inherited_summary(format!("{output} · {}", cadence_label(settings)).into());
        }
        ui.set_step_kind(kind);
        ui.set_step_delay(delay.to_string().into());
        ui.set_step_count(count.to_string().into());
        ui.set_step_button(button);
        ui.set_step_direction(direction);
        ui.set_step_key(key.into());
        ui.set_step_pixel_preview("".into());
        ui.set_step_image_capture_rect("0, 0, 32, 32".into());
        ui.set_step_image_search(image_search.into());
        ui.set_step_image_template(image_template.into());
        if let Ok(template) =
            serde_json::from_str::<targets::ImageTemplate>(&ui.get_step_image_template())
        {
            ui.set_step_image_reference_image(review_image_template(&template).unwrap_or_default());
        } else {
            ui.set_step_image_reference_image(Default::default());
        }
        ui.set_step_image_has_match(false);
        ui.set_step_image_threshold(image_threshold.to_string().into());
        ui.set_step_image_timeout(image_timeout.to_string().into());
        ui.set_step_image_preview("".into());
        ui.set_step_image_output(match image_output {
            targets::ImageOutput::Wait => 0,
            targets::ImageOutput::Move => 1,
            targets::ImageOutput::Click => 2,
            targets::ImageOutput::If => 3,
        });
        ui.set_step_image_scaled(image_scaled);
        ui.set_step_flow_kind(flow_kind);
        ui.set_step_flow_count(flow_count.to_string().into());
        ui.set_step_flow_skip(flow_skip);
        ui.set_step_pixel_tolerance(pixel_tolerance.to_string().into());
        ui.set_step_pixel_different(pixel_different);
        ui.set_step_pixel_stable(pixel_stable_ms.to_string().into());
        ui.set_step_pixel_skip_next(pixel_skip_next);
        ui.set_step_pixel_check_once(pixel_check_once);
        ui.set_step_pixel_retries(pixel_retry_clicks.to_string().into());
        ui.set_step_error("".into());
        ui.set_step_edited(false);
        ui.set_step_editing(true);
        ui.invoke_reveal_step_editor();
    }

    fn save_step(&self, ui: &App) {
        use targets::{Action, ActionStep, Destination, Direction};
        if !ui.get_step_editing() || ui.get_active() || ui.get_capturing() {
            return;
        }
        let mut state = self.state.borrow_mut();
        let settings = &mut state.settings;
        let index = usize::try_from(ui.get_step_edit_index()).ok();
        let existing = index.and_then(|index| settings.route.get(index));
        let target = existing.and_then(Step::destination);
        let result = if ui.get_step_recorded() {
            (|| {
                let existing = existing.ok_or("The recorded step no longer exists.")?;
                if !matches!(existing, Step::Action(step) if matches!(step.action, Action::Recorded { .. }))
                {
                    return Err("The recorded step no longer exists.");
                }
                let delay_ms = ui
                    .get_step_delay()
                    .trim()
                    .parse()
                    .map_err(|_| "Enter a whole delay in milliseconds.")?;
                let point = || -> Result<Point, &'static str> {
                    Ok(Point {
                        x: ui
                            .get_recorded_x()
                            .trim()
                            .parse()
                            .map_err(|_| "Enter a whole X coordinate.")?,
                        y: ui
                            .get_recorded_y()
                            .trim()
                            .parse()
                            .map_err(|_| "Enter a whole Y coordinate.")?,
                    })
                };
                let event = match ui.get_recorded_kind() {
                    0 => recording::Event::Move(point()?),
                    1 => recording::Event::Button {
                        button: Button::from_index(ui.get_step_button()),
                        down: ui.get_recorded_down(),
                        point: point()?,
                    },
                    2 => recording::Event::Key {
                        code: ui
                            .get_recorded_key()
                            .trim()
                            .parse()
                            .map_err(|_| "Enter a whole native key code.")?,
                        extended: ui.get_recorded_extended(),
                        down: ui.get_recorded_down(),
                    },
                    3 => recording::Event::Scroll {
                        point: point()?,
                        horizontal: ui.get_step_direction() == 1,
                        delta: ui
                            .get_step_count()
                            .trim()
                            .parse()
                            .map_err(|_| "Enter a whole scroll amount.")?,
                    },
                    _ => return Err("The recorded event type is invalid."),
                };
                let step = ActionStep {
                    target: None,
                    action: Action::Recorded { event },
                    delay_ms,
                };
                if let Action::Recorded {
                    event: event @ recording::Event::Scroll { .. },
                } = &step.action
                {
                    recording::validate_for(
                        &[recording::Entry {
                            delay_ms: 0,
                            event: event.clone(),
                        }],
                        settings.recording_origin,
                    )?;
                }
                if let Action::Recorded {
                    event: recording::Event::Key { code, extended, .. },
                } = &step.action
                    && !recording::valid_key_code(settings.recording_origin, *code, *extended)
                {
                    return Err("The recorded key code is invalid for this desktop.");
                }
                automation::validate_action(&step, self.hotkeys.toggle_binding())?;
                Ok(Step::Action(Box::new(step)))
            })()
        } else if ui.get_step_inherits() {
            match target {
                Some(Destination::Point(point)) => Ok(Step::Point(point)),
                Some(Destination::Control(control)) => Ok(Step::Control(control)),
                None => Err("Only a saved point or control can use Clicker settings."),
            }
        } else {
            (|| {
                let delay_ms = if ui.get_step_kind() == 7 {
                    0
                } else {
                    ui.get_step_delay()
                        .trim()
                        .parse()
                        .map_err(|_| "Enter a whole delay in milliseconds.")?
                };
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
                    3 => Action::Key {
                        key: ui.get_step_key().trim().to_owned(),
                    },
                    4 => Action::Text {
                        text: ui.get_step_key().to_string(),
                    },
                    5 => {
                        let (point, rgb) = targets::parse_pixel(&ui.get_step_key())
                            .ok_or("Enter a pixel as x, y, #RRGGBB.")?;
                        let tolerance = ui
                            .get_step_pixel_tolerance()
                            .trim()
                            .parse::<u8>()
                            .map_err(|_| "Set pixel tolerance from 0 to 255.")?;
                        let stable_ms = ui
                            .get_step_pixel_stable()
                            .trim()
                            .parse::<u32>()
                            .map_err(|_| "Set pixel stability from 0 to 60,000 ms.")?;
                        let check_once = ui.get_step_pixel_check_once();
                        let retry_clicks = ui
                            .get_step_pixel_retries()
                            .trim()
                            .parse::<u8>()
                            .map_err(|_| "Set click retries from 0 to 10.")?;
                        Action::WaitColor {
                            point,
                            rgb,
                            tolerance,
                            different: ui.get_step_pixel_different(),
                            stable_ms,
                            skip_next_on_timeout: !check_once && ui.get_step_pixel_skip_next(),
                            check_once,
                            skip_next_on_mismatch: check_once && ui.get_step_pixel_skip_next(),
                            retry_clicks,
                        }
                    }
                    6 => {
                        let search = targets::parse_image_rect(&ui.get_step_image_search())
                            .ok_or("Enter search region as x, y, width, height.")?;
                        let template = serde_json::from_str(&ui.get_step_image_template())
                            .map_err(|_| "Capture a reference image first.")?;
                        let threshold = ui
                            .get_step_image_threshold()
                            .trim()
                            .parse()
                            .map_err(|_| "Enter an image threshold from 70 to 100%.")?;
                        let timeout_ms = ui
                            .get_step_image_timeout()
                            .trim()
                            .parse()
                            .map_err(|_| "Enter an image timeout in milliseconds.")?;
                        let output = match ui.get_step_image_output() {
                            1 => targets::ImageOutput::Move,
                            2 => targets::ImageOutput::Click,
                            3 => targets::ImageOutput::If,
                            _ => targets::ImageOutput::Wait,
                        };
                        Action::WaitImage {
                            search,
                            template,
                            threshold,
                            timeout_ms,
                            output,
                            scaled: ui.get_step_image_scaled(),
                        }
                    }
                    _ => {
                        use workflow::{Operation, PixelTest};
                        let pixel_test = || -> Result<PixelTest, &'static str> {
                            let (point, rgb) = targets::parse_pixel(&ui.get_step_key())
                                .ok_or("Enter a pixel as x, y, #RRGGBB.")?;
                            let tolerance = ui
                                .get_step_pixel_tolerance()
                                .trim()
                                .parse()
                                .map_err(|_| "Set pixel tolerance from 0 to 255.")?;
                            Ok(PixelTest {
                                point,
                                rgb,
                                tolerance,
                                different: ui.get_step_pixel_different(),
                            })
                        };
                        let operation = match ui.get_step_flow_kind() {
                            0 => Operation::If {
                                test: pixel_test()?,
                            },
                            1 => Operation::Else,
                            2 => Operation::EndIf,
                            3 => Operation::Repeat {
                                count: ui
                                    .get_step_flow_count()
                                    .trim()
                                    .parse()
                                    .map_err(|_| "Enter a repeat count from 1 to 1,000.")?,
                            },
                            4 => Operation::EndRepeat,
                            5 => Operation::BreakIf {
                                test: pixel_test()?,
                            },
                            _ => Operation::Recover {
                                retries: ui
                                    .get_step_flow_count()
                                    .trim()
                                    .parse()
                                    .map_err(|_| "Set retries from 0 to 10.")?,
                                skip: ui.get_step_flow_skip(),
                            },
                        };
                        Action::Flow { operation }
                    }
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
                Ok(Step::Action(Box::new(step)))
            })()
        };
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
            if ui.get_step_inherits() && *existing == step {
                ui.set_step_editing(false);
                return;
            }
            *existing = step;
            index
        } else {
            if settings.route.len() >= settings::MAX_POINTS {
                ui.set_step_error(
                    format!(
                        "The route has {} steps. Remove a step to add another.",
                        settings::MAX_POINTS
                    )
                    .into(),
                );
                return;
            }
            let selected = usize::try_from(ui.get_route_index())
                .ok()
                .filter(|index| *index < settings.route.len())
                .map_or(settings.route.len(), |index| index + 1);
            settings.route_enabled |= settings.route.is_empty();
            settings.route.insert(selected, step);
            selected
        };
        ui.set_route_enabled(settings.route_enabled);
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

    fn cancel_zone_replacement(&self, ui: &App, kind: i32) {
        if ui.get_active() || ui.get_capturing() || ui.get_step_editing() {
            return;
        }
        let mut state = self.state.borrow_mut();
        if state
            .zone_origin
            .is_some_and(|(pending, _)| pending == kind)
        {
            state.zone_origin = None;
            Self::display_points(ui, &state.settings, state.zone_origin);
        }
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
        let summaries: Vec<slint::SharedString> = library
            .profiles
            .values()
            .map(|settings| preset_preview(settings).into())
            .collect();
        ui.set_profile_summaries(slint::ModelRc::new(slint::VecModel::from(summaries)));
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

    fn display_share_review(
        ui: &App,
        kind: i32,
        name: &str,
        path: &str,
        preset: &Settings,
    ) -> Result<(), String> {
        let items = share_review_items(preset)?;
        ui.set_share_review_index(0);
        ui.set_share_review_items(slint::ModelRc::new(slint::VecModel::from(items)));
        ui.set_share_review_name(name.into());
        ui.set_share_review_path(path.into());
        ui.set_share_review_metadata(if kind == 2 {
            format!(
                "Transfer format: afterbeat-preset, version {}",
                settings::pixel_transfer_version(preset)
            )
            .into()
        } else {
            "The saved name can be changed below; all displayed settings are imported.".into()
        });
        ui.set_share_review_kind(kind);
        Ok(())
    }

    fn profile_action(&self, ui: &App, operation: i32) {
        if ui.get_active()
            || ui.get_capturing()
            || ui.get_step_editing()
            || ui.get_preset_load_open()
            || matches!(ui.get_recording_state(), 1 | 2)
        {
            return;
        }
        if operation == 7 {
            let mut state = self.state.borrow_mut();
            state.pending_import = None;
            state.pending_export = None;
            ui.set_import_ready(false);
            ui.set_import_error("".into());
            ui.set_share_review_kind(0);
            return;
        }
        if operation == 8 {
            let state = self.state.borrow();
            let Some((path, name, preset)) = state.pending_export.as_ref() else {
                return;
            };
            let result = settings::export_preset(path, name, preset);
            drop(state);
            match result {
                Ok(()) => {
                    self.state.borrow_mut().pending_export = None;
                    ui.set_share_review_kind(0);
                    status(ui, "Reviewed preset exported.");
                }
                Err(error) => failure(ui, &format!("Could not export preset: {error}")),
            }
            return;
        }
        if (0..=3).contains(&operation) {
            let mut state = self.state.borrow_mut();
            state.pending_import = None;
            state.pending_export = None;
            ui.set_import_ready(false);
            ui.set_share_review_kind(0);
        }
        if operation == 6 {
            let name = ui.get_import_name().to_string();
            let mut state = self.state.borrow_mut();
            let Some(preset) = state.pending_import.as_ref() else {
                return;
            };
            let library = match new_preset(&state.library, name.trim(), preset) {
                Ok(library) => library,
                Err(error) => {
                    ui.set_import_error(error.into());
                    return;
                }
            };
            state.library = library;
            state.library_dirty = true;
            state.pending_import = None;
            ui.set_import_ready(false);
            ui.set_share_review_kind(0);
            ui.set_import_error("".into());
            ui.set_selected_preset(name.trim().into());
            Self::display_library(ui, &state.library);
            drop(state);
            if self.save() {
                status(ui, "Preset imported. Review it before loading.");
            }
            return;
        }
        if operation == 4 || operation == 5 {
            let path_text = ui.get_preset_transfer_path().to_string();
            let path = std::path::Path::new(path_text.trim());
            if path.as_os_str().is_empty() {
                failure(ui, "Enter a preset file path.");
                return;
            }
            if operation == 4 {
                let path = match std::path::absolute(path) {
                    Ok(path) => path,
                    Err(error) => {
                        failure(ui, &format!("Could not resolve the export path: {error}"));
                        return;
                    }
                };
                let state = self.state.borrow();
                let name = ui.get_selected_preset().to_string();
                let Some(preset) = state.library.profiles.get(&name) else {
                    failure(ui, "Choose a saved preset to export.");
                    return;
                };
                let preset = preset.clone();
                drop(state);
                if let Err(error) =
                    Self::display_share_review(ui, 2, &name, &path.to_string_lossy(), &preset)
                {
                    failure(ui, &error);
                    return;
                }
                let mut state = self.state.borrow_mut();
                state.pending_import = None;
                state.pending_export = Some((path, name, preset));
                ui.set_import_ready(false);
            } else {
                self.state.borrow_mut().pending_import = None;
                self.state.borrow_mut().pending_export = None;
                ui.set_import_ready(false);
                ui.set_share_review_kind(0);
                let (name, preset) = match settings::import_preset(path) {
                    Ok(preset) => preset,
                    Err(error) => {
                        failure(ui, &format!("Could not import preset: {error}"));
                        return;
                    }
                };
                let summary = import_preview(&preset);
                if let Err(error) =
                    Self::display_share_review(ui, 1, &name, path_text.trim(), &preset)
                {
                    failure(ui, &error);
                    return;
                }
                ui.set_import_name(name.into());
                ui.set_import_summary(summary.into());
                ui.set_import_error("".into());
                self.state.borrow_mut().pending_import = Some(preset);
                ui.set_import_ready(true);
            }
            return;
        }
        if operation == 1 {
            let name = ui.get_selected_preset().to_string();
            let mut state = self.state.borrow_mut();
            if !state.library.profiles.contains_key(&name) {
                failure(ui, "Choose a saved preset to load.");
                return;
            }
            if setup_needs_preserving(
                &state.settings,
                &state.library,
                state.active_preset.is_some(),
            ) {
                state.pending_preset = Some(name.clone());
                ui.set_pending_preset(name.into());
                ui.set_load_save_name("".into());
                ui.set_load_error("".into());
                ui.set_load_saved(false);
                drop(state);
                ui.invoke_confirm_preset_load();
            } else {
                drop(state);
                if let Err(error) = self.load_preset(ui, &name) {
                    failure(ui, &error);
                }
            }
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
                let library = match new_preset(&state.library, name, &state.settings) {
                    Ok(library) => library,
                    Err(error) => {
                        ui.set_preset_error(error.into());
                        return;
                    }
                };
                state.library = library;
                state.library_dirty = true;
                state.active_preset = Some(name.to_owned());
                ui.set_selected_preset(name.into());
                ui.set_profile_name("".into());
                ui.set_preset_error("".into());
                "Preset saved."
            }
            2 => {
                if state.library.profiles.remove(name).is_none() {
                    failure(ui, "Choose a saved preset to delete.");
                    return;
                }
                state.library_dirty = true;
                if state.active_preset.as_deref() == Some(name) {
                    state.active_preset = None;
                }
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
        Self::display_active_preset(ui, &state);
        drop(state);
        let saved = self.save();
        status(ui, if saved { notice } else { "" });
    }

    fn load_preset(&self, ui: &App, name: &str) -> Result<(), String> {
        let mut state = self.state.borrow_mut();
        let mut settings = state
            .library
            .profiles
            .get(name)
            .cloned()
            .ok_or("Choose a saved preset to load.")?;
        self.hotkeys.rebind(&settings.trigger)?;
        settings.normalize();
        state.settings = settings;
        state.active_preset = Some(name.to_owned());
        state.route_undo = None;
        state.zone_origin = None;
        ui.set_can_undo_route(false);
        self.display_settings(ui, &state.settings);
        Self::display_active_preset(ui, &state);
        self.display_shortcuts(ui);
        ui.set_shortcut_error("".into());
        drop(state);
        self.save();
        status(ui, "Preset loaded.");
        Ok(())
    }

    fn resolve_preset_load(&self, ui: &App, operation: i32) {
        if ui.get_active() || ui.get_capturing() || ui.get_step_editing() {
            return;
        }
        let mut state = self.state.borrow_mut();
        let Some(target) = state.pending_preset.clone() else {
            return;
        };
        if operation == 0 {
            state.pending_preset = None;
            ui.invoke_close_preset_load();
            return;
        }
        if !matches!(operation, 1 | 2) {
            return;
        }
        if operation == 2 {
            let name = ui.get_load_save_name();
            let name = name.trim();
            let candidate = match new_preset(&state.library, name, &state.settings) {
                Ok(library) => library,
                Err(error) => {
                    ui.set_load_error(error.into());
                    return;
                }
            };
            // The current setup must be durable before loading can replace it.
            let saved = self
                .path
                .as_ref()
                .ok_or_else(|| io::Error::other("No data folder."))
                .and_then(|path| settings::save(&path.with_file_name("library.json"), &candidate));
            if let Err(error) = saved {
                diagnostics::record(&format!("Save before loading preset: {error}"));
                ui.set_load_error(
                    "Could not save the current setup. Try again or cancel to keep it.".into(),
                );
                return;
            }
            state.library = candidate;
            state.library_dirty = false;
            state.active_preset = Some(name.to_owned());
            Self::display_library(ui, &state.library);
            Self::display_active_preset(ui, &state);
            ui.set_load_saved(true);
        }
        drop(state);
        match self.load_preset(ui, &target) {
            Ok(()) => {
                self.state.borrow_mut().pending_preset = None;
                ui.invoke_close_preset_load();
            }
            Err(error) => {
                ui.set_load_error(error.into());
            }
        }
    }

    fn queue_save(&self) {
        if let Some(ui) = self.app.upgrade() {
            let mut state = self.state.borrow_mut();
            if state
                .route_undo
                .as_ref()
                .is_some_and(|undo| !undo.after.matches(&state.settings))
            {
                state.route_undo = None;
            }
            ui.set_can_undo_route(state.route_undo.is_some());
            Self::display_active_preset(&ui, &state);
        }
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

    fn display_active_preset(ui: &App, state: &State) {
        let name = state.active_preset.as_deref().unwrap_or_default();
        let modified = state.library.profiles.get(name).is_some_and(|preset| {
            let mut preset = preset.clone();
            preset.normalize();
            state.settings != preset
        });
        ui.set_active_preset(name.into());
        ui.set_preset_modified(modified);
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
        #[cfg(not(windows))]
        if (matches!(kind, PIXEL_CAPTURE | PIXEL_PREVIEW) || image_capture_kind(kind)) {
            return;
        }
        if ui.get_active()
            || (ui.get_step_editing()
                && !(matches!(kind, PIXEL_CAPTURE | PIXEL_PREVIEW) || image_capture_kind(kind)))
            || (matches!(kind, PIXEL_CAPTURE | PIXEL_PREVIEW)
                && (!ui.get_step_editing() || !matches!(ui.get_step_kind(), 5 | 7)))
            || (image_capture_kind(kind) && (!ui.get_step_editing() || ui.get_step_kind() != 6))
            || ui.get_preset_load_open()
            || matches!(ui.get_recording_state(), 1 | 2)
            || (ui.get_route_recorded() && matches!(kind, 1 | CONTROL_CAPTURE))
        {
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
        if kind == PIXEL_PREVIEW {
            ui.set_step_pixel_preview("".into());
        }
        if image_capture_kind(kind) {
            ui.set_step_image_preview("".into());
            ui.set_step_image_has_match(false);
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
        if (ui.get_step_editing()
            && !(ui.get_capturing()
                && (matches!(ui.get_pick_kind(), PIXEL_CAPTURE | PIXEL_PREVIEW)
                    || image_capture_kind(ui.get_pick_kind()))))
            || ui.get_preset_load_open()
            || ui.get_capture_shortcut_kind() < 0
            || (ui.get_route_recorded()
                && matches!(ui.get_capture_shortcut_kind(), 1 | CONTROL_CAPTURE))
        {
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
        if image_capture_kind(kind) {
            self.end_capture(ui);
            if !ui.get_step_editing() || ui.get_step_kind() != 6 {
                return;
            }
            if matches!(kind, IMAGE_REFERENCE_FIRST | IMAGE_SEARCH_FIRST) {
                if let Some(point) = point.filter(|p| mouse::on_monitor(*p)) {
                    ui.set_step_image_corner_x(point.x);
                    ui.set_step_image_corner_y(point.y);
                    self.capture(ui, kind + 1);
                } else {
                    ui.set_step_error("Could not select the first corner.".into());
                    clear_capture_status(ui);
                }
                return;
            }
            if matches!(kind, IMAGE_REFERENCE_SECOND | IMAGE_SEARCH_SECOND) {
                let first = Point {
                    x: ui.get_step_image_corner_x(),
                    y: ui.get_step_image_corner_y(),
                };
                let limit = if kind == IMAGE_REFERENCE_SECOND {
                    64
                } else {
                    512
                };
                let rect = point
                    .and_then(|second| {
                        Some(targets::ImageRect {
                            x: first.x.min(second.x),
                            y: first.y.min(second.y),
                            width: u16::try_from(first.x.abs_diff(second.x).checked_add(1)?)
                                .ok()?,
                            height: u16::try_from(first.y.abs_diff(second.y).checked_add(1)?)
                                .ok()?,
                        })
                    })
                    .filter(|rect| target_pixels::image_region(*rect, limit).is_some());
                if let Some(rect) = rect {
                    let value = format!("{}, {}, {}, {}", rect.x, rect.y, rect.width, rect.height);
                    if kind == IMAGE_REFERENCE_SECOND {
                        ui.set_step_image_capture_rect(value.into());
                    } else {
                        ui.set_step_image_search(value.into());
                        ui.set_step_edited(true);
                    }
                    ui.set_step_error("".into());
                } else {
                    ui.set_step_error(
                        format!(
                            "Select a rectangle up to {limit} × {limit} pixels on one display."
                        )
                        .into(),
                    );
                }
                clear_capture_status(ui);
                return;
            }
            let result = if kind == IMAGE_CAPTURE {
                (|| -> Result<String, &'static str> {
                    let rect = targets::parse_image_rect(&ui.get_step_image_capture_rect())
                        .ok_or("Enter reference region as x, y, width, height.")?;
                    target_pixels::image_region(rect, 64).ok_or(
                        "Reference must fit within one display and be 8–64 pixels on each side.",
                    )?;
                    let template = target_pixels::Frame::capture_image(rect)
                        .ok_or("Could not capture the reference image (minimum 8 × 8 pixels).")?;
                    ui.set_step_image_reference_image(
                        review_image_template(&template).unwrap_or_default(),
                    );
                    ui.set_step_image_template(
                        serde_json::to_string(&template)
                            .map_err(|_| "Could not save the reference image.")?
                            .into(),
                    );
                    ui.set_step_edited(true);
                    Ok(format!(
                        "Reference captured: {} × {} grayscale pixels.",
                        template.width, template.height
                    ))
                })()
            } else {
                (|| -> Result<String, &'static str> {
                    let rect = targets::parse_image_rect(&ui.get_step_image_search())
                        .ok_or("Enter search region as x, y, width, height.")?;
                    let region = target_pixels::image_region(rect, 512).ok_or("Search region must fit within one display and be at most 512 × 512 pixels.")?;
                    let template: targets::ImageTemplate =
                        serde_json::from_str(&ui.get_step_image_template())
                            .map_err(|_| "Capture a reference image first.")?;
                    let threshold = ui
                        .get_step_image_threshold()
                        .trim()
                        .parse::<u8>()
                        .map_err(|_| "Enter an image threshold from 70 to 100%.")?;
                    if !(70..=100).contains(&threshold) {
                        return Err("Enter an image threshold from 70 to 100%.");
                    }
                    let frame = target_pixels::Frame::capture_color(region)
                        .ok_or("The screen image could not be read.")?;
                    let deadline = Instant::now() + Duration::from_secs(2);
                    use target_pixels::ImageSearch;
                    let result = frame.search_image(&template, threshold, ui.get_step_image_scaled(), true, || Instant::now() >= deadline)
                        .map_err(|error| if error == "Image search interrupted." { "Preview took too long. Reduce the search region or increase the threshold." } else { error })?;
                    let (found, state) = match result {
                        ImageSearch::Missing => {
                            return Ok(format!("No match at {threshold}% threshold."));
                        }
                        ImageSearch::Unique(found) => (found, "Match"),
                        ImageSearch::Ambiguous(found) => {
                            (found, "Multiple locations match. Best match")
                        }
                    };
                    ui.set_step_image_match_image(
                        frame.matched_image(found.region).unwrap_or_default(),
                    );
                    ui.set_step_image_has_match(true);
                    let point = found.region.center();
                    Ok(format!(
                        "{state} at {}, {} · {}%.",
                        point.x,
                        point.y,
                        found.score()
                    ))
                })()
            };
            match result {
                Ok(message) => {
                    ui.set_step_image_preview(message.into());
                    ui.set_step_error("".into());
                }
                Err(error) => {
                    ui.set_step_error(error.into());
                }
            }
            clear_capture_status(ui);
            return;
        }
        #[cfg(windows)]
        if kind == PIXEL_PREVIEW {
            self.end_capture(ui);
            if !ui.get_step_editing() || !matches!(ui.get_step_kind(), 5 | 7) {
                return;
            }
            match preview_pixel_condition(
                &ui.get_step_key(),
                &ui.get_step_pixel_tolerance(),
                ui.get_step_pixel_different(),
                mouse::on_monitor,
                target_pixels::pixel_rgb,
            ) {
                Ok(message) => {
                    ui.set_step_pixel_preview(message.into());
                    ui.set_step_error("".into());
                    clear_capture_status(ui);
                }
                Err(error) => {
                    ui.set_step_error(error.into());
                    clear_capture_status(ui);
                }
            }
            return;
        }
        #[cfg(windows)]
        if kind == PIXEL_CAPTURE {
            self.end_capture(ui);
            if !ui.get_step_editing() || !matches!(ui.get_step_kind(), 5 | 7) {
                return;
            }
            let result = capture_pixel_value(point, mouse::on_monitor, target_pixels::pixel_rgb);
            if let Some(value) = result {
                ui.set_step_key(value.into());
                ui.set_step_pixel_preview("".into());
                ui.set_step_edited(true);
                ui.set_step_error("".into());
                status(
                    ui,
                    "Pixel captured. Review the condition and save the step.",
                );
            } else {
                ui.set_step_error("Could not read a pixel on the displays. Try again.".into());
                clear_capture_status(ui);
            }
            return;
        }
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
            failure(
                ui,
                &format!(
                    "The route has {} steps. Remove a step to add another.",
                    settings::MAX_POINTS
                ),
            );
            return;
        }
        ui.set_route_index(state.settings.route.len() as i32);
        let step = Step::Control(Box::new(step));
        let message = format!("Captured {}.", step.label());
        state.settings.route_enabled |= state.settings.route.is_empty();
        state.settings.route.push(step);
        state.settings.route_wayland = false;
        ui.set_route_enabled(state.settings.route_enabled);
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
                        &format!(
                            "The route has {} points. Remove a point to add another.",
                            settings::MAX_POINTS
                        ),
                    );
                    return;
                }
                ui.set_route_index(state.settings.route.len() as i32);
                state.settings.route_enabled |= state.settings.route.is_empty();
                state.settings.route.push(Step::Point(point));
                state.settings.route_wayland = native_wayland;
                ui.set_route_enabled(state.settings.route_enabled);
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
            || ui.get_shortcut_editing()
            || !ui.get_can_start()
            || !ui.get_input_ready()
            || ui.get_capturing()
            || ui.get_step_editing()
            || ui.get_preset_load_open()
            || matches!(ui.get_recording_state(), 1 | 2)
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
        let shortcut = self.hotkeys.toggle_binding();
        #[cfg(target_os = "macos")]
        let double_click_interval = if settings.route_enabled && settings.has_recording() {
            macos::double_click_interval()
        } else {
            Duration::ZERO
        };
        let (schedule, injector, guard) = automation::prepare(&settings, shortcut, binding)?;
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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let smart_run = settings.route_enabled && settings.has_recording();
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
                    let mut countdown_gate = || guard.borrow_mut().check();
                    engine::run_sequence(
                        schedule,
                        &worker_stop,
                        &worker_progress,
                        guarded.then_some(&mut countdown_gate),
                        |control| {
                            smart.as_mut().unwrap().sequence(
                                &settings,
                                schedule,
                                &mut guard.borrow_mut(),
                                control,
                                || dispatch_probe.as_mut().is_some_and(|probe| probe.pending()),
                            )
                        },
                    )
                } else {
                    run()
                };
                #[cfg(target_os = "linux")]
                let result = if smart_run {
                    let mut countdown_gate = || guard.borrow_mut().check();
                    engine::run_sequence(
                        schedule,
                        &worker_stop,
                        &worker_progress,
                        guarded.then_some(&mut countdown_gate),
                        |control| {
                            recording::x11_player::run(
                                &settings,
                                schedule,
                                shortcut,
                                &mut guard.borrow_mut(),
                                control,
                                || dispatch_probe.as_mut().is_some_and(|probe| probe.pending()),
                            )
                        },
                    )
                } else {
                    run()
                };
                #[cfg(target_os = "macos")]
                let result = if smart_run {
                    let mut countdown_gate = || guard.borrow_mut().check();
                    engine::run_sequence(
                        schedule,
                        &worker_stop,
                        &worker_progress,
                        guarded.then_some(&mut countdown_gate),
                        |control| {
                            recording::macos_player::run(
                                &settings,
                                schedule,
                                shortcut,
                                double_click_interval,
                                &mut guard.borrow_mut(),
                                control,
                                || dispatch_probe.as_mut().is_some_and(|probe| probe.pending()),
                            )
                        },
                    )
                } else {
                    run()
                };
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
            let skipped = run.progress.skipped_steps();
            let last_skipped = run.progress.last_skipped_step();
            let retried = run.progress.retried_clicks();
            let with_skips =
                |message: &str| run_feedback_message(message, skipped, last_skipped, retried);
            match result {
                Ok(Outcome::Completed) => status(ui, &with_skips("Completed.")),
                Ok(Outcome::Stopped) => status(ui, &with_skips("Stopped.")),
                Ok(Outcome::Guarded(reason)) => status(ui, &with_skips(reason)),
                Err(error) => {
                    diagnostics::record(&format!("Run stopped with an error: {error}"));
                    failure(ui, &with_skips(&error));
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
            let hint = pause_hint(reason);
            #[cfg(windows)]
            if let Some((index, deadline)) = run.progress.target_wait(reason)
                && let Some(step) = state.settings.route.get(index)
            {
                status(
                    ui,
                    &format!(
                        "Step {} of {} · {} · {} s left. {hint}",
                        index + 1,
                        state.settings.route.len(),
                        step.label(),
                        seconds_left(deadline, now)
                    ),
                );
                return;
            }
            status(ui, hint);
        } else if run.progress.started.get().is_some() {
            ui.set_run_state("Running".into());
            let skipped = run.progress.skipped_steps();
            status(
                ui,
                &run_feedback_message(
                    "",
                    skipped,
                    run.progress.last_skipped_step(),
                    run.progress.retried_clicks(),
                ),
            );
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
            } else if capture.kind == PIXEL_CAPTURE {
                status(
                    ui,
                    &format!(
                        "Picking pixel in {left} s. Move the pointer to the color, or press the capture shortcut."
                    ),
                );
            } else if capture.kind == PIXEL_PREVIEW {
                status(
                    ui,
                    &format!(
                        "Checking pixel in {left} s. Uncover the target, or press the capture shortcut."
                    ),
                );
            } else if (IMAGE_REFERENCE_FIRST..=IMAGE_SEARCH_SECOND).contains(&capture.kind) {
                let corner = if matches!(capture.kind, IMAGE_REFERENCE_FIRST | IMAGE_SEARCH_FIRST) {
                    "first"
                } else {
                    "opposite"
                };
                status(
                    ui,
                    &format!(
                        "Point at the {corner} corner. Picks in {left} s, or press the capture shortcut."
                    ),
                );
            } else if matches!(capture.kind, IMAGE_CAPTURE | IMAGE_PREVIEW) {
                status(
                    ui,
                    &format!(
                        "{} in {left} s. Uncover the screen, or press the capture shortcut.",
                        if capture.kind == IMAGE_CAPTURE {
                            "Capturing image"
                        } else {
                            "Searching for image"
                        }
                    ),
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
        self.recording_stop.request(hotkeys::tick_count());
        {
            self.recording_timer.stop();
            self.recorder.borrow_mut().take();
        }
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
        PauseReason::PixelColor => "Waiting for the screen condition.",
        PauseReason::PendingStop => "Waiting for the stop shortcut. Use Stop run if needed.",
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

fn run_skip_message(message: &str, skipped: u64, last: Option<u64>) -> String {
    if skipped == 0 {
        return message.to_owned();
    }
    let separator = if message.is_empty() { "" } else { " " };
    let last = last.map_or_else(String::new, |step| {
        format!(" Last skipped: step {}.", grouped(step))
    });
    format!(
        "{message}{separator}{} {} skipped.{last}",
        grouped(skipped),
        if skipped == 1 { "step" } else { "steps" }
    )
}

fn run_feedback_message(message: &str, skipped: u64, last: Option<u64>, retried: u64) -> String {
    let mut result = run_skip_message(message, skipped, last);
    if retried > 0 {
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(&format!(
            "{} click {} retried.",
            grouped(retried),
            if retried == 1 {
                "step was"
            } else {
                "steps were"
            }
        ));
    }
    result
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

fn shared_output_label(settings: &Settings) -> String {
    if settings.keyboard {
        if settings.text_output {
            format!("Type {}", targets::display_label(&settings.text_character))
        } else {
            format!("Press {}", targets::display_label(&settings.key))
        }
    } else if settings.scroll {
        format!(
            "Scroll {:?} · {} ticks",
            settings.scroll_direction,
            settings.scroll_ticks.trim()
        )
    } else {
        format!("{:?} click ×{}", settings.button, settings.click_count())
    }
}

fn cadence_label(settings: &Settings) -> String {
    match settings.rates() {
        Ok(Some((low, high))) => format!("{}–{} /s", rate_label(low), rate_label(high)),
        Ok(None) => match settings.interval() {
            Ok(interval) if interval.is_zero() => "Max speed".into(),
            Ok(interval) => format!("{} /s", rate_label(interval.as_secs_f64().recip())),
            Err(_) => "Check timing".into(),
        },
        Err(_) => "Check rate range".into(),
    }
}

fn preset_preview(saved: &Settings) -> String {
    let mut settings = saved.clone();
    settings.normalize();
    let output = if settings.has_action_route() {
        "Custom route actions".into()
    } else if settings.hold_down {
        if settings.keyboard {
            format!("Hold {}", targets::display_label(&settings.key))
        } else {
            format!("Hold {:?} button", settings.button)
        }
    } else {
        shared_output_label(&settings)
    };
    let pace = if settings.has_action_route() {
        "Step timing".into()
    } else if settings.hold_down {
        "Held down".into()
    } else {
        cadence_label(&settings)
    };
    let press = if !settings.has_action_route()
        && !settings.hold_down
        && settings
            .press_duration()
            .is_ok_and(|value| !value.is_zero())
    {
        format!(" · {} ms press", settings.press.trim())
    } else {
        String::new()
    };
    let target = if settings.route_enabled {
        let mode = if settings.route_once {
            "Once".to_owned()
        } else if !settings.has_recording() && settings.route_iterations.trim() != "0" {
            format!("{} loops", settings.route_iterations.trim())
        } else {
            "Loop".to_owned()
        };
        format!("{}-step route · {mode}", settings.route.len())
    } else if settings.keyboard {
        "Focused app".into()
    } else if settings.fixed {
        format!(
            "Fixed point · {}",
            settings
                .point
                .map_or_else(|| "Not captured".into(), point_label)
        )
    } else {
        "Follow cursor".into()
    };
    let actions = match settings.action_limit() {
        Ok(Some(count)) => format!("{} actions", grouped(count)),
        Ok(None) => "No action limit".into(),
        Err(_) => "Check action limit".into(),
    };
    let time = match settings.time_limit.trim().replace(',', ".").parse::<f64>() {
        Ok(0.0) => "No time limit".into(),
        Ok(value) if value.is_finite() && (0.0..=86_400.0).contains(&value) => {
            format!("{} s limit", settings.time_limit.trim())
        }
        _ => "Check time limit".into(),
    };
    let activation = if settings.hold_activation {
        format!(
            "Hold {} · release to stop",
            targets::display_label(&settings.trigger)
        )
    } else {
        format!(
            "Toggle {} · {} s start delay",
            targets::display_label(&settings.trigger),
            settings.delay_seconds
        )
    };
    let mut guards = Vec::new();
    if settings.edge_mode != 0 {
        guards.push(if settings.edge_mode == 1 {
            "Screen corners".into()
        } else {
            "Screen edges".into()
        });
    }
    if settings.inside_zone.is_some() {
        guards.push("Inside only".into());
    }
    if settings.stop_zone.is_some() {
        guards.push("Stop zone".into());
    }
    if settings.process_mode != 0 {
        guards.push(format!(
            "{} apps: {}",
            if settings.process_mode == 1 {
                "Allow"
            } else {
                "Block"
            },
            targets::display_label(&settings.process_names)
        ));
    }
    let guards = if guards.is_empty() {
        "None".into()
    } else {
        guards.join(" · ")
    };
    format!(
        "{output} · {pace}{press}\n{target} · {actions} · {time}\n{activation}\nGuards · {guards}"
    )
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
    fn moving_a_route_step_preserves_payload_and_every_other_steps_order() {
        let custom = Step::Action(Box::new(targets::ActionStep {
            action: targets::Action::Key {
                key: "Enter".into(),
            },
            target: Some(targets::Destination::Point(Point { x: -100, y: 250 })),
            delay_ms: 725,
        }));
        let mut route: Vec<_> = (0..64).map(|x| Step::Point(Point { x, y: 10 })).collect();
        route[59] = custom.clone();
        let mut expected = route.clone();
        expected.remove(59);
        expected.insert(1, custom);
        assert!(move_route_step(&mut route, 59, 1));
        assert_eq!(route, expected);
        assert!(move_route_step(&mut route, 1, 63));
        let unchanged = route.clone();
        assert!(!move_route_step(&mut route, 63, 63));
        assert!(!move_route_step(&mut route, 64, 0));
        assert!(!move_route_step(&mut route, 0, 64));
        assert_eq!(route, unchanged);
    }

    #[test]
    fn run_limits_describe_unlimited_counted_timed_and_once_runs() {
        let mut settings = Settings::default();
        let describe = |settings: &Settings| run_limit_summary(&settings.schedule().unwrap());
        assert_eq!(
            describe(&settings),
            "  ·  No action limit  ·  No time limit"
        );
        settings.limited = true;
        settings.count = "25".into();
        settings.time_limit = "12,5".into();
        assert_eq!(describe(&settings), "  ·  25 actions max  ·  12.5 s max");
        settings.route_enabled = true;
        settings.route_once = true;
        settings.route = vec![Step::Point(Point { x: 10, y: 20 }); 3];
        assert!(describe(&settings).contains("3 actions max"));
        settings.route_once = false;
        assert!(describe(&settings).contains("25 actions max"));
    }

    #[test]
    fn preset_preview_describes_saved_settings_without_changing_them() {
        let settings = Settings {
            keyboard: true,
            key: "Enter".into(),
            interval: "200".into(),
            limited: true,
            count: "25".into(),
            time_limit: "12,5".into(),
            hold_activation: true,
            trigger: "F9".into(),
            edge_mode: 2,
            process_mode: 1,
            process_names: "notepad.exe".into(),
            ..Settings::default()
        };
        let original = settings.clone();
        let preview = preset_preview(&settings);
        for detail in [
            "Press Enter",
            "5 /s",
            "Focused app",
            "25 actions",
            "12,5 s limit",
            "Hold F9 · release to stop",
            "Screen edges",
            "Allow apps: notepad.exe",
        ] {
            assert!(preview.contains(detail), "Missing {detail}: {preview}");
        }
        assert!(!preview.contains("start delay"), "Hold starts immediately");
        assert_eq!(settings, original);
    }

    #[test]
    fn preset_preview_includes_finite_route_loops() {
        let settings = Settings {
            route_enabled: true,
            route_iterations: "3".into(),
            route: vec![Step::Point(Point { x: 1, y: 2 })],
            ..Settings::default()
        };
        assert!(preset_preview(&settings).contains("1-step route · 3 loops"));
    }

    #[test]
    fn preset_preview_distinguishes_unlimited_runs_and_route_playback() {
        let mut settings = Settings::default();
        let preview = preset_preview(&settings);
        assert!(preview.contains("No action limit · No time limit"));
        assert!(preview.contains("Follow cursor"));
        assert!(preview.contains("Guards · None"));

        settings.route_enabled = true;
        settings.route_once = true;
        settings
            .route
            .push(Step::Action(Box::new(targets::ActionStep {
                action: targets::Action::Wait,
                target: None,
                delay_ms: 500,
            })));
        let preview = preset_preview(&settings);
        assert!(preview.contains("Custom route actions · Step timing"));
        assert!(preview.contains("1-step route · Once"));
        assert!(!preview.contains("10 /s"));
    }

    #[test]
    fn preset_load_protects_modified_and_unnamed_setups() {
        let mut settings = Settings::default();
        let mut library = Library::default();
        assert!(!setup_needs_preserving(&settings, &library, false));
        settings.button = Button::Right;
        assert!(setup_needs_preserving(&settings, &library, false));
        library.set_profile("Saved", settings.clone()).unwrap();
        assert!(!setup_needs_preserving(&settings, &library, true));
        settings.interval = "725".into();
        assert!(setup_needs_preserving(&settings, &library, true));
        // Returning a loaded preset to the defaults is still a modified setup.
        assert!(setup_needs_preserving(&Settings::default(), &library, true));
    }

    #[test]
    fn skipped_step_count_is_visible_in_run_messages() {
        assert_eq!(run_skip_message("Completed.", 0, None), "Completed.");
        assert_eq!(
            run_skip_message("Completed.", 1, Some(2)),
            "Completed. 1 step skipped. Last skipped: step 2."
        );
        assert_eq!(
            run_skip_message("", 2, Some(4)),
            "2 steps skipped. Last skipped: step 4."
        );
        assert_eq!(run_feedback_message("Completed.", 0, None, 0), "Completed.");
        assert_eq!(
            run_feedback_message("Completed.", 0, None, 1),
            "Completed. 1 click step was retried."
        );
        assert_eq!(
            run_feedback_message("", 1, Some(3), 2),
            "1 step skipped. Last skipped: step 3. 2 click steps were retried."
        );
    }

    #[test]
    fn sharing_review_covers_all_settings_steps_and_visual_data() {
        let patch = "0123456789abcdef".repeat(30);
        let control = serde_json::json!({
            "selector": {
                "window": {"executable": "C:\\private\\app.exe", "class": "Window", "title": "Private document"},
                "control": {"role": 50000, "identity": {"Name": "Save"}},
                "label": "Save",
                "visual": {
                    "anchor": {"role": 50000, "identity": {"Name": "Label"}},
                    "target_name": "Save", "anchor_size": {"x": 20, "y": 12},
                    "target_size": {"x": 20, "y": 12}, "offset": {"x": 1, "y": 2},
                    "target": patch, "landmark": "ff".repeat(240)
                }
            }
        });
        let step: Step = serde_json::from_value(control).unwrap();
        let preset = Settings {
            process_names: "private-filter.exe".into(),
            point: Some(Point { x: -123, y: 456 }),
            route: vec![
                step,
                Step::Action(Box::new(targets::ActionStep {
                    action: targets::Action::Text {
                        text: "private text".into(),
                    },
                    target: None,
                    delay_ms: 20,
                })),
            ],
            ..Settings::default()
        };
        let review = share_review_items(&preset).unwrap();
        assert_eq!(review.len(), 3);
        let header: serde_json::Value = serde_json::from_str(review[0].details.as_str()).unwrap();
        let mut expected = serde_json::to_value(&preset).unwrap();
        expected.as_object_mut().unwrap().remove("route");
        assert_eq!(header, expected);
        for (index, step) in preset.route.iter().enumerate() {
            let displayed: serde_json::Value =
                serde_json::from_str(review[index + 1].details.as_str()).unwrap();
            assert_eq!(displayed, serde_json::to_value(step).unwrap());
        }
        assert!(review[1].has_visual);
        assert_eq!(review[1].target_image.size().width, 20);
        assert_eq!(review[1].landmark_image.size().height, 12);
        let target = review[1].target_image.to_rgb8().unwrap();
        assert_eq!(target.as_slice()[0], slint::Rgb8Pixel { r: 1, g: 1, b: 1 });
        assert_eq!(
            target.as_slice()[1],
            slint::Rgb8Pixel {
                r: 0x23,
                g: 0x23,
                b: 0x23
            }
        );
        assert!(!review[2].has_visual);
        assert!(review_fingerprint(&targets::Patch("invalid".into())).is_none());
    }

    #[test]
    fn sharing_review_displays_saved_image_reference() {
        let mut preset = Settings::default();
        preset
            .route
            .push(Step::Action(Box::new(targets::ActionStep {
                action: targets::Action::WaitImage {
                    search: targets::ImageRect {
                        x: 0,
                        y: 0,
                        width: 8,
                        height: 8,
                    },
                    template: targets::ImageTemplate {
                        width: 8,
                        height: 8,
                        pixels: "ab".repeat(64),
                    },
                    threshold: 100,
                    timeout_ms: 100,
                    output: Default::default(),
                    scaled: false,
                },
                target: None,
                delay_ms: 0,
            })));
        let review = share_review_items(&preset).unwrap();
        assert!(review[1].has_image_template);
        assert_eq!(review[1].image_template.size().width, 8);
        let image = review[1].image_template.to_rgb8().unwrap();
        assert_eq!(
            image.as_slice()[0],
            slint::Rgb8Pixel {
                r: 0xab,
                g: 0xab,
                b: 0xab
            }
        );
    }

    #[test]
    fn import_preview_reports_normalization_and_invalid_settings() {
        let settings = Settings {
            delay_seconds: 42,
            interval: "invalid".into(),
            ..Settings::default()
        };
        let preview = import_preview(&settings);
        assert!(preview.contains("adjusted when loaded"));
        assert!(preview.contains("Check before starting:"));
        assert_eq!(settings.delay_seconds, 42);
    }

    #[test]
    fn saving_before_load_builds_a_candidate_without_replacing_existing_presets() {
        let mut library = Library::default();
        library
            .set_profile("Original", Settings::default())
            .unwrap();
        let settings = Settings {
            interval: "725".into(),
            ..Settings::default()
        };
        assert!(new_preset(&library, "Original", &settings).is_err());
        assert!(new_preset(&library, "", &settings).is_err());
        let candidate = new_preset(&library, "New setup", &settings).unwrap();
        assert_eq!(library.profiles.len(), 1);
        assert_eq!(candidate.profiles["Original"], Settings::default());
        assert_eq!(candidate.profiles["New setup"], settings);
    }

    #[test]
    fn undo_last_route_step_restores_modes_and_preserves_other_edits() {
        let step = Step::Action(Box::new(targets::ActionStep {
            action: targets::Action::Wait,
            target: None,
            delay_ms: 725,
        }));
        let mut settings = Settings {
            route: vec![step.clone()],
            route_enabled: true,
            route_once: true,
            route_wayland: true,
            ..Settings::default()
        };
        let undo = RouteUndo::remove(&mut settings, 0);
        assert!(!settings.route_enabled);
        settings.interval = "850".into();
        assert_eq!(undo.restore(&mut settings), Some(0));
        assert_eq!(settings.route, vec![step]);
        assert!(settings.route_enabled && settings.route_once && settings.route_wayland);
        assert_eq!(settings.interval, "850");
    }

    #[test]
    fn route_undo_does_not_overwrite_subsequent_route_changes() {
        let mut settings = Settings {
            route: vec![
                Step::Point(Point { x: 1, y: 2 }),
                Step::Point(Point { x: 3, y: 4 }),
            ],
            route_enabled: true,
            ..Settings::default()
        };
        let undo = RouteUndo::remove(&mut settings, 0);
        settings.route.push(Step::Point(Point { x: 5, y: 6 }));
        let changed = settings.clone();
        assert_eq!(undo.restore(&mut settings), None);
        assert_eq!(settings, changed);
    }

    #[test]
    #[cfg(windows)]
    fn pixel_capture_formats_sample_and_rejects_unavailable_pixels() {
        let point = Point { x: -15, y: 200 };
        let value = capture_pixel_value(Some(point), |_| true, |_| Some(0x00ff04)).unwrap();
        assert_eq!(targets::parse_pixel(&value), Some((point, 0x00ff04)));
        assert_eq!(
            capture_pixel_value(None, |_| panic!("no point"), |_| panic!("no point")),
            None
        );
        assert_eq!(
            capture_pixel_value(Some(point), |_| false, |_| panic!("outside displays")),
            None
        );
        assert_eq!(capture_pixel_value(Some(point), |_| true, |_| None), None);
    }

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

    #[test]
    fn emergency_stop_cancels_pending_pixel_capture() {
        let mut state = StopState {
            capture: Some(Capture {
                deadline: Instant::now() + CAPTURE_DELAY,
                kind: PIXEL_CAPTURE,
                requested_at: 10_000,
                point: None,
            }),
            last_emergency: Some(10_001),
            ..StopState::default()
        };
        assert!(state.cancelled(state.capture.unwrap().requested_at));
        assert!(state.receive_capture(Point { x: 1, y: 2 }, 10_002));
        assert!(state.cancelled(state.capture.unwrap().requested_at));
    }

    #[cfg(windows)]
    #[test]
    fn pixel_preview_uses_configured_point_tolerance_and_condition() {
        let sample = |value: &str, tolerance: &str, different, color| {
            preview_pixel_condition(
                value,
                tolerance,
                different,
                |p| p == Point { x: -4, y: 8 },
                |_| color,
            )
        };
        assert_eq!(
            sample("-4, 8, #102030", "2", false, Some(0x112232)).unwrap(),
            "Sampled (-4, 8): #112232. Condition matches at preview time."
        );
        assert!(
            sample("-4, 8, #102030", "1", false, Some(0x112232))
                .unwrap()
                .contains("does not match")
        );
        assert!(
            sample("-4, 8, #102030", "1", true, Some(0x112232))
                .unwrap()
                .contains("Condition matches")
        );
        assert!(sample("-4, 8, #102030", "255", true, Some(0)).is_err());
        assert!(sample("bad", "0", false, Some(0)).is_err());
        assert!(
            sample("1, 2, #102030", "0", false, Some(0))
                .unwrap_err()
                .contains("outside")
        );
        assert!(
            sample("-4, 8, #102030", "0", false, None)
                .unwrap_err()
                .contains("could not be read")
        );
    }
}
