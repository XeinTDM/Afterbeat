use crate::{
    engine::{Gate, PauseReason, Schedule, low_cpu_batch_size},
    hotkeys::Binding,
    keys,
    mouse::{self, Injector, Input, Press, Zone},
    settings::{Issue, Settings},
};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::{
    io,
    time::{Duration, Instant},
};
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{HWND, POINT},
    System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    },
    UI::WindowsAndMessaging::{
        GUITHREADINFO, GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId,
        WindowFromPoint,
    },
};

pub fn prepare(
    settings: &Settings,
    bound: Option<Binding>,
    invoked: Option<Binding>,
) -> Result<(Schedule, Injector, Guard), String> {
    if !settings.armed {
        return Err("Enable the master switch in Timing to start a run.".into());
    }
    #[cfg(target_os = "linux")]
    if crate::wayland::active() && settings.keyboard && invoked.is_some_and(Binding::has_modifiers)
    {
        return Err(
            "Use an unmodified global shortcut or start this Wayland keyboard run from Afterbeat."
                .into(),
        );
    }
    let Plan {
        mut schedule,
        input,
        points,
        radius,
        press,
        processes,
    } = plan(settings, bound).map_err(|issue| issue.message)?;
    let hold = if settings.hold_activation {
        schedule.delay = Duration::ZERO;
        Some(invoked.ok_or("Hold the activation shortcut to start.")?)
    } else {
        None
    };
    let injector = Injector::new(
        input,
        settings.double && !settings.keyboard,
        points,
        radius,
        press,
    )?;
    #[cfg(target_os = "linux")]
    let injector = injector.with_activation(bound);
    let guard = Guard {
        hold,
        release_modifiers: invoked.filter(|binding| hold.is_none() && binding.has_modifiers()),
        edge: settings.edge_mode,
        inside: settings.inside_zone,
        stop: settings.stop_zone,
        keyboard: settings.keyboard,
        processes,
        next_check: Instant::now(),
        cached: Gate::Open,
    };
    Ok((schedule, injector, guard))
}

struct Plan {
    schedule: Schedule,
    input: Input,
    points: Vec<mouse::Point>,
    radius: i32,
    press: Press,
    processes: Option<ProcessFilter>,
}

pub fn validated_schedule(settings: &Settings, bound: Option<Binding>) -> Result<Schedule, Issue> {
    plan(settings, bound).map(|plan| plan.schedule)
}

pub fn low_cpu_available(settings: &Settings, bound: Option<Binding>) -> bool {
    #[cfg(target_os = "linux")]
    if crate::wayland::active() {
        return false;
    }
    !settings.keyboard
        && !settings.double
        && !settings.hold_down
        && !settings.random_rate
        && !settings.hold_activation
        && !settings.route_enabled
        && settings.edge_mode == 0
        && settings.inside_zone.is_none()
        && settings.stop_zone.is_none()
        && settings.process_mode == 0
        && !bound.is_some_and(Binding::has_modifiers)
        && settings.press_duration() == Ok(Duration::ZERO)
        && settings
            .interval()
            .is_ok_and(|interval| low_cpu_batch_size(interval) > 1)
}

