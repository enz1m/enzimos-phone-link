//! Remote input for the computer's own desktop: phone as touchpad, keyboard
//! and presenter remote (no video).
//!
//! GNOME: Mutter's `org.gnome.Mutter.RemoteDesktop` session (what
//! gnome-remote-desktop uses; GNOME shows its remote-control indicator while
//! a session is open, so it closes after a minute without input).
//! wlroots / Hyprland (enzimOS): the `enzremote-input-wl` helper with the
//! virtual pointer and keyboard protocols on the desktop's Wayland display.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Session closes after this long without input.
const IDLE_CLOSE: Duration = Duration::from_secs(60);
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;
/// Mutter axis flags: finger source (smooth), finish.
const AXIS_FINGER: u32 = 1 << 2;
const AXIS_FINISH: u32 = 1 << 0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    /// Relative pointer motion in desktop pixels.
    Move { dx: f64, dy: f64 },
    Button { button: MouseButton, pressed: bool },
    Click { button: MouseButton },
    /// Smooth scroll in pixels; `stop` ends the gesture (kinetic scrolling).
    Scroll { dx: f64, dy: f64, #[serde(default)] stop: bool },
    /// evdev key code.
    Key { code: u32, pressed: bool },
    Text { text: String },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    fn code(self) -> i32 {
        match self {
            Self::Left => BTN_LEFT,
            Self::Right => BTN_RIGHT,
            Self::Middle => BTN_MIDDLE,
        }
    }
}

// One backend exists per host; its size does not matter.
#[allow(clippy::large_enum_variant)]
enum Backend {
    Mutter(zbus::Proxy<'static>),
    Wayland {
        child: tokio::process::Child,
        stdin: tokio::process::ChildStdin,
        stdout: BufReader<tokio::process::ChildStdout>,
    },
}

struct Session {
    backend: Backend,
    last_used: Instant,
}

#[derive(Default)]
pub struct DesktopInput {
    session: Mutex<Option<Session>>,
}

impl DesktopInput {
    pub fn new() -> Arc<Self> {
        let input = Arc::new(Self::default());
        let weak = Arc::downgrade(&input);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let Some(input) = weak.upgrade() else { return };
                let mut session = input.session.lock().await;
                if session.as_ref().is_some_and(|s| s.last_used.elapsed() > IDLE_CLOSE)
                    && let Some(s) = session.take()
                {
                    close(s).await;
                }
            }
        });
        input
    }

    /// Inject a batch of events, opening the session when needed.
    pub async fn apply(&self, events: &[InputEvent]) -> Result<(), String> {
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            *guard = Some(Session { backend: open().await?, last_used: Instant::now() });
        }
        let session = guard.as_mut().expect("session opened");
        session.last_used = Instant::now();
        let result = match &mut session.backend {
            Backend::Mutter(proxy) => mutter_apply(proxy, events).await,
            Backend::Wayland { stdin, stdout, .. } => wayland_apply(stdin, stdout, events).await,
        };
        if result.is_err() {
            // Re-open next time (the compositor may have closed the session).
            if let Some(s) = guard.take() {
                close(s).await;
            }
        }
        result
    }

    /// Release everything (phone disconnected).
    pub async fn close(&self) {
        if let Some(s) = self.session.lock().await.take() {
            close(s).await;
        }
    }
}

async fn open() -> Result<Backend, String> {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase();
    if desktop.contains("gnome") {
        return open_mutter().await;
    }
    if let Ok(backend) = open_wayland().await {
        return Ok(backend);
    }
    open_mutter().await.map_err(|_| "remote input needs GNOME or a wlroots/Hyprland desktop".to_string())
}

async fn open_mutter() -> Result<Backend, String> {
    let connection = zbus::Connection::session().await.map_err(|e| e.to_string())?;
    let service = "org.gnome.Mutter.RemoteDesktop";
    let manager = zbus::Proxy::new(&connection, service, "/org/gnome/Mutter/RemoteDesktop", service)
        .await
        .map_err(|e| e.to_string())?;
    let path: zbus::zvariant::OwnedObjectPath = manager.call("CreateSession", &()).await.map_err(|e| e.to_string())?;
    let session = zbus::Proxy::new(&connection, service, path, "org.gnome.Mutter.RemoteDesktop.Session")
        .await
        .map_err(|e| e.to_string())?;
    session.call_method("Start", &()).await.map_err(|e| e.to_string())?;
    Ok(Backend::Mutter(session))
}

