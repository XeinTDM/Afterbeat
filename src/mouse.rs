use crate::{
    engine::{BURST_SIZE, GUARD_POLL_INTERVAL, Gate, Outcome, PauseReason, Random, RunControl},
    keys::{self, Key},
};
use serde::{Deserialize, Serialize};
use std::{
    io,
    mem::size_of,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{LPARAM, POINT, RECT},
    Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONULL, MONITORINFO,
        MonitorFromPoint,
    },
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
            SendInput, VK_LSHIFT, VK_SHIFT,
        },
        WindowsAndMessaging::{
            GetClipCursor, GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
            SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
        },
    },
};

const LAYOUT_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const LAYOUT_CHANGED: &str = "The display layout changed. Capture your points again.";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

pub fn cursor() -> io::Result<Point> {
    let mut position = POINT::default();
    if unsafe { GetCursorPos(&mut position) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Point {
        x: position.x,
        y: position.y,
    })
}

pub fn cursor_confinement(point: Point) -> Gate {
    let mut bounds = RECT::default();
    if unsafe { GetClipCursor(&mut bounds) } == 0 {
        return Gate::Stop("Windows cursor access was lost.");
    }
    confinement_gate(point, bounds)
}

fn confinement_gate(point: Point, bounds: RECT) -> Gate {
    if (bounds.left..bounds.right).contains(&point.x)
        && (bounds.top..bounds.bottom).contains(&point.y)
    {
        Gate::Open
    } else {
        Gate::Paused(PauseReason::CursorConfined)
    }
}

#[derive(Clone, Copy, Default, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Button {
    #[default]
    Left,
    Right,
    Middle,
}

impl Button {
    fn flags(self) -> (u32, u32) {
        match self {
            Self::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
            Self::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            Self::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
        }
    }
}

#[derive(PartialEq, Eq)]
struct Desktop {
    origin: Point,
    width: i32,
    height: i32,
    displays: Vec<Zone>,
}

impl Desktop {
    fn current() -> Self {
        let mut displays = display_areas();
        // Compare monitor rectangles regardless of enumeration order or virtual bounds.
        displays.sort_unstable_by_key(|zone| (zone.from.x, zone.from.y, zone.to.x, zone.to.y));
        unsafe {
            Self {
                origin: Point {
                    x: GetSystemMetrics(SM_XVIRTUALSCREEN),
                    y: GetSystemMetrics(SM_YVIRTUALSCREEN),
                },
                width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
                height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
                displays,
            }
        }
    }

    fn absolute(&self, point: Point) -> Option<Point> {
        // Aim at the pixel center on the entire virtual desktop, including negative origins.
        fn scale(value: i32, origin: i32, extent: i32) -> Option<i32> {
            let offset = i64::from(value) - i64::from(origin);
            (0..i64::from(extent))
                .contains(&offset)
                .then(|| ((2 * offset + 1) * 32_768 / i64::from(extent)) as i32)
        }
        Some(Point {
            x: scale(point.x, self.origin.x, self.width)?,
            y: scale(point.y, self.origin.y, self.height)?,
        })
    }
}

pub fn on_monitor(point: Point) -> bool {
    !unsafe {
        MonitorFromPoint(
            POINT {
                x: point.x,
                y: point.y,
            },
            MONITOR_DEFAULTTONULL,
        )
    }
    .is_null()
}

pub fn display_areas() -> Vec<Zone> {
    unsafe extern "system" fn collect(_: HMONITOR, _: HDC, rect: *mut RECT, data: LPARAM) -> i32 {
        let rect = unsafe { &*rect };
        let displays = unsafe { &mut *(data as *mut Vec<Zone>) };
        displays.push(Zone {
            from: Point {
                x: rect.left,
                y: rect.top,
            },
            to: Point {
                x: rect.right - 1,
                y: rect.bottom - 1,
            },
        });
        1
    }
    let mut displays = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(collect),
            (&mut displays as *mut Vec<Zone>) as LPARAM,
        )
    };
    displays
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Zone {
    pub from: Point,
    pub to: Point,
}

impl Zone {
    pub fn contains(&self, point: Point) -> bool {
        (self.from.x.min(self.to.x)..=self.from.x.max(self.to.x)).contains(&point.x)
            && (self.from.y.min(self.to.y)..=self.from.y.max(self.to.y)).contains(&point.y)
    }
}

