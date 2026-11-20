//! Phone or tablet as an extra monitor for the computer (like Sidecar).
//!
//! GNOME: a Mutter RemoteDesktop session with a ScreenCast `RecordVirtual`
//! stream creates a virtual monitor; its size follows the PipeWire format the
//! stream negotiates (webrtc-server asks for `ENZREMOTE_PIPEWIRE_SIZE`). Touch
//! and pointer input from the phone is injected on that monitor's stream.
//!
//! Desktop view: the same session records an existing monitor
//! (`RecordMonitor`) instead, scaled down to the phone's size by the encoder
//! pipeline; input coordinates are scaled back to monitor pixels.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use zbus::zvariant::{OwnedObjectPath, Value};

const RD: &str = "org.gnome.Mutter.RemoteDesktop";
const SC: &str = "org.gnome.Mutter.ScreenCast";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DisplayEvent {
    /// Finger contact (slot = pointer id), in display pixels.
    Down { slot: u32, x: f64, y: f64 },
    Move { slot: u32, x: f64, y: f64 },
    Up { slot: u32 },
    /// Mouse or trackpad pointer in display pixels.
    Pointer { x: f64, y: f64 },
    Button { button: u32, pressed: bool },
    Scroll { dx: f64, dy: f64 },
}

pub struct VirtualDisplay {
    session: zbus::Proxy<'static>,
    stream: OwnedObjectPath,
    pub node: u32,
    /// PipeWire `object.serial` (what `pipewiresrc target-object` takes).
    pub serial: u64,
    /// Encoded (session) size; phone input arrives in these pixels.
    pub width: u32,
    pub height: u32,
    /// Monitor pixels per session pixel (1.0 for a virtual monitor).
    input_scale: f64,
    /// The captured size before scaling (the monitor's mode).
    pub source_width: u32,
    pub source_height: u32,
}

/// A monitor of this computer that can be shown on the phone.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Monitor {
    /// Connector, e.g. `DP-1` (`second_display_start.output`).
    pub connector: String,
    /// Product name for display ("Dell U2723QE").
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

type ModeProps = std::collections::HashMap<String, zbus::zvariant::OwnedValue>;
type MonitorSpec = (String, String, String, String);
type DisplayState = (
    u32,
    Vec<(MonitorSpec, Vec<(String, i32, i32, f64, f64, Vec<f64>, ModeProps)>, ModeProps)>,
    Vec<(i32, i32, f64, u32, bool, Vec<MonitorSpec>, ModeProps)>,
    ModeProps,
);

/// Active monitors from Mutter's DisplayConfig, primary first.
pub async fn monitors() -> Result<Vec<Monitor>, String> {
    let err = |e: zbus::Error| e.to_string();
    let connection = zbus::Connection::session().await.map_err(err)?;
    let config = zbus::Proxy::new(
        &connection,
        "org.gnome.Mutter.DisplayConfig",
        "/org/gnome/Mutter/DisplayConfig",
        "org.gnome.Mutter.DisplayConfig",
    )
    .await
    .map_err(err)?;
    let (_, physical, logical, _): DisplayState = config.call("GetCurrentState", &()).await.map_err(err)?;
    let mut list = Vec::new();
    for (_, _, _, transform, primary, specs, _) in &logical {
        // Mutter records (and maps input) in the rotated orientation.
        let sideways = matches!(transform, 1 | 3 | 5 | 7);
        for spec in specs {
            let Some((_, modes, props)) = physical.iter().find(|(s, _, _)| s.0 == spec.0) else { continue };
            let Some((_, w, h, ..)) = modes.iter().find(|m| {
                m.6.get("is-current").and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
            }) else {
                continue;
            };
            let name = props
                .get("display-name")
                .and_then(|v| String::try_from(v.clone()).ok())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| format!("{} {}", spec.1, spec.2).trim().to_string());
            list.push(Monitor {
                connector: spec.0.clone(),
                name,
                width: if sideways { *h as u32 } else { *w as u32 },
                height: if sideways { *w as u32 } else { *h as u32 },
                primary: *primary,
            });
        }
    }
    list.sort_by_key(|m| !m.primary);
    Ok(list)
}

