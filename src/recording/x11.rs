//! XInput2 capture for a local X11 session. This backend is not used on Wayland.
use super::{CaptureOptions, Entry, Event as RecordedEvent, Recorder, RecordingStop};
use crate::{
    hotkeys::Binding,
    input_filter::{self, Hit},
    keys::{self, Keymap},
    mouse::{Button, Point},
    x11::{Desktop, XResult},
};
use rustix::event::{PollFd, PollFlags, poll};
use std::{
    collections::HashSet,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xinput::{ConnectionExt as _, DeviceType, EventMask, XIEventMask},
        xproto::{self, ConnectionExt as _, CreateWindowAux, WindowClass},
    },
    wrapper::ConnectionExt as _,
};

const MAX_EVENTS: usize = crate::settings::MAX_RECORDING_EVENTS - 253;

struct Capture {
    started: Instant,
    clock: EventClock,
    recorder: Option<Recorder>,
    limit_reached: bool,
    stopped: bool,
    #[cfg(test)]
    ignored_sources: usize,
    #[cfg(test)]
    observed_sources: usize,
}

#[derive(Default)]
struct EventClock {
    last: Option<(u32, Duration)>,
}

impl EventClock {
    fn timestamp(&mut self, server_time: u32, received: Duration) -> Duration {
        let at = match self.last {
            None => received,
            Some((previous, last)) => {
                let delta = server_time.wrapping_sub(previous);
                if delta > i32::MAX as u32 {
                    if received.saturating_sub(last) < Duration::from_millis(i32::MAX as u64) {
                        return last;
                    }
                    received
                } else {
                    last.saturating_add(Duration::from_millis(u64::from(delta)))
                        .min(received)
                }
            }
        };
        self.last = Some((server_time, at));
        at
    }
}

impl Capture {
    fn record(&mut self, event: RecordedEvent, now: Duration) {
        if self.limit_reached || self.stopped {
            return;
        }
        if let Some(recorder) = &mut self.recorder {
            recorder.record(event, now);
            self.check_limit(now);
        }
    }

    fn exclude(&mut self, now: Duration, f: impl FnOnce(&mut Recorder, Duration)) {
        if let Some(recorder) = &mut self.recorder {
            f(recorder, now);
            self.check_limit(now);
        }
    }

    fn check_limit(&mut self, now: Duration) {
        if let Some(recorder) = &mut self.recorder
            && recorder.entries().len() >= MAX_EVENTS
        {
            recorder.pause(now);
            self.limit_reached = true;
        }
    }
}

pub struct Service {
    shared: Arc<Mutex<Capture>>,
    closing: Arc<AtomicBool>,
    wake: UnixStream,
    commands: mpsc::Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

enum Command {
    Resume(mpsc::SyncSender<io::Result<()>>),
}

impl Service {
    pub fn start(
        options: CaptureOptions,
        binding: Option<Binding>,
        stop_requested: Arc<RecordingStop>,
    ) -> io::Result<Self> {
        Self::start_inner(options, binding, false, stop_requested)
    }

    #[cfg(test)]
    fn start_with_xtest(
        options: CaptureOptions,
        stop_requested: Arc<RecordingStop>,
    ) -> io::Result<Self> {
        Self::start_inner(options, None, true, stop_requested)
    }