pub fn at_edge(point: Point, corners_only: bool) -> bool {
    let monitor = unsafe {
        MonitorFromPoint(
            POINT {
                x: point.x,
                y: point.y,
            },
            MONITOR_DEFAULTTONULL,
        )
    };
    if monitor.is_null() {
        return false;
    }
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return false;
    }
    let rect = info.rcMonitor;
    let horizontal = point.x - rect.left < 4 || rect.right - point.x <= 4;
    let vertical = point.y - rect.top < 4 || rect.bottom - point.y <= 4;
    if corners_only {
        horizontal && vertical
    } else {
        horizontal || vertical
    }
}

#[derive(Clone, Copy)]
pub enum Input {
    Mouse(Button),
    Key(Key),
    Text(char),
}

#[derive(Clone, Copy)]
pub enum Press {
    Tap,
    Timed(Duration),
    Held,
}

struct Stroke {
    down: [INPUT; 2],
    up: [INPUT; 2],
    len: usize,
}

impl Stroke {
    fn new(input: Input) -> Self {
        let mut stroke = Self {
            down: [INPUT::default(); 2],
            up: [INPUT::default(); 2],
            len: 1,
        };
        let origin = Point { x: 0, y: 0 };
        match input {
            Input::Mouse(button) => {
                let (down, up) = button.flags();
                stroke.down[0] = mouse_input(down, origin);
                stroke.up[0] = mouse_input(up, origin);
            }
            Input::Key(key) => {
                let flags = if key.extended {
                    KEYEVENTF_EXTENDEDKEY
                } else {
                    0
                };
                let down = keyboard_input(key.vk, flags);
                let up = keyboard_input(key.vk, flags | KEYEVENTF_KEYUP);
                if key.shift && !keys::down(VK_SHIFT) {
                    stroke.down = [keyboard_input(VK_LSHIFT, 0), down];
                    stroke.up = [up, keyboard_input(VK_LSHIFT, KEYEVENTF_KEYUP)];
                    stroke.len = 2;
                } else {
                    stroke.down[0] = down;
                    stroke.up[0] = up;
                }
            }
            Input::Text(character) => {
                let mut units = [0; 2];
                let units = character.encode_utf16(&mut units);
                stroke.len = units.len();
                for (index, &unit) in units.iter().enumerate() {
                    stroke.down[index] = keyboard_event(0, unit, KEYEVENTF_UNICODE);
                    stroke.up[index] = keyboard_event(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP);
                }
            }
        }
        stroke
    }

    fn down(&self) -> &[INPUT] {
        &self.down[..self.len]
    }
    fn up(&self) -> &[INPUT] {
        &self.up[..self.len]
    }
}

struct Targets {
    points: Vec<Point>,
    moves: Vec<INPUT>,
    desktop: Desktop,
    next_check: Instant,
    index: usize,
    radius: i32,
    random: Random,
    pending: Option<(Point, INPUT)>,
}

impl Targets {
    fn new(points: Vec<Point>, radius: i32) -> Result<Option<Self>, String> {
        if points.is_empty() {
            return Ok(None);
        }
        let desktop = Desktop::current();
        if desktop.displays.is_empty() {
            return Err("Windows did not report any displays for the saved points.".into());
        }
        let mut moves = Vec::with_capacity(points.len());
        for &point in &points {
            let event = Self::move_to(&desktop, point)
                .filter(|_| on_monitor(point))
                .ok_or("A saved point is outside your displays. Capture it again.")?;
            moves.push(event);
        }
        Ok(Some(Self {
            points,
            moves,
            desktop,
            next_check: Instant::now(),
            index: 0,
            radius,
            random: Random::new(),
            pending: None,
        }))
    }

    fn validate_layout(&mut self) -> Result<(), &'static str> {
        let now = Instant::now();
        if now >= self.next_check {
            if self.desktop != Desktop::current() {
                return Err(LAYOUT_CHANGED);
            }
            self.next_check = now + LAYOUT_CHECK_INTERVAL;
        }
        Ok(())
    }

    fn move_to(desktop: &Desktop, point: Point) -> Option<INPUT> {
        Some(mouse_input(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            desktop.absolute(point)?,
        ))
    }

    fn take_destination(&mut self) -> (Point, INPUT) {
        let target = self.destination();
        self.pending = None;
        self.index += 1;
        if self.index == self.points.len() {
            self.index = 0;
        }
        target
    }

    fn destination(&mut self) -> (Point, INPUT) {
        if let Some(target) = self.pending {
            return target;
        }
        let target = self.sample();
        self.pending = Some(target);
        target
    }

    fn sample(&mut self) -> (Point, INPUT) {
        let index = self.index;
        let point = self.points[index];
        if self.radius == 0 {
            return (point, self.moves[index]);
        }
        let angle = self.random.unit() * std::f64::consts::TAU;
        let distance = self.random.unit().sqrt() * f64::from(self.radius);
        let shifted = Point {
            x: point
                .x
                .saturating_add((angle.cos() * distance).round() as i32),
            y: point
                .y
                .saturating_add((angle.sin() * distance).round() as i32),
        };
        Self::move_to(&self.desktop, shifted)
            .filter(|_| on_monitor(shifted))
            .map_or((point, self.moves[index]), |movement| (shifted, movement))
    }
}