static MONITORS: std::sync::Mutex<Option<(std::time::Instant, Vec<Monitor>)>> = std::sync::Mutex::new(None);

/// Last known monitors for `computer_info` (refreshed in the background at
/// most every 5 s, so the synchronous info poll never waits for D-Bus).
pub fn monitors_cached() -> Vec<Monitor> {
    if !VirtualDisplay::supported() {
        return Vec::new();
    }
    let mut cache = MONITORS.lock().unwrap_or_else(|e| e.into_inner());
    let stale = cache.as_ref().is_none_or(|(at, _)| at.elapsed() > std::time::Duration::from_secs(5));
    let current = cache.as_ref().map(|(_, list)| list.clone()).unwrap_or_default();
    if stale && let Ok(runtime) = tokio::runtime::Handle::try_current() {
        // Mark as fresh now so one refresh runs at a time.
        *cache = Some((std::time::Instant::now(), current.clone()));
        runtime.spawn(async {
            if let Ok(list) = monitors().await {
                *MONITORS.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), list));
            }
        });
    }
    current
}

/// The size to encode `source` at: fitted inside `bound` (the phone's
/// request), never upscaled, even pixels.
pub fn fitted_size(source: (u32, u32), bound: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = (f64::from(source.0.max(1)), f64::from(source.1.max(1)));
    // The phone may send either orientation; compare like with like.
    let (bw, bh) = if (source.0 >= source.1) == (bound.0 >= bound.1) { bound } else { (bound.1, bound.0) };
    let scale = (f64::from(bw) / sw).min(f64::from(bh) / sh).min(1.0);
    let even = |v: f64| ((v.round() as u32) & !1).max(16);
    (even(sw * scale), even(sh * scale))
}

impl VirtualDisplay {
    /// Whether this desktop can create virtual monitors.
    pub fn supported() -> bool {
        std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase().contains("gnome")
    }

    pub async fn create(width: u32, height: u32) -> Result<Self, String> {
        Self::start(width, height, None).await
    }

    /// Show an existing monitor (`connector`, else the primary) at most
    /// `bound` pixels.
    pub async fn view_monitor(connector: Option<&str>, bound: (u32, u32)) -> Result<Self, String> {
        let list = monitors().await?;
        let monitor = match connector {
            Some(name) => list.iter().find(|m| m.connector == name),
            None => list.first(),
        }
        .ok_or("that monitor is not connected")?;
        Self::start(monitor.width, monitor.height, Some((monitor.connector.clone(), bound))).await
    }