    fn start_inner(
        options: CaptureOptions,
        binding: Option<Binding>,
        include_xtest: bool,
        stop_requested: Arc<RecordingStop>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Mutex::new(Capture {
            started: Instant::now(),
            clock: EventClock::default(),
            recorder: Some(Recorder::with_stop_signal(
                options,
                Duration::ZERO,
                stop_requested,
            )),
            limit_reached: false,
            stopped: false,
            #[cfg(test)]
            ignored_sources: 0,
            #[cfg(test)]
            observed_sources: 0,
        }));
        let worker_capture = shared.clone();
        let closing = Arc::new(AtomicBool::new(false));
        let worker_closing = closing.clone();
        let (wake, reader) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        let (commands, incoming) = mpsc::channel();
        let (ready, response) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("macro-capture-x11".into())
            .spawn(move || {
                let mut ready = Some(ready);
                let result = listen(
                    ListenOptions {
                        capture: options,
                        binding,
                        include_xtest,
                    },
                    &worker_capture,
                    &reader,
                    &worker_closing,
                    &incoming,
                    &mut ready,
                );
                if ready.is_none()
                    && let Err(error) = &result
                {
                    crate::diagnostics::record(&format!("X11 recording stopped: {error}"));
                }
                if let Some(ready) = ready {
                    let _ = ready.send(Err(io::Error::other(result.err().map_or_else(
                        || "The X11 capture listener stopped.".into(),
                        |error| error.to_string(),
                    ))));
                }
                if let Ok(mut capture) = worker_capture.lock() {
                    let now = capture.started.elapsed();
                    if let Some(recorder) = &mut capture.recorder {
                        recorder.pause(now);
                    }
                    capture.stopped = true;
                }
            })?;
        response.recv().map_err(io::Error::other)??;
        Ok(Self {
            shared,
            closing,
            wake,
            commands,
            worker: Some(worker),
        })
    }

    pub fn pause(&self) {
        let mut capture = self.shared.lock().unwrap();
        let now = capture.started.elapsed();
        if let Some(recorder) = &mut capture.recorder {
            recorder.pause(now);
        }
    }

    pub fn resume(&self) -> io::Result<()> {
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .send(Command::Resume(reply))
            .map_err(io::Error::other)?;
        (&self.wake).write_all(&[1])?;
        result.recv().map_err(io::Error::other)?
    }

    pub fn count(&self) -> (usize, bool, bool) {
        let capture = self.shared.lock().unwrap();
        (
            capture
                .recorder
                .as_ref()
                .map_or(0, |recorder| recorder.entries().len()),
            capture.limit_reached,
            capture.stopped,
        )
    }

    #[cfg(test)]
    fn ignored_sources(&self) -> usize {
        self.shared.lock().unwrap().ignored_sources
    }

    #[cfg(test)]
    fn observed_sources(&self) -> usize {
        self.shared.lock().unwrap().observed_sources
    }

    pub fn finish(mut self) -> Vec<Entry> {
        let entries = {
            let mut capture = self.shared.lock().unwrap();
            let now = capture.started.elapsed();
            capture
                .recorder
                .take()
                .map_or_else(Vec::new, |recorder| recorder.finish(now))
        };
        self.shutdown();
        entries
    }

