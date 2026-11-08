use super::SPIN_TAIL;
use std::{
    hint::spin_loop,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr::null,
    sync::atomic::{AtomicBool, Ordering},
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
pub(super) struct Pacer {
    timer: Option<OwnedHandle>,
    pub(super) spin_tail: Duration,
}

impl Pacer {
    pub(super) fn new() -> Self {
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

    pub(super) fn wait_until(
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

pub(super) fn prioritize_worker() {
    // Give short load-paced intervals a better chance to wake on time.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
}
