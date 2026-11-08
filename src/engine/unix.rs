use super::SPIN_TAIL;
use std::{
    hint::spin_loop,
    io,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub struct StopSignal {
    requested: AtomicBool,
    wait_lock: Mutex<()>,
    wake: Condvar,
}

impl StopSignal {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            requested: AtomicBool::new(false),
            wait_lock: Mutex::new(()),
            wake: Condvar::new(),
        })
    }

    pub fn request(&self) {
        let _lock = self.wait_lock.lock().unwrap();
        self.requested.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    pub fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

#[derive(Default)]
pub(super) struct Pacer {
    pub(super) spin_tail: Duration,
}

impl Pacer {
    pub(super) fn new() -> Self {
        Self {
            spin_tail: SPIN_TAIL,
        }
    }

    pub(super) fn wait_until(
        &self,
        stop: &StopSignal,
        deadline: Instant,
        spin_tail: Duration,
    ) -> io::Result<bool> {
        let sleep_until =
            deadline - spin_tail.min(deadline.saturating_duration_since(Instant::now()));
        if Instant::now() < sleep_until {
            let lock = stop.wait_lock.lock().unwrap();
            let sleep = sleep_until.saturating_duration_since(Instant::now());
            drop(
                stop.wake
                    .wait_timeout_while(lock, sleep, |_| !stop.requested())
                    .unwrap(),
            );
        }
        while !stop.requested() {
            if Instant::now() >= deadline {
                return Ok(true);
            }
            spin_loop();
        }
        Ok(false)
    }
}
