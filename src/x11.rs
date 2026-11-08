use crate::mouse::{Point, Zone};
use std::{cell::OnceCell, env, error::Error, io};
use x11rb::{
    CURRENT_TIME, NONE,
    connection::Connection,
    protocol::{
        randr::ConnectionExt as _,
        res::{ClientIdMask, ClientIdSpec, ConnectionExt as _},
        xinput::{ConnectionExt as _, DeviceType},
        xproto::{ConnectionExt as _, GrabMode, GrabStatus, InputFocus, Window},
        xtest::ConnectionExt as _,
    },
    xcb_ffi::XCBConnection,
};

pub mod shell;

pub type XResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const X11_REQUIRED: &str = "Afterbeat requires a local X11 desktop. Sign in to an X11/Xorg session to use desktop automation.";

thread_local! {
    static DESKTOP: OnceCell<Desktop> = const { OnceCell::new() };
}

pub fn with<T>(action: impl FnOnce(&Desktop) -> XResult<T>) -> io::Result<T> {
    DESKTOP.with(|slot| {
        if slot.get().is_none() {
            let _ = slot.set(Desktop::connect().map_err(io::Error::other)?);
        }
        action(slot.get().unwrap()).map_err(io::Error::other)
    })
}

pub struct Desktop {
    pub conn: XCBConnection,
    pub root: Window,
    pub pointer: u16,
}

impl Desktop {
    pub fn connect() -> XResult<Self> {
        let display = env::var("DISPLAY").unwrap_or_default();
        let wayland = match env::var("XDG_SESSION_TYPE") {
            Ok(kind) if kind.eq_ignore_ascii_case("x11") => false,
            Ok(kind) if kind.eq_ignore_ascii_case("wayland") => true,
            _ => env::var_os("WAYLAND_DISPLAY").is_some_and(|name| !name.is_empty()),
        };
        if wayland
            || !(display.starts_with(':')
                || display.starts_with("unix:")
                || display.starts_with("unix/:"))
        {
            return Err(X11_REQUIRED.into());
        }
        let (conn, screen) = XCBConnection::connect(None)
            .map_err(|error| format!("Could not connect to the X11 display ({error})."))?;
        let root = conn.setup().roots[screen].root;
        if conn.query_extension(b"XWAYLAND")?.reply()?.present {
            return Err(X11_REQUIRED.into());
        }
        let xi = conn.xinput_xi_query_version(2, 1)?.reply()?;
        let rr = conn.randr_query_version(1, 5)?.reply()?;
        let res = conn.res_query_version(1, 2)?.reply()?;
        let xtest = conn.xtest_get_version(2, 2)?.reply()?;
        if (xi.major_version, xi.minor_version) < (2, 1)
            || (rr.major_version, rr.minor_version) < (1, 5)
            || (res.server_major, res.server_minor) < (1, 2)
            || (xtest.major_version, xtest.minor_version) < (2, 2)
        {
            return Err(
                "The X11 server needs XInput 2.1, RandR 1.5, X-Resource 1.2 and XTEST 2.2.".into(),
            );
        }
        let pointer = conn
            .xinput_xi_query_device(1u16)?
            .reply()?
            .infos
            .into_iter()
            .find(|device| device.enabled && device.type_ == DeviceType::MASTER_POINTER)
            .ok_or("No X11 pointer is available.")?
            .deviceid;
        Ok(Self {
            conn,
            root,
            pointer,
        })
    }

    pub fn atom(&self, name: &[u8]) -> XResult<u32> {
        Ok(self.conn.intern_atom(false, name)?.reply()?.atom)
    }

    pub fn cursor(&self) -> XResult<Point> {
        let pointer = self.conn.query_pointer(self.root)?.reply()?;
        if !pointer.same_screen {
            return Err("The pointer is on another X11 screen.".into());
        }
        Ok(Point {
            x: pointer.root_x.into(),
            y: pointer.root_y.into(),
        })
    }

