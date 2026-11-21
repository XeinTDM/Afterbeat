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
        GA_ROOT, GUITHREADINFO, GetAncestor, GetForegroundWindow, GetGUIThreadInfo,
        GetWindowThreadProcessId, WindowFromPoint,
    },
};

pub(crate) const INHERITED_ROUTE_OUTPUT: &str =
    "Select Mouse or Wheel for unedited route steps, or set an action on each step.";

pub fn prepare(
    settings: &Settings,
    bound: Option<Binding>,
    invoked: Option<Binding>,
) -> Result<(Schedule, Injector, Guard), String> {
    if !settings.armed {
        return Err("Enable the master switch in Timing to start a run.".into());
    }
    if settings.route_enabled
        && settings.has_recording()
        && crate::recording::current_origin() == Some(settings.recording_origin)
        && settings
            .route
            .iter()
            .filter_map(crate::targets::Step::point)
            .any(|point| !mouse::on_monitor(point))
    {
        return Err("A recorded point is outside your displays. Edit it before playback.".into());
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
        if settings.keyboard || settings.scroll || settings.has_action_route() {
            1
        } else {
            settings.click_count()
        },
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
        keyboard: settings.keyboard && !settings.has_action_route(),
        #[cfg(windows)]
        wheel: matches!(input, Input::Wheel(..)) && !settings.has_smart_route(),
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
    low_cpu_unavailable_reason(settings, bound).is_none()
}

pub fn low_cpu_unavailable_reason(
    settings: &Settings,
    bound: Option<Binding>,
) -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    if crate::wayland::active() {
        return Some("Wayland delivers each action individually through the desktop portal.");
    }
    if settings.keyboard || settings.scroll {
        Some("Low CPU is available for mouse clicks only.")
    } else if settings.click_count() != 1 {
        Some("Low CPU requires Single click instead of Double or Triple.")
    } else if settings.hold_down || settings.press_duration() != Ok(Duration::ZERO) {
        Some("Low CPU requires Tap instead of Timed or Hold output.")
    } else if settings.random_rate {
        Some("Low CPU is unavailable while Variable rate is on.")
    } else if settings.hold_activation {
        Some("Low CPU requires Toggle instead of While held.")
    } else if settings.route_enabled {
        Some("Low CPU is unavailable while a route is enabled.")
    } else if settings.edge_mode != 0
        || settings.inside_zone.is_some()
        || settings.stop_zone.is_some()
        || settings.process_mode != 0
    {
        Some("Low CPU is unavailable while zones, screen stops, or app filters are enabled.")
    } else if bound.is_some_and(Binding::has_modifiers) {
        Some(
            "Low CPU requires an activation shortcut without Ctrl, Alt, Shift, or other modifiers.",
        )
    } else if !settings
        .interval()
        .is_ok_and(|interval| low_cpu_batch_size(interval) > 1)
    {
        Some("Low CPU requires a fixed rate of 50–1,000 clicks/s.")
    } else {
        None
    }
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
    if settings.has_smart_route() || settings.has_action_route() {
        if settings.has_recording()
            && crate::recording::current_origin() != Some(settings.recording_origin)
        {
            return Err(Issue::new(
                "route",
                "This recording was made on a different desktop backend.",
            ));
        }
        if !cfg!(windows)
            && !(settings.has_recording()
                && matches!(
                    crate::recording::current_origin(),
                    Some(crate::recording::Origin::X11 | crate::recording::Origin::Macos)
                ))
        {
            return Err(Issue::new(
                "route",
                "Control targets and per-step actions require Windows.",
            ));
        }
        let target_timeout = settings
            .target_timeout()
            .map_err(|message| Issue::new("target-timeout", message))?;
        if settings
            .route
            .iter()
            .filter_map(crate::targets::Step::control)
            .any(|control| !control.selector.valid())
        {
            return Err(Issue::new(
                "route",
                "A control has no saved identity. Capture it again.",
            ));
        }
        if settings.has_action_route() && settings.route.iter().all(|step| matches!(step, crate::targets::Step::Action(action) if !crate::workflow::sends_input(&action.action))) {
            return Err(Issue::new("route", "Add an input action to the route."));
        }
        crate::workflow::compile(&settings.route)
            .map_err(|message| Issue::new("route", message))?;
        for (index, step) in settings.route.iter().enumerate() {
            if let crate::targets::Step::Action(action) = step {
                validate_action(action, bound).map_err(|error| Issue::new("route", error))?;
                if matches!(
                    action.action,
                    crate::targets::Action::WaitColor {
                        retry_clicks: 1..,
                        ..
                    }
                ) && !crate::targets::has_retryable_previous_click(&settings.route, index)
                {
                    return Err(Issue::new(
                        "route",
                        "Click retries require an immediately preceding custom click step.",
                    ));
                }
                if matches!(
                    action.action,
                    crate::targets::Action::WaitColor {
                        skip_next_on_timeout: true,
                        ..
                    } | crate::targets::Action::WaitColor {
                        skip_next_on_mismatch: true,
                        ..
                    }
                ) && index + 1 == settings.route.len()
                {
                    return Err(Issue::new(
                        "route",
                        "Add a step after the pixel condition to use Skip next.",
                    ));
                }
                if let crate::targets::Action::WaitColor { stable_ms, .. } = action.action
                    && stable_ms > 0
                    && Duration::from_millis(stable_ms.into()) >= target_timeout
                {
                    return Err(Issue::new(
                        "target-timeout",
                        "Set target timeout longer than pixel stability.",
                    ));
                }
            }
        }
        if settings.has_recording() {
            let events = settings
                .route
                .iter()
                .map(|step| match step {
                    crate::targets::Step::Action(action) => match &action.action {
                        crate::targets::Action::Recorded { event } => Ok(crate::recording::Entry {
                            delay_ms: action.delay_ms.into(),
                            event: event.clone(),
                        }),
                        _ => Err(Issue::new(
                            "route",
                            "Recorded events cannot be mixed with other route steps.",
                        )),
                    },
                    _ => Err(Issue::new(
                        "route",
                        "Recorded events cannot be mixed with other route steps.",
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?;
            crate::recording::validate_for(&events, settings.recording_origin)
                .map_err(|message| Issue::new("route", message))?;
        }
        #[cfg(windows)]
        if settings.has_action_route()
            && settings.hold_activation
            && let Some(binding) = bound
        {
            use crate::targets::{Action, Step};
            if settings.route.iter().any(|step| match step {
                Step::Action(step) => match step.action {
                    Action::Key { .. } | Action::Text { .. } => binding.has_modifiers(),
                    Action::Scroll { .. } => binding.has_modifiers() || binding.mouse(),
                    Action::Recorded {
                        event: crate::recording::Event::Key { .. },
                    } => binding.has_modifiers(),
                    _ => false,
                },
                _ => false,
            }) {
                return Err(Issue::new(
                    "shortcut",
                    "Use an unmodified keyboard shortcut for a Hold route with wheel output, or an unmodified shortcut for key output.",
                ));
            }
        }
        if settings.has_smart_route()
            && !settings.has_action_route()
            && (settings.keyboard
                || settings.hold_down
                || settings.press_duration() != Ok(Duration::ZERO))
        {
            return Err(Issue::new(
                if settings.keyboard {
                    "input-mode"
                } else {
                    "press"
                },
                "Use mouse taps or wheel output for a route with control targets.",
            ));
        }
        if settings.radius() != Ok(0) {
            return Err(Issue::new(
                "radius",
                "Set the radius to zero for a route with control targets or per-step actions.",
            ));
        }
    }
    let mut schedule = settings.schedule()?;
    if settings.route_enabled && settings.has_recording() && !settings.route_once && schedule.limit.is_some()
        && !settings.route.iter().any(|step| matches!(step, crate::targets::Step::Action(action)
            if matches!(action.action, crate::targets::Action::Recorded { event: crate::recording::Event::Button { down: true, .. } | crate::recording::Event::Key { down: true, .. } | crate::recording::Event::Scroll { .. } })))
    {
        return Err(Issue::new("route", "A counted looping recording needs at least one button, key, or scroll event."));
    }
    #[cfg(windows)]
    if settings.scroll
        && !settings.keyboard
        && settings.hold_activation
        && bound.is_some_and(|binding| binding.has_modifiers() || binding.mouse())
        && (!settings.has_action_route() || settings.inherits_route_cadence())
    {
        return Err(Issue::new(
            "shortcut",
            "Use an unmodified keyboard shortcut for Hold with wheel output.",
        ));
    }
    schedule.low_cpu = schedule.low_cpu && low_cpu_available(settings, bound);
    if settings.has_action_route() && settings.inherits_route_cadence() {
        if settings.keyboard {
            return Err(Issue::new("input-mode", INHERITED_ROUTE_OUTPUT));
        }
        if settings.scroll {
            settings
                .scroll_ticks()
                .map_err(|error| Issue::new("input", error))?;
        } else if bound.is_some_and(|binding| binding.matches_button(settings.button)) {
            return Err(Issue::new(
                "input",
                "Use a different mouse button for output and activation.",
            ));
        }
    }
    let input = if settings.has_action_route() {
        Input::Mouse(settings.button)
    } else if settings.keyboard && settings.text_output {
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
    } else if settings.scroll {
        if settings.hold_down || settings.press_duration() != Ok(Duration::ZERO) {
            return Err(Issue::new("press", "Select Tap for wheel output."));
        }
        Input::Wheel(
            settings.scroll_direction,
            settings
                .scroll_ticks()
                .map_err(|error| Issue::new("input", error))?,
        )
    } else {
        Input::Mouse(settings.button)
    };
    if let Input::Mouse(button) = input
        && !settings.has_action_route()
        && bound.is_some_and(|binding| binding.matches_button(button))
    {
        return Err(Issue::new(
            "input",
            "Use a different mouse button for output and activation.",
        ));
    }
    let points = if settings.route_enabled {
        if settings.has_smart_route() || settings.has_action_route() {
            Vec::new()
        } else {
            settings
                .route
                .iter()
                .filter_map(crate::targets::Step::point)
                .collect()
        }
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
            .map_err(|message| Issue::new("radius", message))?
    } else {
        0
    };
    let press = if settings.has_action_route() {
        Press::Tap
    } else if settings.hold_down {
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

pub fn validate_action(
    action: &crate::targets::ActionStep,
    bound: Option<Binding>,
) -> Result<(), &'static str> {
    action.validate()?;
    if matches!(
        action.action,
        crate::targets::Action::WaitImage {
            output: crate::targets::ImageOutput::Click,
            ..
        }
    ) && bound.is_some_and(|binding| binding.matches_button(crate::mouse::Button::Left))
    {
        return Err("Use a different activation shortcut for image clicks.");
    }
    if let crate::targets::Action::Click { button, .. } = action.action
        && bound.is_some_and(|binding| binding.matches_button(button))
    {
        return Err("Use a different mouse button for output and activation.");
    }
    if let crate::targets::Action::Key { key } = &action.action {
        let key = keys::parse(key).ok_or("Enter one key such as Enter, Space, A, or F5.")?;
        if key.is_safety_key() || bound.is_some_and(|binding| binding.matches_key(key)) {
            return Err("Choose a key different from the activation, stop, and capture shortcuts.");
        }
        if action.target.as_ref().is_some_and(|target| matches!(target, crate::targets::Destination::Control(control) if control.selector.control.is_none())) {
            return Err("A targeted key step needs a control that exposes a keyboard focus identity.");
        }
    }
    #[cfg(windows)]
    if let crate::targets::Action::Recorded { event } = &action.action {
        match event {
            crate::recording::Event::Key { code: 0, .. } => {
                return Err("Enter a Windows key code from 1 to 255.");
            }
            crate::recording::Event::Key { code, .. }
                if *code == u32::from(keys::F7)
                    || *code == u32::from(keys::F8)
                    || bound.is_some_and(|binding| u32::from(binding.vk) == *code) =>
            {
                return Err(
                    "A recorded key conflicts with an activation, stop, or capture shortcut.",
                );
            }
            crate::recording::Event::Button { button, .. }
                if bound.is_some_and(|binding| binding.matches_button(*button)) =>
            {
                return Err("A recorded mouse button conflicts with the activation shortcut.");
            }
            _ => {}
        }
    }
    #[cfg(target_os = "linux")]
    if let crate::targets::Action::Recorded {
        event: crate::recording::Event::Button { button, .. },
    } = &action.action
        && bound.is_some_and(|binding| binding.matches_button(*button))
    {
        return Err("A recorded mouse button conflicts with the activation shortcut.");
    }
    #[cfg(target_os = "macos")]
    if let crate::targets::Action::Recorded { event } = &action.action {
        match event {
            crate::recording::Event::Key { code, .. }
                if *code == u32::from(keys::F7)
                    || *code == u32::from(keys::F8)
                    || bound.is_some_and(|binding| binding.uses_code(*code as u16)) =>
            {
                return Err(
                    "A recorded key conflicts with an activation, stop, or capture shortcut.",
                );
            }
            crate::recording::Event::Button { button, .. }
                if bound.is_some_and(|binding| binding.matches_button(*button)) =>
            {
                return Err("A recorded mouse button conflicts with the activation shortcut.");
            }
            _ => {}
        }
    }
    Ok(())
}

pub struct Guard {
    hold: Option<Binding>,
    release_modifiers: Option<Binding>,
    edge: u8,
    inside: Option<Zone>,
    stop: Option<Zone>,
    keyboard: bool,
    #[cfg(windows)]
    wheel: bool,
    processes: Option<ProcessFilter>,
    next_check: Instant,
    cached: Gate,
}

impl Guard {
    pub fn check_route_key(&mut self, recipient_pid: u32) -> Gate {
        self.observe_safety(mouse::cursor, Binding::modifiers_pressed)
            .and_then(|()| self.check_keyboard_recipient(recipient_pid))
            .err()
            .unwrap_or(Gate::Open)
    }

    pub fn enabled(&self) -> bool {
        self.hold.is_some()
            || self.release_modifiers.is_some()
            || self.watch_cursor()
            || self.keyboard
            || self.processes.is_some()
            || self.wheel_guard()
    }

    fn wheel_guard(&self) -> bool {
        #[cfg(windows)]
        {
            self.wheel
        }
        #[cfg(not(windows))]
        {
            false
        }
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
        let target = if follows_pointer
            && !self.keyboard
            && (self.processes.is_some() || self.wheel_guard())
        {
            Some(cursor().map_err(|_| Gate::Stop("Cursor access was lost."))?)
        } else {
            target
        };
        if let Some(point) = target {
            #[cfg(windows)]
            if self.wheel {
                let hwnd = unsafe {
                    WindowFromPoint(POINT {
                        x: point.x,
                        y: point.y,
                    })
                };
                let foreground = unsafe { GetForegroundWindow() };
                if foreground.is_null()
                    || hwnd.is_null()
                    || unsafe { GetAncestor(hwnd, GA_ROOT) } != foreground
                    || window_pid(foreground) == std::process::id()
                {
                    return Err(Gate::Paused(PauseReason::WheelFocus));
                }
            }
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
        self.observe_safety(cursor, modifiers_pressed)?;
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
            self.check_keyboard_recipient(pid)?;
            return Ok(Some(pid));
        }
        Ok(None)
    }

    fn check_keyboard_recipient(&mut self, pid: u32) -> Result<(), Gate> {
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
        Ok(())
    }

    fn observe_safety(
        &mut self,
        cursor: impl FnOnce() -> io::Result<mouse::Point>,
        modifiers_pressed: impl FnOnce(Binding) -> bool,
    ) -> Result<(), Gate> {
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
        Ok(())
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
pub(crate) fn window_pid(window: HWND) -> u32 {
    let mut pid = 0;
    if !window.is_null() {
        unsafe { GetWindowThreadProcessId(window, &mut pid) };
    }
    pid
}

#[cfg(windows)]
fn process_name(process: &OwnedHandle) -> Option<String> {
    process_image(process)
        .as_deref()
        .and_then(|path| path.rsplit('\\').next())
        .map(str::to_lowercase)
}

#[cfg(windows)]
pub(crate) fn process_image(process: &OwnedHandle) -> Option<String> {
    let mut path = [0; 1_024];
    let mut len = path.len() as u32;
    if unsafe {
        QueryFullProcessImageNameW(process.as_raw_handle(), 0, path.as_mut_ptr(), &mut len)
    } == 0
    {
        return None;
    }
    String::from_utf16(&path[..len as usize]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mouse::Point;

    #[test]
    fn low_cpu_explains_the_current_blocker_and_rate_boundaries() {
        let mut settings = Settings {
            interval: "10".into(),
            ..Settings::default()
        };
        let plain = Binding::parse("F6");
        for interval in ["1", "10", "20"] {
            settings.interval = interval.into();
            assert!(low_cpu_available(&settings, plain));
            assert_eq!(low_cpu_unavailable_reason(&settings, plain), None);
        }
        for interval in ["0", "0.5", "21", "100", "invalid"] {
            settings.interval = interval.into();
            assert!(!low_cpu_available(&settings, plain));
            assert!(
                low_cpu_unavailable_reason(&settings, plain)
                    .unwrap()
                    .contains("50–1,000")
            );
        }
        settings.interval = "10".into();
        settings.random_rate = true;
        assert!(
            low_cpu_unavailable_reason(&settings, plain)
                .unwrap()
                .contains("Variable rate")
        );
        settings.random_rate = false;
        settings.keyboard = true;
        assert!(
            low_cpu_unavailable_reason(&settings, plain)
                .unwrap()
                .contains("mouse clicks only")
        );
        settings.keyboard = false;
        settings.route_enabled = true;
        assert!(
            low_cpu_unavailable_reason(&settings, plain)
                .unwrap()
                .contains("route")
        );
        settings.route_enabled = false;
        assert!(
            low_cpu_unavailable_reason(&settings, Binding::parse("Ctrl+F6"))
                .unwrap()
                .contains("modifiers")
        );
    }

    #[cfg(windows)]
    #[test]
    fn recording_requires_balanced_edges_and_rejects_shortcuts_and_mixed_steps() {
        use crate::{
            recording::Event,
            targets::{Action, ActionStep, Step},
        };
        let recorded = |event| {
            Step::Action(Box::new(ActionStep {
                action: Action::Recorded { event },
                target: None,
                delay_ms: 15,
            }))
        };
        let point = Point { x: 10, y: 10 };
        let mut settings = Settings {
            route_enabled: true,
            route_once: true,
            route: vec![
                recorded(Event::Button {
                    button: crate::mouse::Button::Left,
                    down: true,
                    point,
                }),
                recorded(Event::Move(Point { x: 20, y: 20 })),
                recorded(Event::Button {
                    button: crate::mouse::Button::Left,
                    down: false,
                    point: Point { x: 20, y: 20 },
                }),
            ],
            ..Settings::default()
        };
        assert!(validated_schedule(&settings, Binding::parse("F6")).is_ok());
        settings.recording_origin = crate::recording::Origin::Macos;
        assert!(validated_schedule(&settings, Binding::parse("F6")).is_err());
        settings.recording_origin = crate::recording::Origin::Windows;
        let encoded = serde_json::to_string(&settings).unwrap();
        let decoded: Settings = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.route, settings.route);
        settings.route.pop();
        assert!(validated_schedule(&settings, Binding::parse("F6")).is_err());
        settings.route.push(recorded(Event::Button {
            button: crate::mouse::Button::Left,
            down: false,
            point,
        }));
        settings.route.push(Step::Point(point));
        assert!(validated_schedule(&settings, Binding::parse("F6")).is_err());
        settings.route = vec![
            recorded(Event::Key {
                code: u32::from(crate::keys::F7),
                extended: false,
                down: true,
            }),
            recorded(Event::Key {
                code: u32::from(crate::keys::F7),
                extended: false,
                down: false,
            }),
        ];
        assert!(validated_schedule(&settings, Binding::parse("F6")).is_err());

        settings.route = vec![recorded(Event::Move(point))];
        settings.limited = true;
        settings.count = "1".into();
        settings.route_once = false;
        assert!(validated_schedule(&settings, Binding::parse("F6")).is_err());
        settings.route_once = true;
        assert!(validated_schedule(&settings, Binding::parse("F6")).is_ok());
        settings.limited = false;
        settings.hold_activation = true;
        settings.route = vec![
            recorded(Event::Key {
                code: 65,
                extended: false,
                down: true,
            }),
            recorded(Event::Key {
                code: 65,
                extended: false,
                down: false,
            }),
        ];
        assert!(validated_schedule(&settings, Binding::parse("Ctrl+F6")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn pixel_retries_require_a_valid_immediately_preceding_custom_click() {
        use crate::targets::{Action, ActionStep, Destination, Step};
        let point = Point { x: 10, y: 10 };
        let click = Step::Action(Box::new(ActionStep {
            action: Action::Click {
                button: crate::mouse::Button::Left,
                count: 1,
            },
            target: Some(Destination::Point(point)),
            delay_ms: 0,
        }));
        let condition = Step::Action(Box::new(ActionStep {
            action: Action::WaitColor {
                point,
                rgb: 0,
                tolerance: 0,
                different: false,
                stable_ms: 0,
                skip_next_on_timeout: false,
                check_once: true,
                skip_next_on_mismatch: false,
                retry_clicks: 2,
            },
            target: None,
            delay_ms: 0,
        }));
        let mut settings = Settings {
            route_enabled: true,
            route: vec![click.clone(), condition.clone()],
            ..Settings::default()
        };
        assert!(validated_schedule(&settings, None).is_ok());
        settings.route = vec![condition.clone(), click.clone()];
        assert!(
            validated_schedule(&settings, None)
                .err()
                .unwrap()
                .message
                .contains("immediately preceding custom click")
        );
        settings.route = vec![Step::Point(point), condition];
        assert!(
            validated_schedule(&settings, None)
                .err()
                .unwrap()
                .message
                .contains("immediately preceding custom click")
        );
    }

    #[cfg(windows)]
    #[test]
    fn pixel_condition_does_not_count_as_an_input_action() {
        use crate::targets::{Action, ActionStep, Step};
        let mut settings = Settings {
            route_enabled: true,
            route: vec![Step::Action(Box::new(ActionStep {
                action: Action::WaitColor {
                    point: Point { x: 0, y: 0 },
                    rgb: 0x123456,
                    tolerance: 0,
                    different: false,
                    stable_ms: 0,
                    skip_next_on_timeout: false,
                    check_once: false,
                    skip_next_on_mismatch: false,
                    retry_clicks: 0,
                },
                target: None,
                delay_ms: 0,
            }))],
            ..Settings::default()
        };
        assert_eq!(
            validated_schedule(&settings, None).err().unwrap().message,
            "Add an input action to the route."
        );
        settings.route.push(Step::Action(Box::new(ActionStep {
            action: Action::Key {
                key: "Enter".into(),
            },
            target: None,
            delay_ms: 0,
        })));
        assert!(validated_schedule(&settings, None).is_ok());
        if let Step::Action(step) = &mut settings.route[0]
            && let Action::WaitColor { stable_ms, .. } = &mut step.action
        {
            *stable_ms = 10_000;
        }
        assert_eq!(
            validated_schedule(&settings, None).err().unwrap().field,
            "target-timeout"
        );
        settings.target_timeout = "11".into();
        assert!(validated_schedule(&settings, None).is_ok());
        if let Step::Action(step) = &mut settings.route[0]
            && let Action::WaitColor {
                skip_next_on_timeout,
                ..
            } = &mut step.action
        {
            *skip_next_on_timeout = true;
        }
        assert!(validated_schedule(&settings, None).is_ok());
        settings.route.swap(0, 1);
        assert_eq!(
            validated_schedule(&settings, None).err().unwrap().message,
            "Add a step after the pixel condition to use Skip next."
        );
        if let Step::Action(step) = &mut settings.route[1]
            && let Action::WaitColor {
                stable_ms,
                check_once,
                skip_next_on_timeout,
                skip_next_on_mismatch,
                ..
            } = &mut step.action
        {
            *stable_ms = 0;
            *check_once = true;
            *skip_next_on_timeout = false;
            *skip_next_on_mismatch = true;
        }
        assert_eq!(
            validated_schedule(&settings, None).err().unwrap().message,
            "Add a step after the pixel condition to use Skip next."
        );
        settings.route.swap(0, 1);
        assert!(validated_schedule(&settings, None).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn mixed_routes_require_mouse_or_wheel_for_inherited_steps() {
        use crate::targets::{Action, ActionStep, Step};

        let mut settings = Settings {
            keyboard: true,
            route_enabled: true,
            route: vec![Step::Point(Point { x: 10, y: 10 })],
            ..Settings::default()
        };
        assert!(validated_schedule(&settings, None).is_ok());
        settings.route.push(Step::Action(Box::new(ActionStep {
            action: Action::Wait,
            target: None,
            delay_ms: 50,
        })));
        assert_eq!(
            validated_schedule(&settings, None).err().unwrap().message,
            INHERITED_ROUTE_OUTPUT
        );
        assert!(prepare(&settings, None, None).is_err());

        settings.keyboard = false;
        assert!(validated_schedule(&settings, None).is_ok());
        settings.scroll = true;
        assert!(validated_schedule(&settings, None).is_ok());

        settings.keyboard = true;
        settings.scroll = false;
        settings.route[0] = Step::Action(Box::new(ActionStep {
            action: Action::Key {
                key: "Enter".into(),
            },
            target: None,
            delay_ms: 0,
        }));
        assert!(validated_schedule(&settings, None).is_ok());
    }

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
