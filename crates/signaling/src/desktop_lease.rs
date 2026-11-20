use crate::shared_desktop::{DesktopConnection, DesktopTarget};
use enzremote_session::Viewport;
use serde_json::{Value, json};
use std::process::Stdio;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::{Child, Command};

pub(crate) enum DesktopSession {
    App(Box<DesktopAppSession>),
    Mirror(crate::desktop_mirror::DesktopMirrorSession),
}

impl DesktopSession {
    pub fn claim_watch(&mut self) -> Option<String> {
        if let Self::Mirror(desktop) = self {
            return desktop.claim_watch();
        }
        let Self::App(app) = self else { return None };
        let lease = app.active.as_mut()?;
        if lease.watched {
            return None;
        }
        lease.watched = true;
        Some(lease.id.clone())
    }

    pub fn watch_id(&self) -> Option<String> {
        match self {
            Self::App(app) => app.lease_id().map(str::to_owned),
            Self::Mirror(desktop) => desktop.watch_id(),
        }
    }
    pub fn watch_interval(&self) -> Duration {
        Duration::from_millis(if matches!(self, Self::Mirror(_)) {
            1000
        } else {
            250
        })
    }
    pub fn viewport(&self) -> Viewport {
        match self {
            Self::App(app) => app.viewport,
            Self::Mirror(desktop) => desktop.viewport,
        }
    }
    pub fn lease_id(&self) -> Option<&str> {
        match self {
            Self::App(app) => app.lease_id(),
            Self::Mirror(_) => None,
        }
    }
    pub async fn acquire(&mut self) -> Result<(), String> {
        match self {
            Self::App(app) => app.acquire().await,
            Self::Mirror(desktop) => desktop.acquire().await,
        }
    }
    pub async fn release(&mut self) -> Result<(), String> {
        match self {
            Self::App(app) => app.release().await,
            Self::Mirror(desktop) => desktop.release().await,
        }
    }
    pub async fn is_active(&mut self) -> bool {
        match self {
            Self::App(app) => app.is_active().await,
            Self::Mirror(desktop) => desktop.is_active().await,
        }
    }
    pub async fn inspection(&mut self, id: &str) -> Result<enzremote_session::Inspection, String> {
        match self {
            Self::App(app) => app.inspection(id).await,
            Self::Mirror(desktop) => desktop.inspection(id).await,
        }
    }
    pub async fn resize(&mut self, viewport: Viewport) -> Result<(), String> {
        match self {
            Self::App(app) => app.resize(viewport).await,
            Self::Mirror(_) => {
                Err("Desktop resolution is controlled locally; fit the video instead".into())
            }
        }
    }
    pub fn worker_environment(&self) -> Result<Vec<(&'static str, String)>, String> {
        match self {
            Self::App(app) => app.worker_environment(),
            Self::Mirror(desktop) => desktop.worker_environment(),
        }
    }
}

pub(crate) struct DesktopAppSession {
    pub target: DesktopTarget,
    pub window_id: String,
    pub viewport: Viewport,
    active: Option<ActiveLease>,
}

struct ActiveLease {
    control: DesktopConnection,
    placeholder: Child,
    input_token: String,
    output: String,
    id: String,
    watched: bool,
}

impl DesktopAppSession {
    pub fn new(target: DesktopTarget, window_id: String, viewport: Viewport) -> Self {
        Self {
            target,
            window_id,
            viewport,
            active: None,
        }
    }

