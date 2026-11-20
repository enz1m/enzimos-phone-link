use serde::Deserialize;
use serde_json::{Value, json};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const MAX_REPLY: usize = 262_144;
const RPC_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone)]
pub struct DesktopTarget {
    pub runroot: PathBuf,
    pub buildroot: PathBuf,
    pub instance: String,
    pub display: String,
    pub pid: u32,
    pub start_ticks: u64,
    socket: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DesktopOutput {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    #[serde(rename = "refreshRate")]
    pub refresh: f64,
    #[serde(default)]
    pub focused: bool,
    pub transform: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DesktopWindow {
    pub window_id: String,
    /// Empty for roots and older endpoints that do not report ancestry.
    #[serde(default)]
    pub parent_window_id: String,
    pub pid: u32,
    pub title: String,
    pub class: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub floating: bool,
}

fn app_window_root(windows: &[DesktopWindow], selected: &DesktopWindow) -> Result<String, String> {
    let mut current = selected;
    for _ in 0..32 {
        if current.parent_window_id.is_empty() {
            return Ok(current.window_id.clone());
        }
        let parent = windows
            .iter()
            .find(|window| window.window_id == current.parent_window_id)
            .ok_or("Desktop app window parent is unavailable")?;
        if parent.pid != selected.pid {
            return Ok(current.window_id.clone());
        }
        current = parent;
    }
    Err("Desktop app window ancestry is cyclic or too deep".into())
}

impl DesktopTarget {
    pub(crate) async fn output(&self, name: Option<&str>) -> Result<DesktopOutput, String> {
        self.check_process().await?;
        let mut command = tokio::process::Command::new(
            enzremote_webrtc_ipc::layout::hyprctl(&self.buildroot),
        );
        command
            .kill_on_drop(true)
            .args(["-i", &self.instance, "-j", "monitors"])
            .env("LD_LIBRARY_PATH", self.buildroot.join("prefix/lib"))
            .stdin(std::process::Stdio::null());
        let response = tokio::time::timeout(RPC_TIMEOUT, command.output())
            .await
            .map_err(|_| "Desktop output query timed out")?
            .map_err(|_| "Desktop output query failed")?;
        if !response.status.success() || response.stdout.len() > 131_072 {
            return Err("Invalid desktop output response".into());
        }
        let outputs: Vec<DesktopOutput> = serde_json::from_slice(&response.stdout)
            .map_err(|_| "Invalid desktop output metadata")?;
        let output = select_desktop_output(outputs, name)?;
        self.check_process().await?;
        Ok(output)
    }

    pub(crate) fn control_socket(&self) -> &Path {
        &self.socket
    }

    pub(crate) async fn find_or_launch_app(&self, app_id: &str) -> Result<String, String> {
        crate::desktop::app_command(app_id)?;
        let mut connection = self.connect().await?;
        if connection.status().await?["active"] == true {
            return Err("Another app is already controlled by remote".into());
        }
        if let Some(id) = self.find_app(app_id).await? {
            return Ok(id);
        }
        let root = self.installation()?;
        let launcher = root.join("scripts/launch-desktop-app");
        let command = format!(
            "'{}' {}",
            launcher.to_string_lossy().replace('\'', "'\\''"),
            app_id
        );
        let expression = format!(
            "return hl.dispatch(hl.dsp.exec_cmd({}))",
            serde_json::to_string(&command).map_err(|error| error.to_string())?
        );
        let result = tokio::time::timeout(
            RPC_TIMEOUT,
            tokio::process::Command::new(
                enzremote_webrtc_ipc::layout::hyprctl(&self.buildroot),
            )
            .kill_on_drop(true)
            .args(["-i", &self.instance, "eval", &expression])
            .env("LD_LIBRARY_PATH", self.buildroot.join("prefix/lib"))
            .stdin(std::process::Stdio::null())
            .output(),
        )
        .await
        .map_err(|_| "Desktop app launch timed out")?
        .map_err(|_| "Desktop app launcher failed")?;
        if !result.status.success() || result.stdout != b"ok\n" {
            return Err("Desktop rejected app launch".into());
        }
        for _ in 0..80 {
            if let Some(id) = self.find_app(app_id).await? {
                return Ok(id);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Err("Application did not open a desktop window".into())
    }

    async fn find_app(&self, app_id: &str) -> Result<Option<String>, String> {
        let windows = self.connect().await?.windows().await?;
        for window in windows.iter().rev() {
            if let Ok(executable) = tokio::fs::read_link(format!("/proc/{}/exe", window.pid)).await
            {
                let name = executable
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default();
                if name.strip_suffix(" (deleted)").unwrap_or(name) == app_id {
                    return app_window_root(&windows, window).map(Some);
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn installation(&self) -> Result<PathBuf, String> {
        let root = std::env::var_os("ENZREMOTE_DESKTOP_ROOT")
            .map(PathBuf::from)
            .or_else(|| Path::new("/usr/share/enzui/enzui-hypr").is_file().then(|| PathBuf::from("/usr/share/enzui")))
            .or_else(|| self.buildroot.parent()?.parent().map(Path::to_path_buf))
            .ok_or("Desktop installation root is unavailable")?;
        if !root.is_absolute() || !root.join("enzui-hypr").is_file() {
            return Err("Invalid desktop installation root".into());
        }
        Ok(root)
    }

    /// Reads one explicitly selected desktop; missing configuration never selects the host implicitly.
    pub async fn from_environment() -> Result<Option<Self>, String> {
        let Some(runroot) = std::env::var_os("ENZREMOTE_DESKTOP_RUNROOT") else {
            return Ok(None);
        };
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .ok_or("XDG_RUNTIME_DIR is required for desktop control")?;
        let buildroot = std::env::var_os("ENZREMOTE_DESKTOP_BUILDROOT")
            .map(PathBuf::from)
            .ok_or("ENZREMOTE_DESKTOP_BUILDROOT is required for desktop control")?;
        Self::read(Path::new(&runroot), &runtime, buildroot)
            .await
            .map(Some)
    }

    pub async fn read(runroot: &Path, runtime: &Path, buildroot: PathBuf) -> Result<Self, String> {
        if !runroot.is_absolute() || !runtime.is_absolute() || !buildroot.is_absolute() {
            return Err("Desktop paths must be absolute".into());
        }
        let instance = small_file(&runroot.join("instance")).await?;
        let display = small_file(&runroot.join("wayland-display")).await?;
        if !endpoint_name(&instance) || !endpoint_name(&display) {
            return Err("Invalid desktop endpoint metadata".into());
        }
        let pid = small_file(&runroot.join("hyprland.pid"))
            .await?
            .parse::<u32>()
            .ok()
            .filter(|pid| *pid > 1)
            .ok_or("Invalid desktop process identity")?;
        let start_ticks = small_file(&runroot.join("hyprland.pid.start"))
            .await?
            .parse::<u64>()
            .map_err(|_| "Invalid desktop start identity")?;
        let socket = runtime.join("hypr").join(&instance).join(".enzremote.sock");
        Ok(Self {
            runroot: runroot.into(),
            buildroot,
            instance,
            display,
            pid,
            start_ticks,
            socket,
        })
    }

    /// Holds a private, peer-PID-checked control connection; dropping it revokes its compositor ownership.
    pub async fn connect(&self) -> Result<DesktopConnection, String> {
        tokio::time::timeout(RPC_TIMEOUT, async {
            self.check_process().await?;
            let stream = UnixStream::connect(&self.socket)
                .await
                .map_err(|error| format!("Shared desktop is unavailable: {error}"))?;
            let peer = stream.peer_cred().map_err(|error| error.to_string())?;
            let uid = tokio::fs::metadata("/proc/self")
                .await
                .map_err(|error| error.to_string())?
                .uid();
            if peer.uid() != uid
                || peer.pid().and_then(|pid| u32::try_from(pid).ok()) != Some(self.pid)
            {
                return Err("Desktop control peer does not match its process identity".into());
            }
            self.check_process().await?;
            Ok(DesktopConnection {
                stream: Some(BufReader::new(stream)),
            })
        })
        .await
        .map_err(|_| "Desktop connection timed out".to_owned())?
    }

    async fn check_process(&self) -> Result<(), String> {
        let stat = tokio::fs::read_to_string(format!("/proc/{}/stat", self.pid))
            .await
            .map_err(|_| "Desktop process is no longer running")?;
        if process_start(&stat) != Some(self.start_ticks) {
            return Err("Desktop process identity changed".into());
        }
        Ok(())
    }
}

pub struct DesktopConnection {
    stream: Option<BufReader<UnixStream>>,
}

impl DesktopConnection {
    pub async fn windows(&mut self) -> Result<Vec<DesktopWindow>, String> {
        let response = self.request(json!({"op":"list"})).await?;
        serde_json::from_value(
            response
                .get("windows")
                .cloned()
                .ok_or("Missing desktop window list")?,
        )
        .map_err(|error| format!("Invalid desktop window list: {error}"))
    }

    pub async fn status(&mut self) -> Result<Value, String> {
        self.request(json!({"op":"status"})).await
    }

    pub async fn capabilities(&mut self) -> Result<Value, String> {
        self.request(json!({"op":"ping"})).await
    }

    pub(crate) async fn request(&mut self, mut request: Value) -> Result<Value, String> {
        let object = request
            .as_object_mut()
            .ok_or("Control request must be an object")?;
        object.insert("v".into(), json!(1));
        let mut encoded = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        if encoded.len() > 65_535 {
            return Err("Desktop control request exceeds its bound".into());
        }
        encoded.push(b'\n');
        let mut stream = self
            .stream
            .take()
            .ok_or("Desktop control connection is closed")?;
        let result = tokio::time::timeout(RPC_TIMEOUT, async {
            stream
                .get_mut()
                .write_all(&encoded)
                .await
                .map_err(|error| error.to_string())?;
            let line = bounded_line(&mut stream).await?;
            let response: Value = serde_json::from_slice(&line)
                .map_err(|error| format!("Invalid desktop reply: {error}"))?;
            if response.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err(response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Desktop request was rejected")
                    .chars()
                    .take(240)
                    .collect());
            }
            Ok(response)
        })
        .await
        .unwrap_or_else(|_| Err("Desktop control request timed out".to_owned()));
        if result.is_ok() {
            self.stream = Some(stream);
        }
        result
    }
}

async fn bounded_line(stream: &mut BufReader<UnixStream>) -> Result<Vec<u8>, String> {
    let mut line = Vec::new();
    loop {
        let available = stream.fill_buf().await.map_err(|error| error.to_string())?;
        if available.is_empty() {
            return Err("Desktop closed its control connection".into());
        }
        let end = available.iter().position(|byte| *byte == b'\n');
        let count = end.map_or(available.len(), |offset| offset + 1);
        if line.len() + count > MAX_REPLY {
            return Err("Desktop control reply exceeds its bound".into());
        }
        line.extend_from_slice(&available[..count]);
        stream.consume(count);
        if end.is_some() {
            return Ok(line);
        }
    }
}

async fn small_file(path: &Path) -> Result<String, String> {
    let metadata = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|error| format!("Desktop metadata unavailable: {error}"))?;
    if !metadata.is_file() || metadata.len() > 4096 {
        return Err("Invalid desktop metadata file".into());
    }
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| error.to_string())?;
    if text.len() > 4096 {
        return Err("Desktop metadata exceeds its bound".into());
    }
    Ok(text.trim().into())
}

fn select_desktop_output(
    outputs: Vec<DesktopOutput>,
    name: Option<&str>,
) -> Result<DesktopOutput, String> {
    let mut outputs: Vec<_> = outputs
        .into_iter()
        .filter(|output| {
            !output.name.starts_with("ENZREMOTE-") && output.width >= 240 && output.height >= 240
        })
        .collect();
    outputs.sort_by_key(|output| !output.focused);
    let output = outputs
        .into_iter()
        .find(|output| name.is_none_or(|name| name == output.name))
        .ok_or("Desktop output is unavailable")?;
    if !endpoint_name(&output.name)
        || output.width > 8192
        || output.height > 8192
        || output.transform != 0
        || !output.scale.is_finite()
        || !(0.5..=4.).contains(&output.scale)
        || !output.refresh.is_finite()
    {
        return Err("Desktop output geometry is unsupported".into());
    }
    Ok(output)
}

fn endpoint_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 180
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        && value != "."
        && value != ".."
}

fn process_start(stat: &str) -> Option<u64> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(id: &str, pid: u32, parent: Option<&str>) -> DesktopWindow {
        let mut value = serde_json::json!({"window_id":id,"pid":pid,"title":"fixture","class":"fixture",
            "x":0,"y":0,"width":400,"height":300,"floating":true});
        if let Some(parent) = parent {
            value["parent_window_id"] = parent.into();
        }
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn app_selection_follows_same_process_transients_to_the_parent() {
        let windows = vec![
            window("1", 10, None),
            window("2", 10, Some("1")),
            window("3", 10, Some("2")),
        ];
        assert_eq!(app_window_root(&windows, &windows[2]).unwrap(), "1");
        assert_eq!(app_window_root(&windows, &windows[0]).unwrap(), "1");
        assert!(windows[0].parent_window_id.is_empty());
    }

    #[test]
    fn app_selection_does_not_cross_processes_or_follow_invalid_ancestry() {
        let windows = vec![
            window("1", 10, None),
            window("2", 20, Some("1")),
            window("3", 10, Some("missing")),
            window("4", 10, Some("5")),
            window("5", 10, Some("4")),
        ];
        assert_eq!(app_window_root(&windows, &windows[1]).unwrap(), "2");
        assert!(app_window_root(&windows, &windows[2]).is_err());
        assert!(app_window_root(&windows, &windows[3]).is_err());
    }

    fn output(name: &str, focused: bool) -> DesktopOutput {
        DesktopOutput {
            name: name.into(),
            width: 1440,
            height: 900,
            scale: 1.,
            refresh: 60.,
            focused,
            transform: 0,
        }
    }

    #[test]
    fn output_selection_excludes_loan_canvases_and_honors_explicit_selection() {
        let outputs = vec![
            output("ENZREMOTE-APP", true),
            output("DP-1", false),
            output("HDMI-A-1", true),
        ];
        assert_eq!(
            select_desktop_output(outputs.clone(), None).unwrap().name,
            "HDMI-A-1"
        );
        assert_eq!(
            select_desktop_output(outputs.clone(), Some("DP-1"))
                .unwrap()
                .name,
            "DP-1"
        );
        assert!(select_desktop_output(outputs.clone(), Some("ENZREMOTE-APP")).is_err());
        assert!(select_desktop_output(outputs, Some("missing")).is_err());
    }

    #[test]
    fn output_geometry_rejects_unmapped_transforms_and_invalid_density() {
        for bad in [
            DesktopOutput {
                transform: 1,
                ..output("DP-1", true)
            },
            DesktopOutput {
                scale: f64::NAN,
                ..output("DP-1", true)
            },
            DesktopOutput {
                scale: 8.,
                ..output("DP-1", true)
            },
            DesktopOutput {
                width: 16384,
                ..output("DP-1", true)
            },
            DesktopOutput {
                height: 0,
                ..output("DP-1", true)
            },
            output("../other", true),
        ] {
            assert!(select_desktop_output(vec![bad], None).is_err());
        }
    }

    #[test]
    fn endpoint_and_process_identity_are_not_path_or_pid_only_checks() {
        assert!(endpoint_name("hash_1234_5678"));
        for invalid in ["", "..", "../another", "/absolute", "one two"] {
            assert!(!endpoint_name(invalid));
        }
        let suffix = (0..20).map(|n| n.to_string()).collect::<Vec<_>>().join(" ");
        assert_eq!(
            process_start(&format!("123 (name with ) spaces) {suffix}")),
            Some(19)
        );
        assert_eq!(process_start("bad stat"), None);
    }

    #[tokio::test]
    async fn cancelling_an_inflight_request_closes_its_ownership_socket() {
        let (local, remote) = UnixStream::pair().unwrap();
        let mut connection = DesktopConnection {
            stream: Some(BufReader::new(local)),
        };
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut reader = BufReader::new(remote);
            bounded_line(&mut reader).await.unwrap();
            sent.send(()).unwrap();
            let mut remainder = String::new();
            assert_eq!(reader.read_line(&mut remainder).await.unwrap(), 0);
        });
        {
            let request = connection.status();
            tokio::pin!(request);
            tokio::select! { _=&mut request => panic!("server must not reply"), _=received => {} }
        }
        assert!(connection.stream.is_none());
        assert!(connection.status().await.is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_or_unterminated_replies_fail_closed() {
        let (local, mut remote) = UnixStream::pair().unwrap();
        let sender = tokio::spawn(async move {
            let _ = remote.write_all(&vec![b'x'; MAX_REPLY + 1]).await;
        });
        let mut reader = BufReader::new(local);
        assert!(
            bounded_line(&mut reader)
                .await
                .unwrap_err()
                .contains("bound")
        );
        drop(reader);
        sender.await.unwrap();
        let (local, remote) = UnixStream::pair().unwrap();
        drop(remote);
        assert!(
            bounded_line(&mut BufReader::new(local))
                .await
                .unwrap_err()
                .contains("closed")
        );
    }
}
