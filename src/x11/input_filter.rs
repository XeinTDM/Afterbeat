use crate::{
    App,
    mouse::Point,
    x11::{Desktop, XResult, shell},
};
use slint::{ComponentHandle, Timer};
use std::{
    collections::HashMap,
    io,
    sync::{
        Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
use x11rb::protocol::xproto::{ConnectionExt as _, Window};

static WINDOW: AtomicU32 = AtomicU32::new(0);
type PadHits = HashMap<(i16, i16), u32>;
static HITS: Mutex<Option<PadHits>> = Mutex::new(None);

pub struct InputFilter;

impl InputFilter {
    pub fn attach(window: Window) -> io::Result<Self> {
        shell::publish_window(window)?;
        WINDOW.store(window, Ordering::Release);
        Ok(Self)
    }
}

impl Drop for InputFilter {
    fn drop(&mut self) {
        WINDOW.store(0, Ordering::Release);
        clear_hits();
    }
}

pub fn clear_hits() {
    if let Some(hits) = HITS.lock().unwrap().as_mut() {
        hits.clear();
    }
}

pub enum Hit {
    External(Window),
    Frame,
    Client(i16, i16),
}

pub fn hit(desktop: &Desktop, position: Point) -> XResult<Hit> {
    let window = WINDOW.load(Ordering::Acquire);
    if window == 0 {
        return Err("Afterbeat's X11 window protection is unavailable.".into());
    }
    let path = desktop.windows_at(position)?;
    if path.contains(&window) {
        let coordinates = desktop
            .conn
            .translate_coordinates(
                desktop.root,
                window,
                i16::try_from(position.x)?,
                i16::try_from(position.y)?,
            )?
            .reply()?;
        return Ok(Hit::Client(coordinates.dst_x, coordinates.dst_y));
    }
    let target = path.last().copied().unwrap_or(desktop.root);
    if path.first() == Some(&desktop.top_level(window)?)
        || desktop.window_pid(target)? == std::process::id()
    {
        return Ok(Hit::Frame);
    }
    Ok(Hit::External(target))
}

pub fn count_pad(x: i16, y: i16, count: u32) {
    let mut queue = HITS.lock().unwrap();
    let schedule = queue.is_none();
    let hits = queue.get_or_insert_with(HashMap::new);
    if hits.len() < 4096 || hits.contains_key(&(x, y)) {
        let total = hits.entry((x, y)).or_default();
        *total = total.saturating_add(count);
    }
    if schedule
        && slint::invoke_from_event_loop(|| {
            Timer::single_shot(Duration::from_millis(32), flush_pad);
        })
        .is_err()
    {
        *queue = None;
    }
}

fn flush_pad() {
    let hits = HITS.lock().unwrap().take().unwrap_or_default();
    let app: Option<App> = crate::ACTIVE_CONTROLLER
        .with(|local| local.borrow().upgrade())
        .and_then(|controller| controller.app.upgrade());
    if let Some(app) = app {
        let scale = app.window().scale_factor();
        let count = hits
            .into_iter()
            .filter(|((x, y), _)| {
                app.invoke_test_pad_at(f32::from(*x) / scale, f32::from(*y) / scale)
            })
            .map(|(_, count)| u64::from(count))
            .sum::<u64>();
        if count != 0 {
            app.invoke_test_hits(count.min(i32::MAX as u64) as i32);
        }
    }
}
