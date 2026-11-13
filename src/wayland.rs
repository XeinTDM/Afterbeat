mod portal;
#[cfg(test)]
mod tests;
mod ui;

pub use ui::{attach, click_own_window, focused, layout_unchanged, protect, select_backend};

use crate::{
    diagnostics,
    hotkeys::{Action, Binding, tick_count},
    keys,
    mouse::{Point, Zone},
    settings::{self, Issue, Settings},
};
use futures_lite::{
    StreamExt,
    future::{block_on, pending, race},
};
use portal::{GLOBAL, Options, Portal, REMOTE, Result, SCREENS, Shortcuts, string};
use std::{
    env, fs,
    fs::{File, OpenOptions, TryLockError},
    io,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};
use zbus::Proxy;

const KEYBOARD: u32 = 1;
const POINTER: u32 = 2;
const MONITOR: u32 = 1;
const ACTIVATE: &str = "activate";
const STOP: &str = "stop";
static ENABLED: OnceLock<bool> = OnceLock::new();
static HELD: AtomicBool = AtomicBool::new(false);
static HOLD_MODE: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<State> = Mutex::new(State {
    status: Status {
        message: String::new(),
        pending: false,
        ready: false,
        configure: false,
        toggle: None,
        stop: None,
        displays: String::new(),
    },
    remote: None,
});

pub fn detect() -> bool {
    match env::var("XDG_SESSION_TYPE") {
        Ok(kind) if kind.eq_ignore_ascii_case("x11") => false,
        Ok(kind) if kind.eq_ignore_ascii_case("wayland") => true,
        _ => ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
            .into_iter()
            .any(|name| env::var_os(name).is_some_and(|value| !value.is_empty())),
    }
}

pub fn select(enabled: bool) {
    let _ = ENABLED.set(enabled);
}

pub fn active() -> bool {
    ENABLED.get().copied().unwrap_or(false)
}

pub fn acquire_instance() -> io::Result<File> {
    let path = settings::path()
        .ok_or_else(|| io::Error::other("No local configuration folder is available."))?;
    fs::create_dir_all(path.parent().unwrap())?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_file_name(".lock"))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(io::Error::other(
            "Another Afterbeat instance is using this configuration.",
        )),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

#[derive(Clone)]
pub struct Status {
    pub message: String,
    pub pending: bool,
    pub ready: bool,
    pub configure: bool,
    pub toggle: Option<String>,
    pub stop: Option<String>,
    pub displays: String,
}

struct State {
    status: Status,
    remote: Option<Arc<Remote>>,
}

pub fn status() -> Status {
    STATE.lock().unwrap().status.clone()
}

pub fn set_hold_mode(hold: bool) {
    HOLD_MODE.store(hold, Ordering::Release);
}

pub fn activation_down() -> bool {
    HELD.load(Ordering::Acquire)
}

pub fn toggle_binding() -> Option<Binding> {
    let state = STATE.lock().unwrap();
    let label = state.status.toggle.as_deref()?;
    Some(Binding::portal(
        shortcut_key(label).map_or(0, |key| key.0),
        label.contains('+'),
    ))
}

pub fn displays() -> Vec<Zone> {
    remote().map_or_else(Vec::new, |remote| {
        remote.monitors.iter().map(|m| m.bounds).collect()
    })
}

pub fn remote() -> Option<Arc<Remote>> {
    STATE.lock().unwrap().remote.clone()
}

pub fn can_dispatch(remote: &Arc<Remote>) -> bool {
    let state = STATE.lock().unwrap();
    state.status.stop.is_some()
        && state
            .remote
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(remote, current))
}

