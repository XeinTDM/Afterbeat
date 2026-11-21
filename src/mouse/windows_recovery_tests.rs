use super::*;
use crate::engine::{Checks, Progress, Schedule, StopSignal, run_controlled};
use std::cell::Cell;

#[test]
fn stop_during_held_mouse_press_releases_once_and_stops() {
    let stop = StopSignal::new().unwrap();
    let progress = Progress::default();
    let mut injector = Injector::with_targets(Input::Mouse(Button::Left), 3, None, Press::Held);
    let mut events = Vec::new();
    let result = run_controlled(
        Schedule {
            limit: Some(2),
            ..Schedule::default()
        },
        &stop,
        &progress,
        false,
        Checks::default(),
        None,
        |_, control| {
            injector.send_checked(
                1,
                control,
                None,
                || false,
                |packet, _| {
                    let flags = unsafe { packet[0].Anonymous.mi.dwFlags };
                    events.push(flags);
                    if flags == MOUSEEVENTF_LEFTDOWN {
                        stop.request();
                    }
                    Ok(())
                },
            )
        },
    );
    assert_eq!(result, Ok(Outcome::Stopped));
    assert_eq!(events, [MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP]);
    assert_eq!(progress.actions(), 1);
}

#[test]
fn focus_loss_during_held_key_releases_before_next_dispatch() {
    let stop = StopSignal::new().unwrap();
    let progress = Progress::default();
    let pressed = Cell::new(false);
    let mut gate = || {
        if pressed.get() {
            Gate::Paused(PauseReason::KeyboardFocus)
        } else {
            Gate::Open
        }
    };
    let mut injector = Injector::with_targets(
        Input::Key(keys::parse("Space").unwrap()),
        3,
        None,
        Press::Held,
    );
    let mut events = Vec::new();
    let result = run_controlled(
        Schedule {
            limit: Some(2),
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
            injector.send_checked(
                1,
                control,
                None,
                || false,
                |packet, _| {
                    let key = unsafe { packet[0].Anonymous.ki };
                    events.push((key.wVk, key.dwFlags));
                    if key.dwFlags & KEYEVENTF_KEYUP == 0 {
                        pressed.set(true);
                    } else {
                        assert_eq!(progress.pause_reason(), Some(PauseReason::KeyboardFocus));
                        stop.request();
                    }
                    Ok(())
                },
            )
        },
    );
    assert_eq!(result, Ok(Outcome::Stopped));
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, events[1].0);
    assert_eq!(events[0].1 & KEYEVENTF_KEYUP, 0);
    assert_ne!(events[1].1 & KEYEVENTF_KEYUP, 0);
    assert_eq!(progress.actions(), 1);
}