    pub fn displays(&self) -> XResult<Vec<Zone>> {
        let reply = self.conn.randr_get_monitors(self.root, true)?.reply()?;
        let mut displays: Vec<_> = reply
            .monitors
            .into_iter()
            .filter(|monitor| monitor.width > 0 && monitor.height > 0)
            .map(|monitor| {
                let (x, y) = (i32::from(monitor.x), i32::from(monitor.y));
                Zone {
                    from: Point { x, y },
                    to: Point {
                        x: x + i32::from(monitor.width) - 1,
                        y: y + i32::from(monitor.height) - 1,
                    },
                }
            })
            .collect();
        displays.sort_unstable_by_key(|zone| (zone.from.x, zone.from.y, zone.to.x, zone.to.y));
        Ok(displays)
    }

    pub fn windows_at(&self, point: Point) -> XResult<Vec<Window>> {
        let (x, y) = (i16::try_from(point.x)?, i16::try_from(point.y)?);
        let mut window = self.root;
        let mut path = Vec::new();
        for _ in 0..64 {
            let hit = self
                .conn
                .translate_coordinates(self.root, window, x, y)?
                .reply()?;
            if hit.child == NONE {
                return Ok(path);
            }
            window = hit.child;
            path.push(window);
        }
        Err("The X11 window hierarchy is too deep.".into())
    }

    pub fn top_level(&self, mut window: Window) -> XResult<Window> {
        for _ in 0..64 {
            let parent = self.conn.query_tree(window)?.reply()?.parent;
            if parent == self.root {
                return Ok(window);
            }
            if parent == NONE {
                break;
            }
            window = parent;
        }
        Err("The Afterbeat window is on another X11 screen.".into())
    }

    pub fn window_pid(&self, window: Window) -> XResult<u32> {
        if window == NONE || window == self.root {
            return Ok(0);
        }
        let spec = ClientIdSpec {
            client: window,
            mask: ClientIdMask::LOCAL_CLIENT_PID,
        };
        let reply = self.conn.res_query_client_ids(&[spec])?.reply()?;
        Ok(reply
            .ids
            .iter()
            .find_map(|id| id.value.first().copied())
            .unwrap_or_default())
    }

    pub fn pointer_pid(&self, point: Point) -> XResult<u32> {
        self.window_pid(self.windows_at(point)?.last().copied().unwrap_or(NONE))
    }

    pub fn focus_pid(&self) -> XResult<u32> {
        let focus = self.conn.get_input_focus()?.reply()?.focus;
        if focus == u32::from(u8::from(InputFocus::POINTER_ROOT)) {
            return self.pointer_pid(self.cursor()?);
        }
        self.window_pid(focus)
    }

    pub fn pointer_free(&self, window: Window) -> XResult<bool> {
        let status = self
            .conn
            .grab_pointer(
                true,
                window,
                x11rb::protocol::xproto::EventMask::NO_EVENT,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                NONE,
                NONE,
                CURRENT_TIME,
            )?
            .reply()?
            .status;
        if status == GrabStatus::SUCCESS {
            self.conn.ungrab_pointer(CURRENT_TIME)?.check()?;
        }
        Ok(status == GrabStatus::SUCCESS)
    }

    pub fn keyboard_free(&self) -> XResult<bool> {
        let focus = self.conn.get_input_focus()?.reply()?.focus;
        let window = if focus <= 1 { self.root } else { focus };
        let status = self
            .conn
            .grab_keyboard(true, window, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?
            .reply()?
            .status;
        if status == GrabStatus::SUCCESS {
            self.conn.ungrab_keyboard(CURRENT_TIME)?.check()?;
        }
        Ok(status == GrabStatus::SUCCESS)
    }

    pub fn grab(&self) -> XResult<ServerGrab<'_>> {
        self.conn.grab_server()?.check()?;
        Ok(ServerGrab(&self.conn))
    }
}

pub struct ServerGrab<'a>(&'a XCBConnection);

impl Drop for ServerGrab<'_> {
    fn drop(&mut self) {
        let _ = self.0.ungrab_server();
        let _ = self.0.flush();
    }
}