pub struct Injector {
    packet: [INPUT; BURST_SIZE as usize * 9],
    used: usize,
    stroke: Stroke,
    input: Input,
    press: Press,
    repeats: usize,
    targets: Option<Targets>,
    dynamic: bool,
}

impl Injector {
    pub fn new(
        input: Input,
        double: bool,
        points: Vec<Point>,
        radius: i32,
        press: Press,
    ) -> Result<Self, String> {
        Ok(Self::with_targets(
            input,
            double,
            Targets::new(points, radius)?,
            press,
        ))
    }

    fn with_targets(input: Input, double: bool, targets: Option<Targets>, press: Press) -> Self {
        let stroke = Stroke::new(input);
        let dynamic = targets
            .as_ref()
            .is_some_and(|t| t.points.len() > 1 || t.radius > 0);
        let mut injector = Self {
            packet: [INPUT::default(); BURST_SIZE as usize * 9],
            used: 0,
            stroke,
            input,
            press,
            repeats: if double { 2 } else { 1 },
            targets,
            dynamic,
        };
        injector.rebuild_packet();
        injector
    }

    fn rebuild_packet(&mut self) {
        let stroke = &self.stroke;
        let mut action = [INPUT::default(); 9];
        let mut used = 0;
        if let Some(targets) = &self.targets {
            // Reserve the first event for movement. Dynamic routes fill it on each action.
            if !self.dynamic {
                action[0] = targets.moves[0];
            }
            used += 1;
        }
        for _ in 0..self.repeats {
            if matches!(self.input, Input::Text(_)) {
                for (&down, &up) in stroke.down().iter().zip(stroke.up()) {
                    action[used..used + 2].copy_from_slice(&[down, up]);
                    used += 2;
                }
            } else {
                action[used..used + stroke.len].copy_from_slice(stroke.down());
                used += stroke.len;
                action[used..used + stroke.len].copy_from_slice(stroke.up());
                used += stroke.len;
            }
        }
        for events in self.packet[..used * BURST_SIZE as usize].chunks_exact_mut(used) {
            events.copy_from_slice(&action[..used]);
        }
        self.used = used;
    }

    pub fn batchable(&self) -> bool {
        matches!(self.press, Press::Tap)
    }

    pub fn has_targets(&self) -> bool {
        self.targets.is_some()
    }

    pub fn has_changing_targets(&self) -> bool {
        self.dynamic
    }

    pub fn destination(&mut self) -> Result<Option<Point>, String> {
        self.targets
            .as_mut()
            .map(|targets| {
                targets.validate_layout()?;
                Ok::<_, String>(targets.destination().0)
            })
            .transpose()
    }

    fn prepare_packet(&mut self, actions: u32) -> Result<usize, String> {
        let len = actions as usize * self.used;
        if let Some(targets) = &mut self.targets {
            targets.validate_layout()?;
            if self.dynamic {
                // Keep each point's move before its press, even within a single submission.
                for action in self.packet[..len].chunks_exact_mut(self.used) {
                    action[0] = targets.take_destination().1;
                }
            }
        }
        Ok(len)
    }

    pub fn send(
        &mut self,
        actions: u32,
        control: &mut RunControl<'_>,
        check_destination: Option<&mut dyn FnMut(Option<Point>) -> Gate>,
    ) -> Result<Option<Outcome>, String> {
        if matches!(self.input, Input::Key(key) if key.shift) {
            let stroke = Stroke::new(self.input);
            if stroke.len != self.stroke.len {
                self.stroke = stroke;
                self.rebuild_packet();
            }
        }
        match self.press {
            Press::Tap => {
                let len = self.prepare_packet(actions)?;
                if let Some(outcome) = control.begin_dispatch() {
                    return Ok(Some(outcome));
                }
                send(&self.packet[..len], self.stroke.up())?;
                Ok(None)
            }
            Press::Timed(duration) => self.send_timed(duration, control, check_destination, send),
            Press::Held => {
                let duration = Duration::from_secs(365 * 86_400);
                self.send_timed(duration, control, check_destination, send)
            }
        }
    }