    fn shutdown(&mut self) {
        self.closing.store(true, Ordering::Release);
        if let Ok(mut capture) = self.shared.lock() {
            capture.recorder = None;
        }
        let _ = self.wake.shutdown(Shutdown::Write);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn physical_sources(desktop: &Desktop, include_xtest: bool) -> XResult<HashSet<u16>> {
    let devices = desktop.conn.xinput_xi_query_device(0u16)?.reply()?.infos;
    Ok(devices
        .into_iter()
        .filter(|device| {
            physical_source(device.type_, device.enabled, &device.name)
                || (include_xtest
                    && device.enabled
                    && matches!(
                        device.type_,
                        DeviceType::SLAVE_POINTER | DeviceType::SLAVE_KEYBOARD
                    )
                    && String::from_utf8_lossy(&device.name)
                        .to_ascii_lowercase()
                        .contains("xtest"))
        })
        .map(|device| device.deviceid)
        .collect())
}

fn physical_source(kind: DeviceType, enabled: bool, name: &[u8]) -> bool {
    enabled
        && matches!(kind, DeviceType::SLAVE_POINTER | DeviceType::SLAVE_KEYBOARD)
        && !String::from_utf8_lossy(name)
            .to_ascii_lowercase()
            .contains("xtest")
}

fn button(detail: u32) -> Option<Button> {
    match detail {
        1 => Some(Button::Left),
        2 => Some(Button::Middle),
        3 => Some(Button::Right),
        8 => Some(Button::Mouse4),
        9 => Some(Button::Mouse5),
        _ => None,
    }
}

fn raw_source(event: &Event) -> Option<u16> {
    match event {
        Event::XinputRawMotion(event) => Some(event.sourceid),
        Event::XinputRawButtonPress(event) | Event::XinputRawButtonRelease(event) => {
            Some(event.sourceid)
        }
        Event::XinputRawKeyPress(event) | Event::XinputRawKeyRelease(event) => Some(event.sourceid),
        _ => None,
    }
}

fn record_pointer(
    desktop: &Desktop,
    shared: &Mutex<Capture>,
    server_time: u32,
    event: impl FnOnce(Point) -> RecordedEvent,
) -> XResult<()> {
    let point = desktop.cursor()?;
    let own = !matches!(input_filter::hit(desktop, point)?, Hit::External(_));
    let mut capture = shared
        .lock()
        .map_err(|_| "The recording state is unavailable.")?;
    let elapsed = capture.started.elapsed();
    let at = capture.clock.timestamp(server_time, elapsed);
    if own {
        capture.exclude(at, Recorder::exclude_buttons);
    } else {
        capture.record(event(point), at);
    }
    Ok(())
}

fn record_key(
    desktop: &Desktop,
    shared: &Mutex<Capture>,
    code: u32,
    down: bool,
    server_time: u32,
) -> XResult<()> {
    let pid = desktop.focus_pid()?;
    let mut capture = shared
        .lock()
        .map_err(|_| "The recording state is unavailable.")?;
    let elapsed = capture.started.elapsed();
    let at = capture.clock.timestamp(server_time, elapsed);
    if pid == 0 || pid == std::process::id() {
        capture.exclude(at, Recorder::exclude_keys);
    } else {
        capture.record(
            RecordedEvent::Key {
                code,
                extended: false,
                down,
            },
            at,
        );
    }
    Ok(())
}

struct ListenOptions {
    capture: CaptureOptions,
    binding: Option<Binding>,
    include_xtest: bool,
}

fn listen(
    ListenOptions {
        capture: options,
        binding,
        include_xtest,
    }: ListenOptions,
    shared: &Mutex<Capture>,
    wake: &UnixStream,
    closing: &AtomicBool,
    commands: &mpsc::Receiver<Command>,
    ready: &mut Option<mpsc::SyncSender<io::Result<()>>>,
) -> XResult<()> {
    let desktop = Desktop::connect()?;
    let map = Keymap::read(&desktop)?;
    let reserved: Vec<u8> = [keys::F7, keys::F8]
        .into_iter()
        .filter_map(|symbol| map.resolve(symbol, false).map(|(code, _)| code))
        .collect();
    let mut sources = physical_sources(&desktop, include_xtest)?;
    let conn = &desktop.conn;
    // A private property notification is a server-ordered fence. Raw events
    // already queued at Resume are drained while the recorder is still paused.
    let fence_window = conn.generate_id()?;
    conn.create_window(
        0,
        fence_window,
        desktop.root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE),
    )?
    .check()?;
    let fence_atom = desktop.atom(b"_AFTERBEAT_RECORDING_FENCE")?;
    let mut resuming: Option<mpsc::SyncSender<io::Result<()>>> = None;
    let mut mask = XIEventMask::default();
    if options.movement {
        mask |= XIEventMask::RAW_MOTION;
    }
    if options.buttons {
        mask |= XIEventMask::RAW_BUTTON_PRESS | XIEventMask::RAW_BUTTON_RELEASE;
    }
    if options.keys {
        mask |= XIEventMask::RAW_KEY_PRESS | XIEventMask::RAW_KEY_RELEASE;
    }
    conn.xinput_xi_select_events(
        desktop.root,
        &[
            EventMask {
                deviceid: 0,
                mask: vec![XIEventMask::HIERARCHY],
            },
            EventMask {
                deviceid: 1,
                mask: vec![mask],
            },
        ],
    )?
    .check()?;
    conn.flush()?;
    let _ = ready.take().unwrap().send(Ok(()));
    loop {
        if resuming.is_none()
            && let Ok(Command::Resume(reply)) = commands.try_recv()
        {
            conn.change_property8(
                xproto::PropMode::REPLACE,
                fence_window,
                fence_atom,
                xproto::AtomEnum::INTEGER,
                &[1],
            )?
            .check()?;
            conn.flush()?;
            resuming = Some(reply);
        }
        if let Some(event) = conn.poll_for_event()? {
            if closing.load(Ordering::Acquire) {
                return Ok(());
            }
            if raw_source(&event).is_some_and(|source| !sources.contains(&source)) {
                #[cfg(test)]
                if let Ok(mut capture) = shared.lock() {
                    capture.ignored_sources += 1;
                }
                continue;
            }
            #[cfg(test)]
            if raw_source(&event).is_some()
                && let Ok(mut capture) = shared.lock()
            {
                capture.observed_sources += 1;
            }
            match event {
                Event::PropertyNotify(event)
                    if event.window == fence_window && event.atom == fence_atom =>
                {
                    if let Some(reply) = resuming.take() {
                        let mut capture = shared
                            .lock()
                            .map_err(|_| "The recording state is unavailable.")?;
                        let now = capture.started.elapsed();
                        let stopped = capture.limit_reached
                            || capture.stopped
                            || capture
                                .recorder
                                .as_ref()
                                .is_none_or(Recorder::stop_requested);
                        let result = if stopped {
                            Err(io::Error::other("The recording has stopped."))
                        } else {
                            capture.recorder.as_mut().unwrap().resume(now);
                            Ok(())
                        };
                        let _ = reply.send(result);
                    }
                }
                Event::XinputHierarchy(_) => sources = physical_sources(&desktop, include_xtest)?,
                Event::XinputRawMotion(event) if sources.contains(&event.sourceid) => {
                    record_pointer(&desktop, shared, event.time, RecordedEvent::Move)?
                }
                Event::XinputRawButtonPress(event) if sources.contains(&event.sourceid) => {
                    let scroll = match event.detail {
                        4 => Some((false, 120)),
                        5 => Some((false, -120)),
                        6 => Some((true, -120)),
                        7 => Some((true, 120)),
                        _ => None,
                    };
                    if let Some((horizontal, delta)) = scroll {
                        record_pointer(&desktop, shared, event.time, |point| {
                            RecordedEvent::Scroll {
                                point,
                                horizontal,
                                delta,
                            }
                        })?;
                    } else if let Some(button) = button(event.detail)
                        && !binding.is_some_and(|binding| binding.matches_button(button))
                    {
                        record_pointer(&desktop, shared, event.time, |point| {
                            RecordedEvent::Button {
                                button,
                                down: true,
                                point,
                            }
                        })?;
                    }
                }
                Event::XinputRawButtonRelease(event) if sources.contains(&event.sourceid) => {
                    if let Some(button) = button(event.detail)
                        && !binding.is_some_and(|binding| binding.matches_button(button))
                    {
                        record_pointer(&desktop, shared, event.time, |point| {
                            RecordedEvent::Button {
                                button,
                                down: false,
                                point,
                            }
                        })?;
                    }
                }
                Event::XinputRawKeyPress(event)
                    if sources.contains(&event.sourceid) && (8..=255).contains(&event.detail) =>
                {
                    let code = event.detail as u8;
                    if !reserved.contains(&code)
                        && !binding.is_some_and(|binding| binding.uses_code(code))
                    {
                        record_key(&desktop, shared, event.detail, true, event.time)?;
                    }
                }
                Event::XinputRawKeyRelease(event)
                    if sources.contains(&event.sourceid) && (8..=255).contains(&event.detail) =>
                {
                    let code = event.detail as u8;
                    if !reserved.contains(&code)
                        && !binding.is_some_and(|binding| binding.uses_code(code))
                    {
                        record_key(&desktop, shared, event.detail, false, event.time)?;
                    }
                }
                _ => {}
            }
            // Check commands between events even under a continuous input stream.
            continue;
        }
        if closing.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut fds = [
            PollFd::new(conn, PollFlags::IN),
            PollFd::new(wake, PollFlags::IN),
        ];
        match poll(&mut fds, None) {
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        if fds[0]
            .revents()
            .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL)
        {
            return Err("The X11 recording connection closed.".into());
        }
        if !fds[1].revents().is_empty() {
            let mut buffer = [0u8; 64];
            let mut reader = wake;
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_capture_records_drag_keys_and_excludes_paused_input() {
        if std::env::var_os("AFTERBEAT_TEST_X11_RECORDING").is_none() {
            return;
        }
        use std::{
            io::{BufRead, BufReader},
            process::{Child, Command, Stdio},
        };
        use x11rb::{
            COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE,
            protocol::{
                xproto::{self, CreateWindowAux, InputFocus, WindowClass},
                xtest::ConnectionExt as _,
            },
        };
        struct Helper(Child);
        impl Drop for Helper {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Helper(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "recording::x11_player::tests::isolated_x11_route_replays_drag_and_keys",
                    "--nocapture",
                ])
                .env("AFTERBEAT_X11_PLAYER_CHILD", "1")
                .env("AFTERBEAT_X11_RECORD_CHILD", "1")
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut lines = BufReader::new(child.0.stdout.take().unwrap()).lines();
        let window: u32 = loop {
            let line = lines
                .next()
                .expect("The recipient window did not start")
                .unwrap();
            if let Some(value) = line.split("X11_WINDOW=").nth(1) {
                break value.trim().parse().unwrap();
            }
        };
        let desktop = Desktop::connect().unwrap();
        let own = desktop.conn.generate_id().unwrap();
        desktop
            .conn
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                own,
                desktop.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &CreateWindowAux::new(),
            )
            .unwrap()
            .check()
            .unwrap();
        desktop
            .conn
            .set_input_focus(InputFocus::PARENT, window, CURRENT_TIME)
            .unwrap()
            .check()
            .unwrap();
        let previous = crate::input_filter::test_window(own);
        let stop = Arc::new(RecordingStop::default());
        let service = Service::start_with_xtest(
            CaptureOptions {
                keys: true,
                ..CaptureOptions::default()
            },
            stop.clone(),
        )
        .unwrap();
        let emit = |kind, code, x, y| {
            let root = if kind == xproto::MOTION_NOTIFY_EVENT {
                desktop.root
            } else {
                NONE
            };
            desktop
                .conn
                .xtest_fake_input(kind, code, CURRENT_TIME, root, x, y, 0)
                .unwrap()
                .check()
                .unwrap();
        };
        let wait_for = |expected| {
            let deadline = Instant::now() + Duration::from_secs(2);
            while service.count().0 < expected && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            let (count, _, stopped) = service.count();
            assert!(!stopped, "The recording listener stopped unexpectedly");
            assert!(
                count >= expected,
                "Expected {expected} captured events, received {count}"
            );
        };
        for (index, (kind, code, x, y)) in [
            (xproto::MOTION_NOTIFY_EVENT, 0, 120, 120),
            (xproto::BUTTON_PRESS_EVENT, 1, 0, 0),
            (xproto::MOTION_NOTIFY_EVENT, 0, 130, 130),
            (xproto::BUTTON_RELEASE_EVENT, 1, 0, 0),
            (xproto::KEY_PRESS_EVENT, 38, 0, 0),
            (xproto::KEY_RELEASE_EVENT, 38, 0, 0),
        ]
        .into_iter()
        .enumerate()
        {
            emit(kind, code, x, y);
            wait_for(index + 1);
        }
        emit(xproto::BUTTON_PRESS_EVENT, 5, 0, 0);
        emit(xproto::BUTTON_RELEASE_EVENT, 5, 0, 0);
        wait_for(7);
        service.pause();
        emit(xproto::KEY_PRESS_EVENT, 39, 0, 0);
        emit(xproto::KEY_RELEASE_EVENT, 39, 0, 0);
        service.resume().unwrap();
        assert_eq!(service.count().0, 7, "Paused keys must not be captured");
        emit(xproto::KEY_PRESS_EVENT, 40, 0, 0);
        emit(xproto::KEY_RELEASE_EVENT, 40, 0, 0);
        wait_for(9);
        let before_stop = service.observed_sources();
        stop.request(crate::hotkeys::tick_count());
        emit(xproto::KEY_PRESS_EVENT, 41, 0, 0);
        emit(xproto::KEY_RELEASE_EVENT, 41, 0, 0);
        // The fence confirms both events were processed before finishing.
        assert!(service.resume().is_err());
        assert!(
            service.observed_sources() >= before_stop + 2,
            "The listener did not process input after Stop"
        );
        let entries = service.finish();
        crate::input_filter::test_window(previous);
        assert_eq!(entries.len(), 9);
        assert!(matches!(
            entries[6].event,
            RecordedEvent::Scroll {
                horizontal: false,
                delta: -120,
                ..
            }
        ));
        assert_eq!(
            entries[0].event,
            RecordedEvent::Move(Point { x: 120, y: 120 })
        );
        assert!(matches!(
            entries[1].event,
            RecordedEvent::Button {
                button: Button::Left,
                down: true,
                ..
            }
        ));
        assert_eq!(
            entries[2].event,
            RecordedEvent::Move(Point { x: 130, y: 130 })
        );
        assert!(matches!(
            entries[3].event,
            RecordedEvent::Button {
                button: Button::Left,
                down: false,
                ..
            }
        ));
        assert!(
            !entries
                .iter()
                .any(|entry| matches!(entry.event, RecordedEvent::Key { code: 39, .. }))
        );
        assert!(
            !entries
                .iter()
                .any(|entry| matches!(entry.event, RecordedEvent::Key { code: 41, .. }))
        );
        assert_eq!(
            super::super::validate_for(&entries, super::super::Origin::X11),
            Ok(())
        );
    }
    #[test]
    fn queued_events_keep_server_timing_across_wraparound() {
        let mut clock = EventClock::default();
        assert_eq!(
            clock.timestamp(u32::MAX - 4, Duration::from_millis(20)),
            Duration::from_millis(20)
        );
        assert_eq!(
            clock.timestamp(5, Duration::from_millis(100)),
            Duration::from_millis(30)
        );
        assert_eq!(
            clock.timestamp(4, Duration::from_millis(101)),
            Duration::from_millis(30)
        );
        assert_eq!(
            clock.timestamp(8, Duration::from_millis(102)),
            Duration::from_millis(33)
        );
    }

    #[test]
    fn server_timing_omits_the_paused_interval() {
        let ms = Duration::from_millis;
        let mut clock = EventClock::default();
        let mut recorder = Recorder::new(CaptureOptions::default(), Duration::ZERO);
        let point = RecordedEvent::Move(Point { x: 10, y: 20 });
        recorder.record(point.clone(), clock.timestamp(1_000, ms(10)));
        recorder.pause(ms(20));
        recorder.record(point.clone(), clock.timestamp(1_090, ms(100)));
        recorder.resume(ms(120));
        recorder.record(point, clock.timestamp(1_120, ms(130)));
        assert_eq!(
            recorder
                .entries()
                .iter()
                .map(|entry| entry.delay_ms)
                .collect::<Vec<_>>(),
            [10, 20]
        );
    }
    #[test]
    fn wheel_buttons_are_not_regular_buttons() {
        assert_eq!(button(1), Some(Button::Left));
        assert_eq!(button(2), Some(Button::Middle));
        assert_eq!(button(9), Some(Button::Mouse5));
        assert_eq!(button(4), None);
        assert_eq!(button(7), None);
    }

    #[test]
    fn excludes_xtest_and_unavailable_sources() {
        assert!(physical_source(
            DeviceType::SLAVE_POINTER,
            true,
            b"USB mouse"
        ));
        assert!(physical_source(
            DeviceType::SLAVE_KEYBOARD,
            true,
            b"USB keyboard"
        ));
        assert!(!physical_source(
            DeviceType::SLAVE_POINTER,
            true,
            b"Virtual core XTEST pointer"
        ));
        assert!(!physical_source(
            DeviceType::SLAVE_KEYBOARD,
            false,
            b"USB keyboard"
        ));
        assert!(!physical_source(
            DeviceType::MASTER_POINTER,
            true,
            b"Virtual core pointer"
        ));
    }

    #[test]
    fn isolated_x11_listener_ignores_xtest_input() {
        if std::env::var_os("AFTERBEAT_TEST_X11_RECORDING").is_none() {
            return;
        }
        use x11rb::{
            CURRENT_TIME, NONE,
            protocol::{xproto, xtest::ConnectionExt as _},
        };
        let service = Service::start(
            CaptureOptions {
                keys: true,
                ..CaptureOptions::default()
            },
            None,
            Arc::new(RecordingStop::default()),
        )
        .unwrap();
        let desktop = Desktop::connect().unwrap();
        for (kind, code) in [
            (xproto::MOTION_NOTIFY_EVENT, 0),
            (xproto::BUTTON_PRESS_EVENT, 1),
            (xproto::BUTTON_RELEASE_EVENT, 1),
            (xproto::KEY_PRESS_EVENT, 38),
            (xproto::KEY_RELEASE_EVENT, 38),
        ] {
            let root = if kind == xproto::MOTION_NOTIFY_EVENT {
                desktop.root
            } else {
                NONE
            };
            desktop
                .conn
                .xtest_fake_input(kind, code, CURRENT_TIME, root, 30, 30, 0)
                .unwrap()
                .check()
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while service.ignored_sources() < 5 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            service.ignored_sources() >= 5,
            "The listener did not observe the XTEST events"
        );
        let (count, limit, stopped) = service.count();
        assert!(!stopped, "The recording listener stopped unexpectedly");
        assert!(!limit);
        assert_eq!(
            count, 0,
            "Synthetic XTEST events must not enter the recording"
        );
        service.pause();
        // Requests acknowledged by the server before Resume must have been
        // drained by the time the resume fence is acknowledged.
        let before = service.ignored_sources();
        for index in 0..10 {
            desktop
                .conn
                .xtest_fake_input(
                    if index % 2 == 0 {
                        xproto::KEY_PRESS_EVENT
                    } else {
                        xproto::KEY_RELEASE_EVENT
                    },
                    38,
                    CURRENT_TIME,
                    NONE,
                    0,
                    0,
                    0,
                )
                .unwrap()
                .check()
                .unwrap();
        }
        service.resume().unwrap();
        assert!(service.ignored_sources() >= before + 10);
        let producing = Arc::new(AtomicBool::new(true));
        let producer_flag = producing.clone();
        let (started, ready) = mpsc::sync_channel(1);
        let producer = std::thread::spawn(move || {
            let desktop = Desktop::connect().unwrap();
            let mut started = Some(started);
            let mut x = 31;
            while producer_flag.load(Ordering::Acquire) {
                if desktop
                    .conn
                    .xtest_fake_input(
                        xproto::MOTION_NOTIFY_EVENT,
                        0,
                        CURRENT_TIME,
                        desktop.root,
                        x,
                        31,
                        0,
                    )
                    .is_err()
                {
                    break;
                }
                if desktop.conn.flush().is_err() {
                    break;
                }
                x = if x == 31 { 32 } else { 31 };
                if let Some(started) = started.take() {
                    let _ = started.send(());
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        if ready.recv_timeout(Duration::from_secs(2)).is_err() {
            producing.store(false, Ordering::Release);
            producer.join().unwrap();
            panic!("The input stream did not start");
        }
        service.pause();
        let (resumed, result) = mpsc::sync_channel(1);
        let resumer = std::thread::spawn(move || {
            let outcome = service.resume();
            let _ = resumed.send((service, outcome));
        });
        let timely = result.recv_timeout(Duration::from_secs(2));
        let resumed_during_stream = timely.is_ok();
        if !resumed_during_stream {
            producing.store(false, Ordering::Release);
        }
        let (service, outcome) = timely.unwrap_or_else(|_| {
            result
                .recv_timeout(Duration::from_secs(2))
                .expect("The listener did not resume after input ended")
        });
        resumer.join().unwrap();
        outcome.unwrap();
        assert!(
            resumed_during_stream,
            "Resuming should not wait for a continuous input stream to end"
        );
        let (finished, result) = mpsc::sync_channel(1);
        let finisher = std::thread::spawn(move || {
            let _ = finished.send(service.finish());
        });
        let timely = result.recv_timeout(Duration::from_secs(2));
        let stopped_during_stream = timely.is_ok();
        producing.store(false, Ordering::Release);
        producer.join().unwrap();
        let entries = match timely {
            Ok(entries) => entries,
            Err(_) => result
                .recv_timeout(Duration::from_secs(2))
                .expect("The listener did not stop after input ended"),
        };
        finisher.join().unwrap();
        assert!(entries.is_empty());
        assert!(
            stopped_during_stream,
            "Stopping should not wait for a continuous input stream to end"
        );
    }
}
