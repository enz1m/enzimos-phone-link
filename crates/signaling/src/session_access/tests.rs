use super::*;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
};

struct Bus(Child);
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[derive(Clone)]
struct Properties {
    uid: u32,
    seat: String,
    kind: String,
    remote: bool,
    active: bool,
    locked: bool,
}
struct Session(Arc<Mutex<Properties>>);
#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl Session {
    #[zbus(property)]
    fn user(&self) -> (u32, OwnedObjectPath) {
        (self.0.lock().unwrap().uid, "/user".try_into().unwrap())
    }
    #[zbus(property)]
    fn seat(&self) -> (String, OwnedObjectPath) {
        (
            self.0.lock().unwrap().seat.clone(),
            "/seat".try_into().unwrap(),
        )
    }
    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        self.0.lock().unwrap().kind.clone()
    }
    #[zbus(property)]
    fn remote(&self) -> bool {
        self.0.lock().unwrap().remote
    }
    #[zbus(property)]
    fn active(&self) -> bool {
        self.0.lock().unwrap().active
    }
    #[zbus(property)]
    fn locked_hint(&self) -> bool {
        self.0.lock().unwrap().locked
    }
}
struct Manager;
#[zbus::interface(name = "org.freedesktop.login1.Manager")]
impl Manager {
    fn get_session(&self, id: &str) -> zbus::fdo::Result<OwnedObjectPath> {
        if id == "graphical" {
            Ok("/session".try_into().unwrap())
        } else {
            Err(zbus::fdo::Error::Failed("No such session".into()))
        }
    }
    #[zbus(name = "GetSessionByPID")]
    fn get_session_by_pid(&self, _pid: u32) -> OwnedObjectPath {
        "/session".try_into().unwrap()
    }
}

/// A phone must lose access on lock, seat loss or account mismatch. This talks
/// to a private logind fixture rather than changing the real login session.
#[tokio::test]
async fn graphical_session_access_tracks_lock_and_account_boundaries() {
    let directory = tempfile::tempdir().unwrap();
    let mut child = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .arg(format!(
            "--address=unix:path={}",
            directory.path().join("bus").display()
        ))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut address = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let _bus = Bus(child);
    let initial = Properties {
        uid: nix::unistd::getuid().as_raw(),
        seat: "seat0".into(),
        kind: "wayland".into(),
        remote: false,
        active: true,
        locked: false,
    };
    let state = Arc::new(Mutex::new(initial.clone()));
    let server = zbus::connection::Builder::address(address.trim())
        .unwrap()
        .name("org.freedesktop.login1")
        .unwrap()
        .serve_at("/org/freedesktop/login1", Manager)
        .unwrap()
        .serve_at("/session", Session(state.clone()))
        .unwrap()
        .build()
        .await
        .unwrap();
    let client = zbus::connection::Builder::address(address.trim())
        .unwrap()
        .build()
        .await
        .unwrap();
    let gate = bind(client.clone(), Some("graphical".into()), 1)
        .await
        .expect("imported graphical session");
    assert!(
        gate.allowed().await,
        "unlocked local graphical session must allow the paired phone"
    );
    assert!(
        bind(client.clone(), Some("missing".into()), 1)
            .await
            .is_none()
    );
    assert!(
        bind(client.clone(), Some("../graphical".into()), 1)
            .await
            .is_none()
    );
    for (label, changed) in [
        (
            "locked",
            Properties {
                locked: true,
                ..initial.clone()
            },
        ),
        (
            "inactive",
            Properties {
                active: false,
                ..initial.clone()
            },
        ),
        (
            "remote login",
            Properties {
                remote: true,
                ..initial.clone()
            },
        ),
        (
            "another account",
            Properties {
                uid: initial.uid + 1,
                ..initial.clone()
            },
        ),
        (
            "no seat",
            Properties {
                seat: String::new(),
                ..initial.clone()
            },
        ),
        (
            "user manager",
            Properties {
                kind: "manager".into(),
                ..initial.clone()
            },
        ),
    ] {
        *state.lock().unwrap() = changed;
        assert!(!gate.allowed().await, "{label} must withdraw access");
    }
    *state.lock().unwrap() = Properties {
        kind: "x11".into(),
        ..initial.clone()
    };
    assert!(gate.allowed().await, "local X11 is supported too");
    server.close().await.unwrap();
    assert!(!gate.allowed().await, "logind loss must fail closed");
}
