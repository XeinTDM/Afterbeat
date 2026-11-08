use crate::{
    App, settings,
    x11::{self, Desktop, XResult},
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
};
use x11rb::{
    COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE,
    connection::Connection,
    protocol::xproto::{
        AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
        Window, WindowClass,
    },
    wrapper::ConnectionExt as _,
};

const INSTANCE: &[u8] = b"_AFTERBEAT_INSTANCE";
const MAIN_WINDOW: &[u8] = b"_AFTERBEAT_MAIN_WINDOW";

pub fn select_backend() -> Result<(), slint::PlatformError> {
    use slint::winit_030::{
        SlintEvent,
        winit::{
            event_loop::EventLoop,
            platform::x11::{EventLoopBuilderExtX11, WindowAttributesExtX11},
        },
    };
    let mut builder = EventLoop::<SlintEvent>::with_user_event();
    builder.with_x11();
    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name("software".into())
        .with_winit_event_loop_builder(builder)
        .with_winit_window_attributes_hook(|attrs| attrs.with_name("Afterbeat", "afterbeat"))
        .select()
}

pub fn window_handle(app: &App) -> io::Result<Window> {
    let handle = app.window().window_handle();
    match handle.window_handle().map_err(io::Error::other)?.as_raw() {
        RawWindowHandle::Xlib(window) => u32::try_from(window.window).map_err(io::Error::other),
        RawWindowHandle::Xcb(window) => Ok(window.window.get()),
        _ => Err(io::Error::other("An X11 window is required.")),
    }
}

pub struct Instance {
    owner: Window,
    _lock: Option<File>,
}

impl Instance {
    pub fn acquire() -> io::Result<Option<Self>> {
        x11::with(|desktop| {
            let lock = if let Some(path) = settings::path() {
                fs::create_dir_all(path.parent().unwrap())?;
                let file = OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(path.with_file_name(".lock"))?;
                file.try_lock().map(|()| Some(file))
            } else {
                Ok(None)
            };
            let _grab = desktop.grab()?;
            let selection = desktop.atom(INSTANCE)?;
            let owner = desktop.conn.get_selection_owner(selection)?.reply()?.owner;
            if owner != NONE {
                activate(desktop, owner)?;
                return Ok(None);
            }
            let lock = match lock {
                Ok(lock) => lock,
                Err(TryLockError::WouldBlock) => {
                    return Err("Another Afterbeat instance is using this configuration.".into());
                }
                Err(TryLockError::Error(error)) => return Err(error.into()),
            };
            let owner = desktop.conn.generate_id()?;
            desktop
                .conn
                .create_window(
                    COPY_DEPTH_FROM_PARENT,
                    owner,
                    desktop.root,
                    0,
                    0,
                    1,
                    1,
                    0,
                    WindowClass::INPUT_ONLY,
                    0,
                    &CreateWindowAux::new(),
                )?
                .check()?;
            if desktop.window_pid(owner)? != std::process::id() {
                return Err(
                    "The X11 server could not identify Afterbeat as a local process.".into(),
                );
            }
            desktop
                .conn
                .set_selection_owner(owner, selection, CURRENT_TIME)?
                .check()?;
            Ok(Some(Self { owner, _lock: lock }))
        })
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = x11::with(|desktop| {
            desktop.conn.destroy_window(self.owner)?.check()?;
            Ok(())
        });
    }
}

pub fn publish_window(window: Window) -> io::Result<()> {
    x11::with(|desktop| {
        let owner = desktop
            .conn
            .get_selection_owner(desktop.atom(INSTANCE)?)?
            .reply()?
            .owner;
        desktop
            .conn
            .change_property32(
                PropMode::REPLACE,
                owner,
                desktop.atom(MAIN_WINDOW)?,
                AtomEnum::WINDOW,
                &[window],
            )?
            .check()?;
        Ok(())
    })
}

fn activate(desktop: &Desktop, owner: Window) -> XResult<()> {
    let reply = desktop
        .conn
        .get_property(
            false,
            owner,
            desktop.atom(MAIN_WINDOW)?,
            AtomEnum::WINDOW,
            0,
            1,
        )?
        .reply()?;
    if let Some(window) = reply.value32().and_then(|mut values| values.next()) {
        let event = ClientMessageEvent::new(
            32,
            window,
            desktop.atom(b"_NET_ACTIVE_WINDOW")?,
            [1, CURRENT_TIME, 0, 0, 0],
        );
        desktop
            .conn
            .send_event(
                false,
                desktop.root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )?
            .check()?;
    }
    Ok(())
}