async fn open_wayland() -> Result<Backend, String> {
    let helper = std::env::var("ENZREMOTE_INPUT_WL_HELPER").map_err(|_| "no Wayland input helper".to_string())?;
    let mut child = tokio::process::Command::new(helper)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdin = child.stdin.take().ok_or("helper stdin")?;
    let stdout = BufReader::new(child.stdout.take().ok_or("helper stdout")?);
    Ok(Backend::Wayland { child, stdin, stdout })
}

async fn close(session: Session) {
    match session.backend {
        Backend::Mutter(proxy) => {
            let _ = proxy.call_method("Stop", &()).await;
        }
        Backend::Wayland { mut child, .. } => {
            let _ = child.kill().await;
        }
    }
}

async fn mutter_apply(session: &zbus::Proxy<'static>, events: &[InputEvent]) -> Result<(), String> {
    for event in events {
        let result = match event {
            InputEvent::Move { dx, dy } => session.call_method("NotifyPointerMotionRelative", &(*dx, *dy)).await,
            InputEvent::Button { button, pressed } => {
                session.call_method("NotifyPointerButton", &(button.code(), *pressed)).await
            }
            InputEvent::Click { button } => {
                session.call_method("NotifyPointerButton", &(button.code(), true)).await.map_err(|e| e.to_string())?;
                session.call_method("NotifyPointerButton", &(button.code(), false)).await
            }
            InputEvent::Scroll { dx, dy, stop } => {
                let flags = AXIS_FINGER | if *stop { AXIS_FINISH } else { 0 };
                session.call_method("NotifyPointerAxis", &(*dx, *dy, flags)).await
            }
            InputEvent::Key { code, pressed } => session.call_method("NotifyKeyboardKeycode", &(*code, *pressed)).await,
            InputEvent::Text { text } => {
                for c in text.chars() {
                    let keysym = keysym(c);
                    session.call_method("NotifyKeyboardKeysym", &(keysym, true)).await.map_err(|e| e.to_string())?;
                    session.call_method("NotifyKeyboardKeysym", &(keysym, false)).await.map_err(|e| e.to_string())?;
                }
                continue;
            }
        };
        result.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// X keysym for a character (Latin-1 directly, the rest as Unicode keysyms).
fn keysym(c: char) -> u32 {
    match c {
        '\n' => 0xff0d,
        '\t' => 0xff09,
        '\u{8}' => 0xff08,
        c if (0x20..=0x7e).contains(&(c as u32)) || (0xa0..=0xff).contains(&(c as u32)) => c as u32,
        c => 0x0100_0000 + c as u32,
    }
}

async fn wayland_apply(
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    events: &[InputEvent],
) -> Result<(), String> {
    let mut commands = Vec::new();
    for event in events {
        match event {
            InputEvent::Move { dx, dy } => commands.push(format!("pointer_relative {dx} {dy}")),
            InputEvent::Button { button, pressed } => commands.push(format!("button {} {}", button.code(), u8::from(*pressed))),
            InputEvent::Click { button } => {
                commands.push(format!("button {} 1", button.code()));
                commands.push(format!("button {} 0", button.code()));
            }
            InputEvent::Scroll { dx, dy, .. } => commands.push(format!("scroll {} {}", dx / 15.0, dy / 15.0)),
            InputEvent::Key { code, pressed } => commands.push(format!("key {code} {}", u8::from(*pressed))),
            InputEvent::Text { text } => {
                let clean: String = text.chars().filter(|c| !c.is_control() || *c == '\n').collect();
                if !clean.is_empty() {
                    commands.push(format!("text {}", clean.replace('\n', " ")));
                }
            }
        }
    }
    for command in commands {
        stdin.write_all(format!("{command}\n").as_bytes()).await.map_err(|e| e.to_string())?;
        let mut line = String::new();
        stdout.read_line(&mut line).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}
