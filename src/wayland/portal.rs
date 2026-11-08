use async_io::Timer;
use futures_lite::{StreamExt, future::race};
use serde::Serialize;
use std::{
    collections::HashMap,
    error::Error,
    io,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use zbus::{
    Connection, Proxy,
    connection::Builder,
    fdo::DBusProxy,
    names::OwnedUniqueName,
    proxy::OwnerChangedStream,
    zvariant::{DynamicType, OwnedObjectPath, OwnedValue, Str},
};

pub type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
pub type Options = HashMap<String, OwnedValue>;
pub type Shortcuts = Vec<(String, Options)>;

const SERVICE: &str = "org.freedesktop.portal.Desktop";
const DESKTOP: &str = "/org/freedesktop/portal/desktop";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";
pub const GLOBAL: &str = "org.freedesktop.portal.GlobalShortcuts";
pub const REMOTE: &str = "org.freedesktop.portal.RemoteDesktop";
pub const SCREENS: &str = "org.freedesktop.portal.ScreenCast";
pub const APP_ID: &str = "io.github.xeintdm.Afterbeat";

pub struct Portal {
    pub connection: Connection,
    owner: OwnedUniqueName,
}

impl Portal {
    pub async fn connect(connection: Connection) -> Result<Self> {
        let dbus = DBusProxy::new(&connection).await?;
        let name = SERVICE.try_into()?;
        let owner = match dbus.get_name_owner(name).await {
            Ok(owner) => owner,
            Err(zbus::fdo::Error::NameHasNoOwner(_)) => {
                dbus.start_service_by_name(SERVICE.try_into()?, 0).await?;
                dbus.get_name_owner(SERVICE.try_into()?).await?
            }
            Err(error) => return Err(error.into()),
        };
        let portal = Self { connection, owner };
        // Older portals and direct launches without an installed desktop file can
        // still use the application identity assigned by the portal itself.
        let registry = portal
            .desktop("org.freedesktop.host.portal.Registry")
            .await?;
        let _: zbus::Result<()> = registry.call("Register", &(APP_ID, Options::new())).await;
        Ok(portal)
    }

    pub async fn connection() -> Result<Connection> {
        Ok(Builder::session()?
            .method_timeout(Duration::from_secs(1))
            .build()
            .await?)
    }

    pub async fn desktop(&self, interface: &'static str) -> Result<Proxy<'static>> {
        self.proxy(DESKTOP.to_owned(), interface).await
    }

    async fn proxy(&self, path: String, interface: &'static str) -> Result<Proxy<'static>> {
        Ok(Proxy::new_owned(self.connection.clone(), self.owner.clone(), path, interface).await?)
    }

    pub async fn watch_owner(&self) -> Result<OwnerChangedStream<'static>> {
        let proxy = Proxy::new_owned(self.connection.clone(), SERVICE, DESKTOP, GLOBAL).await?;
        let stream = proxy.receive_owner_changed().await?;
        let actual = DBusProxy::new(&self.connection)
            .await?
            .get_name_owner(SERVICE.try_into()?)
            .await?;
        if actual.as_str() != self.owner.as_str() {
            return Err("The desktop portal restarted. Enable Wayland input again.".into());
        }
        Ok(stream)
    }

    pub async fn request<B: Serialize + DynamicType>(
        &self,
        proxy: &Proxy<'_>,
        method: &str,
        make_args: impl FnOnce(Options) -> B,
    ) -> Result<Options> {
        let handle = token();
        let sender = self
            .connection
            .unique_name()
            .ok_or("The session bus did not assign an application name.")?
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let expected = format!("{DESKTOP}/request/{sender}/{handle}");
        let request = self.proxy(expected.clone(), REQUEST).await?;
        // Subscribe first. Some portals reply before the method call returns.
        let mut responses = request.receive_signal("Response").await?;
        let args = make_args(Options::from([("handle_token".into(), string(&handle))]));
        let path: OwnedObjectPath = proxy.call(method, &args).await?;
        if path.as_str() != expected {
            let old_request = self.proxy(path.to_string(), REQUEST).await?;
            let _: zbus::Result<()> = old_request.call("Close", &()).await;
            return Err("The desktop portal returned an unexpected request handle.".into());
        }
        let response = race(
            async {
                responses
                    .next()
                    .await
                    .ok_or("The desktop portal disconnected.")
            },
            async {
                Timer::after(Duration::from_secs(120)).await;
                Err("The desktop permission request timed out.")
            },
        )
        .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let _: zbus::Result<()> = request.call("Close", &()).await;
                return Err(error.into());
            }
        };
        let (code, results): (u32, Options) = response.body().deserialize()?;
        match code {
            0 => Ok(results),
            1 => Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "The desktop permission request was cancelled.",
            )
            .into()),
            _ => Err("The desktop portal declined the request.".into()),
        }
    }

    pub async fn session(&self, proxy: &Proxy<'_>) -> Result<Session> {
        let mut response = self
            .request(proxy, "CreateSession", |mut options| {
                options.insert("session_handle_token".into(), string(&token()));
                (options,)
            })
            .await?;
        let handle = response
            .remove("session_handle")
            .ok_or("Missing portal session handle.")?;
        let path = match <&str>::try_from(&handle) {
            Ok(path) => OwnedObjectPath::try_from(path)?,
            Err(_) => OwnedObjectPath::try_from(handle)?,
        };
        let proxy = self.proxy(path.to_string(), SESSION).await?;
        Ok(Session { path, proxy })
    }
}

pub struct Session {
    pub path: OwnedObjectPath,
    pub proxy: Proxy<'static>,
}

pub fn string(value: &str) -> OwnedValue {
    Str::from(value).into()
}

fn token() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("afterbeat_{}", NEXT.fetch_add(1, Ordering::Relaxed))
}
