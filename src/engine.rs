use std::{
    hint::spin_loop,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::null,
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0},
    System::Threading::{
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateEventW, CreateWaitableTimerExW,
        GetCurrentThread, INFINITE, SetEvent, SetThreadPriority, SetWaitableTimer,
        THREAD_PRIORITY_ABOVE_NORMAL, TIMER_ALL_ACCESS, WaitForMultipleObjects,
        WaitForSingleObject,
    },
};

// Bound the number of taps submitted before the next stop check.
pub const BURST_SIZE: u32 = 4;
const SPIN_TAIL: Duration = Duration::from_micros(75);
pub const GUARD_POLL_INTERVAL: Duration = Duration::from_millis(10);
const FASTEST_LOAD_PACED_INTERVAL: Duration = Duration::from_millis(1);
const PRIORITY_PACED_INTERVAL: Duration = Duration::from_millis(2);
const MAX_RECOVERABLE_DELAY: Duration = Duration::from_millis(8);

pub struct StopSignal {
    requested: AtomicBool,
    event: OwnedHandle,
}

impl StopSignal {
    pub fn new() -> io::Result<Self> {
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        Ok(Self {
            requested: AtomicBool::new(false),
            event: own_handle(event)?,
        })
    }

    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
        // The worker holds the same Arc while it waits.
        unsafe { SetEvent(self.event.as_raw_handle()) };
    }

    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

fn own_handle(handle: HANDLE) -> io::Result<OwnedHandle> {
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

#[derive(Default)]
struct Pacer {
    timer: Option<OwnedHandle>,
    spin_tail: Duration,
}

impl Pacer {
    fn new() -> Self {
        let timer = |flags| {
            own_handle(unsafe { CreateWaitableTimerExW(null(), null(), flags, TIMER_ALL_ACCESS) })
        };
        match timer(CREATE_WAITABLE_TIMER_HIGH_RESOLUTION) {
            Ok(timer) => Self {
                timer: Some(timer),
                spin_tail: SPIN_TAIL,
            },
            Err(_) => Self {
                timer: timer(0).ok(),
                spin_tail: Duration::ZERO,
            },
        }
    }

    fn wait_until(
        &self,
        stop: &StopSignal,
        deadline: Instant,
        spin_tail: Duration,
    ) -> io::Result<bool> {
        let stop_event = stop.event.as_raw_handle();
        while !stop.requested() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(true);
            }
            if remaining <= spin_tail {
                spin_loop();
                continue;
            }

            let sleep = remaining - spin_tail;
            let mut wait = WAIT_FAILED;
            if let Some(timer) = &self.timer {
                let timer = timer.as_raw_handle();
                // A negative due time is relative, in 100 ns units.
                let due = -(sleep.as_nanos().div_ceil(100) as i64);
                if unsafe { SetWaitableTimer(timer, &due, 0, None, null(), 0) } != 0 {
                    let handles = [stop_event, timer];
                    wait = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
                }
            }
            if wait == WAIT_FAILED {
                let millis = sleep
                    .as_micros()
                    .div_ceil(1_000)
                    .clamp(1, (INFINITE - 1) as u128);
                wait = unsafe { WaitForSingleObject(stop_event, millis as u32) };
            }
            match wait {
                WAIT_OBJECT_0 => return Ok(false),
                WAIT_FAILED => return Err(io::Error::last_os_error()),
                _ => {}
            }
        }
        Ok(false)
    }
}

#[derive(Default)]
pub struct Progress {
    actions: AtomicU64,
    pause: AtomicU8,
    pub started: OnceLock<Instant>,
}

impl Progress {
    pub fn actions(&self) -> u64 {
        self.actions.load(Ordering::Relaxed)
    }

    pub fn pause_reason(&self) -> Option<PauseReason> {
        match self.pause.load(Ordering::Relaxed) {
            1 => Some(PauseReason::OutsideZone),
            2 => Some(PauseReason::Modifiers),
            3 => Some(PauseReason::KeyboardFocus),
            4 => Some(PauseReason::KeyboardFilter),
            5 => Some(PauseReason::PointerFilter),
            6 => Some(PauseReason::TargetFilter),
            7 => Some(PauseReason::CursorConfined),
            _ => None,
        }
    }

    fn pause(&self, reason: PauseReason) {
        self.pause.store(reason as u8, Ordering::Relaxed);
    }