pub fn validate(settings: &Settings) -> std::result::Result<(), Issue> {
    if settings.process_mode != 0 {
        return Err(Issue::new(
            "filter",
            "Wayland does not expose other apps' process identities. Turn off App filter for this setup.",
        ));
    }
    if settings.edge_mode != 0 || settings.inside_zone.is_some() || settings.stop_zone.is_some() {
        return Err(Issue::new(
            "safety",
            "Wayland does not expose the live desktop pointer. Turn off edges and clear the cursor zones for this setup.",
        ));
    }
    let state = STATE.lock().unwrap();
    let Some(remote) = &state.remote else {
        return Ok(());
    };
    if state.status.stop.is_none() {
        return Err(Issue::new(
            "shortcut",
            "Bind a Wayland emergency stop shortcut before running.",
        ));
    }
    if (settings.route_enabled || (settings.fixed && !settings.keyboard))
        && remote.monitors.is_empty()
    {
        return Err(Issue::new(
            "target",
            "Reconnect with displays to use fixed points or routes on Wayland.",
        ));
    }
    let device = if settings.keyboard { KEYBOARD } else { POINTER };
    let required = if settings.route_enabled {
        device | POINTER
    } else {
        device
    };
    if remote.devices & required != required {
        return Err(Issue::new(
            "input",
            "The desktop did not grant this input device. Reconnect and allow it in the permission dialog.",
        ));
    }
    if settings.keyboard {
        // Portal labels are presentation strings. Use only labels whose keys can
        // be recognized, so emitted keys cannot start a second run or mask Stop.
        if settings.hold_activation
            && state
                .status
                .toggle
                .as_deref()
                .is_some_and(|label| label.contains('+'))
        {
            return Err(Issue::new(
                "shortcut",
                "Use an unmodified Wayland shortcut for Hold with keyboard output.",
            ));
        }
        let stop = state.status.stop.as_deref().and_then(shortcut_key);
        let toggle = state.status.toggle.as_deref().map(shortcut_key);
        if stop.is_none() || matches!(toggle, Some(None)) {
            return Err(Issue::new(
                "shortcut",
                "Use identifiable keyboard shortcuts such as F6 and F7 for Wayland keyboard output.",
            ));
        }
        let output = if settings.text_output {
            settings
                .text_character
                .chars()
                .next()
                .map(|c| keys::Key(xkeysym::Keysym::from_char(c).raw()))
        } else {
            keys::parse(&settings.key)
        };
        if let Some(output) = output
            && [stop, toggle.flatten()]
                .into_iter()
                .flatten()
                .any(|key| same_key(key, output))
        {
            return Err(Issue::new(
                "input",
                "Choose output different from the Wayland activation and emergency stop shortcuts.",
            ));
        }
    }
    Ok(())
}

fn shortcut_key(label: &str) -> Option<keys::Key> {
    let mut parts = label.rsplit('+');
    let key = keys::parse(parts.next()?)?;
    parts
        .all(|modifier| {
            matches!(
                modifier.trim().to_ascii_lowercase().as_str(),
                "ctrl" | "control" | "shift" | "alt" | "meta" | "super" | "logo" | "num"
            )
        })
        .then_some(key)
}

fn same_key(left: keys::Key, right: keys::Key) -> bool {
    let character = |key: keys::Key| xkeysym::Keysym::new(key.0).key_char();
    left.0 == right.0
        || match (character(left), character(right)) {
            (Some(a), Some(b)) => a.to_lowercase().eq(b.to_lowercase()),
            _ => false,
        }
}

enum Command {
    Enable(bool),
    Disable,
    Configure,
}

