use super::*;
use crate::{
    automation,
    engine::{self, Checks, Outcome, Progress, StopSignal},
};
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

#[test]
fn shortcut_descriptions_are_checked_before_keyboard_delivery() {
    assert!(shortcut_key("Ctrl+Alt+F6").is_some_and(|key| key.0 == xkeysym::key::F6));
    assert!(shortcut_key("Other+F6").is_none());
    assert!(shortcut_key("Mouse4").is_none());
    assert!(same_key(
        keys::parse("a").unwrap(),
        keys::parse("A").unwrap()
    ));
    assert!(!same_key(
        keys::parse("F6").unwrap(),
        keys::parse("F7").unwrap()
    ));
    assert_eq!(
        Binding::preferred_trigger("Ctrl+Alt+G").as_deref(),
        Some("CTRL+ALT+g")
    );
    assert!(Binding::preferred_trigger("Mouse4").is_none());
}

#[test]
fn rejects_live_cursor_and_process_guards_without_desktop_data() {
    for settings in [
        Settings {
            edge_mode: 1,
            ..Settings::default()
        },
        Settings {
            process_mode: 2,
            ..Settings::default()
        },
        Settings {
            stop_zone: Some(Zone {
                from: Point { x: 10, y: 10 },
                to: Point { x: 20, y: 20 },
            }),
            ..Settings::default()
        },
    ] {
        assert!(validate(&settings).is_err());
    }
}

#[test]
#[ignore = "requires the isolated portal fixture on a private D-Bus session"]
fn portal_lifecycle() {
    const FIXTURE: &str = "io.github.xeintdm.Afterbeat.TestPortal";
    let connection = block_on(Portal::connection()).unwrap();
    let fixture = block_on(Proxy::new(
        &connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        FIXTURE,
    ))
    .unwrap();
    let call = |method, args: &(bool,)| {
        block_on(fixture.call::<_, _, ()>(method, args)).unwrap();
    };
    let emit = |id, pressed| {
        block_on(fixture.call::<_, _, ()>("EmitShortcut", &(id, pressed))).unwrap();
    };
    let trace = || block_on(fixture.call::<_, _, Vec<String>>("Trace", &())).unwrap();
    let (actions, events) = mpsc::channel();
    let slot = Arc::new(Mutex::new(None::<Arc<StopSignal>>));
    let callback_slot = slot.clone();
    select(true);
    let shortcuts = Hotkeys::start("F6", move |action| {
        if matches!(action, Action::Stop(_) | Action::PortalChanged)
            && let Some(stop) = callback_slot.lock().unwrap().as_ref()
        {
            stop.request();
        }
        let _ = actions.send(action);
    })
    .unwrap();

    assert!(shortcuts.enable(true));
    wait_for(|| status().ready, || status().message);
    assert_eq!(status().stop.as_deref(), Some("F7"));
    assert_eq!(status().toggle.as_deref(), Some("F6"));
    assert_eq!(displays()[0].from, Point { x: -800, y: 0 });
    let fixed = Settings {
        fixed: true,
        point: Some(Point { x: -600, y: 40 }),
        hold_down: true,
        hold_activation: true,
        delay_seconds: 0,
        ..Settings::default()
    };
    assert!(automation::validated_schedule(&fixed, toggle_binding()).is_err());
    let fixed = Settings {
        point_wayland: true,
        ..fixed
    };
    assert!(automation::validated_schedule(&fixed, toggle_binding()).is_ok());
    set_hold_mode(true);

    for stop_key in [ACTIVATE, STOP] {
        let previous = trace().len();
        emit(ACTIVATE, true);
        let binding = loop {
            if let Action::Toggle(binding, _) = events.recv_timeout(Duration::from_secs(3)).unwrap()
            {
                break binding;
            }
        };
        emit(ACTIVATE, true);
        let stop = Arc::new(StopSignal::new().unwrap());
        *slot.lock().unwrap() = Some(stop.clone());
        let (schedule, mut injector, mut guard) =
            automation::prepare(&fixed, Some(binding), Some(binding)).unwrap();
        let handle = thread::spawn(move || {
            let mut check = || guard.check();
            engine::run_controlled(
                schedule,
                &stop,
                &Progress::default(),
                injector.batchable(),
                Checks {
                    poll: Some(&mut check),
                    destination: None,
                    batch_destination: true,
                },
                None,
                |_, control| injector.send(1, control, None),
            )
        });
        wait_for(
            || trace().len() >= previous + 2,
            || format!("{:?}", trace()),
        );
        assert_eq!(&trace()[previous..], ["move:7:200:40", "button:272:1"]);
        emit(stop_key, stop_key == STOP);
        assert!(matches!(
            handle.join().unwrap(),
            Ok(Outcome::Stopped | Outcome::Guarded("Activation released."))
        ));
        *slot.lock().unwrap() = None;
        emit(ACTIVATE, false);
        assert_eq!(
            &trace()[previous..],
            ["move:7:200:40", "button:272:1", "button:272:0"]
        );
    }

    let remote = remote().unwrap();
    let keyboard = Settings {
        keyboard: true,
        press: "30".into(),
        ..Settings::default()
    };
    let (_, _, mut guard) = automation::prepare(&keyboard, toggle_binding(), None).unwrap();
    assert!(matches!(
        guard.check(),
        engine::Gate::Paused(engine::PauseReason::KeyboardFocus)
    ));
    remote.send(true, xkeysym::key::space as i32, true).unwrap();
    remote
        .send(true, xkeysym::key::space as i32, false)
        .unwrap();
    assert_eq!(&trace()[trace().len() - 2..], ["key:32:1", "key:32:0"]);
    call("EnableStop", &(false,));
    wait_for(
        || status().stop.is_none(),
        || "Stop removal was not observed.".into(),
    );
    assert!(
        validate(&Settings {
            keyboard: true,
            ..Settings::default()
        })
        .is_err()
    );
    block_on(fixture.call::<_, _, ()>("Revoke", &())).unwrap();
    wait_for(
        || !status().ready && self::remote().is_none(),
        || status().message,
    );
    assert!(shortcuts.enable(false));
    wait_for(
        || status().message.contains("Bind the emergency stop"),
        || status().message,
    );
    assert!(self::remote().is_none());
}

fn wait_for(condition: impl Fn() -> bool, diagnostic: impl FnOnce() -> String) {
    let start = Instant::now();
    while !condition() {
        if start.elapsed() > Duration::from_secs(6) {
            panic!("{}", diagnostic());
        }
        thread::sleep(Duration::from_millis(15));
    }
}