    fn clear_pause(&self) {
        if self.pause.load(Ordering::Relaxed) != 0 {
            self.pause.store(0, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Schedule {
    pub interval: Duration,
    pub delay: Duration,
    pub limit: Option<u64>,
    pub duration: Option<Duration>,
    pub rates: Option<(f64, f64)>,
    pub low_cpu: bool,
}

pub fn low_cpu_batch_size(interval: Duration) -> u32 {
    if !(FASTEST_LOAD_PACED_INTERVAL..=Duration::from_millis(20)).contains(&interval) {
        1
    } else if interval < PRIORITY_PACED_INTERVAL {
        3
    } else {
        2
    }
}

impl Schedule {
    pub fn uses_load_pacing(self, batchable: bool, guarded: bool) -> bool {
        batchable
            && self.rates.is_none()
            && self.interval >= FASTEST_LOAD_PACED_INTERVAL
            && (!guarded || self.interval <= PRIORITY_PACED_INTERVAL)
    }

    pub fn uses_priority_pacing(self, load_pacing: bool) -> bool {
        load_pacing && self.interval <= PRIORITY_PACED_INTERVAL
    }

    pub fn prioritize_paced_worker() {
        // Give short load-paced intervals a better chance to wake on time.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Stopped,
    Guarded(&'static str),
}

#[derive(Clone, Copy)]
pub enum Gate {
    Open,
    Paused(PauseReason),
    Stop(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PauseReason {
    OutsideZone = 1,
    Modifiers = 2,
    KeyboardFocus = 3,
    KeyboardFilter = 4,
    PointerFilter = 5,
    TargetFilter = 6,
    CursorConfined = 7,
}

#[derive(Default)]
pub struct Checks<'a> {
    pub poll: Option<&'a mut dyn FnMut() -> Gate>,
    pub destination: Option<&'a mut dyn FnMut() -> Result<Gate, String>>,
    // One destination check can cover a packet that repeats the same point.
    pub batch_destination: bool,
}

enum Wait {
    Ready(Instant),
    Paused,
    End(Outcome),
}

pub struct RunControl<'a> {
    stop: &'a StopSignal,
    gate: Option<&'a mut dyn FnMut() -> Gate>,
    guard_stop: Option<&'static str>,
    pacer: Pacer,
    progress: &'a Progress,
    duration: Option<Duration>,
    expires: Option<Instant>,
}

impl RunControl<'_> {
    fn interruption(&self) -> Option<Outcome> {
        if self.stop.requested() {
            Some(Outcome::Stopped)
        } else {
            self.guard_stop.map(Outcome::Guarded)
        }
    }

    fn expired(&self) -> bool {
        self.expires.is_some_and(|end| Instant::now() >= end)
    }

    pub fn begin_dispatch(&mut self) -> Option<Outcome> {
        if self.stop.requested() {
            return Some(Outcome::Stopped);
        }
        if self.expired() {
            return Some(Outcome::Completed);
        }
        self.progress.started.get_or_init(|| {
            let now = Instant::now();
            self.expires = self.duration.map(|duration| now + duration);
            now
        });
        None
    }

    fn sleep(&self, deadline: Instant, spin: Duration) -> Result<(), String> {
        let deadline = self.expires.map_or(deadline, |end| deadline.min(end));
        self.pacer
            .wait_until(self.stop, deadline, spin)
            .map(|_| ())
            .map_err(|err| format!("Windows timer failed ({err})."))
    }

    fn wait(
        &mut self,
        deadline: Instant,
        spin: Duration,
        allow_pause: bool,
        mut held_gate: Option<(&mut dyn FnMut() -> Gate, Duration)>,
    ) -> Result<Wait, String> {
        loop {
            if let Some(outcome) = self.interruption() {
                return Ok(Wait::End(outcome));
            }
            let now = Instant::now();
            if self.expires.is_some_and(|end| now >= end) {
                return Ok(Wait::End(Outcome::Completed));
            }
            let mut wake = deadline;
            let (gate, poll_interval) = match &mut held_gate {
                Some((check, interval)) => (Some(check()), *interval),
                None => (self.gate.as_mut().map(|check| check()), GUARD_POLL_INTERVAL),
            };
            if let Some(gate) = gate {
                match gate {
                    Gate::Stop(reason) => {
                        self.guard_stop = Some(reason);
                        return Ok(Wait::End(Outcome::Guarded(reason)));
                    }
                    Gate::Paused(reason) if allow_pause => {
                        self.progress.pause(reason);
                        return Ok(Wait::Paused);
                    }
                    _ => {}
                }
                wake = wake.min(now + poll_interval);
            }
            if now >= deadline {
                return Ok(Wait::Ready(now));
            }
            self.sleep(
                wake,
                if wake == deadline {
                    spin
                } else {
                    Duration::ZERO
                },
            )?;
        }
    }

    pub fn hold(
        &mut self,
        duration: Duration,
        destination: Option<(&mut dyn FnMut() -> Gate, Duration)>,
    ) -> Result<bool, String> {
        let result = self.wait(
            Instant::now() + duration,
            self.pacer.spin_tail,
            true,
            destination,
        )?;
        Ok(matches!(result, Wait::Ready(_)))
    }

    pub fn can_press(&mut self) -> Result<bool, String> {
        Ok(matches!(
            self.wait(Instant::now(), Duration::ZERO, true, None)?,
            Wait::Ready(_)
        ))
    }

    pub fn allow_press(&mut self, gate: Gate) -> bool {
        match gate {
            Gate::Open => !self.stop.requested() && !self.expired(),
            Gate::Paused(reason) => {
                self.progress.pause(reason);
                false
            }
            Gate::Stop(reason) => {
                self.guard_stop = Some(reason);
                false
            }
        }
    }
}

fn next_priority_deadline(
    phase: &mut Instant,
    ready: Instant,
    finished: Instant,
    step: Duration,
    grouped: bool,
) -> Instant {
    // Keep the intended cadence through short stalls. Discard older missed slots
    // and recover the rest over subsequent submissions with a minimum spacing.
    *phase += step;
    if finished.saturating_duration_since(*phase) > MAX_RECOVERABLE_DELAY {
        *phase = finished - MAX_RECOVERABLE_DELAY;
    }
    let minimum_gap = if grouped { step / 2 } else { step - step / 4 };
    (*phase).max(ready + minimum_gap)
}

fn next_tap_deadline(deadline: Instant, ready: Instant, pace: Duration) -> Instant {
    if ready.saturating_duration_since(deadline) > pace / 8 {
        ready + pace
    } else {
        deadline + pace
    }
}

pub struct Random(u64);

impl Random {
    pub fn new() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        Self(seed | 1)
    }

    pub fn unit(&mut self) -> f64 {
        let mut bits = self.0;
        bits ^= bits << 13;
        bits ^= bits >> 7;
        bits ^= bits << 17;
        self.0 = bits;
        (bits >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub fn run_controlled<'a>(
    schedule: Schedule,
    stop: &'a StopSignal,
    progress: &'a Progress,
    batchable: bool,
    mut checks: Checks<'a>,
    mut pending_stop: Option<&mut dyn FnMut() -> bool>,
    mut dispatch: impl FnMut(u32, &mut RunControl<'_>) -> Result<Option<Outcome>, String>,
) -> Result<Outcome, String> {
    let guarded =
        checks.poll.is_some() || (checks.destination.is_some() && !checks.batch_destination);
    let unpaced = batchable
        && schedule.interval.is_zero()
        && schedule.rates.is_none()
        && schedule.duration.is_none()
        && !guarded;
    let mut control = RunControl {
        stop,
        gate: checks.poll,
        guard_stop: None,
        pacer: if unpaced && schedule.delay.is_zero() {
            Pacer::default()
        } else {
            Pacer::new()
        },
        progress,
        duration: schedule.duration,
        expires: None,
    };
    let base_spin = control.pacer.spin_tail;
    if !schedule.delay.is_zero()
        && let Wait::End(outcome) =
            control.wait(Instant::now() + schedule.delay, base_spin, false, None)?
    {
        return Ok(outcome);
    }

    let mut random_rate = schedule.rates.map(|(low, high)| {
        let mut rng = Random::new();
        move || Duration::from_secs_f64((low + (high - low) * rng.unit()).recip())
    });
    let mut interval = || {
        random_rate
            .as_mut()
            .map_or(schedule.interval, |rate| rate())
    };
    let mut pace = interval();
    let burst = pace.is_zero();
    let batched = burst && batchable && !guarded;
    let paced_batch = if schedule.low_cpu && batchable && !guarded && schedule.rates.is_none() {
        low_cpu_batch_size(pace)
    } else {
        1
    };
    let grouped = paced_batch > 1;
    let max_batch = if batched { BURST_SIZE } else { paced_batch };
    let load_pacing = schedule.uses_load_pacing(batchable, guarded);
    let priority_pacing = schedule.uses_priority_pacing(load_pacing);
    let mut deadline = Instant::now();
    let mut phase = deadline;
    let mut draining_input = false;
    let mut actions = 0;

    loop {
        let remaining = schedule.limit.map_or(u64::MAX, |limit| limit - actions);
        if remaining == 0 {
            return Ok(control.interruption().unwrap_or(Outcome::Completed));
        }
        let ready = if unpaced {
            if stop.requested() {
                return Ok(Outcome::Stopped);
            }
            None
        } else {
            let spin = if grouped || draining_input {
                Duration::ZERO
            } else if priority_pacing && !base_spin.is_zero() {
                // Keep using the timer when recovery leaves less time between taps.
                Duration::from_micros(if deadline > phase { 600 } else { 750 })
            } else if pace <= Duration::from_micros(500) {
                pace
            } else if pace <= Duration::from_millis(2) && !base_spin.is_zero() {
                Duration::from_micros(if pace < Duration::from_millis(1) {
                    400
                } else {
                    325
                })
            } else {
                base_spin
            };
            let ready = match control.wait(deadline, spin, true, None)? {
                Wait::End(outcome) => return Ok(outcome),
                Wait::Paused => {
                    control.sleep(Instant::now() + Duration::from_millis(10), Duration::ZERO)?;
                    deadline = Instant::now();
                    phase = deadline;
                    continue;
                }
                Wait::Ready(ready) => ready,
            };
            Some(ready)
        };
        if let Some(check) = &mut checks.destination {
            match check()? {
                Gate::Open => {}
                Gate::Paused(reason) => {
                    progress.pause(reason);
                    control.sleep(Instant::now() + Duration::from_millis(10), Duration::ZERO)?;
                    deadline = Instant::now();
                    phase = deadline;
                    continue;
                }
                Gate::Stop(reason) => return Ok(Outcome::Guarded(reason)),
            }
        }
        if pending_stop.as_mut().is_some_and(|pending| pending()) {
            // Check after the destination lookup, just before dispatch.
            // The shortcut listener owns the toggle and signals the stop event.
            control.sleep(Instant::now() + Duration::from_millis(1), Duration::ZERO)?;
            deadline = Instant::now();
            phase = deadline;
            continue;
        }
        if stop.requested() {
            return Ok(Outcome::Stopped);
        }
        if control.expired() {
            return Ok(Outcome::Completed);
        }
        progress.clear_pause();
        let mut batch = remaining.min(u64::from(max_batch)) as u32;
        if grouped
            && let Some(time_left) = control.expires.map_or(schedule.duration, |end| {
                Some(end.saturating_duration_since(Instant::now()))
            })
        {
            // Keep only the clicks whose nominal slots fit before the time limit.
            batch = time_left
                .as_nanos()
                .div_ceil(pace.as_nanos())
                .clamp(1, u128::from(batch)) as u32;
        }
        if let Some(outcome) = dispatch(batch, &mut control)? {
            return Ok(outcome);
        }
        actions += u64::from(batch);
        progress.actions.store(actions, Ordering::Relaxed);
        if load_pacing {
            let step = pace * batch;
            let ready = ready.unwrap();
            let now = Instant::now();
            deadline = if priority_pacing {
                next_priority_deadline(&mut phase, ready, now, step, grouped)
            } else if grouped {
                // Recover batch cadence after a late wake without dispatching back to back.
                (deadline + step).max(ready + step / 2)
            } else {
                next_tap_deadline(deadline, ready, step)
            };
            draining_input = deadline <= now;
            if draining_input {
                // A slow input submission has already used the interval. Leave
                // a short, interruptible gap before issuing another action.
                let slack = if pace < Duration::from_millis(2) {
                    pace / 8
                } else {
                    pace.min(Duration::from_millis(4)) / 2
                };
                deadline = now + slack;
            }
        } else if !burst {
            pace = interval();
            deadline += pace;
            let now = Instant::now();
            if deadline <= now {
                deadline = now + pace;
            }
        }
    }
}

#[cfg(test)]
fn run(
    schedule: Schedule,
    stop: &StopSignal,
    progress: &Progress,
    mut dispatch: impl FnMut(u32) -> Result<(), String>,
) -> Result<Outcome, String> {
    run_controlled(
        schedule,
        stop,
        progress,
        true,
        Checks::default(),
        None,
        |size, control| match control.begin_dispatch() {
            Some(outcome) => Ok(Some(outcome)),
            None => dispatch(size).map(|()| None),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, thread};

    #[test]
    fn guarded_load_pacing_is_limited_to_fast_fixed_taps() {
        let schedule = Schedule {
            interval: Duration::from_millis(2),
            ..Schedule::default()
        };
        assert!(schedule.uses_load_pacing(true, false));
        assert!(schedule.uses_load_pacing(true, true));
        assert!(!schedule.uses_load_pacing(false, false));
        assert!(!schedule.uses_load_pacing(false, true));
        let slower = Schedule {
            interval: Duration::from_millis(10),
            ..schedule
        };
        assert!(slower.uses_load_pacing(true, false));
        assert!(!slower.uses_load_pacing(true, true));
        let faster = Schedule {
            interval: Duration::from_micros(999),
            ..schedule
        };
        assert!(!faster.uses_load_pacing(true, false));
        assert!(!faster.uses_load_pacing(true, true));
        let one_millisecond = Schedule {
            interval: Duration::from_millis(1),
            ..schedule
        };
        assert!(one_millisecond.uses_load_pacing(true, false));
        assert!(one_millisecond.uses_load_pacing(true, true));
        let random = Schedule {
            rates: Some((50.0, 100.0)),
            ..schedule
        };
        assert!(!random.uses_load_pacing(true, false));
        assert!(!random.uses_load_pacing(true, true));
        assert!(!Schedule::default().uses_load_pacing(true, false));
    }

    #[test]
    fn tap_deadline_preserves_phase_only_for_small_delays() {
        let slot = Instant::now();
        let pace = Duration::from_millis(2);
        let slightly_late = slot + Duration::from_micros(50);
        assert_eq!(next_tap_deadline(slot, slightly_late, pace), slot + pace);
        let missed_slot = slot + Duration::from_millis(19);
        assert_eq!(
            next_tap_deadline(slot, missed_slot, pace),
            missed_slot + pace
        );
    }

    #[test]
    fn fast_pacing_recovers_after_a_slow_submission_with_bounded_spacing() {
        let start = Instant::now();
        let finished = start + Duration::from_millis(30);
        for (step, grouped, minimum_gap) in [(2_000, false, 1_500), (4_000, true, 2_000)] {
            let step = Duration::from_micros(step);
            let minimum_gap = Duration::from_micros(minimum_gap);
            let mut phase = start;
            next_priority_deadline(&mut phase, start, finished, step, grouped);
            assert_eq!(phase, finished - MAX_RECOVERABLE_DELAY);

            let mut ready = finished + Duration::from_millis(1);
            assert_eq!(
                next_priority_deadline(&mut phase, ready, ready, step, grouped),
                ready + minimum_gap
            );
            ready += minimum_gap;
            for _ in 0..32 {
                if ready == phase {
                    break;
                }
                let next = next_priority_deadline(&mut phase, ready, ready, step, grouped);
                assert!(next >= ready + minimum_gap && next < ready + step);
                ready = next;
            }
            assert_eq!(ready, phase);
            assert_eq!(
                next_priority_deadline(&mut phase, ready, ready, step, grouped),
                ready + step
            );
        }
    }

    #[test]
    fn pending_stop_shortcut_blocks_new_dispatch() {
        let stop = StopSignal::new().unwrap();
        let mut pending = || {
            stop.request();
            true
        };
        let outcome = run_controlled(
            Schedule {
                interval: Duration::from_millis(1),
                ..Schedule::default()
            },
            &stop,
            &Progress::default(),
            true,
            Checks::default(),
            Some(&mut pending),
            |_, _| panic!("a pending stop must suppress the tap"),
        );
        assert_eq!(outcome, Ok(Outcome::Stopped));
    }

    #[test]
    fn guarded_fast_tap_checks_stop_after_the_destination_lookup() {
        let stop = StopSignal::new().unwrap();
        let destination_checked = std::cell::Cell::new(false);
        let mut destination = || {
            destination_checked.set(true);
            Ok(Gate::Open)
        };
        let mut pending = || {
            if destination_checked.get() {
                stop.request();
                true
            } else {
                false
            }
        };
        let outcome = run_controlled(
            Schedule {
                interval: Duration::from_millis(1),
                ..Schedule::default()
            },
            &stop,
            &Progress::default(),
            true,
            Checks {
                destination: Some(&mut destination),
                ..Checks::default()
            },
            Some(&mut pending),
            |_, _| panic!("a Stop during the destination lookup must suppress the tap"),
        );
        assert_eq!(outcome, Ok(Outcome::Stopped));
    }

    #[test]
    fn time_limit_blocks_dispatch_after_a_slow_destination_check() {
        let progress = Progress::default();
        let limit = Duration::from_millis(100);
        let mut checks = 0;
        let mut destination = || {
            checks += 1;
            if checks == 2 {
                thread::sleep(limit);
            }
            Ok(Gate::Open)
        };
        let result = run_controlled(
            Schedule {
                duration: Some(limit),
                ..Schedule::default()
            },
            &StopSignal::new().unwrap(),
            &progress,
            true,
            Checks {
                destination: Some(&mut destination),
                ..Checks::default()
            },
            None,
            |_, control| {
                assert_eq!(progress.actions(), 0);
                Ok(control.begin_dispatch())
            },
        );
        assert_eq!(result, Ok(Outcome::Completed));
        assert_eq!(checks, 2);
        assert_eq!(progress.actions(), 1);
    }

    #[test]
    fn paused_wait_does_not_outlast_the_time_limit() {
        let stop = StopSignal::new().unwrap();
        let progress = Progress::default();
        let control = RunControl {
            stop: &stop,
            gate: None,
            guard_stop: None,
            pacer: Pacer::default(),
            progress: &progress,
            duration: None,
            expires: Some(Instant::now()),
        };
        let (finished, watchdog) = std::sync::mpsc::channel();
        thread::scope(|scope| {
            let watchdog_stop = &stop;
            scope.spawn(move || {
                if watchdog.recv_timeout(Duration::from_secs(1)).is_err() {
                    watchdog_stop.request();
                }
            });
            let result = control.sleep(Instant::now() + Duration::from_secs(30), Duration::ZERO);
            finished.send(()).unwrap();
            assert!(result.is_ok());
            assert!(!stop.requested());
        });
    }

    #[test]
    fn burst_respects_exact_limit_across_batches() {
        let progress = Progress::default();
        let mut sizes = Vec::new();
        let limit = u64::from(BURST_SIZE) * 2 + 3;
        let result = run(
            Schedule {
                interval: Duration::ZERO,
                delay: Duration::ZERO,
                limit: Some(limit),
                ..Schedule::default()
            },
            &StopSignal::new().unwrap(),
            &progress,
            |size| {
                sizes.push(size);
                Ok(())
            },
        );
        assert_eq!(result, Ok(Outcome::Completed));
        assert_eq!(sizes, [BURST_SIZE, BURST_SIZE, 3]);
        assert_eq!(progress.actions(), limit);
    }

    #[test]
    fn low_cpu_paces_groups_and_caps_the_last_group_to_the_count() {
        for (millis, expected) in [(1, vec![3, 3, 1]), (2, vec![2, 2, 2, 1])] {
            let progress = Progress::default();
            let mut sizes = Vec::new();
            let result = run(
                Schedule {
                    interval: Duration::from_millis(millis),
                    limit: Some(7),
                    low_cpu: true,
                    ..Schedule::default()
                },
                &StopSignal::new().unwrap(),
                &progress,
                |size| {
                    sizes.push(size);
                    Ok(())
                },
            );
            assert_eq!(result, Ok(Outcome::Completed));
            assert_eq!(sizes, expected);
            assert_eq!(progress.actions(), 7);
        }
    }

    #[test]
    fn low_cpu_group_respects_a_short_time_limit() {
        let progress = Progress::default();
        let result = run(
            Schedule {
                interval: Duration::from_millis(2),
                duration: Some(Duration::from_millis(1)),
                low_cpu: true,
                ..Schedule::default()
            },
            &StopSignal::new().unwrap(),
            &progress,
            |size| {
                assert_eq!(size, 1);
                Ok(())
            },
        );
        assert_eq!(result, Ok(Outcome::Completed));
        assert_eq!(progress.actions(), 1);
    }

    #[test]
    fn unpaced_stop_prevents_another_batch() {
        let progress = Progress::default();
        let stop = StopSignal::new().unwrap();
        let result = run(Schedule::default(), &stop, &progress, |size| {
            assert!(!stop.requested());
            assert_eq!(size, BURST_SIZE);
            stop.request();
            Ok(())
        });
        assert_eq!(result, Ok(Outcome::Stopped));
        assert_eq!(progress.actions(), u64::from(BURST_SIZE));
    }

    #[test]
    fn stop_wakes_long_delay_without_dispatching() {
        let stop = Arc::new(StopSignal::new().unwrap());
        let worker_stop = stop.clone();
        let start = Instant::now();
        let worker = thread::spawn(move || {
            run(
                Schedule {
                    interval: Duration::from_secs(1),
                    delay: Duration::from_secs(3600),
                    limit: None,
                    ..Schedule::default()
                },
                &worker_stop,
                &Progress::default(),
                |_| panic!("stop must cancel the delay"),
            )
        });
        thread::sleep(Duration::from_millis(30));
        stop.request();
        assert_eq!(worker.join().unwrap(), Ok(Outcome::Stopped));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn dispatch_error_exits_without_retrying() {
        let progress = Progress::default();
        let result = run(
            Schedule {
                interval: Duration::ZERO,
                delay: Duration::ZERO,
                limit: None,
                ..Schedule::default()
            },
            &StopSignal::new().unwrap(),
            &progress,
            |_| Err("blocked".into()),
        );
        assert_eq!(result, Err("blocked".into()));
        assert_eq!(progress.actions(), 0);
    }

    #[test]
    fn destination_guards_run_before_the_first_action_clock() {
        let progress = Progress::default();
        let mut polls = 0;
        let mut destination = || {
            assert!(progress.started.get().is_none());
            polls += 1;
            Ok(if polls == 1 {
                Gate::Paused(PauseReason::TargetFilter)
            } else {
                assert_eq!(progress.pause_reason(), Some(PauseReason::TargetFilter));
                Gate::Stop("blocked destination")
            })
        };
        let result = run_controlled(
            Schedule::default(),
            &StopSignal::new().unwrap(),
            &progress,
            true,
            Checks {
                destination: Some(&mut destination),
                ..Checks::default()
            },
            None,
            |_, _| panic!("an unsafe destination must not dispatch"),
        );
        assert_eq!(result, Ok(Outcome::Guarded("blocked destination")));
        assert_eq!(progress.actions(), 0);
        assert!(progress.started.get().is_none());
    }

    #[test]
    fn confined_destinations_pause_before_dispatch_and_resume_with_safe_batch_sizes() {
        for batch_destination in [false, true] {
            let progress = Progress::default();
            let mut checks = 0;
            let mut destination = || {
                checks += 1;
                Ok(if checks == 1 {
                    Gate::Paused(PauseReason::CursorConfined)
                } else {
                    Gate::Open
                })
            };
            let mut dispatched = Vec::new();
            let result = run_controlled(
                Schedule {
                    limit: Some(u64::from(BURST_SIZE)),
                    ..Schedule::default()
                },
                &StopSignal::new().unwrap(),
                &progress,
                true,
                Checks {
                    destination: Some(&mut destination),
                    batch_destination,
                    ..Checks::default()
                },
                None,
                |count, _| {
                    dispatched.push(count);
                    Ok(None)
                },
            );
            assert_eq!(result, Ok(Outcome::Completed));
            let expected = if batch_destination { BURST_SIZE } else { 1 };
            assert_eq!(dispatched, vec![expected; (BURST_SIZE / expected) as usize]);
            assert_eq!(checks, dispatched.len() + 1);
        }
    }

    #[test]
    fn duration_interrupts_a_held_press() {
        let progress = Progress::default();
        let now = Instant::now();
        let result = run_controlled(
            Schedule {
                duration: Some(Duration::from_millis(25)),
                ..Schedule::default()
            },
            &StopSignal::new().unwrap(),
            &progress,
            false,
            Checks::default(),
            None,
            |batch, control| {
                assert_eq!(batch, 1);
                assert_eq!(control.begin_dispatch(), None);
                control.hold(Duration::from_secs(3600), None)?;
                assert!(!control.allow_press(Gate::Open));
                Ok(None)
            },
        );
        assert_eq!(result, Ok(Outcome::Completed));
        assert_eq!(progress.actions(), 1);
        assert!(now.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn gate_pauses_before_dispatch_and_reports_the_stop_reason() {
        let progress = Progress::default();
        let now = Instant::now();
        let mut gate = || {
            if progress.actions() > 0 {
                Gate::Stop("stop zone")
            } else if now.elapsed() < Duration::from_millis(20) {
                Gate::Paused(PauseReason::OutsideZone)
            } else {
                Gate::Open
            }
        };
        let result = run_controlled(
            Schedule::default(),
            &StopSignal::new().unwrap(),
            &progress,
            true,
            Checks {
                poll: Some(&mut gate),
                ..Checks::default()
            },
            None,
            |batch, control| {
                assert_eq!(batch, 1);
                assert!(now.elapsed() >= Duration::from_millis(20));
                assert_eq!(progress.pause_reason(), None);
                Ok(control.begin_dispatch())
            },
        );
        assert_eq!(result, Ok(Outcome::Guarded("stop zone")));
        assert_eq!(progress.actions(), 1);
    }

    #[test]
    fn stop_zone_stays_latched_after_a_press_is_released() {
        let progress = Progress::default();
        let trigger = std::cell::Cell::new(false);
        let mut gate = || {
            if trigger.replace(false) {
                Gate::Stop("stop zone")
            } else {
                Gate::Open
            }
        };
        let result = run_controlled(
            Schedule {
                limit: Some(2),
                ..Schedule::default()
            },
            &StopSignal::new().unwrap(),
            &progress,
            false,
            Checks {
                poll: Some(&mut gate),
                ..Checks::default()
            },
            None,
            |_, control| {
                assert_eq!(control.begin_dispatch(), None);
                trigger.set(true);
                control.hold(Duration::from_millis(1), None)?;
                Ok(None)
            },
        );
        assert_eq!(result, Ok(Outcome::Guarded("stop zone")));
        assert_eq!(progress.actions(), 1);
    }

    #[test]
    fn interruption_on_final_press_preserves_the_stop_reason_and_count() {
        for emergency_stop in [false, true] {
            let stop = StopSignal::new().unwrap();
            let progress = Progress::default();
            let trigger = std::cell::Cell::new(false);
            let mut gate = || {
                if trigger.replace(false) {
                    Gate::Stop("stop zone")
                } else {
                    Gate::Open
                }
            };
            let result = run_controlled(
                Schedule {
                    limit: Some(1),
                    ..Schedule::default()
                },
                &stop,
                &progress,
                false,
                Checks {
                    poll: Some(&mut gate),
                    ..Checks::default()
                },
                None,
                |_, control| {
                    assert_eq!(control.begin_dispatch(), None);
                    if emergency_stop {
                        stop.request();
                    } else {
                        trigger.set(true);
                    }
                    control.hold(Duration::from_millis(1), None)?;
                    Ok(None)
                },
            );
            let expected = if emergency_stop {
                Outcome::Stopped
            } else {
                Outcome::Guarded("stop zone")
            };
            assert_eq!(result, Ok(expected));
            assert_eq!(progress.actions(), 1);
        }
    }

    #[test]
    #[ignore = "host timing benchmark"]
    fn benchmark_timed_scheduler() {
        for micros in [10_000, 1_000, 100] {
            let interval = Duration::from_micros(micros);
            let count = (1_000_000 / micros).clamp(100, 10_000);
            let mut stamps = Vec::with_capacity(count as usize);
            let result = run(
                Schedule {
                    interval,
                    delay: Duration::ZERO,
                    limit: Some(count),
                    ..Schedule::default()
                },
                &StopSignal::new().unwrap(),
                &Progress::default(),
                |_| {
                    stamps.push(Instant::now());
                    Ok(())
                },
            );
            assert_eq!(result, Ok(Outcome::Completed));
            let mut deltas: Vec<_> = stamps
                .windows(2)
                .map(|t| (t[1] - t[0]).as_micros())
                .collect();
            let rate = (count - 1) as f64 / (*stamps.last().unwrap() - stamps[0]).as_secs_f64();
            deltas.sort_unstable();
            eprintln!(
                "{micros} us requested: {rate:.1} actions/s, median {} us, p99 {} us",
                deltas[deltas.len() / 2],
                deltas[deltas.len() * 99 / 100]
            );
        }
    }
}