fn plan(settings: &Settings, bound: Option<Binding>) -> Result<Plan, Issue> {
    #[cfg(target_os = "linux")]
    let native_wayland = crate::wayland::active();
    #[cfg(not(target_os = "linux"))]
    let native_wayland = false;
    #[cfg(target_os = "linux")]
    if native_wayland {
        crate::wayland::validate(settings)?;
    }
    if settings.route_enabled
        && !settings.route.is_empty()
        && settings.route_wayland != native_wayland
    {
        return Err(Issue::new(
            "route",
            "This route uses a different desktop coordinate system. Replace its points for this session.",
        ));
    }
    if !settings.route_enabled
        && settings.fixed
        && !settings.keyboard
        && settings.point.is_some()
        && settings.point_wayland != native_wayland
    {
        return Err(Issue::new(
            "target",
            "Set the fixed point again for this desktop coordinate system.",
        ));
    }
    let mut schedule = settings.schedule()?;
    schedule.low_cpu = schedule.low_cpu && low_cpu_available(settings, bound);
    let input = if settings.keyboard && settings.text_output {
        let mut chars = settings.text_character.chars();
        let character = chars
            .next()
            .filter(|ch| !ch.is_control() && chars.next().is_none())
            .ok_or(Issue::new(
                "input",
                "Enter exactly one printable character.",
            ))?;
        if settings.hold_down || settings.press_duration() != Ok(Duration::ZERO) {
            return Err(Issue::new("press", "Select Tap for text output."));
        }
        Input::Text(character)
    } else if settings.keyboard {
        if settings.hold_activation && bound.is_some_and(Binding::has_modifiers) {
            return Err(Issue::new(
                "shortcut",
                "Use an activation shortcut without modifiers for Hold with Key press, or select Toggle.",
            ));
        }
        let key = keys::parse(&settings.key).ok_or(Issue::new(
            "input",
            "Enter a key such as a, A, Space, Enter, or F5.",
        ))?;
        if key.is_safety_key() || bound.is_some_and(|binding| binding.matches_key(key)) {
            return Err(Issue::new(
                "input",
                "Choose an output key different from the activation, stop, and capture keys.",
            ));
        }
        Input::Key(key)
    } else {
        Input::Mouse(settings.button)
    };
    let points = if settings.route_enabled {
        settings.route.clone()
    } else if settings.fixed && !settings.keyboard {
        vec![settings.point.ok_or(Issue::new(
            "target",
            "Capture a fixed point or select Follow cursor.",
        ))?]
    } else {
        Vec::new()
    };
    let radius = if settings.route_enabled {
        settings
            .radius()
            .map_err(|message| Issue::new("route", message))?
    } else {
        0
    };
    let press = if settings.hold_down {
        Press::Held
    } else {
        let duration = settings
            .press_duration()
            .map_err(|message| Issue::new("press", message))?;
        if duration.is_zero() {
            Press::Tap
        } else {
            Press::Timed(duration)
        }
    };
    #[cfg(target_os = "linux")]
    if !native_wayland && matches!(input, Input::Key(_)) && !matches!(press, Press::Tap) {
        return Err(Issue::new(
            "press",
            "Select Tap for X11 key output. Focus can change before a held key is released.",
        ));
    }
    Ok(Plan {
        schedule,
        input,
        points,
        radius,
        press,
        processes: ProcessFilter::new(settings.process_mode, &settings.process_names)
            .map_err(|message| Issue::new("filter", message))?,
    })
}

pub struct Guard {
    hold: Option<Binding>,
    release_modifiers: Option<Binding>,
    edge: u8,
    inside: Option<Zone>,
    stop: Option<Zone>,
    keyboard: bool,
    processes: Option<ProcessFilter>,
    next_check: Instant,
    cached: Gate,
}

impl Guard {
    pub fn enabled(&self) -> bool {
        self.hold.is_some()
            || self.release_modifiers.is_some()
            || self.watch_cursor()
            || self.keyboard
            || self.processes.is_some()
    }

    fn watch_cursor(&self) -> bool {
        self.edge != 0 || self.inside.is_some() || self.stop.is_some()
    }

    pub fn check(&mut self) -> Gate {
        let now = Instant::now();
        if now >= self.next_check {
            self.next_check = now + Duration::from_millis(8);
            self.cached = self.observe(mouse::cursor, Binding::modifiers_pressed);
        }
        self.cached
    }

    pub fn check_destination(&mut self, target: Option<mouse::Point>) -> Gate {
        self.destination_target(target, mouse::cursor)
            .err()
            .unwrap_or(Gate::Open)
    }

    #[cfg(target_os = "macos")]
    pub fn dispatch_target(
        &mut self,
        target: Option<mouse::Point>,
        cursor: mouse::Point,
    ) -> Result<Option<u32>, Gate> {
        self.destination_target(target, || Ok(cursor))
    }

    fn destination_target(
        &mut self,
        target: Option<mouse::Point>,
        cursor: impl Fn() -> io::Result<mouse::Point>,
    ) -> Result<Option<u32>, Gate> {
        let foreground = self.observe_target(&cursor, Binding::modifiers_pressed)?;
        let follows_pointer = target.is_none();
        let target = if follows_pointer && !self.keyboard && self.processes.is_some() {
            Some(cursor().map_err(|_| Gate::Stop("Cursor access was lost."))?)
        } else {
            target
        };
        if let Some(point) = target {
            let spatial = spatial_gate(point, self.edge, self.inside, self.stop);
            if !matches!(spatial, Gate::Open) {
                return Err(spatial);
            }
            if !self.keyboard
                && let Some(filter) = &mut self.processes
            {
                let blocked = Gate::Paused(if follows_pointer {
                    PauseReason::PointerFilter
                } else {
                    PauseReason::TargetFilter
                });
                #[cfg(target_os = "macos")]
                {
                    let pid = crate::macos::window_pid(mouse::to_quartz(point));
                    return filter.permits(pid).then_some(Some(pid)).ok_or(blocked);
                }
                #[cfg(not(target_os = "macos"))]
                if !filter.permits_mouse(point) {
                    return Err(blocked);
                }
            }
            #[cfg(windows)]
            if !follows_pointer {
                let gate = mouse::cursor_confinement(point);
                if !matches!(gate, Gate::Open) {
                    return Err(gate);
                }
            }
        }
        Ok(foreground)
    }

