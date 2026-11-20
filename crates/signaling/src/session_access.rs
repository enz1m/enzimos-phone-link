//! Bind phone access to the local graphical login that starts the host.

use std::time::Duration;
use zbus::{Connection, Proxy, zvariant::OwnedObjectPath};

pub(crate) struct Gate {
    session: Proxy<'static>,
}

impl Gate {
    pub(crate) async fn open() -> Option<Self> {
        tokio::time::timeout(Duration::from_secs(2), open())
            .await
            .ok()
            .flatten()
    }
    pub(crate) async fn allowed(&self) -> bool {
        if self.session.connection().is_closed() {
            return false;
        }
        tokio::time::timeout(Duration::from_secs(2), read(&self.session))
            .await
            .unwrap_or(false)
    }
}

async fn open() -> Option<Gate> {
    let Ok(connection) = Connection::system().await else {
        return None;
    };
    bind(
        connection,
        std::env::var("XDG_SESSION_ID").ok(),
        std::process::id(),
    )
    .await
}

async fn bind(connection: Connection, hint: Option<String>, pid: u32) -> Option<Gate> {
    let Ok(manager) = Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await
    else {
        return None;
    };
    // User services belong to logind's manager session, so the graphical
    // shell imports its session ID. Validate the resolved session below.
    let path = if let Some(hint) = hint {
        if hint.is_empty()
            || hint.len() > 32
            || !hint
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return None;
        }
        let Ok(path) = manager
            .call::<_, _, OwnedObjectPath>("GetSession", &(hint,))
            .await
        else {
            return None;
        };
        path
    } else {
        let Ok(path) = manager
            .call::<_, _, OwnedObjectPath>("GetSessionByPID", &(pid,))
            .await
        else {
            return None;
        };
        path
    };
    let session = zbus::proxy::Builder::<Proxy<'static>>::new(&connection)
        .destination("org.freedesktop.login1")
        .ok()?
        .path(path)
        .ok()?
        .interface("org.freedesktop.login1.Session")
        .ok()?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .ok()?;
    Some(Gate { session })
}

async fn read(session: &Proxy<'_>) -> bool {
    let Ok((uid, _)) = session.get_property::<(u32, OwnedObjectPath)>("User").await else {
        return false;
    };
    let Ok((seat, _)) = session
        .get_property::<(String, OwnedObjectPath)>("Seat")
        .await
    else {
        return false;
    };
    let Ok(kind) = session.get_property::<String>("Type").await else {
        return false;
    };
    let Ok(remote) = session.get_property::<bool>("Remote").await else {
        return false;
    };
    let Ok(active) = session.get_property::<bool>("Active").await else {
        return false;
    };
    let Ok(locked) = session.get_property::<bool>("LockedHint").await else {
        return false;
    };
    uid == nix::unistd::getuid().as_raw()
        && !seat.is_empty()
        && matches!(kind.as_str(), "wayland" | "x11")
        && !remote
        && active
        && !locked
}

#[cfg(test)]
mod tests;
