mod visual;

use crate::{
    automation::{Guard, INHERITED_ROUTE_OUTPUT, process_image, window_pid},
    engine::{Gate, Outcome, PauseReason, Random, RunControl, Schedule},
    mouse::{Injector, Point, RouteDelivery, RouteInput, Zone, on_monitor},
    targets::{
        Action, ControlIdentity, ControlStep, Identity, Selector, Step, WindowIdentity,
        display_label,
    },
};
use std::{
    collections::HashMap,
    mem::size_of,
    os::windows::io::{FromRawHandle, OwnedHandle},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{HWND, POINT, RECT},
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize,
            },
            Variant::VARIANT,
        },
        UI::{Accessibility::*, WindowsAndMessaging as wm},
    },
    core::{BSTR, Interface},
};
use windows_sys::Win32::{
    System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    UI::Input::KeyboardAndMouse::IsWindowEnabled,
};

const RETRY: Duration = Duration::from_millis(50);
const RESPONSE_POLL: Duration = Duration::from_millis(8);
const FRESHNESS: Duration = Duration::from_millis(150);
const MAX_ANCESTORS: usize = 24;
const UIA_TIMEOUT_MS: u32 = 350;

type Job = Box<dyn FnOnce(Result<&Automation, String>) + Send>;

pub struct Request<T> {
    reply: mpsc::Receiver<Result<T, String>>,
    cancelled: Arc<AtomicBool>,
}

impl<T> Request<T> {
    pub fn try_recv(&self) -> Result<Result<T, String>, TryRecvError> {
        self.reply.try_recv()
    }
}

impl<T> Drop for Request<T> {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub struct Service {
    jobs: SyncSender<Job>,
}

impl Service {
    pub fn new() -> std::io::Result<Self> {
        let (jobs, pending) = mpsc::sync_channel::<Job>(1);
        thread::Builder::new()
            .name("afterbeat-ui-automation".into())
            .spawn(move || unsafe {
                let apartment = CoInitializeEx(None, COINIT_MULTITHREADED).ok();
                let mut automation = None;
                while let Ok(job) = pending.recv() {
                    let result = apartment
                        .as_ref()
                        .map_err(ToString::to_string)
                        .and_then(|()| {
                            if automation.is_none() {
                                automation =
                                    Some(Automation::new().map_err(|err| err.to_string())?);
                            }
                            Ok(automation.as_ref().unwrap())
                        });
                    job(result);
                }
                drop(automation);
                if apartment.is_ok() {
                    CoUninitialize();
                }
            })?;
        Ok(Self { jobs })
    }

    fn submit<T: Send + 'static>(
        &self,
        work: impl FnOnce(&Automation, &AtomicBool) -> Result<T, String> + Send + 'static,
    ) -> Result<Request<T>, String> {
        let (reply, result) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        self.jobs
            .try_send(Box::new(move |automation| {
                if !worker_cancelled.load(Ordering::Relaxed) {
                    let value =
                        automation.and_then(|automation| work(automation, &worker_cancelled));
                    let _ = reply.send(value);
                }
            }))
            .map_err(|err| match err {
                mpsc::TrySendError::Full(_) => "Windows UI Automation is busy. Try again.",
                mpsc::TrySendError::Disconnected(_) => {
                    "Windows UI Automation stopped. Reopen Afterbeat."
                }
            })?;
        Ok(Request {
            reply: result,
            cancelled,
        })
    }

    pub fn capture(&self, point: Point, visual: bool) -> Result<Request<ControlStep>, String> {
        self.submit(move |automation, _| automation.capture(point, visual))
    }

    pub fn repair(&self, step: ControlStep, shown: Target) -> Result<Request<ControlStep>, String> {
        self.submit(move |automation, _| automation.repair(step, shown))
    }

    pub fn preview(&self, route: Vec<Step>) -> Result<Request<Vec<Resolution>>, String> {
        self.submit(move |automation, cancelled| {
            Ok(route
                .iter()
                .take_while(|_| !cancelled.load(Ordering::Relaxed))
                .map(|step| {
                    step.control().map_or(Resolution::Missing, |step| {
                        automation.resolve(&step.selector, None)
                    })
                })
                .collect())
        })
    }

    fn resolve(
        &self,
        selector: Selector,
        clicked: Option<Target>,
        key: bool,
    ) -> Result<Request<Resolution>, String> {
        self.submit(move |automation, _| {
            Ok(if key {
                automation.resolve_key(&selector)
            } else {
                automation.resolve(&selector, clicked)
            })
        })
    }
}

#[derive(Clone)]
pub enum Resolution {
    Ready(Target),
    Recovered(Target),
    Conflict(Target),
    VisualMissing,
    Missing,
    Ambiguous,
    Unavailable,
}

impl Resolution {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Ready(_) => "Control located and ready.",
            Self::Recovered(_) => "Anchored visual match. Repair can update the saved identity.",
            Self::Conflict(_) => {
                "The saved selector and visual evidence disagree. Review the proposed repair."
            }
            Self::VisualMissing => "The saved visual target and its anchor could not be confirmed.",
            Self::Missing => "Control or its saved window is missing.",
            Self::Ambiguous => "Multiple windows or controls match.",
            Self::Unavailable => "Control is disabled, obscured, or unavailable.",
        }
    }

    pub fn preview_status(&self, targeted_key: bool) -> (bool, &'static str) {
        let (Self::Ready(target) | Self::Recovered(target)) = self else {
            return (true, self.message());
        };
        if !targeted_key {
            return (false, self.message());
        }
        if !target.supports_targeted_key() {
            return (
                true,
                "This control has no distinct native focus handle. Use a foreground key step.",
            );
        }
        let message = if matches!(self, Self::Recovered(_)) {
            "Anchored match. Focus the control before the key step runs."
        } else {
            "Focus this control before the key step runs."
        };
        (false, message)
    }

    fn pause(&self) -> PauseReason {
        match self {
            Self::Missing => PauseReason::TargetMissing,
            Self::Ambiguous => PauseReason::TargetAmbiguous,
            Self::Conflict(_) | Self::VisualMissing => PauseReason::TargetEvidence,
            Self::Unavailable | Self::Ready(_) | Self::Recovered(_) => {
                PauseReason::TargetUnavailable
            }
        }
    }
}

#[derive(Clone)]
pub struct Target {
    pub point: Point,
    pub bounds: Zone,
    native: NativeHit,
    observed_at: Instant,
    control_window: usize,
    visual_check: Option<Arc<visual::LiveEvidence>>,
}

impl Target {
    fn supports_targeted_key(&self) -> bool {
        self.control_window != 0 && self.control_window != self.native.window
    }