    async fn start(width: u32, height: u32, monitor: Option<(String, (u32, u32))>) -> Result<Self, String> {
        if !Self::supported() {
            return Err("a second display needs GNOME on this computer for now".into());
        }
        let err = |e: zbus::Error| e.to_string();
        let connection = zbus::Connection::session().await.map_err(err)?;
        let remote = zbus::Proxy::new(&connection, RD, "/org/gnome/Mutter/RemoteDesktop", RD).await.map_err(err)?;
        let session_path: OwnedObjectPath = remote.call("CreateSession", &()).await.map_err(err)?;
        let session = zbus::Proxy::new(&connection, RD, session_path, format!("{RD}.Session")).await.map_err(err)?;
        let session_id: String = session.get_property("SessionId").await.map_err(err)?;
        let screencast = zbus::Proxy::new(&connection, SC, "/org/gnome/Mutter/ScreenCast", SC).await.map_err(err)?;
        let mut options = std::collections::HashMap::<&str, Value<'_>>::new();
        options.insert("remote-desktop-session-id", Value::from(session_id.as_str()));
        let cast_path: OwnedObjectPath = screencast.call("CreateSession", &(options,)).await.map_err(err)?;
        let cast = zbus::Proxy::new(&connection, SC, cast_path, format!("{SC}.Session")).await.map_err(err)?;
        let mut record = std::collections::HashMap::<&str, Value<'_>>::new();
        // Embedded cursor: the phone sees where a connected mouse points.
        record.insert("cursor-mode", Value::from(1u32));
        let stream_path: OwnedObjectPath = match &monitor {
            Some((connector, _)) => cast.call("RecordMonitor", &(connector.as_str(), record)).await.map_err(err)?,
            None => cast.call("RecordVirtual", &(record,)).await.map_err(err)?,
        };
        let stream = zbus::Proxy::new(&connection, SC, stream_path.clone(), format!("{SC}.Stream")).await.map_err(err)?;
        let mut added = stream.receive_signal("PipeWireStreamAdded").await.map_err(err)?;
        session.call_method("Start", &()).await.map_err(err)?;
        let node = tokio::time::timeout(std::time::Duration::from_secs(5), added.next())
            .await
            .map_err(|_| "the computer did not create the display".to_string())?
            .ok_or("the computer did not create the display")?
            .body()
            .deserialize::<u32>()
            .map_err(|e| e.to_string())?;
        let serial = match node_serial(node).await {
            Some(serial) => serial,
            None => {
                let _ = session.call_method("Stop", &()).await;
                return Err("the display's video stream is not available".into());
            }
        };
        let (out_w, out_h) = match &monitor {
            Some((_, bound)) => fitted_size((width, height), *bound),
            None => (width, height),
        };
        Ok(Self {
            session,
            stream: stream_path,
            node,
            serial,
            width: out_w,
            height: out_h,
            input_scale: f64::from(width) / f64::from(out_w),
            source_width: width,
            source_height: height,
        })
    }

    pub async fn input(&self, events: &[DisplayEvent]) -> Result<(), String> {
        let stream = self.stream.as_str();
        let (w, h) = (f64::from(self.source_width - 1), f64::from(self.source_height - 1));
        let k = self.input_scale;
        let clamp = |x: f64, y: f64| ((x * k).clamp(0.0, w), (y * k).clamp(0.0, h));
        for event in events {
            let result = match event {
                DisplayEvent::Down { slot, x, y } => {
                    let (x, y) = clamp(*x, *y);
                    self.session.call_method("NotifyTouchDown", &(stream, *slot, x, y)).await
                }
                DisplayEvent::Move { slot, x, y } => {
                    let (x, y) = clamp(*x, *y);
                    self.session.call_method("NotifyTouchMotion", &(stream, *slot, x, y)).await
                }
                DisplayEvent::Up { slot } => self.session.call_method("NotifyTouchUp", &(*slot,)).await,
                DisplayEvent::Pointer { x, y } => {
                    let (x, y) = clamp(*x, *y);
                    self.session.call_method("NotifyPointerMotionAbsolute", &(stream, x, y)).await
                }
                DisplayEvent::Button { button, pressed } => {
                    self.session.call_method("NotifyPointerButton", &(*button as i32, *pressed)).await
                }
                DisplayEvent::Scroll { dx, dy } => self.session.call_method("NotifyPointerAxis", &(*dx, *dy, 1u32 << 2)).await,
            };
            result.map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Remove the virtual monitor.
    pub async fn stop(&self) {
        let _ = self.session.call_method("Stop", &()).await;
    }
}

/// `object.serial` of a PipeWire node id (`pw-dump`), retried briefly while
/// the node appears.
async fn node_serial(node: u32) -> Option<u64> {
    for _ in 0..10 {
        if let Ok(output) = tokio::process::Command::new("pw-dump").arg(node.to_string()).output().await
            && let Ok(objects) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)
            && let Some(serial) = objects.iter().find(|o| o["id"].as_u64() == Some(u64::from(node)))
                .and_then(|o| o["info"]["props"]["object.serial"].as_u64())
        {
            return Some(serial);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}
