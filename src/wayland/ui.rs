use crate::{App, StopSlot, input_filter, signal_stop};
use slint::{
    ComponentHandle,
    winit_030::{
        EventResult, SlintEvent, WinitWindowAccessor,
        winit::{
            event::{ElementState, MouseButton, WindowEvent},
            event_loop::EventLoop,
            keyboard::{Key, NamedKey},
            platform::wayland::{EventLoopBuilderExtWayland, WindowAttributesExtWayland},
        },
    },
};
use std::{
    cell::RefCell,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

static FOCUSED: AtomicBool = AtomicBool::new(true);
static PROTECTION: AtomicU8 = AtomicU8::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static POINTER: Mutex<Pointer> = Mutex::new(Pointer::Outside);
const MOUSE: u8 = 1;
const KEYBOARD: u8 = 2;

enum Pointer {
    Outside,
    Inside(Option<(i16, i16)>),
}

type Output = (Option<String>, i32, i32, u32, u32, u64);
type Layout = (Weak<super::Remote>, Vec<Output>);

thread_local! {
    static LAYOUT: RefCell<Option<Layout>> = const { RefCell::new(None) };
}

pub fn select_backend() -> Result<(), slint::PlatformError> {
    let mut builder = EventLoop::<SlintEvent>::with_user_event();
    builder.with_wayland();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .with_winit_event_loop_builder(builder)
        .with_winit_window_attributes_hook(|attrs| {
            attrs.with_name(super::portal::APP_ID, "afterbeat")
        })
        .select()
}

pub fn protect(keyboard: Option<bool>) {
    let generation = GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    let mode = match keyboard {
        None => {
            if PROTECTION.load(Ordering::Acquire) != 0 {
                // The compositor can still have queued input after the portal
                // acknowledges the final release.
                slint::Timer::single_shot(Duration::from_millis(150), move || {
                    if GENERATION.load(Ordering::Acquire) == generation {
                        PROTECTION.store(0, Ordering::Release);
                    }
                });
            }
            return;
        }
        Some(false) => MOUSE,
        Some(true) => KEYBOARD,
    };
    PROTECTION.store(mode, Ordering::Release);
}

pub fn focused() -> bool {
    FOCUSED.load(Ordering::Acquire)
}

pub fn click_own_window() -> bool {
    match *POINTER.lock().unwrap() {
        Pointer::Outside => false,
        Pointer::Inside(position) => {
            if let Some((x, y)) = position {
                input_filter::count_pad(x, y, 1);
            }
            true
        }
    }
}

pub fn pointer_over_own_window() -> bool {
    matches!(*POINTER.lock().unwrap(), Pointer::Inside(_))
}

fn stop_at_pointer(app: &App) -> bool {
    let Pointer::Inside(Some((x, y))) = *POINTER.lock().unwrap() else {
        return false;
    };
    let scale = app.window().scale_factor();
    app.invoke_stop_run_at(f32::from(x) / scale, f32::from(y) / scale)
}

pub fn layout_unchanged(app: &App) -> bool {
    LAYOUT.with(|layout| {
        let mut layout = layout.borrow_mut();
        let Some(remote) = super::remote().filter(|remote| !remote.monitors.is_empty()) else {
            *layout = None;
            return true;
        };
        let Some(mut outputs) = app.window().with_winit_window(|window| {
            window
                .available_monitors()
                .map(|monitor| {
                    let position = monitor.position();
                    let size = monitor.size();
                    (
                        monitor.name(),
                        position.x,
                        position.y,
                        size.width,
                        size.height,
                        monitor.scale_factor().to_bits(),
                    )
                })
                .collect::<Vec<_>>()
        }) else {
            return false;
        };
        outputs.sort_unstable();
        let remote = Arc::downgrade(&remote);
        if let Some((previous, old_outputs)) = &*layout
            && previous.ptr_eq(&remote)
        {
            return *old_outputs == outputs;
        }
        *layout = Some((remote, outputs));
        true
    })
}

pub fn attach(app: &App, stop: StopSlot) {
    let window = app.window();
    let app = app.as_weak();
    window.with_winit_window(|window| FOCUSED.store(window.has_focus(), Ordering::Release));
    window.on_winit_window_event(move |_, event| {
        let protection = PROTECTION.load(Ordering::Acquire);
        match event {
            WindowEvent::Focused(focused) => FOCUSED.store(*focused, Ordering::Release),
            WindowEvent::CursorEntered { .. } => *POINTER.lock().unwrap() = Pointer::Inside(None),
            WindowEvent::CursorMoved { position, .. } => {
                *POINTER.lock().unwrap() =
                    Pointer::Inside(Some((position.x as i16, position.y as i16)));
            }
            WindowEvent::CursorLeft { .. } => *POINTER.lock().unwrap() = Pointer::Outside,
            WindowEvent::KeyboardInput { event, .. }
                if protection != 0 && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                if event.state == ElementState::Pressed {
                    signal_stop(&stop);
                }
                return EventResult::PreventDefault;
            }
            WindowEvent::KeyboardInput { .. } | WindowEvent::Ime(_) if protection == KEYBOARD => {
                return EventResult::PreventDefault;
            }
            WindowEvent::MouseInput { state, button, .. } if protection == MOUSE => {
                if *state == ElementState::Pressed {
                    // Wayland cannot distinguish generated clicks here. Allow only a stop.
                    if *button == MouseButton::Left
                        && app.upgrade().is_some_and(|app| stop_at_pointer(&app))
                    {
                        signal_stop(&stop);
                    } else {
                        click_own_window();
                    }
                }
                return EventResult::PreventDefault;
            }
            WindowEvent::MouseWheel { .. } if protection == MOUSE => {
                return EventResult::PreventDefault;
            }
            _ => {}
        }
        EventResult::Propagate
    });
}