    fn current(&self) -> bool {
        self.observed_at.elapsed() < FRESHNESS
            && NativeHit::at(self.point) == Some(self.native)
            && self
                .visual_check
                .as_ref()
                .is_none_or(|evidence| evidence.confirm(self.native.window))
            && NativeHit::at(self.point) == Some(self.native)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Focus {
    window: usize,
    leaf: usize,
    recipient_pid: u32,
}

impl Focus {
    fn authorize_key(
        read_focus: impl FnOnce() -> Option<Self>,
        check_recipient: impl FnOnce(u32) -> Gate,
    ) -> Result<Self, Gate> {
        let focus = read_focus();
        match check_recipient(focus.map_or(0, |focus| focus.recipient_pid)) {
            Gate::Open => focus.ok_or(Gate::Paused(PauseReason::KeyboardFocus)),
            gate => Err(gate),
        }
    }

    fn current(self, read_focus: impl FnOnce() -> Option<Self>) -> bool {
        read_focus() == Some(self)
    }
}

fn text_focus_matches(saved: Option<Focus>, current: Option<Focus>) -> bool {
    saved.is_none_or(|saved| Some(saved) == current)
}

fn stable_pixel_match(
    since: &mut Option<Instant>,
    matches: bool,
    now: Instant,
    duration: Duration,
) -> bool {
    if !matches {
        *since = None;
        return false;
    }
    let started = *since.get_or_insert(now);
    now.saturating_duration_since(started) >= duration
}

fn may_skip_pixel_timeout(enabled: bool, gate: Gate, stop_pending: bool) -> bool {
    enabled && matches!(gate, Gate::Open) && !stop_pending
}

fn immediate_pixel_failure(check_once: bool, matches: bool, skip_next: bool) -> Option<bool> {
    (check_once && !matches).then_some(skip_next)
}

#[derive(Default)]
struct PixelRetries {
    condition: Option<usize>,
    used: u8,
    pending_click: Option<usize>,
}

impl PixelRetries {
    fn enter(&mut self, index: usize) {
        if self.condition != Some(index) {
            self.condition = Some(index);
            self.used = 0;
        }
    }

    fn available(&self, limit: u8) -> bool {
        self.used < limit
    }

    fn schedule(&mut self, limit: u8) -> Option<usize> {
        if !self.available(limit) {
            return None;
        }
        let previous = self.condition?.checked_sub(1)?;
        self.used += 1;
        self.pending_click = Some(previous);
        Some(previous)
    }

    fn sent(&mut self, index: usize) -> bool {
        if self.pending_click == Some(index) {
            self.pending_click = None;
            true
        } else {
            false
        }
    }

    fn finish(&mut self) {
        self.condition = None;
        self.used = 0;
        self.pending_click = None;
    }
}

fn wait_for_pending_stop(control: &mut RunControl<'_>) -> Result<Option<Outcome>, String> {
    match control.retry_dispatch(Gate::Paused(PauseReason::PendingStop))? {
        Some(Outcome::Completed) => {
            // Do not end before a queued toggle shortcut is handled, even if the run time expires.
            thread::sleep(RESPONSE_POLL);
            Ok(None)
        }
        outcome => Ok(outcome),
    }
}

fn advance_route(
    index: &mut usize,
    len: usize,
    completed: &mut u64,
    once: bool,
    limit: Option<u64>,
) -> bool {
    *index += 1;
    if *index < len {
        return false;
    }
    *completed += 1;
    if once || limit.is_some_and(|limit| *completed >= limit) {
        return true;
    }
    *index = 0;
    false
}

fn foreground_focus() -> Option<Focus> {
    unsafe {
        let window = wm::GetForegroundWindow();
        if window.is_invalid() {
            return None;
        }
        let mut info = wm::GUITHREADINFO {
            cbSize: size_of::<wm::GUITHREADINFO>() as u32,
            ..Default::default()
        };
        wm::GetGUIThreadInfo(0, &mut info).ok()?;
        if info.hwndFocus.is_invalid() || wm::GetAncestor(info.hwndFocus, wm::GA_ROOT) != window {
            return None;
        }
        Some(Focus {
            window: window.0 as usize,
            leaf: info.hwndFocus.0 as usize,
            recipient_pid: window_pid(info.hwndFocus.0),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct NativeHit {
    // Only handles and coordinates cross threads. COM objects stay on the UIA worker.
    window: usize,
    leaf: usize,
    pid: u32,
    rect: (i32, i32, i32, i32),
    leaf_rect: (i32, i32, i32, i32),
    dpi: u32,
}

impl NativeHit {
    fn at(point: Point) -> Option<Self> {
        unsafe {
            let leaf = wm::WindowFromPoint(POINT {
                x: point.x,
                y: point.y,
            });
            let window = wm::GetAncestor(leaf, wm::GA_ROOT);
            let mut rect = RECT::default();
            let mut leaf_rect = RECT::default();
            wm::GetWindowRect(window, &mut rect).ok()?;
            wm::GetWindowRect(leaf, &mut leaf_rect).ok()?;
            if IsWindowEnabled(leaf.0) == 0 {
                return None;
            }
            let thread = wm::GetWindowThreadProcessId(leaf, None);
            if thread == 0 {
                return None;
            }
            let mut gui = wm::GUITHREADINFO {
                cbSize: size_of::<wm::GUITHREADINFO>() as u32,
                ..Default::default()
            };
            for thread in [0, thread] {
                wm::GetGUIThreadInfo(thread, &mut gui).ok()?;
                if !gui.hwndCapture.is_invalid() {
                    return None;
                }
            }
            Some(Self {
                window: window.0 as usize,
                leaf: leaf.0 as usize,
                pid: window_pid(window.0),
                rect: (rect.left, rect.top, rect.right, rect.bottom),
                leaf_rect: (
                    leaf_rect.left,
                    leaf_rect.top,
                    leaf_rect.right,
                    leaf_rect.bottom,
                ),
                dpi: windows_sys::Win32::UI::HiDpi::GetDpiForWindow(window.0),
            })
        }
    }
}

struct Automation {
    client: IUIAutomation,
    walker: IUIAutomationTreeWalker,
}

impl Automation {
    fn resolve_key(&self, selector: &Selector) -> Resolution {
        let result = self.resolve(selector, None);
        let target = match &result {
            Resolution::Ready(target) | Resolution::Recovered(target) => target,
            _ => return result,
        };
        // Only a distinct native focus handle can be rechecked on the sender.
        if !target.supports_targeted_key() {
            return Resolution::Unavailable;
        }
        let Some(focus) = foreground_focus().filter(|focus| {
            focus.window == target.native.window && focus.leaf == target.control_window
        }) else {
            return Resolution::Unavailable;
        };
        let matches = unsafe {
            self.client
                .GetFocusedElement()
                .and_then(|element| element.CurrentNativeWindowHandle())
        }
        .is_ok_and(|handle| handle.0 as usize == focus.leaf);
        if !matches || !focus.current(foreground_focus) {
            return Resolution::Unavailable;
        }
        result
    }
    fn new() -> windows::core::Result<Self> {
        unsafe {
            let client: IUIAutomation =
                CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
            let options = client.cast::<IUIAutomation2>()?;
            options.SetConnectionTimeout(UIA_TIMEOUT_MS)?;
            options.SetTransactionTimeout(UIA_TIMEOUT_MS)?;
            options.SetAutoSetFocus(false)?;
            Ok(Self {
                walker: client.RawViewWalker()?,
                client,
            })
        }
    }

    fn capture(&self, point: Point, visual: bool) -> Result<ControlStep, String> {
        let strict = self.capture_control(point);
        if !visual {
            return strict;
        }
        let mut selector = match strict {
            Ok(step) => step.selector,
            Err(_) => {
                let native =
                    NativeHit::at(point).ok_or("Point at a visible control or graphic.")?;
                if native.pid == std::process::id() {
                    return Err("Point at a target in another app.".into());
                }
                let hwnd = HWND(native.window as _);
                let root = unsafe { self.client.ElementFromHandle(hwnd) }
                    .map_err(|_| "Could not inspect the target window.")?;
                let hit = self
                    .interactive_at(&root, point)
                    .map_err(|_| "Could not inspect the target.")?;
                let (control, label) = if let Some(element) = hit {
                    let control = control_identity(&element)
                        .map_err(|_| "Could not identify the target.")?
                        .ok_or("The control has no accessible identity.")?;
                    let name = unsafe { element.CurrentName() }
                        .map_err(|_| "Could not read the target name.")?
                        .to_string();
                    (
                        Some(control),
                        if name.trim().is_empty() {
                            "Visual target".into()
                        } else {
                            name
                        },
                    )
                } else {
                    (None, "Visual target".into())
                };
                Selector {
                    window: identity_for_window(hwnd)
                        .ok_or("Could not identify the app window.")?,
                    control,
                    container: None,
                    visual: None,
                    label,
                }
            }
        };
        selector.visual = Some(self.capture_visual(&selector, point)?);
        if selector.control.is_none() {
            let anchor = &selector.visual.as_ref().unwrap().anchor.identity;
            if let Identity::Name(name) = anchor {
                selector.label = format!("Visual near {}", display_label(name));
            }
        }
        if !matches!(
            self.resolve_visual(&selector, None),
            Resolution::Recovered(_) | Resolution::Ready(_)
        ) {
            return Err(
                "Could not confirm a unique visual target and nearby landmark. Try another target."
                    .into(),
            );
        }
        Ok(ControlStep {
            selector,
            wait_for_disappearance: false,
        })
    }

    fn capture_control(&self, point: Point) -> Result<ControlStep, String> {
        let describe = |_: windows::core::Error| {
            "Windows could not read the control under the pointer.".to_owned()
        };
        let native =
            NativeHit::at(point).ok_or("Move the pointer over an available app control.")?;
        if native.pid == std::process::id() {
            return Err("Point at a control in another app.".into());
        }
        let hwnd = HWND(native.window as _);
        let window = identity_for_window(hwnd).ok_or("Could not identify the app window.")?;
        let root = unsafe { self.client.ElementFromHandle(hwnd) }.map_err(describe)?;
        let hit = unsafe {
            self.client.ElementFromPoint(POINT {
                x: point.x,
                y: point.y,
            })
        }
        .map_err(describe)?;
        let mut selected = None;
        for candidate in self.ancestors(hit, &root) {
            let candidate = candidate.map_err(describe)?;
            if interactive(role(&candidate).map_err(describe)?) {
                selected = Some(candidate);
                break;
            }
        }
        let element = selected
            .ok_or("No supported accessible control under the pointer. Try a route point.")?;
        let control = control_identity(&element)
            .map_err(describe)?
            .ok_or("This control has no accessible name or automation ID.")?;
        let name = unsafe { element.CurrentName() }
            .map_err(describe)?
            .to_string();
        let label = if name.trim().is_empty() {
            match &control.identity {
                Identity::AutomationId(id) | Identity::Name(id) => id.clone(),
            }
        } else {
            name
        };
        let (_, actual_root) = self
            .find_window(&window)
            .map_err(|status| status.message())?;
        if actual_root != hwnd {
            return Err(Resolution::Ambiguous.message().into());
        }
        let matches = self.matches(&root, &control).map_err(describe)?;
        let container = match unique(&matches) {
            ResolutionCount::One(candidate)
                if self.same(&candidate, &element).map_err(describe)? =>
            {
                None
            }
            ResolutionCount::Many => {
                let parent = unsafe { self.walker.GetParentElement(&element) }.map_err(describe)?;
                let mut container = None;
                for ancestor in self.ancestors(parent, &root) {
                    let ancestor = ancestor.map_err(describe)?;
                    let Some(identity) = control_identity(&ancestor).map_err(describe)? else {
                        continue;
                    };
                    let roots = self.matches(&root, &identity).map_err(describe)?;
                    if let ResolutionCount::One(candidate) = unique(&roots)
                        && self.same(&candidate, &ancestor).map_err(describe)?
                        && let ResolutionCount::One(candidate) =
                            unique(&self.matches(&ancestor, &control).map_err(describe)?)
                        && self.same(&candidate, &element).map_err(describe)?
                    {
                        container = Some(identity);
                        break;
                    }
                }
                Some(container.ok_or(
                    "More than one control matches, and no unique container distinguishes them.",
                )?)
            }
            _ => return Err("The control changed during capture. Try again.".into()),
        };
        if NativeHit::at(point) != Some(native) {
            return Err("The target window changed during capture. Try again.".into());
        }
        Ok(ControlStep {
            selector: Selector {
                window,
                control: Some(control),
                container,
                visual: None,
                label,
            },
            wait_for_disappearance: false,
        })
    }

    fn find_window(
        &self,
        identity: &WindowIdentity,
    ) -> Result<(IUIAutomationElement, HWND), Resolution> {
        let windows = self.app_windows(identity, true)?;
        let hwnd = match windows.as_slice() {
            [hwnd] => *hwnd,
            [] => return Err(Resolution::Missing),
            _ => return Err(Resolution::Ambiguous),
        };
        let window =
            unsafe { self.client.ElementFromHandle(hwnd) }.map_err(|_| Resolution::Unavailable)?;
        Ok((window, hwnd))
    }

    fn app_windows(
        &self,
        identity: &WindowIdentity,
        exact_title: bool,
    ) -> Result<Vec<HWND>, Resolution> {
        let mut windows = Vec::<HWND>::new();
        unsafe extern "system" fn collect(
            hwnd: HWND,
            data: windows::Win32::Foundation::LPARAM,
        ) -> windows::core::BOOL {
            unsafe { (*(data.0 as *mut Vec<HWND>)).push(hwnd) };
            true.into()
        }
        unsafe {
            wm::EnumWindows(
                Some(collect),
                windows::Win32::Foundation::LPARAM((&mut windows as *mut Vec<HWND>) as isize),
            )
            .map_err(|_| Resolution::Unavailable)?;
        }
        let mut matched = Vec::new();
        let mut processes = HashMap::new();
        for hwnd in windows {
            if !unsafe { wm::IsWindowVisible(hwnd) }.as_bool()
                || !window_parts(hwnd).is_some_and(|(class, title)| {
                    class == identity.class && (!exact_title || title == identity.title)
                })
            {
                continue;
            }
            let pid = window_pid(hwnd.0);
            if pid == 0 || pid == std::process::id() {
                continue;
            }
            let image = processes
                .entry(pid)
                .or_insert_with(|| executable(pid))
                .as_ref()
                .ok_or(Resolution::Unavailable)?;
            if image.eq_ignore_ascii_case(&identity.executable) {
                matched.push(hwnd);
            }
        }
        Ok(matched)
    }

    fn resolve(&self, selector: &Selector, clicked: Option<Target>) -> Resolution {
        if selector.visual.is_some() {
            self.resolve_visual(selector, clicked)
        } else {
            self.resolve_checked(selector, clicked)
                .unwrap_or(Resolution::Unavailable)
        }
    }

    fn resolve_checked(
        &self,
        selector: &Selector,
        clicked: Option<Target>,
    ) -> windows::core::Result<Resolution> {
        let root_window = clicked.as_ref().map_or_else(
            || self.find_window(&selector.window),
            |target| self.clicked_window(target),
        );
        let (mut root, hwnd) = match root_window {
            Ok(found) => found,
            Err(status) => return Ok(status),
        };
        if let Some(container) = &selector.container {
            root = match unique(&self.matches(&root, container)?) {
                ResolutionCount::One(element) => element,
                ResolutionCount::None => return Ok(Resolution::Missing),
                ResolutionCount::Many => return Ok(Resolution::Ambiguous),
            };
        }
        let Some(control) = &selector.control else {
            return Ok(Resolution::Missing);
        };
        let element = match unique(&self.matches(&root, control)?) {
            ResolutionCount::One(element) => element,
            ResolutionCount::None => return Ok(Resolution::Missing),
            ResolutionCount::Many => return Ok(Resolution::Ambiguous),
        };
        unsafe {
            if clicked.is_some() || element.CurrentIsOffscreen()?.as_bool() {
                return Ok(Resolution::Unavailable);
            }
            if !element.CurrentIsEnabled()?.as_bool() || wm::IsIconic(hwnd).as_bool() {
                return Ok(Resolution::Unavailable);
            }
            let mut point = POINT::default();
            if !element.GetClickablePoint(&mut point)?.as_bool() {
                return Ok(Resolution::Unavailable);
            }
            let rect = element.CurrentBoundingRectangle()?;
            let control_window = element.CurrentNativeWindowHandle()?.0 as usize;
            let screen_point = Point {
                x: point.x,
                y: point.y,
            };
            let Some(native) =
                NativeHit::at(screen_point).filter(|hit| hit.window == hwnd.0 as usize)
            else {
                return Ok(Resolution::Unavailable);
            };
            let observed_at = Instant::now();
            let hit = self.client.ElementFromPoint(point)?;
            if !self.hit_is_control(hit, &element, &root)?
                || !window_matches(hwnd, &selector.window)
                || !(rect.left..rect.right).contains(&screen_point.x)
                || !(rect.top..rect.bottom).contains(&screen_point.y)
                || !on_monitor(screen_point)
                || NativeHit::at(screen_point) != Some(native)
            {
                return Ok(Resolution::Unavailable);
            }
            Ok(Resolution::Ready(Target {
                point: screen_point,
                bounds: Zone {
                    from: Point {
                        x: rect.left,
                        y: rect.top,
                    },
                    to: Point {
                        x: rect.right - 1,
                        y: rect.bottom - 1,
                    },
                },
                native,
                observed_at,
                control_window,
                visual_check: None,
            }))
        }
    }

    fn clicked_window(&self, target: &Target) -> Result<(IUIAutomationElement, HWND), Resolution> {
        let hwnd = HWND(target.native.window as _);
        let control = HWND(target.control_window as _);
        unsafe {
            if window_pid(hwnd.0) != target.native.pid
                || !wm::IsWindowVisible(hwnd).as_bool()
                || (target.control_window != 0
                    && wm::GetAncestor(control, wm::GA_ROOT) == hwnd
                    && !wm::IsWindowVisible(control).as_bool())
            {
                return Err(Resolution::Missing);
            }
            if wm::IsIconic(hwnd).as_bool() {
                return Err(Resolution::Unavailable);
            }
            self.client
                .ElementFromHandle(hwnd)
                .map(|root| (root, hwnd))
                .map_err(|_| Resolution::Unavailable)
        }
    }

    fn hit_is_control(
        &self,
        hit: IUIAutomationElement,
        element: &IUIAutomationElement,
        root: &IUIAutomationElement,
    ) -> windows::core::Result<bool> {
        for candidate in self.ancestors(hit, root) {
            let candidate = candidate?;
            if self.same(&candidate, element)? {
                return Ok(true);
            }
            if interactive(role(&candidate)?) {
                break;
            }
        }
        Ok(false)
    }

    fn same(
        &self,
        left: &IUIAutomationElement,
        right: &IUIAutomationElement,
    ) -> windows::core::Result<bool> {
        unsafe { self.client.CompareElements(left, right) }.map(|same| same.as_bool())
    }

    fn ancestors<'a>(
        &'a self,
        first: IUIAutomationElement,
        root: &'a IUIAutomationElement,
    ) -> impl Iterator<Item = windows::core::Result<IUIAutomationElement>> + 'a {
        let mut element = Some(first);
        std::iter::from_fn(move || {
            let current = element.take()?;
            match self.same(&current, root) {
                Ok(true) => return None,
                Ok(false) => {}
                Err(error) => return Some(Err(error)),
            }
            element = unsafe { self.walker.GetParentElement(&current) }.ok();
            Some(Ok(current))
        })
        .take(MAX_ANCESTORS)
    }

    fn matches(
        &self,
        root: &IUIAutomationElement,
        target: &ControlIdentity,
    ) -> windows::core::Result<IUIAutomationElementArray> {
        let (property, value) = match &target.identity {
            Identity::AutomationId(id) => (UIA_AutomationIdPropertyId, id),
            Identity::Name(name) => (UIA_NamePropertyId, name),
        };
        unsafe {
            let identity = self
                .client
                .CreatePropertyCondition(property, &VARIANT::from(BSTR::from(value.as_str())))?;
            let role = self
                .client
                .CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(target.role))?;
            let condition = self.client.CreateAndCondition(&identity, &role)?;
            root.FindAll(TreeScope_Descendants, &condition)
        }
    }
}

enum ResolutionCount {
    None,
    One(IUIAutomationElement),
    Many,
}

fn unique(matches: &IUIAutomationElementArray) -> ResolutionCount {
    unsafe {
        match matches.Length() {
            Ok(0) => ResolutionCount::None,
            Ok(1) => matches
                .GetElement(0)
                .map_or(ResolutionCount::Many, ResolutionCount::One),
            _ => ResolutionCount::Many,
        }
    }
}

fn role(element: &IUIAutomationElement) -> windows::core::Result<UIA_CONTROLTYPE_ID> {
    unsafe { element.CurrentControlType() }
}

fn interactive(role: UIA_CONTROLTYPE_ID) -> bool {
    [
        UIA_ButtonControlTypeId,
        UIA_CheckBoxControlTypeId,
        UIA_ComboBoxControlTypeId,
        UIA_DataItemControlTypeId,
        UIA_EditControlTypeId,
        UIA_HyperlinkControlTypeId,
        UIA_ListItemControlTypeId,
        UIA_MenuItemControlTypeId,
        UIA_RadioButtonControlTypeId,
        UIA_SplitButtonControlTypeId,
        UIA_TabItemControlTypeId,
        UIA_TreeItemControlTypeId,
    ]
    .contains(&role)
}

fn control_identity(
    element: &IUIAutomationElement,
) -> windows::core::Result<Option<ControlIdentity>> {
    unsafe {
        let role = role(element)?.0;
        let id = element.CurrentAutomationId()?.to_string();
        if !id.trim().is_empty() {
            return Ok(Some(ControlIdentity {
                role,
                identity: Identity::AutomationId(id),
            }));
        }
        let name = element.CurrentName()?.to_string();
        Ok((!name.trim().is_empty()).then_some(ControlIdentity {
            role,
            identity: Identity::Name(name),
        }))
    }
}

fn window_parts(hwnd: HWND) -> Option<(String, String)> {
    let mut class = [0; 256];
    let mut title = [0; 1_024];
    let class_len = unsafe { wm::GetClassNameW(hwnd, &mut class) };
    if class_len <= 0 {
        return None;
    }
    let title_len = unsafe { wm::GetWindowTextW(hwnd, &mut title) };
    if title_len >= title.len() as i32 - 1 {
        return None;
    }
    Some((
        String::from_utf16(&class[..class_len as usize]).ok()?,
        String::from_utf16(&title[..title_len as usize]).ok()?,
    ))
}

fn window_matches(hwnd: HWND, identity: &WindowIdentity) -> bool {
    window_parts(hwnd)
        .is_some_and(|(class, title)| class == identity.class && title == identity.title)
}

fn executable(pid: u32) -> Option<String> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    process_image(&unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn identity_for_window(hwnd: HWND) -> Option<WindowIdentity> {
    let (class, title) = window_parts(hwnd)?;
    let executable = executable(window_pid(hwnd.0))?;
    Some(WindowIdentity {
        executable,
        class,
        title,
    })
}

pub struct Route {
    service: Option<Service>,
    steps: Vec<Step>,
    index: usize,
    timeout: Duration,
    reply: Option<Request<Resolution>>,
    layouts: Option<Vec<Zone>>,
}

impl Route {
    pub fn new(service: Option<Service>, steps: Vec<Step>, timeout: Duration) -> Self {
        let layouts = steps
            .iter()
            .any(|step| step.point().is_some() || matches!(step, Step::Action(action) if matches!(action.action, Action::WaitColor { .. } | Action::WaitImage { .. } | Action::Flow { .. })))
            .then(crate::mouse::display_areas);
        Self {
            service,
            steps,
            index: 0,
            timeout,
            reply: None,
            layouts,
        }
    }

    pub fn sequence(
        &mut self,
        settings: &crate::settings::Settings,
        schedule: Schedule,
        guard: &mut Guard,
        control: &mut RunControl<'_>,
        mut stop_pending: impl FnMut() -> bool,
    ) -> Result<Outcome, String> {
        if settings.has_recording() {
            return self.recorded_sequence(settings, schedule, guard, control, &mut stop_pending);
        }
        let mut random = Random::new();
        let mut last_action = None;
        let mut completed = 0;
        let mut retries = PixelRetries::default();
        let loop_limit = if settings.route_once {
            None
        } else {
            settings.route_loop_limit().map_err(|issue| issue.message)?
        };
        use crate::workflow::Operation;
        let workflow = crate::workflow::compile(&self.steps)?;
        let mut loops = vec![0u16; self.steps.len()];
        let mut recovery_attempts = vec![0u8; self.steps.len()];
        let mut actions_at_traversal_start = control.route_actions();
        'route: loop {
            if stop_pending() {
                loop {
                    if let Some(outcome) = wait_for_pending_stop(control)? {
                        return Ok(outcome);
                    }
                }
            }
            if schedule
                .limit
                .is_some_and(|limit| control.route_actions() >= limit)
            {
                return Ok(control
                    .wait_route(Instant::now())?
                    .unwrap_or(Outcome::Completed));
            }
            if self.index == self.steps.len() {
                self.index -= 1;
                if advance_route(
                    &mut self.index,
                    self.steps.len(),
                    &mut completed,
                    settings.route_once,
                    loop_limit,
                ) {
                    return Ok(control
                        .wait_route(Instant::now())?
                        .unwrap_or(Outcome::Completed));
                }
                let actions = control.route_actions();
                if actions == actions_at_traversal_start
                    && let Some(outcome) = wait_delay(control, guard, Instant::now() + RETRY)?
                {
                    return Ok(outcome);
                }
                actions_at_traversal_start = actions;
            }
            let step = &self.steps[self.index];
            let interval = schedule.rates.map_or(schedule.interval, |(low, high)| {
                Duration::from_secs_f64((low + (high - low) * random.unit()).recip())
            });
            let delay = match step {
                Step::Action(action) => Duration::from_millis(action.delay_ms.into()),
                _ if last_action.is_some() => interval,
                _ => Duration::ZERO,
            };
            let from = if matches!(step, Step::Action(_)) {
                Instant::now()
            } else {
                last_action.unwrap_or_else(Instant::now)
            };
            if let Some(outcome) = wait_delay(control, guard, from + delay)? {
                if retries.pending_click.is_some() && stop_pending() {
                    loop {
                        if let Some(outcome) = wait_for_pending_stop(control)? {
                            return Ok(outcome);
                        }
                    }
                }
                return Ok(outcome);
            }
            macro_rules! timeout {
                ($error:expr, $recoverable:expr) => {{
                    let index = self.index;
                    if $recoverable && let Some(policy) = workflow.recovery[index] {
                        control.clear_target_wait();
                        if recovery_attempts[index] < policy.retries {
                            recovery_attempts[index] += 1;
                            continue 'route;
                        }
                        if policy.skip {
                            control.record_skipped_step(index);
                            recovery_attempts[index] = 0;
                            self.index += 1;
                            continue 'route;
                        }
                    }
                    return Err($error);
                }};
            }
            if let Step::Action(action) = step
                && let Action::Flow { operation } = &action.action
            {
                let index = self.index;
                self.index = match operation {
                    Operation::If { test } | Operation::BreakIf { test } => {
                        let passed = match self.test_workflow_pixel(
                            test,
                            guard,
                            control,
                            &mut stop_pending,
                        )? {
                            Ok(passed) => passed,
                            Err(outcome) => return Ok(outcome),
                        };
                        match operation {
                            Operation::If { .. } if !passed => workflow.jumps[index],
                            Operation::BreakIf { .. } if passed => {
                                let end = workflow.jumps[index];
                                loops[workflow.jumps[end]] = 0;
                                end + 1
                            }
                            _ => index + 1,
                        }
                    }
                    Operation::Else => workflow.jumps[index],
                    Operation::Repeat { count } => {
                        loops[index] = *count;
                        index + 1
                    }
                    Operation::EndRepeat => {
                        let start = workflow.jumps[index];
                        loops[start] -= 1;
                        if loops[start] > 0 {
                            start + 1
                        } else {
                            index + 1
                        }
                    }
                    Operation::EndIf | Operation::Recover { .. } => index + 1,
                };
                continue;
            }
            if let Step::Action(action) = step
                && let Action::WaitImage {
                    search,
                    template,
                    threshold,
                    timeout_ms,
                    output,
                    scaled,
                } = &action.action
            {
                let deadline = Instant::now() + Duration::from_millis(u64::from(*timeout_ms));
                use crate::{target_pixels::ImageSearch, targets::ImageOutput};
                let mut stop_observed = false;
                let input = match output {
                    ImageOutput::Wait | ImageOutput::If => None,
                    ImageOutput::Move => Some(RouteInput::Move),
                    ImageOutput::Click => Some(RouteInput::Click(crate::mouse::Button::Left, 1)),
                };
                let mut last_hint = "Waiting for the image to match.";
                let mut recoverable = true;
                let mut observed_missing = false;
                'image_wait: loop {
                    if stop_pending() {
                        stop_observed = true;
                    }
                    if stop_observed {
                        if let Some(outcome) = wait_for_pending_stop(control)? {
                            return Ok(outcome);
                        }
                        continue 'image_wait;
                    }
                    if let Some(outcome) = control.wait_route(Instant::now())? {
                        if stop_pending() {
                            stop_observed = true;
                            continue 'image_wait;
                        }
                        return Ok(outcome);
                    }
                    if let Err(error) = self.validate_layout(Point {
                        x: search.x,
                        y: search.y,
                    }) {
                        if stop_pending() {
                            stop_observed = true;
                            continue 'image_wait;
                        }
                        return Err(error);
                    }
                    let Some(region) = crate::target_pixels::image_region(*search, 512) else {
                        if stop_pending() {
                            stop_observed = true;
                            continue 'image_wait;
                        }
                        return Err("Image search region must fit within one display.".into());
                    };
                    let gate = guard.check();
                    if stop_pending() {
                        stop_observed = true;
                        continue 'image_wait;
                    }
                    if let Gate::Stop(reason) = gate {
                        return Ok(Outcome::Guarded(reason));
                    }
                    if Instant::now() >= deadline {
                        let hint = match gate {
                            Gate::Paused(reason) => crate::pause_hint(reason),
                            _ => last_hint,
                        };
                        if stop_pending() {
                            stop_observed = true;
                            continue 'image_wait;
                        }
                        if *output == ImageOutput::If
                            && observed_missing
                            && matches!(gate, Gate::Open)
                        {
                            control.clear_target_wait();
                            self.index = workflow.jumps[self.index];
                            continue 'route;
                        }
                        timeout!(
                            self.timed_out(&step.label(), hint),
                            recoverable && matches!(gate, Gate::Open)
                        );
                    }
                    let reason = match gate {
                        Gate::Paused(reason) => {
                            observed_missing = false;
                            reason
                        }
                        Gate::Open => 'search: {
                            let Some(frame) = crate::target_pixels::Frame::capture_color(region)
                            else {
                                if stop_pending() {
                                    stop_observed = true;
                                    continue 'image_wait;
                                }
                                return Err("The screen image could not be read.".into());
                            };
                            let mut interrupted = None;
                            let mut stop_seen_during_search = false;
                            let matched = frame.search_image(
                                template,
                                *threshold,
                                *scaled,
                                input.is_some(),
                                || {
                                    if stop_pending() {
                                        stop_seen_during_search = true;
                                        return true;
                                    }
                                    if Instant::now() >= deadline {
                                        return true;
                                    }
                                    match control.wait_route(Instant::now()) {
                                        Ok(None) => false,
                                        Ok(Some(outcome)) => {
                                            interrupted = Some(Ok(outcome));
                                            true
                                        }
                                        Err(error) => {
                                            interrupted = Some(Err(error));
                                            true
                                        }
                                    }
                                },
                            );
                            if stop_seen_during_search || stop_pending() {
                                stop_observed = true;
                                continue 'image_wait;
                            }
                            if let Some(outcome) = interrupted {
                                return outcome;
                            }
                            if Instant::now() >= deadline {
                                continue 'image_wait;
                            }
                            let matched = matched?;
                            if let Some(outcome) = control.wait_route(Instant::now())? {
                                if stop_pending() {
                                    stop_observed = true;
                                    continue 'image_wait;
                                }
                                return Ok(outcome);
                            }
                            if stop_pending() {
                                stop_observed = true;
                                continue 'image_wait;
                            }
                            let found = match matched {
                                ImageSearch::Missing => {
                                    observed_missing = true;
                                    last_hint = "Waiting for the image to match.";
                                    recoverable = true;
                                    break 'search PauseReason::PixelColor;
                                }
                                ImageSearch::Ambiguous(_) => {
                                    last_hint = "Multiple image locations match. Narrow the region or raise the threshold.";
                                    recoverable = true;
                                    break 'search PauseReason::TargetAmbiguous;
                                }
                                ImageSearch::Unique(found) => found,
                            };
                            if let Some(input) = input {
                                let point = found.region.center();
                                self.validate_layout(point)?;
                                match guard.check_destination(Some(point)) {
                                    Gate::Stop(reason) => return Ok(Outcome::Guarded(reason)),
                                    Gate::Paused(reason) => {
                                        last_hint = crate::pause_hint(reason);
                                        recoverable = false;
                                        break 'search reason;
                                    }
                                    Gate::Open => {}
                                }
                                let Some(hit) = NativeHit::at(point) else {
                                    last_hint = "The matched location has no available window.";
                                    recoverable = false;
                                    break 'search PauseReason::TargetUnavailable;
                                };
                                let mut stop_during_dispatch = false;
                                let mut guard_stop = None;
                                let delivery = input.send(Some(point), control, || {
                                    if stop_pending() {
                                        stop_during_dispatch = true;
                                        return false;
                                    }
                                    if RouteInput::Move.is_held() || !frame.unchanged(found.region)
                                    {
                                        return false;
                                    }
                                    if stop_pending() {
                                        stop_during_dispatch = true;
                                        return false;
                                    }
                                    if Instant::now() >= deadline
                                        || self.validate_layout(point).is_err()
                                        || NativeHit::at(point) != Some(hit)
                                    {
                                        return false;
                                    }
                                    match guard.check_destination(Some(point)) {
                                        Gate::Open => {
                                            stop_during_dispatch = stop_pending();
                                            !stop_during_dispatch
                                        }
                                        Gate::Paused(_) => false,
                                        Gate::Stop(reason) => {
                                            guard_stop = Some(reason);
                                            false
                                        }
                                    }
                                })?;
                                match delivery {
                                    RouteDelivery::Sent => last_action = Some(Instant::now()),
                                    RouteDelivery::End(outcome) => return Ok(outcome),
                                    RouteDelivery::Retry => {
                                        stop_observed |= stop_during_dispatch;
                                        last_hint = "The image or destination changed before input could be sent.";
                                        recoverable = false;
                                        if stop_observed {
                                            continue 'image_wait;
                                        }
                                        if let Some(reason) = guard_stop {
                                            return Ok(Outcome::Guarded(reason));
                                        }
                                        break 'search PauseReason::TargetUnavailable;
                                    }
                                }
                            }
                            control.clear_target_wait();
                            break 'image_wait;
                        }
                        Gate::Stop(_) => unreachable!(),
                    };
                    let now = Instant::now();
                    if let Some(outcome) = control.wait_for_target(
                        reason,
                        RETRY.min(deadline.saturating_duration_since(now)),
                        self.index,
                        deadline,
                    )? {
                        if stop_pending() {
                            stop_observed = true;
                            continue 'image_wait;
                        }
                        return Ok(outcome);
                    }
                }
            }
            if let Step::Action(action) = step
                && let Action::WaitColor {
                    point,
                    rgb,
                    tolerance,
                    different,
                    stable_ms,
                    skip_next_on_timeout,
                    check_once,
                    skip_next_on_mismatch,
                    retry_clicks,
                } = &action.action
            {
                retries.enter(self.index);
                let deadline = Instant::now() + self.timeout;
                let mut matched_since = None;
                let mut skip_next = false;
                let mut stop_observed = false;
                macro_rules! retry_click {
                    ($allowed:expr) => {
                        if $allowed && retries.available(*retry_clicks) {
                            if !crate::targets::has_retryable_previous_click(&self.steps, self.index) {
                                return Err("Click retries require an immediately preceding custom click step.".into());
                            }
                            if let Some(outcome) = control.wait_route(Instant::now())? {
                                if stop_pending() {
                                    stop_observed = true;
                                    continue;
                                }
                                return Ok(outcome);
                            }
                            if stop_pending() {
                                stop_observed = true;
                                continue;
                            }
                            control.clear_target_wait();
                            self.index = retries.schedule(*retry_clicks).ok_or("Click retries require a previous step.")?;
                            continue 'route;
                        }
                    };
                }
                loop {
                    if stop_pending() {
                        stop_observed = true;
                    }
                    if stop_observed {
                        if let Some(outcome) = wait_for_pending_stop(control)? {
                            return Ok(outcome);
                        }
                        continue;
                    }
                    if let Some(outcome) = control.wait_route(Instant::now())? {
                        if stop_pending() {
                            stop_observed = true;
                            continue;
                        }
                        return Ok(outcome);
                    }
                    if let Err(error) = self.validate_layout(*point) {
                        if stop_pending() {
                            stop_observed = true;
                            continue;
                        }
                        return Err(error);
                    }
                    let gate = guard.check();
                    if let Gate::Stop(reason) = gate {
                        if stop_pending() {
                            stop_observed = true;
                            continue;
                        }
                        return Ok(Outcome::Guarded(reason));
                    }
                    let stop_requested = stop_pending();
                    if stop_requested {
                        stop_observed = true;
                        matched_since = None;
                        continue;
                    }
                    if Instant::now() >= deadline {
                        retry_click!(!*check_once && matches!(gate, Gate::Open));
                        if !*check_once
                            && may_skip_pixel_timeout(*skip_next_on_timeout, gate, stop_observed)
                        {
                            if self.index + 1 >= self.steps.len() {
                                return Err(
                                    "Add a step after the pixel condition to use Skip next.".into(),
                                );
                            }
                            if let Some(outcome) = control.wait_route(Instant::now())? {
                                if stop_pending() {
                                    stop_observed = true;
                                    continue;
                                }
                                return Ok(outcome);
                            }
                            if stop_pending() {
                                stop_observed = true;
                                continue;
                            }
                            control.clear_target_wait();
                            control.record_skipped_step(self.index + 1);
                            skip_next = true;
                            break;
                        }
                        let reason = match gate {
                            Gate::Paused(reason) => reason,
                            _ => PauseReason::PixelColor,
                        };
                        if stop_pending() {
                            stop_observed = true;
                            continue;
                        }
                        timeout!(
                            self.timed_out(&step.label(), crate::pause_hint(reason)),
                            matches!(gate, Gate::Open)
                        );
                    }
                    let reason = match gate {
                        Gate::Stop(reason) => return Ok(Outcome::Guarded(reason)),
                        Gate::Paused(reason) => {
                            matched_since = None;
                            reason
                        }
                        Gate::Open => {
                            let Some(color) = crate::target_pixels::pixel_rgb(*point) else {
                                if stop_pending() {
                                    stop_observed = true;
                                    continue;
                                }
                                return Err("The screen pixel could not be read.".into());
                            };
                            let matches = crate::target_pixels::pixel_condition_matches(
                                color, *rgb, *tolerance, *different,
                            );
                            if let Some(skip_on_mismatch) = immediate_pixel_failure(
                                *check_once,
                                matches,
                                *skip_next_on_mismatch,
                            ) {
                                if stop_pending() {
                                    stop_observed = true;
                                    continue;
                                }
                                if let Some(outcome) = control.wait_route(Instant::now())? {
                                    if stop_pending() {
                                        stop_observed = true;
                                        continue;
                                    }
                                    return Ok(outcome);
                                }
                                if stop_pending() {
                                    stop_observed = true;
                                    continue;
                                }
                                if Instant::now() >= deadline {
                                    continue;
                                }
                                retry_click!(true);
                                if !skip_on_mismatch {
                                    timeout!(
                                        format!(
                                            "Step {} pixel condition did not match.",
                                            self.index + 1
                                        ),
                                        true
                                    );
                                }
                                if self.index + 1 >= self.steps.len() {
                                    return Err(
                                        "Add a step after the pixel condition to use Skip next."
                                            .into(),
                                    );
                                }
                                if stop_pending() {
                                    stop_observed = true;
                                    continue;
                                }
                                control.clear_target_wait();
                                control.record_skipped_step(self.index + 1);
                                skip_next = true;
                                break;
                            }
                            if stable_pixel_match(
                                &mut matched_since,
                                matches,
                                Instant::now(),
                                Duration::from_millis((*stable_ms).into()),
                            ) {
                                if let Some(outcome) = control.wait_route(Instant::now())? {
                                    if stop_pending() {
                                        stop_observed = true;
                                        continue;
                                    }
                                    return Ok(outcome);
                                }
                                if stop_pending() {
                                    stop_observed = true;
                                    continue;
                                }
                                if Instant::now() >= deadline {
                                    continue;
                                }
                                control.clear_target_wait();
                                break;
                            }
                            PauseReason::PixelColor
                        }
                    };
                    let now = Instant::now();
                    let outcome = control.wait_for_target(
                        reason,
                        RETRY.min(deadline.saturating_duration_since(now)),
                        self.index,
                        deadline,
                    )?;
                    if stop_pending() {
                        stop_observed = true;
                        continue;
                    }
                    if let Some(outcome) = outcome {
                        return Ok(outcome);
                    }
                }
                if skip_next {
                    // A skip never crosses a loop boundary.
                    self.index += 1;
                }
                retries.finish();
            }
            if let Step::Action(action) = step
                && let Action::Text { text } = &action.action
            {
                let mut text_focus = None;
                for character in text.chars() {
                    if schedule
                        .limit
                        .is_some_and(|limit| control.route_actions() >= limit)
                    {
                        return Ok(control
                            .wait_route(Instant::now())?
                            .unwrap_or(Outcome::Completed));
                    }
                    match self.send_step(
                        step,
                        RouteInput::Text(character),
                        guard,
                        control,
                        &mut stop_pending,
                        Some(&mut text_focus),
                    )? {
                        StepResult::End(outcome) => return Ok(outcome),
                        StepResult::Sent(at) => last_action = Some(at),
                        StepResult::Timeout(error, _) => return Err(error),
                    }
                }
                self.index += 1;
                continue;
            }
            let input = match step {
                Step::Action(action) => match &action.action {
                    Action::Wait => None,
                    Action::Click { button, count } => Some(RouteInput::Click(*button, *count)),
                    Action::Scroll { direction, ticks } => {
                        Some(RouteInput::Scroll(*direction, *ticks))
                    }
                    Action::Key { key } => Some(RouteInput::Key(
                        crate::keys::parse(key)
                            .ok_or("The route key is unavailable on this keyboard layout.")?,
                    )),
                    Action::Text { .. } => unreachable!("text is sent character by character"),
                    Action::WaitColor { .. } | Action::WaitImage { .. } | Action::Flow { .. } => {
                        None
                    }
                    Action::Recorded { .. } => {
                        return Err(
                            "Recorded events cannot be mixed with other route steps.".into()
                        );
                    }
                },
                _ if settings.keyboard => return Err(INHERITED_ROUTE_OUTPUT.into()),
                _ if settings.scroll => Some(RouteInput::Scroll(
                    settings.scroll_direction,
                    settings.scroll_ticks()?,
                )),
                _ => Some(RouteInput::Click(settings.button, settings.click_count())),
            };
            if let Some(input) = input {
                let before = control.route_actions();
                let result = self.send_step(step, input, guard, control, &mut stop_pending, None);
                if control.route_actions() > before && retries.sent(self.index) {
                    control.record_click_retry();
                }
                match result? {
                    StepResult::End(outcome) => return Ok(outcome),
                    StepResult::Sent(at) => last_action = Some(at),
                    StepResult::Timeout(error, recoverable) => timeout!(error, recoverable),
                }
            }
            recovery_attempts[self.index] = 0;
            self.index += 1;
        }
    }

    fn recorded_sequence(
        &mut self,
        settings: &crate::settings::Settings,
        schedule: Schedule,
        guard: &mut Guard,
        control: &mut RunControl<'_>,
        stop_pending: &mut impl FnMut() -> bool,
    ) -> Result<Outcome, String> {
        use crate::recording::Event;
        let mut player = crate::mouse::MacroPlayer::new()?;
        let mut key_focus: Option<Focus> = None;
        loop {
            if !player.has_held()
                && schedule
                    .limit
                    .is_some_and(|limit| control.route_actions() >= limit)
            {
                return Ok(Outcome::Completed);
            }
            let Step::Action(step) = &self.steps[self.index] else {
                return Err("The recording contains another type of route step.".into());
            };
            let Action::Recorded { event } = &step.action else {
                return Err("The recording contains another type of route step.".into());
            };
            let deadline = Instant::now() + Duration::from_millis(step.delay_ms.into());
            loop {
                if stop_pending() {
                    return Ok(Outcome::Stopped);
                }
                match guard.check() {
                    Gate::Stop(reason) => return Ok(Outcome::Guarded(reason)),
                    Gate::Paused(_) if player.has_held() => {
                        return Ok(Outcome::Guarded(
                            "A safety condition changed while an input was held.",
                        ));
                    }
                    _ => {}
                }
                if player.has_held_keys()
                    && key_focus.is_none_or(|focus| !focus.current(foreground_focus))
                {
                    return Ok(Outcome::Guarded(
                        "Keyboard focus changed while a recorded key was held.",
                    ));
                }
                if player.has_held_keys()
                    && key_focus.is_some_and(|focus| {
                        !matches!(guard.check_route_key(focus.recipient_pid), Gate::Open)
                    })
                {
                    return Ok(Outcome::Guarded("Keyboard input is no longer permitted."));
                }
                if player.has_held_buttons() {
                    let cursor = crate::mouse::cursor().map_err(|_| "Cursor access was lost.")?;
                    if !matches!(guard.check_destination(Some(cursor)), Gate::Open) {
                        return Ok(Outcome::Guarded("Mouse input is no longer permitted."));
                    }
                }
                let now = Instant::now();
                if let Some(outcome) = control.wait_route(deadline.min(now + RESPONSE_POLL))? {
                    return Ok(outcome);
                }
                if now >= deadline {
                    break;
                }
            }
            let dispatch_deadline = Instant::now() + self.timeout;
            loop {
                if stop_pending() {
                    return Ok(Outcome::Stopped);
                }
                let mut authorized_focus = None;
                let mut authorized_mouse = None;
                let gate = match event {
                    Event::Key { .. } => {
                        match Focus::authorize_key(foreground_focus, |pid| {
                            guard.check_route_key(pid)
                        }) {
                            Ok(focus) if key_focus.is_none_or(|saved| saved == focus) => {
                                authorized_focus = Some(focus);
                                Gate::Open
                            }
                            Ok(_) => Gate::Paused(PauseReason::KeyboardFocus),
                            Err(gate) => gate,
                        }
                    }
                    Event::Move(point)
                    | Event::Button { point, .. }
                    | Event::Scroll { point, .. } => {
                        let hit = unsafe {
                            wm::WindowFromPoint(POINT {
                                x: point.x,
                                y: point.y,
                            })
                        };
                        let pid = window_pid(hit.0);
                        if hit.is_invalid() || pid == 0 || pid == std::process::id() {
                            Gate::Paused(PauseReason::TargetUnavailable)
                        } else {
                            authorized_mouse = Some((hit.0 as usize, pid));
                            guard.check_destination(Some(*point))
                        }
                    }
                };
                match gate {
                    Gate::Stop(reason) => return Ok(Outcome::Guarded(reason)),
                    Gate::Paused(_) if player.has_held() => {
                        return Ok(Outcome::Guarded(
                            "A safety condition changed while an input was held.",
                        ));
                    }
                    Gate::Paused(_) => {
                        if Instant::now() >= dispatch_deadline {
                            return Err(self.timed_out(
                                &self.steps[self.index].label(),
                                "The recorded input target was unavailable.",
                            ));
                        }
                        if let Some(outcome) = control.retry_dispatch(gate)? {
                            return Ok(outcome);
                        }
                        continue;
                    }
                    Gate::Open => {}
                }
                if let Some(outcome) = control.begin_dispatch() {
                    return Ok(outcome);
                }
                if stop_pending() {
                    return Ok(Outcome::Stopped);
                }
                if matches!(event, Event::Key { down: true, .. }) && !player.has_held_keys() {
                    key_focus = authorized_focus;
                }
                if key_focus.is_some_and(|focus| !focus.current(foreground_focus))
                    || authorized_focus.is_some_and(|focus| !focus.current(foreground_focus))
                {
                    return Ok(Outcome::Guarded(
                        "Keyboard focus changed before recorded input was sent.",
                    ));
                }
                if let (
                    Some((window, pid)),
                    Event::Move(point) | Event::Button { point, .. } | Event::Scroll { point, .. },
                ) = (authorized_mouse, event)
                {
                    let hit = unsafe {
                        wm::WindowFromPoint(POINT {
                            x: point.x,
                            y: point.y,
                        })
                    };
                    if hit.0 as usize != window
                        || window_pid(hit.0) != pid
                        || !matches!(guard.check_destination(Some(*point)), Gate::Open)
                    {
                        return Ok(Outcome::Guarded(
                            "The recorded mouse target changed before input was sent.",
                        ));
                    }
                }
                player.send(event)?;
                if matches!(
                    event,
                    Event::Key { down: true, .. }
                        | Event::Button { down: true, .. }
                        | Event::Scroll { .. }
                ) {
                    control.record_tap();
                }
                if !player.has_held_keys() {
                    key_focus = None;
                }
                break;
            }
            self.index += 1;
            if self.index == self.steps.len() {
                if settings.route_once {
                    return Ok(Outcome::Completed);
                }
                self.index = 0;
            }
        }
    }

    fn test_workflow_pixel(
        &self,
        test: &crate::workflow::PixelTest,
        guard: &mut Guard,
        control: &mut RunControl<'_>,
        stop_pending: &mut impl FnMut() -> bool,
    ) -> Result<Result<bool, Outcome>, String> {
        let deadline = Instant::now() + self.timeout;
        let mut stop_observed = false;
        loop {
            stop_observed |= stop_pending();
            if stop_observed {
                if let Some(outcome) = wait_for_pending_stop(control)? {
                    return Ok(Err(outcome));
                }
                continue;
            }
            if let Some(outcome) = control.wait_route(Instant::now())? {
                if stop_pending() {
                    stop_observed = true;
                    continue;
                }
                return Ok(Err(outcome));
            }
            match guard.check() {
                Gate::Stop(reason) => return Ok(Err(Outcome::Guarded(reason))),
                gate if Instant::now() >= deadline => {
                    if stop_pending() {
                        stop_observed = true;
                        continue;
                    }
                    let hint = match gate {
                        Gate::Paused(reason) => crate::pause_hint(reason),
                        _ => "The condition could not be evaluated in time.",
                    };
                    return Err(self.timed_out("the workflow condition", hint));
                }
                Gate::Open => {
                    let value = self.validate_layout(test.point).and_then(|()| {
                        crate::target_pixels::pixel_rgb(test.point)
                            .ok_or_else(|| "The condition's screen pixel could not be read.".into())
                    });
                    if stop_pending() {
                        stop_observed = true;
                        continue;
                    }
                    if let Some(outcome) = control.wait_route(Instant::now())? {
                        return Ok(Err(outcome));
                    }
                    control.clear_target_wait();
                    return Ok(Ok(crate::target_pixels::pixel_condition_matches(
                        value?,
                        test.rgb,
                        test.tolerance,
                        test.different,
                    )));
                }
                Gate::Paused(reason) => {
                    let interval = RETRY.min(deadline.saturating_duration_since(Instant::now()));
                    if let Some(outcome) =
                        control.wait_for_target(reason, interval, self.index, deadline)?
                    {
                        if stop_pending() {
                            stop_observed = true;
                            continue;
                        }
                        return Ok(Err(outcome));
                    }
                }
            }
        }
    }

    fn send_step(
        &self,
        step: &Step,
        input: RouteInput,
        guard: &mut Guard,
        control: &mut RunControl<'_>,
        stop_pending: &mut impl FnMut() -> bool,
        mut text_focus: Option<&mut Option<Focus>>,
    ) -> Result<StepResult, String> {
        let deadline = Instant::now() + self.timeout;
        let mut last_reason = PauseReason::TargetMissing;
        let mut request = None;
        let key = matches!(input, RouteInput::Key(_) | RouteInput::Text(_));
        let scroll = matches!(input, RouteInput::Scroll(..));
        let clicked = 'send: loop {
            if let Some(outcome) = control.wait_route(Instant::now())? {
                return Ok(StepResult::End(outcome));
            }
            let (reason, retry) = match guard.check() {
                Gate::Stop(message) => return Ok(StepResult::End(Outcome::Guarded(message))),
                Gate::Paused(reason) => (reason, RESPONSE_POLL),
                Gate::Open => 'ready: {
                    let target = if let Some(saved) = step.control() {
                        let service = self
                            .service
                            .as_ref()
                            .ok_or("Windows UI Automation is unavailable.")?;
                        let Some(result) = poll(service, &mut request, &saved.selector, None, key)?
                        else {
                            break 'ready (last_reason, RESPONSE_POLL);
                        };
                        match result {
                            Resolution::Ready(target) | Resolution::Recovered(target) => {
                                Some(target)
                            }
                            _ => {
                                last_reason = result.pause();
                                break 'ready (last_reason, RETRY);
                            }
                        }
                    } else {
                        None
                    };
                    let point = target
                        .as_ref()
                        .map(|target| target.point)
                        .or_else(|| step.point());
                    if target.is_none()
                        && let Some(point) = point
                    {
                        self.validate_layout(point)?;
                    }
                    let key_focus = if key {
                        Focus::authorize_key(foreground_focus, |pid| guard.check_route_key(pid))
                            .map(Some)
                    } else {
                        match guard.check_destination(point) {
                            Gate::Open => Ok(None),
                            gate => Err(gate),
                        }
                    };
                    let key_focus = match key_focus {
                        Ok(focus) => focus,
                        Err(Gate::Stop(reason)) => {
                            return Ok(StepResult::End(Outcome::Guarded(reason)));
                        }
                        Err(Gate::Paused(reason)) => break 'ready (reason, RESPONSE_POLL),
                        Err(Gate::Open) => unreachable!(),
                    };
                    if !text_focus_matches(text_focus.as_ref().and_then(|saved| **saved), key_focus)
                    {
                        break 'ready (PauseReason::KeyboardFocus, RESPONSE_POLL);
                    }
                    if input.is_held() {
                        break 'ready (PauseReason::InputGrabbed, RESPONSE_POLL);
                    }
                    let focus = if scroll {
                        foreground_focus()
                    } else {
                        key_focus
                    };
                    let native = if !key {
                        point.and_then(NativeHit::at)
                    } else {
                        None
                    };
                    let focus_ok = if key {
                        focus.is_some_and(|focus| {
                            target.as_ref().is_none_or(|target| {
                                focus.window == target.native.window
                                    && focus.leaf == target.control_window
                            })
                        })
                    } else if scroll {
                        focus
                            .zip(native)
                            .is_some_and(|(focus, hit)| focus.window == hit.window)
                    } else {
                        true
                    };
                    if !focus_ok {
                        break 'ready (
                            if scroll {
                                PauseReason::WheelFocus
                            } else {
                                PauseReason::KeyboardFocus
                            },
                            RESPONSE_POLL,
                        );
                    }
                    if !key && native.is_none() {
                        break 'ready (PauseReason::TargetUnavailable, RESPONSE_POLL);
                    }
                    if Instant::now() >= deadline
                        || stop_pending()
                        || target.as_ref().is_some_and(|target| !target.current())
                    {
                        break 'ready (PauseReason::TargetUnavailable, RESPONSE_POLL);
                    }
                    let destination = if key { None } else { point };
                    match input.send(destination, control, || {
                        !stop_pending()
                            && !input.is_held()
                            && focus.is_none_or(|focus| focus.current(foreground_focus))
                            && (key || point.and_then(NativeHit::at) == native)
                    })? {
                        RouteDelivery::Retry => {
                            break 'ready (PauseReason::TargetUnavailable, RESPONSE_POLL);
                        }
                        RouteDelivery::End(end) => return Ok(StepResult::End(end)),
                        RouteDelivery::Sent => {
                            if let Some(saved) = text_focus.as_deref_mut() {
                                *saved = key_focus;
                            }
                            break 'send (target, Instant::now());
                        }
                    }
                }
            };
            if let Some(outcome) = control.wait_for_target(
                reason,
                retry.min(deadline.saturating_duration_since(Instant::now())),
                self.index,
                deadline,
            )? {
                return Ok(StepResult::End(outcome));
            }
            if Instant::now() >= deadline {
                return Ok(StepResult::Timeout(
                    self.timed_out(&step.label(), crate::pause_hint(reason)),
                    matches!(
                        reason,
                        PauseReason::TargetMissing | PauseReason::TargetAmbiguous
                    ),
                ));
            }
        };
        if let Some(saved) = step.control().filter(|saved| saved.wait_for_disappearance)
            && let Some(target) = clicked.0
            && let Some(outcome) = self.verify(&saved.selector, target, guard, control)?
        {
            return Ok(StepResult::End(outcome));
        }
        Ok(StepResult::Sent(clicked.1))
    }

    pub fn dispatch(
        &mut self,
        injector: &Injector,
        guard: &mut Guard,
        control: &mut RunControl<'_>,
        mut stop_pending: impl FnMut() -> bool,
    ) -> Result<Option<Outcome>, String> {
        let step = &self.steps[self.index];
        let deadline = Instant::now() + self.timeout;
        let mut target_reason = PauseReason::TargetMissing;
        let clicked = 'dispatch: loop {
            let (reason, retry) = match guard.check() {
                Gate::Stop(message) => return Ok(Some(Outcome::Guarded(message))),
                Gate::Paused(reason) => (reason, RESPONSE_POLL),
                Gate::Open => 'ready: {
                    let (point, target) = match step {
                        Step::Point(point) => {
                            self.validate_layout(*point)?;
                            (*point, None)
                        }
                        Step::Control(step) => {
                            let Some(result) = poll(
                                self.service.as_ref().unwrap(),
                                &mut self.reply,
                                &step.selector,
                                None,
                                false,
                            )?
                            else {
                                break 'ready (target_reason, RESPONSE_POLL);
                            };
                            let target = match result {
                                Resolution::Ready(target) | Resolution::Recovered(target) => target,
                                _ => {
                                    target_reason = result.pause();
                                    break 'ready (target_reason, RETRY);
                                }
                            };
                            (target.point, Some(target))
                        }
                        Step::Action(_) => unreachable!("mixed routes use the sequence runner"),
                    };
                    match guard.check_destination(Some(point)) {
                        Gate::Stop(reason) => return Ok(Some(Outcome::Guarded(reason))),
                        Gate::Paused(reason) => (reason, RESPONSE_POLL),
                        Gate::Open if injector.output_held() => {
                            (PauseReason::InputGrabbed, RESPONSE_POLL)
                        }
                        Gate::Open
                            if target.as_ref().is_some_and(|target| !target.current())
                                || Instant::now() >= deadline
                                || stop_pending() =>
                        {
                            (PauseReason::TargetUnavailable, RESPONSE_POLL)
                        }
                        Gate::Open => {
                            if let Some(end) = injector.tap_at(point, control)? {
                                return Ok(Some(end));
                            }
                            break 'dispatch target;
                        }
                    }
                }
            };
            if let Some(end) = control.wait_for_target(
                reason,
                retry.min(deadline.saturating_duration_since(Instant::now())),
                self.index,
                deadline,
            )? {
                return Ok(Some(end));
            }
            if Instant::now() >= deadline {
                return Err(self.timed_out(&step.label(), crate::pause_hint(reason)));
            }
        };
        if let Step::Control(step) = step
            && step.wait_for_disappearance
            && let Some(target) = clicked
            && let Some(end) = self.verify(&step.selector, target, guard, control)?
        {
            return Ok(Some(end));
        }
        self.index = (self.index + 1) % self.steps.len();
        Ok(None)
    }

    fn verify(
        &self,
        selector: &Selector,
        clicked: Target,
        guard: &mut Guard,
        control: &mut RunControl<'_>,
    ) -> Result<Option<Outcome>, String> {
        let deadline = Instant::now() + self.timeout;
        let mut missing = false;
        let mut request = None;
        loop {
            if let Gate::Stop(reason) = guard.check() {
                return Ok(Some(Outcome::Guarded(reason)));
            }
            let result = poll(
                self.service.as_ref().unwrap(),
                &mut request,
                selector,
                Some(clicked.clone()),
                false,
            )?;
            match &result {
                Some(Resolution::Missing) if missing => {
                    control.clear_target_wait();
                    return Ok(None);
                }
                Some(status) => missing = matches!(status, Resolution::Missing),
                None => {}
            }
            if Instant::now() >= deadline {
                return Err(self.timed_out(
                    &display_label(&selector.label),
                    "The click was sent, but the control did not disappear.",
                ));
            }
            let retry = if result.is_some() {
                RETRY
            } else {
                RESPONSE_POLL
            };
            if let Some(end) = control.wait_for_target(
                PauseReason::VerifyingTarget,
                retry.min(deadline.saturating_duration_since(Instant::now())),
                self.index,
                deadline,
            )? {
                return Ok(Some(if end == Outcome::Completed {
                    Outcome::Guarded("Time limit reached before the last click could be verified.")
                } else {
                    end
                }));
            }
        }
    }

    fn timed_out(&self, label: &str, reason: &str) -> String {
        format!(
            "Step {} timed out waiting for {label}. {reason}",
            self.index + 1
        )
    }

    fn validate_layout(&self, point: Point) -> Result<(), String> {
        if self
            .layouts
            .as_ref()
            .is_some_and(|layout| *layout != crate::mouse::display_areas())
        {
            return Err(
                "The display layout changed. Review the coordinate points before running again."
                    .into(),
            );
        }
        if !on_monitor(point) {
            return Err("A saved point is outside your displays.".into());
        }
        Ok(())
    }
}