    fn send_timed(
        &mut self,
        duration: Duration,
        control: &mut RunControl<'_>,
        mut check_destination: Option<&mut dyn FnMut(Option<Point>) -> Gate>,
        mut submit: impl FnMut(&[INPUT], &[INPUT]) -> Result<(), String>,
    ) -> Result<Option<Outcome>, String> {
        let mut down = [INPUT::default(); 3];
        let target = if let Some(targets) = &mut self.targets {
            targets.validate_layout()?;
            let (point, movement) = targets.take_destination();
            down[0] = movement;
            Some(point)
        } else {
            None
        };
        let offset = usize::from(target.is_some());
        down[offset..offset + self.stroke.len].copy_from_slice(self.stroke.down());
        if let Some(outcome) = control.begin_dispatch() {
            return Ok(Some(outcome));
        }
        let poll_interval = if check_destination.is_some() {
            Some(GUARD_POLL_INTERVAL)
        } else {
            self.targets.as_ref().map(|_| LAYOUT_CHECK_INTERVAL)
        };
        let mut check_held_target = || {
            if let Some(targets) = &mut self.targets
                && let Err(reason) = targets.validate_layout()
            {
                return Gate::Stop(reason);
            }
            check_destination
                .as_mut()
                .map_or(Gate::Open, |check| check(target))
        };
        for index in 0..self.repeats {
            if index != 0 && (!control.can_press()? || !control.allow_press(check_held_target())) {
                break;
            }
            submit(&down[..offset + self.stroke.len], self.stroke.up())?;
            let held = control.hold(
                duration,
                poll_interval
                    .map(|interval| (&mut check_held_target as &mut dyn FnMut() -> Gate, interval)),
            );
            let released = submit(self.stroke.up(), self.stroke.up());
            let full_press = held?;
            released?;
            if !full_press {
                break;
            }
        }
        Ok(None)
    }
}

fn send(events: &[INPUT], release: &[INPUT]) -> Result<(), String> {
    let inject = |events: &[INPUT]| unsafe {
        SendInput(
            events.len() as u32,
            events.as_ptr(),
            size_of::<INPUT>() as i32,
        ) as usize
    };
    let sent = inject(events);
    if sent == events.len() {
        return Ok(());
    }
    if sent > 0 || std::ptr::eq(events, release) {
        inject(release);
    }
    Err("Windows blocked input. Check that the target runs at the same privilege level.".into())
}

fn keyboard_input(vk: u16, flags: u32) -> INPUT {
    keyboard_event(vk, 0, flags)
}

fn keyboard_event(vk: u16, scan: u16, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                dwExtraInfo: crate::input_filter::INPUT_TAG,
                ..Default::default()
            },
        },
    }
}