    pub async fn acquire(&mut self) -> Result<(), String> {
        if self.active.is_some() {
            return Err("Desktop app is already leased".into());
        }
        self.target.installation()?;
        let capture_tool = std::env::var_os("ENZREMOTE_WINDOW_SNAPSHOT_HELPER").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/usr/lib/enzui/bin/window-snapshot"));
        let placeholder_tool = std::env::var_os("ENZREMOTE_PLACEHOLDER_HELPER").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/usr/lib/enzui/bin/enzui-remote-placeholder"));
        if !capture_tool.is_file() || !placeholder_tool.is_file() {
            return Err("Desktop handoff helpers have not been built".into());
        }
        let mut control = self.target.connect().await?;
        let reply = control.request(json!({"op":"prepare","window_id":self.window_id,"viewport":viewport_json(self.viewport)})).await?;
        let snapshot = required_string(&reply, "snapshot_path")?;
        let handle = required_string(&reply, "capture_handle")?;
        let ui_token = required_string(&reply, "ui_token")?;
        let input_token = required_string(&reply, "input_token")?;
        let output = required_string(&reply, "output")?;
        let id = required_string(&reply, "lease_id")?;
        let mut capture = Command::new(capture_tool);
        capture
            .args([handle.as_str(), snapshot.as_str()])
            .kill_on_drop(true)
            .env("WAYLAND_DISPLAY", &self.target.display)
            .env("LD_LIBRARY_PATH", self.target.buildroot.join("prefix/lib"))
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let captured = tokio::time::timeout(Duration::from_secs(4), capture.status())
            .await
            .map_err(|_| "Desktop snapshot timed out")?
            .map_err(|_| "Desktop snapshot helper could not start")?;
        if !captured.success() {
            return Err("Desktop snapshot could not be captured".into());
        }
        let settings = self.target.runroot.join("remote-placeholder");
        let mut command = Command::new(placeholder_tool);
        command
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env("WAYLAND_DISPLAY", &self.target.display)
            .env("LD_LIBRARY_PATH", self.target.buildroot.join("prefix/lib"))
            .env("WINIT_UNIX_BACKEND", "wayland")
            .env("WGPU_BACKEND", "vulkan")
            .env_remove("DISPLAY")
            .env("ENZUI_REMOTE_SOCKET", self.target.control_socket())
            .env("ENZUI_REMOTE_TOKEN", ui_token)
            .env("ENZUI_REMOTE_COMPOSITOR_PID", self.target.pid.to_string());
        for name in ["CONFIG", "DATA", "CACHE", "STATE"] {
            let path = settings.join(name.to_lowercase());
            tokio::fs::create_dir_all(&path)
                .await
                .map_err(|_| "Cannot prepare placeholder settings")?;
            command.env(format!("XDG_{name}_HOME"), path);
        }
        let mut placeholder = command
            .spawn()
            .map_err(|_| "Desktop placeholder could not start")?;
        wait_for(&mut control, |value| value["ui_ready"] == true).await?;
        control.request(json!({"op":"activate"})).await?;
        wait_for(&mut control, |value| {
            value["phase"] == "active" && value["input_ready"] == true
        })
        .await?;
        if placeholder
            .try_wait()
            .map_err(|_| "Cannot inspect placeholder")?
            .is_some()
        {
            return Err("Desktop placeholder exited".into());
        }
        self.active = Some(ActiveLease {
            control,
            placeholder,
            input_token,
            output,
            id,
            watched: false,
        });
        Ok(())
    }

    pub fn lease_id(&self) -> Option<&str> {
        self.active.as_ref().map(|lease| lease.id.as_str())
    }