    fn observe(
        &mut self,
        cursor: impl FnOnce() -> io::Result<mouse::Point>,
        modifiers_pressed: impl FnOnce(Binding) -> bool,
    ) -> Gate {
        self.observe_target(cursor, modifiers_pressed)
            .err()
            .unwrap_or(Gate::Open)
    }

    fn observe_target(
        &mut self,
        cursor: impl FnOnce() -> io::Result<mouse::Point>,
        modifiers_pressed: impl FnOnce(Binding) -> bool,
    ) -> Result<Option<u32>, Gate> {
        if self.hold.is_some_and(|binding| !binding.pressed()) {
            return Err(Gate::Stop("Activation released."));
        }
        let cursor_gate = if self.watch_cursor() {
            let Ok(point) = cursor() else {
                return Err(Gate::Stop("Cursor access was lost."));
            };
            spatial_gate(point, self.edge, self.inside, self.stop)
        } else {
            Gate::Open
        };
        if matches!(cursor_gate, Gate::Stop(_)) {
            return Err(cursor_gate);
        }
        if let Some(binding) = self.release_modifiers {
            if modifiers_pressed(binding) {
                return Err(Gate::Paused(PauseReason::Modifiers));
            }
            self.release_modifiers = None;
        }
        if !matches!(cursor_gate, Gate::Open) {
            return Err(cursor_gate);
        }
        if self.keyboard {
            #[cfg(target_os = "linux")]
            if crate::wayland::active() {
                return if crate::wayland::focused() {
                    Err(Gate::Paused(PauseReason::KeyboardFocus))
                } else {
                    Ok(None)
                };
            }
            let pid = foreground_pid();
            if pid == 0 || pid == std::process::id() {
                return Err(Gate::Paused(PauseReason::KeyboardFocus));
            }
            if self
                .processes
                .as_mut()
                .is_some_and(|filter| !filter.permits(pid))
            {
                return Err(Gate::Paused(PauseReason::KeyboardFilter));
            }
            return Ok(Some(pid));
        }
        Ok(None)
    }
}

pub fn spatial_gate(
    point: mouse::Point,
    edge: u8,
    inside: Option<Zone>,
    stop: Option<Zone>,
) -> Gate {
    if stop.is_some_and(|zone| zone.contains(point)) {
        return Gate::Stop("Stopped in the stop zone.");
    }
    if edge != 0 && mouse::at_edge(point, edge == 1) {
        return Gate::Stop("Stopped at a screen edge or corner.");
    }
    if inside.is_some_and(|zone| !zone.contains(point)) {
        return Gate::Paused(PauseReason::OutsideZone);
    }
    Gate::Open
}

struct ProcessFilter {
    allow_list: bool,
    names: Vec<String>,
    #[cfg(windows)]
    cached: [Option<CachedProcess>; 2],
}

#[cfg(windows)]
struct CachedProcess {
    pid: u32,
    permitted: bool,
    // Windows cannot recycle this PID while the process handle remains open.
    _process: OwnedHandle,
}

impl ProcessFilter {
    fn new(mode: u8, input: &str) -> Result<Option<Self>, &'static str> {
        if mode == 0 {
            return Ok(None);
        }
        let mut names = Vec::new();
        for name in input
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            #[cfg(windows)]
            let contains_path = name.contains(['/', '\\', ':']);
            #[cfg(not(windows))]
            let contains_path = name.contains('/');
            if contains_path || name.len() > 260 || names.len() == 32 {
                return Err("Enter up to 32 executable names, separated by commas.");
            }
            #[cfg(windows)]
            let name = {
                let mut name = name.to_lowercase();
                if !name.ends_with(".exe") {
                    name.push_str(".exe");
                }
                name
            };
            #[cfg(not(windows))]
            let name = name.to_owned();
            names.push(name);
        }
        if names.is_empty() {
            return Err("Enter an executable name or turn the app filter off.");
        }
        Ok(Some(Self {
            allow_list: mode == 1,
            names,
            #[cfg(windows)]
            cached: [None, None],
        }))
    }

    #[cfg(target_os = "linux")]
    fn permits(&self, pid: u32) -> bool {
        let Ok(executable) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
            return false;
        };
        executable
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                !name.ends_with(" (deleted)")
                    && self.names.iter().any(|entry| entry == name) == self.allow_list
            })
    }

    #[cfg(target_os = "macos")]
    fn permits(&self, pid: u32) -> bool {
        crate::macos::process_name(pid)
            .is_some_and(|name| self.names.contains(&name) == self.allow_list)
    }

    #[cfg(windows)]
    fn permits(&mut self, pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        if let Some(previous) = self.cached.iter().flatten().find(|entry| entry.pid == pid) {
            return previous.permitted;
        }
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return false;
        }
        let process = unsafe { OwnedHandle::from_raw_handle(process) };
        let Some(name) = process_name(&process) else {
            return false;
        };
        let permitted = self.names.contains(&name) == self.allow_list;
        self.cached[1] = self.cached[0].take();
        self.cached[0] = Some(CachedProcess {
            pid,
            permitted,
            _process: process,
        });
        permitted
    }

    #[cfg(target_os = "linux")]
    fn permits_mouse(&self, point: mouse::Point) -> bool {
        crate::x11::with(|desktop| desktop.pointer_pid(point)).is_ok_and(|pid| self.permits(pid))
    }

    #[cfg(windows)]
    fn permits_mouse(&mut self, point: mouse::Point) -> bool {
        let mut info = GUITHREADINFO {
            cbSize: size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetGUIThreadInfo(0, &mut info) } == 0 {
            return false;
        }
        let hit = unsafe {
            WindowFromPoint(POINT {
                x: point.x,
                y: point.y,
            })
        };
        // A click over another thread's window can go there despite mouse capture.
        self.permits(window_pid(hit))
            && (info.hwndCapture.is_null()
                || info.hwndCapture == hit
                || self.permits(window_pid(info.hwndCapture)))
    }
}