enum StepResult {
    Timeout(String, bool),
    Sent(Instant),
    End(Outcome),
}

fn wait_delay(
    control: &mut RunControl<'_>,
    guard: &mut Guard,
    deadline: Instant,
) -> Result<Option<Outcome>, String> {
    loop {
        if let Gate::Stop(reason) = guard.check() {
            return Ok(Some(Outcome::Guarded(reason)));
        }
        let now = Instant::now();
        if let Some(outcome) = control.wait_route(deadline.min(now + RESPONSE_POLL))? {
            return Ok(Some(outcome));
        }
        if now >= deadline {
            return Ok(None);
        }
    }
}

fn poll(
    service: &Service,
    request: &mut Option<Request<Resolution>>,
    selector: &Selector,
    clicked: Option<Target>,
    key: bool,
) -> Result<Option<Resolution>, String> {
    if request.is_none() {
        *request = Some(service.resolve(selector.clone(), clicked, key)?);
    }
    match request.as_ref().unwrap().try_recv() {
        Ok(result) => {
            *request = None;
            result.map(Some)
        }
        Err(TryRecvError::Empty) => Ok(None),
        Err(TryRecvError::Disconnected) => Err("The Windows UI Automation worker stopped.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_retry_budget_is_bounded_and_resets_after_a_condition() {
        let mut retries = PixelRetries::default();
        retries.enter(2);
        assert!(retries.available(2));
        assert_eq!(retries.schedule(2), Some(1));
        assert!(!retries.sent(0));
        assert!(retries.sent(1));
        retries.enter(2);
        assert_eq!(retries.schedule(2), Some(1));
        assert!(!retries.available(2));
        assert_eq!(retries.schedule(2), None);
        assert!(retries.sent(1));
        retries.finish();
        retries.enter(2);
        assert!(retries.available(2));
        assert_eq!(retries.schedule(2), Some(1));
        retries.finish();
        retries.enter(0);
        assert_eq!(retries.schedule(2), None);
    }

    #[test]
    fn one_time_pixel_check_branches_only_on_a_mismatch() {
        assert_eq!(immediate_pixel_failure(false, false, true), None);
        assert_eq!(immediate_pixel_failure(true, true, true), None);
        assert_eq!(immediate_pixel_failure(true, false, false), Some(false));
        assert_eq!(immediate_pixel_failure(true, false, true), Some(true));
    }

    #[test]
    fn pending_shortcut_waits_for_stop_even_when_run_time_expires() {
        let stop = Arc::new(crate::engine::StopSignal::new().unwrap());
        let delayed = stop.clone();
        let (ready, started) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            started.recv().unwrap();
            std::thread::sleep(Duration::from_millis(40));
            delayed.request();
        });
        let progress = crate::engine::Progress::default();
        let result = crate::engine::run_sequence(
            Schedule {
                duration: Some(Duration::from_millis(5)),
                ..Schedule::default()
            },
            &stop,
            &progress,
            None,
            |control| {
                ready.send(()).unwrap();
                loop {
                    if let Some(outcome) = wait_for_pending_stop(control)? {
                        return Ok(outcome);
                    }
                }
            },
        );
        thread.join().unwrap();
        assert_eq!(result, Ok(Outcome::Stopped));
        assert_eq!(progress.actions(), 0);
    }

    #[test]
    fn pixel_timeout_skip_never_overrides_stop_or_guard() {
        assert!(may_skip_pixel_timeout(true, Gate::Open, false));
        assert!(!may_skip_pixel_timeout(false, Gate::Open, false));
        assert!(!may_skip_pixel_timeout(true, Gate::Open, true));
        assert!(!may_skip_pixel_timeout(
            true,
            Gate::Paused(PauseReason::KeyboardFilter),
            false
        ));
        assert!(!may_skip_pixel_timeout(true, Gate::Stop("stop"), false));
    }

    #[test]
    fn pixel_stability_resets_after_a_mismatch() {
        let start = Instant::now();
        let required = Duration::from_millis(200);
        let mut since = None;
        assert!(!stable_pixel_match(&mut since, true, start, required));
        assert!(!stable_pixel_match(
            &mut since,
            true,
            start + Duration::from_millis(100),
            required
        ));
        assert!(!stable_pixel_match(
            &mut since,
            false,
            start + Duration::from_millis(150),
            required
        ));
        assert!(!stable_pixel_match(
            &mut since,
            true,
            start + Duration::from_millis(200),
            required
        ));
        assert!(!stable_pixel_match(
            &mut since,
            true,
            start + Duration::from_millis(399),
            required
        ));
        assert!(stable_pixel_match(
            &mut since,
            true,
            start + Duration::from_millis(400),
            required
        ));
        assert!(stable_pixel_match(&mut None, true, start, Duration::ZERO));
    }

    #[test]
    fn finite_mixed_route_finishes_only_after_the_last_step() {
        let mut index = 0;
        let mut completed = 0;
        for _ in 0..2 {
            assert!(!advance_route(
                &mut index,
                3,
                &mut completed,
                false,
                Some(2)
            ));
            assert!(!advance_route(
                &mut index,
                3,
                &mut completed,
                false,
                Some(2)
            ));
            if completed == 0 {
                assert!(!advance_route(
                    &mut index,
                    3,
                    &mut completed,
                    false,
                    Some(2)
                ));
                assert_eq!((index, completed), (0, 1));
            } else {
                assert!(advance_route(&mut index, 3, &mut completed, false, Some(2)));
                assert_eq!((index, completed), (3, 2));
            }
        }
    }

    #[test]
    fn route_key_rechecks_the_focus_used_for_authorization() {
        use std::cell::Cell;

        let allowed = Focus {
            window: 100,
            leaf: 101,
            recipient_pid: 200,
        };
        let blocked = Focus {
            window: 300,
            leaf: 301,
            recipient_pid: 400,
        };
        let foreground = Cell::new(Some(allowed));
        let check_recipient = |pid| {
            if pid == allowed.recipient_pid {
                foreground.set(Some(blocked));
                Gate::Open
            } else {
                Gate::Paused(PauseReason::KeyboardFilter)
            }
        };

        let authorized = Focus::authorize_key(|| foreground.get(), check_recipient)
            .ok()
            .unwrap();
        assert!(authorized.current(|| Some(allowed)));
        assert!(!authorized.current(|| foreground.get()));
        assert!(matches!(
            Focus::authorize_key(|| foreground.get(), check_recipient),
            Err(Gate::Paused(PauseReason::KeyboardFilter))
        ));
        assert!(!allowed.current(|| Some(Focus {
            leaf: 102,
            ..allowed
        })));
        // A reused HWND must not inherit authorization for the old process.
        assert!(!allowed.current(|| Some(Focus {
            recipient_pid: 400,
            ..allowed
        })));
        assert!(!allowed.current(|| None));
        assert!(text_focus_matches(None, Some(allowed)));
        assert!(text_focus_matches(Some(allowed), Some(allowed)));
        assert!(!text_focus_matches(Some(allowed), Some(blocked)));
        assert!(!text_focus_matches(Some(allowed), None));
    }

    #[test]
    fn dry_run_reports_key_focus_requirements_for_resolved_controls() {
        let target = Target {
            point: Point { x: 10, y: 10 },
            bounds: Zone {
                from: Point { x: 0, y: 0 },
                to: Point { x: 20, y: 20 },
            },
            native: NativeHit {
                window: 1,
                leaf: 1,
                pid: 10,
                rect: (0, 0, 20, 20),
                leaf_rect: (0, 0, 20, 20),
                dpi: 96,
            },
            observed_at: Instant::now(),
            control_window: 1,
            visual_check: None,
        };
        let shared_handle = Resolution::Ready(target.clone());
        assert_eq!(
            shared_handle.preview_status(false),
            (false, "Control located and ready.")
        );
        for control_window in [0, 1] {
            let resolution = Resolution::Ready(Target {
                control_window,
                ..target.clone()
            });
            assert_eq!(
                resolution.preview_status(true),
                (
                    true,
                    "This control has no distinct native focus handle. Use a foreground key step."
                )
            );
        }
        let distinct_handle = Resolution::Ready(Target {
            control_window: 2,
            ..target
        });
        assert_eq!(
            distinct_handle.preview_status(true),
            (false, "Focus this control before the key step runs.")
        );
    }
}