    pub async fn release(&mut self) -> Result<(), String> {
        let Some(mut lease) = self.active.take() else {
            return Ok(());
        };
        let result = async {
            lease.control.request(json!({"op":"restore"})).await?;
            wait_for(&mut lease.control, |value| {
                matches!(value["phase"].as_str(), Some("restored" | "closed"))
            })
            .await?;
            Ok(())
        }
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(1), lease.placeholder.wait()).await;
        result
    }

    pub async fn resize(&mut self, viewport: Viewport) -> Result<(), String> {
        let Some(lease) = self.active.as_mut() else {
            self.viewport = viewport;
            return Ok(());
        };
        lease
            .control
            .request(json!({"op":"resize","viewport":viewport_json(viewport)}))
            .await?;
        wait_for(&mut lease.control, |value| {
            value["phase"] == "active" && value["input_ready"] == true
        })
        .await?;
        self.viewport = viewport;
        Ok(())
    }

    pub async fn alive(&self) -> Result<bool, String> {
        Ok(self
            .target
            .connect()
            .await?
            .windows()
            .await?
            .iter()
            .any(|window| window.window_id == self.window_id))
    }

    pub async fn is_active(&mut self) -> bool {
        match self.active.as_mut() {
            Some(lease) => lease
                .control
                .status()
                .await
                .is_ok_and(|value| value["phase"] == "active"),
            None => false,
        }
    }

    pub async fn inspection(
        &mut self,
        session_id: &str,
    ) -> Result<enzremote_session::Inspection, String> {
        use enzremote_session::{
            CaptureEndpoint, Inspection, LifecycleStatus, MediaPreferences, Permissions,
            SCHEMA_VERSION, SessionManifest, SessionSource, SessionState,
        };
        let alive = self.alive().await?;
        let active = self.is_active().await;
        Ok(Inspection {
            state: SessionState {
                manifest: SessionManifest {
                    schema_version: SCHEMA_VERSION,
                    id: session_id.into(),
                    source: SessionSource::DesktopApp {
                        window_id: self.window_id.clone(),
                    },
                    viewport: self.viewport,
                    permissions: Permissions {
                        input: true,
                        ..Default::default()
                    },
                    media: MediaPreferences::default(),
                },
                status: if active {
                    LifecycleStatus::Active
                } else {
                    LifecycleStatus::Stopped
                },
                created_at_unix_ms: 0,
                helper: Default::default(),
                runroot: self.target.runroot.clone(),
                buildroot: self.target.buildroot.clone(),
                config: Default::default(),
            },
            compositor_alive: true,
            app_alive: alive,
            stream_alive: false,
            input: None,
            capture: if active {
                self.active.as_ref().map(|lease| CaptureEndpoint {
                    output: lease.output.clone(),
                    wayland_display: self.target.display.clone(),
                    hyprland_instance: self.target.instance.clone(),
                })
            } else {
                None
            },
        })
    }

    pub fn worker_environment(&self) -> Result<Vec<(&'static str, String)>, String> {
        let lease = self.active.as_ref().ok_or("Desktop lease is not active")?;
        let helper = enzremote_webrtc_ipc::layout::helper("ENZREMOTE_DESKTOP_INPUT_HELPER", "desktop-input");
        if !helper.is_absolute() || !helper.is_file() { return Err("Desktop input helper is unavailable".into()); }
        Ok(vec![
            (
                "ENZREMOTE_DESKTOP_INPUT_SOCKET",
                self.target.control_socket().to_string_lossy().into(),
            ),
            ("ENZREMOTE_DESKTOP_INPUT_TOKEN", lease.input_token.clone()),
            ("ENZREMOTE_DESKTOP_PID", self.target.pid.to_string()),
            ("ENZREMOTE_INPUT_WL_HELPER", helper.to_string_lossy().into()),
            ("ENZREMOTE_INPUT_WORKER_UNICODE", "1".into()),
            (
                "ENZREMOTE_COMPOSITOR_BUILD_DIR",
                self.target.buildroot.to_string_lossy().into(),
            ),
        ])
    }
}

fn viewport_json(viewport: Viewport) -> Value {
    json!({"width":viewport.width,"height":viewport.height,"scale":viewport.scale,"refresh":viewport.refresh_hz})
}

fn required_string(value: &Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 4096)
        .map(str::to_owned)
        .ok_or_else(|| format!("Desktop omitted {key}"))
}

async fn wait_for(
    control: &mut DesktopConnection,
    ready: impl Fn(&Value) -> bool,
) -> Result<Value, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let status = control.status().await?;
        if ready(&status) {
            return Ok(status);
        }
        if matches!(
            status["phase"].as_str(),
            Some("restored" | "closed" | "failed")
        ) {
            return Err("Desktop handoff was cancelled".into());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Desktop handoff did not settle".into());
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}