fn mouse_input(flags: u32, position: Point) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: position.x,
                dy: position.y,
                dwFlags: flags,
                dwExtraInfo: crate::input_filter::INPUT_TAG,
                ..Default::default()
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confinement_accepts_only_points_inside_the_windows_rectangle() {
        let bounds = RECT {
            left: -20,
            top: 10,
            right: 50,
            bottom: 60,
        };
        for point in [Point { x: -20, y: 10 }, Point { x: 49, y: 59 }] {
            assert!(matches!(confinement_gate(point, bounds), Gate::Open));
        }
        for point in [
            Point { x: -21, y: 10 },
            Point { x: 50, y: 59 },
            Point { x: 49, y: 60 },
        ] {
            assert!(matches!(
                confinement_gate(point, bounds),
                Gate::Paused(PauseReason::CursorConfined)
            ));
        }
    }

    #[test]
    fn route_batches_keep_move_and_click_order_across_the_batch_boundary() {
        let desktop = Desktop {
            origin: Point { x: 0, y: 0 },
            width: 1000,
            height: 1000,
            displays: vec![],
        };
        let points = vec![
            Point { x: 20, y: 30 },
            Point { x: 40, y: 50 },
            Point { x: 60, y: 70 },
        ];
        let moves: Vec<_> = points
            .iter()
            .map(|&point| Targets::move_to(&desktop, point).unwrap())
            .collect();
        let target = Targets {
            points,
            moves: moves.clone(),
            desktop,
            next_check: Instant::now() + Duration::from_secs(60),
            index: 0,
            radius: 0,
            random: Random::new(),
            pending: None,
        };
        let mut injector =
            Injector::with_targets(Input::Mouse(Button::Left), false, Some(target), Press::Tap);
        assert!(injector.batchable());
        let mut index = 0;
        for count in [BURST_SIZE, 3] {
            let len = injector.prepare_packet(count).unwrap();
            assert_eq!(len, count as usize * 3);
            for action in injector.packet[..len].as_chunks::<3>().0 {
                let expected = unsafe { moves[index % moves.len()].Anonymous.mi };
                let motion = unsafe { action[0].Anonymous.mi };
                assert_eq!(
                    (motion.dx, motion.dy, motion.dwFlags),
                    (expected.dx, expected.dy, expected.dwFlags)
                );
                assert_eq!(
                    unsafe { action[1].Anonymous.mi.dwFlags },
                    MOUSEEVENTF_LEFTDOWN
                );
                assert_eq!(
                    unsafe { action[2].Anonymous.mi.dwFlags },
                    MOUSEEVENTF_LEFTUP
                );
                index += 1;
            }
        }
    }

    #[test]
    fn timed_double_checks_and_moves_to_the_same_route_point_twice() {
        use crate::engine::{Checks, Progress, Schedule, StopSignal, run_controlled};

        let desktop = Desktop {
            origin: Point { x: 0, y: 0 },
            width: 1000,
            height: 1000,
            displays: vec![],
        };
        let first = Point { x: 20, y: 30 };
        let next = Point { x: 40, y: 50 };
        let movement = Targets::move_to(&desktop, first).unwrap();
        let targets = Targets {
            points: vec![first, next],
            moves: vec![movement, Targets::move_to(&desktop, next).unwrap()],
            desktop,
            next_check: Instant::now() + Duration::from_secs(60),
            index: 0,
            radius: 0,
            random: Random::new(),
            pending: None,
        };
        let duration = Duration::from_millis(1);
        let mut injector = Injector::with_targets(
            Input::Mouse(Button::Left),
            true,
            Some(targets),
            Press::Timed(duration),
        );
        let mut checks = Vec::new();
        let mut packets = Vec::new();
        let result = run_controlled(
            Schedule {
                limit: Some(1),
                ..Schedule::default()
            },
            &StopSignal::new().unwrap(),
            &Progress::default(),
            false,
            Checks::default(),
            None,
            |_, control| {
                injector.send_timed(
                    duration,
                    control,
                    Some(&mut |point| {
                        checks.push(point);
                        Gate::Open
                    }),
                    |events, _| {
                        packets.push(events.to_vec());
                        Ok(())
                    },
                )
            },
        );
        assert_eq!(result, Ok(Outcome::Completed));
        assert!(checks.len() >= 3);
        assert!(checks.iter().all(|point| *point == Some(first)));
        assert_eq!(injector.destination().unwrap(), Some(next));
        assert_eq!(packets.len(), 4);
        let expected_move = unsafe { movement.Anonymous.mi };
        for [press, release] in packets.as_chunks::<2>().0 {
            let [motion, down] = press.as_slice() else {
                panic!("each press should include a move and a button-down event");
            };
            let motion = unsafe { motion.Anonymous.mi };
            assert_eq!(
                (motion.dx, motion.dy, motion.dwFlags),
                (expected_move.dx, expected_move.dy, expected_move.dwFlags)
            );
            assert_eq!(unsafe { down.Anonymous.mi.dwFlags }, MOUSEEVENTF_LEFTDOWN);
            assert_eq!(release.len(), 1);
            assert_eq!(
                unsafe { release[0].Anonymous.mi.dwFlags },
                MOUSEEVENTF_LEFTUP
            );
        }
    }

    #[test]
    fn timed_mouse_press_releases_when_its_target_becomes_blocked() {
        use crate::engine::{Checks, PauseReason, Progress, Schedule, StopSignal, run_controlled};

        let duration = Duration::from_secs(60);
        let mut injector = Injector::with_targets(
            Input::Mouse(Button::Left),
            false,
            None,
            Press::Timed(duration),
        );
        let mut rechecks = 0;
        let mut events = Vec::new();
        let progress = Progress::default();
        let result = run_controlled(
            Schedule {
                limit: Some(1),
                duration: Some(Duration::from_secs(1)),
                ..Schedule::default()
            },
            &StopSignal::new().unwrap(),
            &progress,
            false,
            Checks::default(),
            None,
            |_, control| {
                injector.send_timed(
                    duration,
                    control,
                    Some(&mut |_| {
                        rechecks += 1;
                        Gate::Paused(PauseReason::PointerFilter)
                    }),
                    |packet, _| {
                        events.extend(
                            packet
                                .iter()
                                .map(|event| unsafe { event.Anonymous.mi.dwFlags }),
                        );
                        Ok(())
                    },
                )
            },
        );
        assert_eq!(result, Ok(Outcome::Completed));
        assert_eq!(progress.pause_reason(), Some(PauseReason::PointerFilter));
        assert_eq!(rechecks, 1);
        assert_eq!(events, [MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP]);
    }

    #[test]
    fn held_fixed_press_releases_when_the_display_layout_changes() {
        use crate::engine::{Checks, Progress, Schedule, StopSignal, run_controlled};

        let point = Point { x: 20, y: 30 };
        // A stale layout is first detected during the hold, at the next scheduled check.
        let desktop = Desktop {
            origin: Point { x: 0, y: 0 },
            width: 1000,
            height: 1000,
            displays: Vec::new(),
        };
        let targets = Targets {
            points: vec![point],
            moves: vec![Targets::move_to(&desktop, point).unwrap()],
            desktop,
            next_check: Instant::now() + 2 * LAYOUT_CHECK_INTERVAL,
            index: 0,
            radius: 0,
            random: Random::new(),
            pending: None,
        };
        let mut injector = Injector::with_targets(
            Input::Mouse(Button::Left),
            false,
            Some(targets),
            Press::Held,
        );
        let mut events = Vec::new();
        let progress = Progress::default();
        let result = run_controlled(
            Schedule {
                limit: Some(1),
                duration: Some(Duration::from_secs(2)),
                ..Schedule::default()
            },
            &StopSignal::new().unwrap(),
            &progress,
            false,
            Checks::default(),
            None,
            |_, control| {
                injector.send_timed(Duration::from_secs(60), control, None, |packet, _| {
                    events.extend(
                        packet
                            .iter()
                            .map(|event| unsafe { event.Anonymous.mi.dwFlags }),
                    );
                    Ok(())
                })
            },
        );
        assert_eq!(result, Ok(Outcome::Guarded(LAYOUT_CHANGED)));
        assert_eq!(events[1..], [MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP]);
        assert_eq!(progress.actions(), 1);
    }

    #[test]
    fn text_emits_ordered_utf16_taps_without_virtual_keys() {
        let injector = Injector::with_targets(Input::Text('🙂'), false, None, Press::Tap);
        let events: Vec<_> = injector.packet[..injector.used]
            .iter()
            .map(|event| {
                let key = unsafe { event.Anonymous.ki };
                (key.wVk, key.wScan, key.dwFlags)
            })
            .collect();
        assert_eq!(
            events,
            [
                (0, 0xd83d, KEYEVENTF_UNICODE),
                (0, 0xd83d, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                (0, 0xde42, KEYEVENTF_UNICODE),
                (0, 0xde42, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
            ]
        );
    }

    #[test]
    fn zone_accepts_corners_in_either_order() {
        let zone = Zone {
            from: Point { x: 50, y: 70 },
            to: Point { x: -120, y: 10 },
        };
        assert!(zone.contains(Point { x: -120, y: 10 }));
        assert!(zone.contains(Point { x: -10, y: 30 }));
        assert!(!zone.contains(Point { x: 51, y: 30 }));
    }

    #[test]
    fn fixed_position_handles_displays_left_of_primary() {
        let desktop = Desktop {
            origin: Point { x: -1920, y: 0 },
            width: 4480,
            height: 1440,
            displays: vec![],
        };
        let target = Point { x: -300, y: 140 };
        let abs = desktop.absolute(target).unwrap();
        assert_eq!(abs.x * desktop.width / 65_536 + desktop.origin.x, target.x);
        assert_eq!(abs.y * desktop.height / 65_536 + desktop.origin.y, target.y);
        assert!(desktop.absolute(Point { x: -1921, y: 0 }).is_none());
        assert!(desktop.absolute(Point { x: 0, y: 1440 }).is_none());
    }
}