pub struct Hotkeys {
    commands: async_channel::Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl Hotkeys {
    pub fn start(
        preferred: &str,
        mut callback: impl FnMut(Action) + Send + 'static,
    ) -> io::Result<Self> {
        let preferred = Binding::preferred_trigger(preferred).unwrap_or_else(|| "F6".into());
        let (commands, requests) = async_channel::bounded(4);
        let worker = thread::Builder::new()
            .name("afterbeat-wayland".into())
            .spawn(move || block_on(worker(&requests, &preferred, &mut callback)))?;
        Ok(Self {
            commands,
            worker: Some(worker),
        })
    }

    pub fn enable(&self, displays: bool) -> bool {
        self.commands.try_send(Command::Enable(displays)).is_ok()
    }

    pub fn disable(&self) {
        let _ = self.commands.try_send(Command::Disable);
    }

    pub fn configure(&self) {
        let _ = self.commands.try_send(Command::Configure);
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        self.commands.close();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

async fn worker(
    requests: &async_channel::Receiver<Command>,
    preferred: &str,
    callback: &mut impl FnMut(Action),
) {
    while let Ok(command) = requests.recv().await {
        let Command::Enable(screens) = command else {
            continue;
        };
        update(callback, |state| {
            state.status.pending = true;
            state.status.message = "Waiting for desktop permission…".into();
        });
        let result = match Portal::connection().await {
            Ok(connection) => {
                let result =
                    run_session(connection.clone(), requests, preferred, screens, callback).await;
                let _ = connection.close().await;
                result
            }
            Err(error) => Err(error),
        };
        HELD.store(false, Ordering::Release);
        update(callback, |state| {
            state.remote = None;
            state.status.pending = false;
            state.status.ready = false;
            state.status.configure = false;
            state.status.toggle = None;
            state.status.stop = None;
            state.status.displays.clear();
            state.status.message = match result {
                Ok(()) => "Wayland input is disconnected.".into(),
                Err(error) => {
                    diagnostics::record(&format!("Wayland portal: {error}"));
                    format!("Wayland input unavailable. {error}")
                }
            };
        });
    }
}

fn update(callback: &mut impl FnMut(Action), update: impl FnOnce(&mut State)) {
    update(&mut STATE.lock().unwrap());
    callback(Action::PortalChanged);
}

async fn run_session(
    connection: zbus::Connection,
    requests: &async_channel::Receiver<Command>,
    preferred: &str,
    screens: bool,
    callback: &mut impl FnMut(Action),
) -> Result<()> {
    let portal = Portal::connect(connection).await?;
    let mut owner = portal.watch_owner().await?;
    let setup = async {
        let global = portal.desktop(GLOBAL).await?;
        let version: u32 = global
            .get_property("version")
            .await
            .map_err(|_| "This desktop needs a portal backend with Global Shortcuts support.")?;
        let shortcuts = portal.session(&global).await?;
        let closed = shortcuts.proxy.receive_signal("Closed").await?;
        let bindings = vec![
            shortcut(ACTIVATE, "Start or stop Afterbeat", preferred),
            shortcut(STOP, "Afterbeat emergency stop", "F7"),
        ];
        let mut result = portal
            .request(&global, "BindShortcuts", |opts| {
                (&shortcuts.path, &bindings, "", opts)
            })
            .await?;
        let bound = take_shortcuts(&mut result)?;
        if shortcut_label(&bound, STOP).is_none() {
            return Err("Bind the emergency stop shortcut to enable automation.".into());
        }
        let (remote, remote_closed) = Remote::start(&portal, screens).await?;
        let signals = global.receive_all_signals().await?;
        let mut result = portal
            .request(&global, "ListShortcuts", |opts| (&shortcuts.path, opts))
            .await?;
        let bound = take_shortcuts(&mut result)?;
        if shortcut_label(&bound, STOP).is_none() {
            return Err(
                "The emergency stop shortcut was removed. Bind it to enable automation.".into(),
            );
        }
        let result: Result<_> = Ok((
            global,
            version,
            shortcuts,
            signals,
            closed,
            remote,
            remote_closed,
            bound,
        ));
        result
    };
    let cancel = async {
        let _ = requests.recv().await;
        Ok(None)
    };
    let lost = async {
        let _ = owner.next().await;
        Err("The desktop portal disconnected.".into())
    };
    let opened = race(async { setup.await.map(Some) }, race(cancel, lost)).await?;
    let Some((
        global,
        version,
        shortcuts,
        mut signals,
        mut shortcut_closed,
        remote,
        mut remote_closed,
        bound,
    )) = opened
    else {
        return Ok(());
    };
    let displays = remote
        .monitors
        .iter()
        .map(|monitor| {
            let Zone { from, to } = monitor.bounds;
            format!("({}, {}) to ({}, {})", from.x, from.y, to.x, to.y)
        })
        .collect::<Vec<_>>()
        .join(" · ");
    update(callback, |state| {
        state.remote = Some(remote);
        state.status.pending = false;
        state.status.ready = true;
        state.status.configure = version >= 2;
        state.status.message =
            "Wayland input is enabled. Escape stops a run while Afterbeat has focus.".into();
        state.status.displays = displays;
        set_shortcuts(&mut state.status, &bound);
    });
    let mut configuration = None;
    loop {
        let close = async {
            race(race(shortcut_closed.next(), remote_closed.next()), async {
                owner.next().await;
                None
            })
            .await;
            let result: Result<Next> = Err("The desktop closed the Wayland session.".into());
            result
        };
        let next = race(
            async {
                Ok(Next::Signal(
                    signals
                        .next()
                        .await
                        .ok_or("The shortcut listener disconnected.")?,
                ))
            },
            race(
                async { Ok(Next::Command(requests.recv().await.ok())) },
                race(close, async {
                    match &mut configuration {
                        Some(reply) => Ok(Next::Configuration(reply.await)),
                        None => pending().await,
                    }
                }),
            ),
        )
        .await?;
        match next {
            Next::Command(Some(Command::Configure)) if version >= 2 => {
                if configuration.is_none() {
                    configuration = Some(Box::pin(async {
                        global
                            .call("ConfigureShortcuts", &(&shortcuts.path, "", Options::new()))
                            .await
                    }));
                }
            }
            Next::Configuration(result) => {
                configuration = None;
                if let Err(error) = result {
                    STATE.lock().unwrap().status.message =
                        format!("Could not open shortcut settings. {error}");
                    callback(Action::PortalStatus);
                }
            }
            Next::Command(_) => return Ok(()),
            Next::Signal(signal) => {
                let header = signal.header();
                match header.member().map(|member| member.as_str()) {
                    Some("Activated" | "Deactivated") => {
                        let (path, id, _, _): (
                            zbus::zvariant::OwnedObjectPath,
                            String,
                            u64,
                            Options,
                        ) = signal.body().deserialize()?;
                        if path != shortcuts.path {
                            continue;
                        }
                        let down = header.member().unwrap().as_str() == "Activated";
                        if id == STOP && down {
                            callback(Action::Stop(tick_count()));
                        } else if id == ACTIVATE && HELD.swap(down, Ordering::AcqRel) != down {
                            if down && let Some(binding) = toggle_binding() {
                                callback(Action::Toggle(binding, tick_count()));
                            } else if !down && HOLD_MODE.load(Ordering::Acquire) {
                                callback(Action::Stop(tick_count()));
                            }
                        }
                    }
                    Some("ShortcutsChanged") => {
                        let (path, bound): (zbus::zvariant::OwnedObjectPath, Shortcuts) =
                            signal.body().deserialize()?;
                        if path == shortcuts.path {
                            HELD.store(false, Ordering::Release);
                            update(callback, |state| set_shortcuts(&mut state.status, &bound));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

enum Next {
    Command(Option<Command>),
    Configuration(zbus::Result<()>),
    Signal(zbus::Message),
}

fn shortcut(id: &str, description: &str, trigger: &str) -> (String, Options) {
    (
        id.into(),
        Options::from([
            ("description".into(), string(description)),
            ("preferred_trigger".into(), string(trigger)),
        ]),
    )
}

fn take_shortcuts(result: &mut Options) -> Result<Shortcuts> {
    Ok(result
        .remove("shortcuts")
        .ok_or("Missing global shortcut registration.")?
        .try_into()?)
}

fn set_shortcuts(status: &mut Status, shortcuts: &Shortcuts) {
    status.toggle = shortcut_label(shortcuts, ACTIVATE);
    status.stop = shortcut_label(shortcuts, STOP);
}

fn shortcut_label(shortcuts: &Shortcuts, id: &str) -> Option<String> {
    let (_, props) = shortcuts.iter().find(|(key, _)| key == id)?;
    let label = <&str>::try_from(props.get("trigger_description")?)
        .ok()?
        .trim();
    (!label.is_empty()).then(|| label.to_owned())
}

struct Monitor {
    stream: u32,
    bounds: Zone,
}

pub struct Remote {
    devices: u32,
    monitors: Vec<Monitor>,
    session: portal::Session,
    proxy: Proxy<'static>,
}

impl Remote {
    async fn start(
        portal: &Portal,
        screens: bool,
    ) -> Result<(Arc<Self>, zbus::proxy::SignalStream<'static>)> {
        let proxy = portal.desktop(REMOTE).await?;
        let available: u32 = proxy
            .get_property("AvailableDeviceTypes")
            .await
            .map_err(|_| "This desktop needs a portal backend with Remote Desktop support.")?;
        let devices = available & (KEYBOARD | POINTER);
        if devices == 0 {
            return Err("The desktop provides no controllable keyboard or pointer.".into());
        }
        let session = portal.session(&proxy).await?;
        let closed = session.proxy.receive_signal("Closed").await?;
        portal
            .request(&proxy, "SelectDevices", |mut opts| {
                opts.insert("types".into(), devices.into());
                (&session.path, opts)
            })
            .await?;
        if screens {
            let screen = portal.desktop(SCREENS).await?;
            portal
                .request(&screen, "SelectSources", |mut opts| {
                    opts.insert("types".into(), MONITOR.into());
                    opts.insert("multiple".into(), true.into());
                    (&session.path, opts)
                })
                .await?;
        }
        let mut result = portal
            .request(&proxy, "Start", |opts| (&session.path, "", opts))
            .await?;
        let devices = u32::try_from(
            result
                .remove("devices")
                .ok_or("Missing input permission.")?,
        )? & devices;
        if devices == 0 {
            return Err("Allow the keyboard or pointer in the desktop dialog.".into());
        }
        let monitors = if screens {
            let streams: Vec<(u32, Options)> = result
                .remove("streams")
                .ok_or("The desktop did not share a display.")?
                .try_into()?;
            let mut monitors: Vec<_> = streams
                .into_iter()
                .filter_map(|(stream, props)| {
                    if props
                        .get("source_type")
                        .and_then(|value| u32::try_from(value).ok())
                        .is_some_and(|kind| kind != MONITOR)
                    {
                        return None;
                    }
                    let (w, h) = pair(&props, "logical_size").or_else(|| pair(&props, "size"))?;
                    let (x, y) = pair(&props, "position")?;
                    if w <= 0 || h <= 0 {
                        return None;
                    }
                    Some(Monitor {
                        stream,
                        bounds: Zone {
                            from: Point { x, y },
                            to: Point {
                                x: x.checked_add(w - 1)?,
                                y: y.checked_add(h - 1)?,
                            },
                        },
                    })
                })
                .collect();
            if monitors.is_empty() {
                return Err("The portal did not report usable logical display bounds. Connect without displays to follow the cursor.".into());
            }
            monitors.sort_unstable_by_key(|m| (m.bounds.from.x, m.bounds.from.y, m.stream));
            monitors
        } else {
            Vec::new()
        };
        Ok((
            Arc::new(Self {
                devices,
                monitors,
                session,
                proxy,
            }),
            closed,
        ))
    }

    pub fn move_to(&self, point: Point) -> Result<()> {
        let monitor = self
            .monitors
            .iter()
            .find(|m| m.bounds.contains(point))
            .ok_or("A target is outside the shared Wayland displays.")?;
        let from = monitor.bounds.from;
        let x = f64::from(point.x) - f64::from(from.x);
        let y = f64::from(point.y) - f64::from(from.y);
        Ok(block_on(self.proxy.call(
            "NotifyPointerMotionAbsolute",
            &(&self.session.path, Options::new(), monitor.stream, x, y),
        ))?)
    }

    pub fn send(&self, keyboard: bool, code: i32, down: bool) -> Result<()> {
        let method = if keyboard {
            "NotifyKeyboardKeysym"
        } else {
            "NotifyPointerButton"
        };
        Ok(block_on(self.proxy.call(
            method,
            &(&self.session.path, Options::new(), code, u32::from(down)),
        ))?)
    }

    pub fn close(&self) {
        let _: zbus::Result<()> = block_on(self.session.proxy.call("Close", &()));
    }
}

fn pair(props: &Options, name: &str) -> Option<(i32, i32)> {
    props.get(name)?.try_clone().ok()?.try_into().ok()
}