#[cfg(target_os = "linux")]
fn foreground_pid() -> u32 {
    crate::x11::with(|desktop| desktop.focus_pid()).unwrap_or_default()
}

#[cfg(target_os = "macos")]
use crate::macos::foreground_pid;

#[cfg(windows)]
fn foreground_pid() -> u32 {
    window_pid(unsafe { GetForegroundWindow() })
}

#[cfg(windows)]
fn window_pid(window: HWND) -> u32 {
    let mut pid = 0;
    if !window.is_null() {
        unsafe { GetWindowThreadProcessId(window, &mut pid) };
    }
    pid
}

#[cfg(windows)]
fn process_name(process: &OwnedHandle) -> Option<String> {
    let mut path = [0; 1_024];
    let mut len = path.len() as u32;
    if unsafe {
        QueryFullProcessImageNameW(process.as_raw_handle(), 0, path.as_mut_ptr(), &mut len)
    } == 0
    {
        return None;
    }
    String::from_utf16_lossy(&path[..len as usize])
        .rsplit('\\')
        .next()
        .map(str::to_lowercase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mouse::Point;

    #[cfg(target_os = "linux")]
    #[test]
    fn x11_key_output_requires_tap() {
        for (press, hold_down) in [("30", false), ("0", true)] {
            let mut settings = Settings {
                keyboard: true,
                press: press.into(),
                hold_down,
                ..Settings::default()
            };
            assert_eq!(
                validated_schedule(&settings, None).err().unwrap().field,
                "press"
            );
            settings.keyboard = false;
            assert!(validated_schedule(&settings, None).is_ok());
        }
    }

    #[test]
    fn modified_hold_shortcuts_are_rejected_for_key_presses_only() {
        let mut settings = Settings {
            keyboard: true,
            hold_activation: true,
            ..Settings::default()
        };
        let modified = Binding::parse("Ctrl+F6");
        let issue = validated_schedule(&settings, modified).err().unwrap();
        assert_eq!(issue.field, "shortcut");
        assert!(prepare(&settings, modified, modified).is_err());
        assert!(validated_schedule(&settings, Binding::parse("F6")).is_ok());

        settings.hold_activation = false;
        assert!(validated_schedule(&settings, modified).is_ok());
        settings.hold_activation = true;
        settings.text_output = true;
        assert!(validated_schedule(&settings, modified).is_ok());
        settings.keyboard = false;
        assert!(validated_schedule(&settings, modified).is_ok());
    }

    #[test]
    fn stop_zone_takes_precedence_over_waiting_for_modifiers() {
        let inside = Point { x: 10, y: 10 };
        let outside = Point { x: 20, y: 20 };
        let settings = Settings {
            stop_zone: Some(Zone {
                from: Point { x: 5, y: 5 },
                to: Point { x: 15, y: 15 },
            }),
            ..Settings::default()
        };
        let binding = Binding::parse("Ctrl+Alt+F6");
        let (_, _, mut guard) = prepare(&settings, binding, binding).unwrap();

        assert!(matches!(
            guard.observe(|| Ok(outside), |_| true),
            Gate::Paused(PauseReason::Modifiers)
        ));
        assert!(matches!(
            guard.observe(|| Ok(inside), |_| true),
            Gate::Stop("Stopped in the stop zone.")
        ));
    }
}
