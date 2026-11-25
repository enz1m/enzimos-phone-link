#![forbid(unsafe_code)]
//! enzremote session management library.
//!
//! Provides session lifecycle operations for the enzremote streaming system:
//! - Session creation, resumption, and teardown
//! - Compositor helper process management (Hyprland)
//! - Stream start/stop with capture and input bridges
//! - Session manifest validation and persistence
//! - Capability probing (encoders, displays, input methods)
//! - EIS input broker for secure input injection

pub mod eis_broker;

pub mod audio;
use enzremote_webrtc_ipc::compositor::Compositor;
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use serde::{Deserialize, Serialize};
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const COMPOSITOR_TIMEOUT: Duration = Duration::from_secs(5);

/// Current session manifest schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// A session manifest describing how to set up a remote session.
///
/// Desktop sources require an external owner; [`create_session`] only launches
/// private app sources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionManifest {
    pub schema_version: u32,
    pub id: String,
    pub source: SessionSource,
    pub viewport: Viewport,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub media: MediaPreferences,
}

/// The source of content for a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionSource {
    LaunchedApp { command: Vec<String> },
    DesktopMirror { output: Option<String> },
    DesktopApp { window_id: String },
    RemoteWorkspace,
}

/// Display viewport configuration (resolution, scale, refresh rate).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_scale")]
    pub scale: f32,
    #[serde(default = "default_refresh_hz")]
    pub refresh_hz: u32,
}

/// Session-level permissions controlling what the remote client can do.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Permissions {
    pub input: bool,
    pub clipboard: bool,
    pub audio_playback: bool,
    pub microphone: bool,
}

/// Media encoding preferences for the session's video/audio streams.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaPreferences {
    pub video_codecs: Vec<String>,
    pub audio_codec: String,
    pub max_bitrate_kbps: u32,
}

impl Default for MediaPreferences {
    fn default() -> Self {
        Self {
            video_codecs: vec!["h264".into(), "av1".into(), "vp8".into()],
            audio_codec: "opus".into(),
            max_bitrate_kbps: 12_000,
        }
    }
}

/// Session lifecycle status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleStatus {
    Starting,
    Active,
    Stopped,
}

/// Runtime state of a session (persisted to disk as JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionState {
    pub manifest: SessionManifest,
    pub status: LifecycleStatus,
    pub created_at_unix_ms: u64,
    pub helper: PathBuf,
    pub runroot: PathBuf,
    pub buildroot: PathBuf,
    pub config: PathBuf,
}

/// Result of inspecting a session's current state and liveness.
#[derive(Debug, Clone, Serialize)]
pub struct Inspection {
    pub state: SessionState,
    pub compositor_alive: bool,
    pub app_alive: bool,
    pub stream_alive: bool,
    pub capture: Option<CaptureEndpoint>,
    pub input: Option<InputEndpoint>,
}

/// Information about a session's screen capture endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct CaptureEndpoint {
    pub output: String,
    pub wayland_display: String,
    pub hyprland_instance: String,
}

/// Information about a session's EIS input endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct InputEndpoint {
    pub socket: PathBuf,
    pub mapping_id: String,
}

/// Result of sending a pointer event to a session.
#[derive(Debug, Clone, Serialize)]
pub struct PointerResult {
    pub session_id: String,
    pub width: u32,
    pub height: u32,
    pub x: u32,
    pub y: u32,
    pub click: bool,
}

/// Host media and graphics capability probe result.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilityProbe {
    pub schema_version: u32,
    pub pipewire_running: bool,
    pub render_nodes: Vec<String>,
    pub gstreamer: GStreamerProbe,
}

/// GStreamer element availability probe.
#[derive(Debug, Clone, Serialize)]
pub struct GStreamerProbe {
    pub inspect_available: bool,
    pub required: Vec<ElementProbe>,
    pub h264_encoders: Vec<ElementProbe>,
    pub av1_encoders: Vec<ElementProbe>,
}

/// A single GStreamer element's availability and hardware status.
#[derive(Debug, Clone, Serialize)]
pub struct ElementProbe {
    pub name: String,
    pub available: bool,
    pub hardware_candidate: bool,
}

/// Default scale factor for session viewports.
pub fn default_scale() -> f32 {
    1.0
}

/// Default refresh rate (Hz) for session viewports.
pub fn default_refresh_hz() -> u32 {
    60
}

impl SessionManifest {
    /// Validate only the viewport dimensions, scale, and refresh rate.
    ///
    /// This is cheaper than full `validate()` because it doesn't clone the
    /// entire manifest. Used during resize operations.
    pub fn validate_viewport(viewport: &Viewport) -> Result<(), String> {
        if !(240..=8192).contains(&viewport.width) || !(240..=8192).contains(&viewport.height) {
            return Err("viewport dimensions must each be between 240 and 8192 pixels".into());
        }
        if !viewport.scale.is_finite() || !(0.5..=4.0).contains(&viewport.scale) {
            return Err("viewport scale must be finite and between 0.5 and 4.0".into());
        }
        if !(15..=240).contains(&viewport.refresh_hz) {
            return Err("refresh_hz must be between 15 and 240".into());
        }
        Ok(())
    }

    /// Validate all manifest fields: schema version, session ID, viewport,
    /// media preferences, and source-specific constraints.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "unsupported schema_version {}; expected {SCHEMA_VERSION}",
                self.schema_version
            ));
        }
        if !valid_id(&self.id) {
            return Err(
                "id must be 1-64 lowercase ASCII letters, digits, dashes, or underscores".into(),
            );
        }
        Self::validate_viewport(&self.viewport)?;
        if self.media.video_codecs.is_empty() {
            return Err("at least one video codec is required".into());
        }
        for codec in &self.media.video_codecs {
            if !matches!(codec.as_str(), "h264" | "hevc" | "h265" | "av1" | "vp8") {
                return Err(format!("unsupported video codec: {codec}"));
            }
        }
        if self.media.audio_codec != "opus" {
            return Err("audio_codec must be opus".into());
        }
        if !(128..=100_000).contains(&self.media.max_bitrate_kbps) {
            return Err("max_bitrate_kbps must be between 128 and 100000".into());
        }
        match &self.source {
            SessionSource::LaunchedApp { command } => {
                if command.is_empty() || command[0].is_empty() {
                    return Err("launched_app command must not be empty".into());
                }
                if command.iter().any(|argument| argument.contains('\0')) {
                    return Err("command arguments must not contain NUL bytes".into());
                }
            }
            SessionSource::DesktopMirror { output } => {
                if let Some(output) = output {
                    if output.len() > 128 {
                        return Err("desktop_mirror output must be at most 128 characters".into());
                    }
                    if output.contains('/') || output.contains('\0') || output.contains("..") {
                        return Err(
                            "desktop_mirror output must not contain path separators or NUL bytes"
                                .into(),
                        );
                    }
                    if output.bytes().any(|b| b.is_ascii_control()) {
                        return Err(
                            "desktop_mirror output must not contain control characters".into()
                        );
                    }
                }
            }
            SessionSource::DesktopApp { window_id } => {
                if window_id.is_empty()
                    || window_id.len() > 16
                    || !window_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(
                        "desktop_app window_id must be a stable hexadecimal window identity".into(),
                    );
                }
            }
            SessionSource::RemoteWorkspace => {}
        }
        Ok(())
    }
}

/// Check if a session ID is valid (1-64 lowercase ASCII letters, digits, dashes, or underscores).
pub(crate) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
}

/// Load and parse a session manifest from a JSON file.
pub fn load_manifest(path: &Path) -> Result<SessionManifest, String> {
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let manifest: SessionManifest = serde_json::from_str(&contents)
        .map_err(|error| format!("invalid manifest {}: {error}", path.display()))?;
    manifest.validate()?;
    Ok(manifest)
}

/// Get the session runtime root directory (`$ENZREMOTE_RUNTIME_DIR` or `$XDG_RUNTIME_DIR/enzremote`).
pub(crate) fn runtime_root() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("ENZREMOTE_RUNTIME_DIR") {
        return Ok(PathBuf::from(path));
    }
    env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .map(|path| path.join("enzremote"))
        .ok_or_else(|| "XDG_RUNTIME_DIR or ENZREMOTE_RUNTIME_DIR is required".into())
}

/// Get the default compositor helper script path.
pub fn default_helper() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("ENZREMOTE_COMPOSITOR_HELPER") {
        return Ok(PathBuf::from(path));
    }
    Ok(PathBuf::from("/usr/share/enzui/enzui-hypr"))
}

/// Path to the portal capture probe helper script.
pub fn default_capture_helper() -> PathBuf {
    enzremote_webrtc_ipc::layout::helper("ENZREMOTE_CAPTURE_HELPER", "portal-capture-probe")
}

/// Path to the ydotool-based pointer input helper binary.
pub fn default_input_pointer_helper() -> PathBuf {
    enzremote_webrtc_ipc::layout::helper("ENZREMOTE_INPUT_POINTER_HELPER", "enzremote-input-pointer")
}

/// Path to the EIS (Emulated Input Subsystem) input worker binary.
pub(crate) fn default_input_eis_helper() -> PathBuf {
    enzremote_webrtc_ipc::layout::helper("ENZREMOTE_INPUT_EIS_HELPER", "enzremote-input-eis")
}

/// Path to the EIS input broker binary (defaults to the current executable).
pub(crate) fn default_input_broker() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("ENZREMOTE_INPUT_BROKER") {
        return Ok(PathBuf::from(path));
    }
    let executable = env::current_exe()
        .map_err(|error| format!("could not locate the session executable: {error}"))?;
    // An atomic package upgrade may unlink the running daemon. Its procfs
    // executable still opens the same image for the internal broker command.
    Ok(if executable.is_file() { executable } else { PathBuf::from("/proc/self/exe") })
}

/// Path to the shared Hyprland compositor build directory.
pub(crate) fn compositor_buildroot() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("ENZREMOTE_COMPOSITOR_BUILD_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(PathBuf::from("/usr/lib/enzui/compositor"))
}

/// Create a new session from a manifest, launching the compositor and app.
pub fn create_session(manifest: SessionManifest, helper: &Path) -> Result<SessionState, String> {
    manifest.validate()?;
    if !matches!(manifest.source, SessionSource::LaunchedApp { .. }) {
        return Err("the nested compositor adapter currently supports launched_app only".into());
    }
    if !helper.is_file() {
        return Err(format!("compositor helper not found: {}", helper.display()));
    }

    let session_dir = runtime_root()?.join(&manifest.id);
    create_exclusive_dir(&session_dir).map_err(|error| {
        if error.kind() == io::ErrorKind::AlreadyExists {
            format!(
                "session directory already exists: {}; stop it or choose a new id",
                session_dir.display()
            )
        } else {
            format!("could not create {}: {error}", session_dir.display())
        }
    })?;

    let config = session_dir.join("hyprland.lua");
    let runroot = session_dir.join("compositor");
    fs::write(&config, render_hyprland_config(&manifest.viewport))
        .map_err(|error| format!("could not write {}: {error}", config.display()))?;

    let mut state = SessionState {
        manifest,
        status: LifecycleStatus::Starting,
        created_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system clock is before Unix epoch: {error}"))?
            .as_millis() as u64,
        helper: helper.to_path_buf(),
        runroot,
        buildroot: compositor_buildroot()?,
        config,
    };
    write_state(&session_dir, &state)?;

    start_session(&mut state)?;
    Ok(state)
}

/// Resume a session and restore its stored viewport before media reuse.
/// Active app processes are retained; stopped sessions cannot be resumed.
pub fn resume_session(id: &str) -> Result<SessionState, String> {
    let mut state = read_state(id)?;
    if state.status == LifecycleStatus::Stopped {
        return Err("stopped sessions cannot be resumed; create a session with a new id".into());
    }
    if state.status == LifecycleStatus::Active
        && pid_file_alive(&state.runroot.join("hyprland.pid"))
        && app_present(&state)
        && (!state.manifest.permissions.input
            || (pid_file_alive(&state.runroot.join("input-broker.pid"))
                && is_socket(&input_endpoint(&state).socket)))
    {
        let viewport = state.manifest.viewport;
        return resize_session_full(
            id,
            viewport.width,
            viewport.height,
            Some(viewport.scale),
            Some(viewport.refresh_hz),
        );
    }
    start_session(&mut state)?;
    Ok(state)
}

/// Inspect a session's current state, process liveness, and endpoints.
pub fn inspect_session(id: &str) -> Result<Inspection, String> {
    let state = read_state(id)?;
    let compositor_alive = pid_file_alive(&state.runroot.join("hyprland.pid"));
    let input = input_endpoint(&state);
    Ok(Inspection {
        capture: compositor_alive
            .then(|| capture_endpoint(&state))
            .transpose()?,
        app_alive: compositor_alive && app_present(&state),
        stream_alive: pid_file_alive(&state.runroot.join("remote-stream.pid")),
        input: (compositor_alive
            && state.manifest.permissions.input
            && pid_file_alive(&state.runroot.join("input-broker.pid"))
            && is_socket(&input.socket))
        .then_some(input),
        compositor_alive,
        state,
    })
}

/// Start the media stream for a session, launching the capture pipeline
/// and EIS input broker.
pub fn start_stream(
    id: &str,
    consumer: &Path,
    capture_helper: &Path,
) -> Result<Inspection, String> {
    let state = read_state(id)?;
    if state.status != LifecycleStatus::Active
        || !pid_file_alive(&state.runroot.join("hyprland.pid"))
        || !app_present(&state)
        || (state.manifest.permissions.input
            && (!pid_file_alive(&state.runroot.join("input-broker.pid"))
                || !is_socket(&input_endpoint(&state).socket)))
    {
        return Err("session is not active".into());
    }
    if pid_file_alive(&state.runroot.join("remote-stream.pid")) {
        return Err("session stream is already active".into());
    }
    if !capture_helper.is_file() {
        return Err(format!(
            "capture helper not found: {}",
            capture_helper.display()
        ));
    }
    if !consumer.is_file() {
        return Err(format!("stream consumer not found: {}", consumer.display()));
    }

    let capture = capture_endpoint(&state)?;
    let capture_helper = fs::canonicalize(capture_helper).map_err(|error| {
        format!(
            "could not resolve capture helper {}: {error}",
            capture_helper.display()
        )
    })?;
    let consumer = fs::canonicalize(consumer)
        .map_err(|error| format!("could not resolve consumer {}: {error}", consumer.display()))?;
    let mut input_capability = None;
    let mut arguments = vec![
        OsString::from("app"),
        OsString::from("remote-stream"),
        OsString::from("--"),
    ];
    if state.manifest.permissions.input {
        let input = input_endpoint(&state);
        let input_helper = fs::canonicalize(default_input_eis_helper()).map_err(|error| {
            format!("EIS input helper is unavailable; run scripts/build-input-eis: {error}")
        })?;
        let capability = UnixStream::connect(&input.socket).map_err(|error| {
            format!(
                "could not connect to EIS broker {}: {error}",
                input.socket.display()
            )
        })?;
        fcntl(&capability, FcntlArg::F_SETFD(FdFlag::empty()))
            .map_err(|error| format!("could not inherit EIS broker capability: {error}"))?;
        arguments.extend([
            OsString::from("/usr/bin/env"),
            OsString::from(format!(
                "ENZREMOTE_EIS_BROKER_FD={}",
                capability.as_raw_fd()
            )),
            OsString::from(format!(
                "ENZREMOTE_INPUT_EIS_HELPER={}",
                input_helper.display()
            )),
        ]);
        input_capability = Some(capability);
    }
    arguments.extend([
        capture_helper.into_os_string(),
        OsString::from(capture.output),
        consumer.into_os_string(),
    ]);
    invoke_helper(&state, arguments)?;
    drop(input_capability);
    inspect_session(id)
}

/// Stop only the stream process (SIGTERM + fallback SIGKILL), keeping
/// the compositor, app, and input broker alive for reconnection.
pub fn stop_stream(id: &str) -> Result<Inspection, String> {
    let state = read_state(id)?;
    if state.status != LifecycleStatus::Active {
        return Err("session is not active".into());
    }
    let pid_file = state.runroot.join("remote-stream.pid");
    if !pid_file_alive(&pid_file) {
        return inspect_session(id);
    }
    terminate_pid_file(&pid_file)?;
    inspect_session(id)
}

fn terminate_pid_file(pid_file: &Path) -> Result<(), String> {
    if !pid_file_alive(pid_file) {
        let _ = fs::remove_file(pid_file);
        let _ = fs::remove_file(pid_file.with_extension("pid.start"));
        return Ok(());
    }
    let pid_text = fs::read_to_string(pid_file)
        .map_err(|error| format!("could not read {}: {error}", pid_file.display()))?;
    let pid: u32 = pid_text
        .trim()
        .parse()
        .map_err(|_| format!("invalid PID in {}", pid_file.display()))?;
    if pid == 0 {
        return Err(format!(
            "refusing to signal PID 0 from {}",
            pid_file.display()
        ));
    }
    let nix_pid = nix::unistd::Pid::from_raw(
        i32::try_from(pid)
            .map_err(|_| format!("PID {pid} exceeds i32 range in {}", pid_file.display()))?,
    );
    // Send SIGTERM to the process (not the process group — the PID file
    // contains a PID, not a PGID, and the helper may share its group with
    // other processes).
    let _ = nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGTERM);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if !pid_file_alive(pid_file) {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    // Re-verify identity before escalating to SIGKILL.
    if pid_file_alive(pid_file) {
        let _ = nix::sys::signal::kill(nix_pid, nix::sys::signal::Signal::SIGKILL);
        thread::sleep(Duration::from_millis(100));
    }
    let _ = fs::remove_file(pid_file);
    let _ = fs::remove_file(pid_file.with_extension("pid.start"));
    Ok(())
}

/// Send a pointer event (absolute move, optional click) to a session's
/// compositor via the ydotool-based input helper.
pub fn send_pointer(
    id: &str,
    x: u32,
    y: u32,
    click: bool,
    input_helper: &Path,
) -> Result<PointerResult, String> {
    let state = read_state(id)?;
    if state.status != LifecycleStatus::Active
        || !pid_file_alive(&state.runroot.join("hyprland.pid"))
        || !app_present(&state)
    {
        return Err("session is not active".into());
    }
    validate_pointer(&state.manifest, x, y)?;
    let viewport = &state.manifest.viewport;
    if !input_helper.is_file() {
        return Err(format!(
            "input pointer helper not found: {}; run scripts/build-input-pointer",
            input_helper.display()
        ));
    }

    let capture = capture_endpoint(&state)?;
    let mut command = Command::new(input_helper);
    command
        .args([
            viewport.width.to_string(),
            viewport.height.to_string(),
            x.to_string(),
            y.to_string(),
        ])
        .env("WAYLAND_DISPLAY", capture.wayland_display)
        .env("HYPRLAND_INSTANCE_SIGNATURE", capture.hyprland_instance)
        .env_remove("DISPLAY");
    if click {
        command.arg("--click");
    }
    let output = command
        .output()
        .map_err(|error| format!("could not execute {}: {error}", input_helper.display()))?;
    if !output.status.success() {
        return Err(format!(
            "input pointer helper failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(PointerResult {
        session_id: state.manifest.id,
        width: viewport.width,
        height: viewport.height,
        x,
        y,
        click,
    })
}

fn validate_pointer(manifest: &SessionManifest, x: u32, y: u32) -> Result<(), String> {
    if !manifest.permissions.input {
        return Err("session manifest does not grant input".into());
    }
    if x >= manifest.viewport.width || y >= manifest.viewport.height {
        return Err(format!(
            "pointer coordinates {x},{y} are outside {}x{}",
            manifest.viewport.width, manifest.viewport.height
        ));
    }
    Ok(())
}

/// Stop a session and all its child processes (compositor, app, stream, broker).
/// Remove audio sinks left by sessions whose compositor is gone.
pub fn cleanup_audio_sinks() {
    let mut owned = Vec::new();
    if let Ok(root) = runtime_root()
        && let Ok(entries) = fs::read_dir(root)
    {
        for entry in entries.flatten() {
            let Some(id) = entry.file_name().to_str().map(str::to_string) else { continue };
            let Ok(state) = read_state(&id) else { continue };
            let module = fs::read_to_string(state.runroot.join("audio-module")).unwrap_or_default();
            if module.trim().is_empty() {
                continue;
            }
            if state.status == LifecycleStatus::Active && pid_file_alive(&state.runroot.join("hyprland.pid")) {
                owned.push(module.trim().to_string());
            } else {
                audio::remove_sink(&state.runroot);
            }
        }
    }
    audio::remove_stale_sinks(&owned);
}

/// The audio sink a session's app plays into, if one was created.
pub fn audio_sink(id: &str) -> Option<String> {
    let state = read_state(id).ok()?;
    audio::session_sink(&state.runroot)
}

pub fn stop_session(id: &str) -> Result<SessionState, String> {
    let mut state = read_state(id)?;
    if [
        "hyprland.pid",
        "remote-app.pid",
        "remote-stream.pid",
        "input-broker.pid",
    ]
    .iter()
    .any(|name| pid_file_alive(&state.runroot.join(name)))
    {
        invoke_helper(&state, [OsString::from("stop")])?;
    }
    audio::remove_sink(&state.runroot);
    state.status = LifecycleStatus::Stopped;
    let session_dir = runtime_root()?.join(id);
    write_state(&session_dir, &state)?;
    Ok(state)
}

/// Resize a session viewport (width and height only, keeping scale and refresh).
/// Resize with optional scale and refresh rate overrides.
pub fn resize_session_full(
    id: &str,
    width: u32,
    height: u32,
    scale: Option<f32>,
    refresh_hz: Option<u32>,
) -> Result<SessionState, String> {
    let mut state = read_state(id)?;
    if state.status != LifecycleStatus::Active
        || !pid_file_alive(&state.runroot.join("hyprland.pid"))
    {
        return Err("session compositor is not active".into());
    }

    let mut viewport = state.manifest.viewport;
    viewport.width = width;
    viewport.height = height;
    if let Some(s) = scale {
        viewport.scale = s;
    }
    if let Some(r) = refresh_hz {
        viewport.refresh_hz = r;
    }
    SessionManifest::validate_viewport(&viewport)?;

    apply_viewport(&state, &viewport)?;

    state.manifest.viewport = viewport;
    fs::write(
        &state.config,
        render_session_config(&state, &state.manifest.viewport)?,
    )
    .map_err(|error| format!("could not update {}: {error}", state.config.display()))?;
    let session_dir = state
        .config
        .parent()
        .ok_or_else(|| "session config has no parent directory".to_string())?;
    write_state(session_dir, &state)?;
    Ok(state)
}

fn prepare_session_output(state: &SessionState) -> Result<(), String> {
    let mode = env::var("ENZREMOTE_OUTPUT_MODE").unwrap_or_else(|_| "auto".into());
    if !["auto", "headless", "wayland"].contains(&mode.as_str()) {
        return Err("output mode must be auto, headless or wayland".into());
    }
    let monitors = read_monitors(state)?;
    let outputs = monitors.as_array().ok_or("invalid compositor output list")?;
    // A compositor with a render backend but no attached display still needs
    // a private canvas. This occurs on seated hosts as well as headless logins.
    let original = outputs.first().and_then(|output| output["name"].as_str());
    if mode == "wayland" {
        return original.map(|_| ()).ok_or_else(|| "session compositor has no Wayland output".into());
    }
    if original == Some("ENZREMOTE") { return Ok(()); }
    if let Some(original) = original
        && original != "WAYLAND-1"
    {
        return if mode == "auto" { Ok(()) } else { Err("headless conversion requires the private nested Wayland output".into()) };
    }
    // A parent Wayland window can be resized independently by the desktop.
    // Keep its rendering backend, but make the app's scanout a headless output.
    let created = compositor_command(state, "output create headless ENZREMOTE");
    if !created.as_ref().is_ok_and(|reply| reply.trim() == "ok") {
        if mode == "auto"
            && read_monitors(state)?.as_array().is_some_and(|outputs| {
                outputs.len() == 1 && outputs[0]["name"].as_str() == original
            })
        {
            eprintln!("session output: headless unavailable; retaining nested Wayland output");
            return Ok(());
        }
        return Err("could not create headless session output".into());
    }
    if original == Some("WAYLAND-1") {
        submit_viewport_rule(state, "hl.monitor({ output = \"WAYLAND-1\", disabled = true })")?;
    }
    for _ in 0..40 {
        let monitors = read_monitors(state)?;
        if monitors
            .as_array()
            .is_some_and(|a| a.len() == 1 && a[0]["name"].as_str() == Some("ENZREMOTE"))
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    Err("headless session output did not become exclusive".into())
}

fn render_session_config(state: &SessionState, viewport: &Viewport) -> Result<String, String> {
    Ok(render_output_config(
        viewport,
        output_name(state)? == "ENZREMOTE",
    ))
}

fn render_output_config(viewport: &Viewport, headless: bool) -> String {
    let config = render_hyprland_config(viewport);
    if !headless {
        return config;
    }
    config.replacen("output = \"\"", "output = \"ENZREMOTE\"", 1)
        + "\nhl.monitor({ output = \"WAYLAND-1\", disabled = true })\n"
}

fn apply_viewport(state: &SessionState, viewport: &Viewport) -> Result<(), String> {
    let output = output_name(state)?;
    // Keep an open oversized dialog usable across phone resizes.
    let need = floating_need(state).unwrap_or((0, 0));
    let viewport = &Viewport {
        scale: fit_scale(viewport.width, viewport.height, viewport.scale, need),
        ..*viewport
    };
    let monitors = read_monitors(state)?;
    let already_matched = viewport_matches(&monitors, &output, viewport);
    let encoded_output = serde_json::to_string(&output)
        .map_err(|error| format!("could not encode output name: {error}"))?;
    // Hyprland validates fractional scale against the current physical mode,
    // which may still be the parent Wayland backend's transient size.
    for target in viewport_stages(already_matched, *viewport) {
        let code = format!(
            "hl.monitor({{ output = {encoded_output}, mode = \"{}x{}@{}\", position = \"0x0\", scale = {} }})",
            target.width, target.height, target.refresh_hz, target.scale
        );
        submit_viewport_rule(state, &code)?;
        await_viewport(
            || Ok(viewport_matches(&read_monitors(state)?, &output, &target)),
            || submit_viewport_rule(state, &code),
        )?;
    }
    if need != (0, 0) {
        center_floating(state)?;
    }
    Ok(())
}

/// Largest quarter-step scale, no larger than `base`, at which a window of
/// `need` logical pixels fits on a `width` x `height` pixel output. Hyprland
/// accepts only scales that give whole logical pixels; 1.0 and 0.5 always do.
pub(crate) fn fit_scale(width: u32, height: u32, base: f32, need: (u32, u32)) -> f32 {
    let fits = |scale: f64| {
        f64::from(width) / scale + 0.001 >= f64::from(need.0)
            && f64::from(height) / scale + 0.001 >= f64::from(need.1)
    };
    if fits(f64::from(base)) {
        return base;
    }
    let base_quarters = (f64::from(base) * 4.0).floor() as u32;
    (2..base_quarters)
        .rev()
        .find(|&q| (width * 4).is_multiple_of(q) && (height * 4).is_multiple_of(q) && fits(f64::from(q) / 4.0))
        .map_or(0.5, |q| q as f32 / 4.0)
}

/// Largest mapped floating window (dialogs), in logical pixels.
fn floating_need(state: &SessionState) -> Result<(u32, u32), String> {
    Ok(floating_windows(state)?
        .iter()
        .fold((0, 0), |(w, h), window| (w.max(window.1), h.max(window.2))))
}

/// (address, width, height) of mapped, visible floating windows, least
/// recently focused first.
fn floating_windows(state: &SessionState) -> Result<Vec<(String, u32, u32)>, String> {
    let clients = compositor_query(state, "clients")?;
    let clients: Vec<serde_json::Value> = serde_json::from_str(&clients)
        .map_err(|error| format!("compositor returned invalid client JSON: {error}"))?;
    let mut floating: Vec<&serde_json::Value> = clients
        .iter()
        .filter(|c| {
            c["mapped"].as_bool() == Some(true)
                && c["hidden"].as_bool() != Some(true)
                && c["floating"].as_bool() == Some(true)
        })
        .collect();
    floating.sort_by_key(|c| std::cmp::Reverse(c["focusHistoryID"].as_i64().unwrap_or(0)));
    Ok(floating
        .into_iter()
        .filter_map(|c| {
            let size = c["size"].as_array()?;
            Some((
                c["address"].as_str()?.to_owned(),
                u32::try_from(size.first()?.as_i64()?.max(0)).ok()?,
                u32::try_from(size.get(1)?.as_i64()?.max(0)).ok()?,
            ))
        })
        .collect())
}

/// Center dialogs in the work area (above a keyboard reservation; Hyprland's
/// own center ignores it) and keep them above the maximized main window
/// (focusing a floating window raises it; the most recent is focused last).
fn center_floating(state: &SessionState) -> Result<(), String> {
    let monitors = read_monitors(state)?;
    let Some(monitor) = monitors.as_array().and_then(|items| items.first()) else {
        return Ok(());
    };
    let scale = monitor["scale"].as_f64().filter(|s| *s > 0.0).unwrap_or(1.0);
    let logical = |key: &str| monitor[key].as_f64().unwrap_or(0.0) / scale;
    let bottom = monitor["reserved"]
        .as_array()
        .and_then(|r| r.get(3))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    let (work_width, work_height) = (logical("width").round(), logical("height").round() - bottom);
    for (address, width, height) in floating_windows(state)? {
        let selector = serde_json::to_string(&format!("address:{address}"))
            .map_err(|error| format!("could not encode window selector: {error}"))?;
        let x = ((work_width - f64::from(width)) / 2.0).floor().max(0.0);
        let y = ((work_height - f64::from(height)) / 2.0).floor().max(0.0);
        submit_viewport_rule(
            state,
            &format!(
                "hl.dispatch(hl.dsp.window.move({{ x = {x}, y = {y}, window = {selector} }})); hl.dispatch(hl.dsp.focus({{ window = {selector} }}))"
            ),
        )?;
    }
    Ok(())
}

/// Zoom the app's screen out while a dialog is larger than it, and back once
/// it closes, without changing the stream's pixel size. A dialog squeezed
/// into a smaller screen is drawn shrunk but receives input at its real size,
/// so its buttons cannot be clicked. Returns the new scale when it changed.
pub fn fit_windows(id: &str) -> Result<Option<f32>, String> {
    let state = read_state(id)?;
    if state.status != LifecycleStatus::Active || !pid_file_alive(&state.runroot.join("hyprland.pid")) {
        return Ok(None);
    }
    let monitors = read_monitors(&state)?;
    let Some(monitor) = monitors.as_array().and_then(|items| items.first()) else {
        return Ok(None);
    };
    let (Some(output), Some(width), Some(height), Some(current)) = (
        monitor["name"].as_str(),
        monitor["width"].as_u64().and_then(|v| u32::try_from(v).ok()),
        monitor["height"].as_u64().and_then(|v| u32::try_from(v).ok()),
        monitor["scale"].as_f64(),
    ) else {
        return Ok(None);
    };
    let need = floating_need(&state)?;
    let target = fit_scale(width, height, state.manifest.viewport.scale, need);
    if (f64::from(target) - current).abs() < 0.001 {
        return Ok(None);
    }
    // Keep a keyboard reservation the same height in pixels.
    let reserved_pixels = monitor["reserved"]
        .as_array()
        .and_then(|r| r.get(3))
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0)
        * current;
    let bottom = (reserved_pixels / f64::from(target)).round() as u32;
    let encoded_output = serde_json::to_string(output)
        .map_err(|error| format!("could not encode output name: {error}"))?;
    submit_viewport_rule(
        &state,
        &format!(
            "hl.monitor({{ output = {encoded_output}, mode = \"{width}x{height}@{}\", position = \"0x0\", scale = {target}, reserved = {{ bottom = {bottom} }} }})",
            state.manifest.viewport.refresh_hz
        ),
    )?;
    center_floating(&state)?;
    Ok(Some(target))
}

fn await_viewport(
    mut matches: impl FnMut() -> Result<bool, String>,
    mut reassert: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut submitted_at = Instant::now();
    let mut retries = 0;
    let mut settled = 0;
    loop {
        let matched = matches()?;
        settled = if matched { settled + 1 } else { 0 };
        if settled >= 3 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("compositor did not settle at the requested viewport".into());
        }
        // A parent configure can overwrite the first nested mode request.
        if !matched && retries < 3 && submitted_at.elapsed() >= Duration::from_millis(150) {
            reassert()?;
            submitted_at = Instant::now();
            retries += 1;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn submit_viewport_rule(state: &SessionState, code: &str) -> Result<(), String> {
    let response = compositor_command(state, &format!("eval {code}"))?;
    if response.trim() != "ok" {
        return Err(format!("compositor rejected viewport: {}", response.trim()));
    }
    Ok(())
}

fn viewport_stages(already_matched: bool, viewport: Viewport) -> Vec<Viewport> {
    if viewport.scale == 1.0 || already_matched {
        vec![viewport]
    } else {
        vec![
            Viewport {
                scale: 1.0,
                ..viewport
            },
            viewport,
        ]
    }
}

fn viewport_matches(monitors: &serde_json::Value, output: &str, viewport: &Viewport) -> bool {
    monitors.as_array().is_some_and(|items| {
        items.iter().any(|m| {
            m["name"].as_str() == Some(output)
                && m["width"].as_u64() == Some(u64::from(viewport.width))
                && m["height"].as_u64() == Some(u64::from(viewport.height))
                && m["scale"]
                    .as_f64()
                    .is_some_and(|scale| (scale - f64::from(viewport.scale)).abs() < 0.001)
        })
    })
}

fn read_monitors(state: &SessionState) -> Result<serde_json::Value, String> {
    serde_json::from_str(&compositor_query(state, "monitors")?)
        .map_err(|error| format!("compositor returned invalid monitor JSON: {error}"))
}

fn output_name(state: &SessionState) -> Result<String, String> {
    let monitors = read_monitors(state)?;
    monitors
        .as_array()
        .and_then(|monitors| monitors.first())
        .and_then(|monitor| monitor.get("name"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "session compositor has no output".to_string())
}

fn capture_endpoint(state: &SessionState) -> Result<CaptureEndpoint, String> {
    let read_value = |name: &str| {
        let path = state.runroot.join(name);
        fs::read_to_string(&path)
            .map(|value| value.trim().to_owned())
            .map_err(|error| format!("could not read {}: {error}", path.display()))
    };
    Ok(CaptureEndpoint {
        output: output_name(state)?,
        wayland_display: read_value("wayland-display")?,
        hyprland_instance: read_value("instance")?,
    })
}

fn input_endpoint(state: &SessionState) -> InputEndpoint {
    InputEndpoint {
        socket: state.runroot.join("eis-broker.sock"),
        mapping_id: format!("enzremote-{}", state.manifest.id),
    }
}

fn read_state(id: &str) -> Result<SessionState, String> {
    if !valid_id(id) {
        return Err("invalid session id".into());
    }
    let path = runtime_root()?.join(id).join("state.json");
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    serde_json::from_str(&contents)
        .map_err(|error| format!("invalid state {}: {error}", path.display()))
}

fn write_state(session_dir: &Path, state: &SessionState) -> Result<(), String> {
    let path = session_dir.join("state.json");
    let temporary = session_dir.join("state.json.new");
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("could not serialize session state: {error}"))?;
    fs::write(&temporary, bytes)
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .map_err(|error| format!("could not replace {}: {error}", path.display()))
}

fn start_session(state: &mut SessionState) -> Result<(), String> {
    let SessionSource::LaunchedApp { command } = &state.manifest.source else {
        return Err("the nested compositor adapter currently supports launched_app only".into());
    };
    let session_dir = state
        .config
        .parent()
        .ok_or_else(|| "session config has no parent directory".to_string())?;
    let session_dir = session_dir.to_path_buf();

    let mut compositor_alive = pid_file_alive(&state.runroot.join("hyprland.pid"));
    let broker_alive = pid_file_alive(&state.runroot.join("input-broker.pid"));
    let input_ready = state.manifest.permissions.input
        && compositor_alive
        && broker_alive
        && is_socket(&input_endpoint(state).socket);
    if state.manifest.permissions.input && !input_ready && (compositor_alive || broker_alive) {
        invoke_helper(state, [OsString::from("stop")])?;
        compositor_alive = false;
    }
    if !compositor_alive {
        // The nested backend creates its initial mode before applying our requested
        // mode; fractional scale may be invalid for that temporary size.
        let mut bootstrap_viewport = state.manifest.viewport;
        bootstrap_viewport.scale = 1.0;
        fs::write(&state.config, render_hyprland_config(&bootstrap_viewport))
            .map_err(|e| format!("could not write bootstrap viewport: {e}"))?;
    }
    let compositor_control = (state.manifest.permissions.input && !input_ready)
        .then(|| start_input_broker(state))
        .transpose()?;
    if let Err(error) = invoke_helper_with_control(
        state,
        [OsString::from("start")],
        compositor_control.as_ref(),
    ) {
        let _ = invoke_helper(state, [OsString::from("stop")]);
        return Err(error);
    }
    if state.manifest.permissions.input {
        let socket = input_endpoint(state).socket;
        for _ in 0..40 {
            if is_socket(&socket) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if !is_socket(&socket) {
            let _ = invoke_helper(state, [OsString::from("stop")]);
            return Err(format!(
                "compositor did not create the EIS input socket: {}",
                socket.display()
            ));
        }
    }
    drop(compositor_control);
    if !compositor_alive && let Err(error) = prepare_session_output(state) {
        let _ = invoke_helper(state, [OsString::from("stop")]);
        return Err(error);
    }
    if let Err(error) = apply_viewport(state, &state.manifest.viewport) {
        let _ = invoke_helper(state, [OsString::from("stop")]);
        return Err(error);
    }
    fs::write(
        &state.config,
        render_session_config(state, &state.manifest.viewport)?,
    )
    .map_err(|e| format!("could not persist viewport config: {e}"))?;
    if !pid_file_alive(&state.runroot.join("remote-app.pid")) {
        // The app's sound goes to its own sink, streamed to the phone.
        let _ = audio::ensure_sink(&state.runroot, &state.manifest.id, &state.manifest.id);
        let mut arguments = vec![
            OsString::from("app"),
            OsString::from("remote-app"),
            OsString::from("--"),
        ];
        arguments.extend(command.iter().map(OsString::from));
        if let Err(error) = invoke_helper(state, arguments) {
            let _ = invoke_helper(state, [OsString::from("stop")]);
            return Err(error);
        }
    }

    state.status = LifecycleStatus::Active;
    write_state(&session_dir, state)
}

fn invoke_helper<I>(state: &SessionState, arguments: I) -> Result<(), String>
where
    I: IntoIterator<Item = OsString>,
{
    invoke_helper_with_control(state, arguments, None)
}

fn invoke_helper_with_control<I>(
    state: &SessionState,
    arguments: I,
    compositor_control: Option<&UnixStream>,
) -> Result<(), String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut command = Command::new(&state.helper);
    command
        .args(arguments)
        .env("ENZUI_HYPR_CONFIG", &state.config)
        .env("ENZUI_HYPR_RUNROOT", &state.runroot)
        .env("ENZUI_HYPR_BUILDROOT", &state.buildroot);
    if let Some(sink) = audio::session_sink(&state.runroot) {
        command.env("PULSE_SINK", sink);
    }
    if let Some(control) = compositor_control {
        command
            .env("ENZREMOTE_EIS_CONTROL_FD", control.as_raw_fd().to_string())
            .env("ENZREMOTE_EIS_MAPPING_ID", input_endpoint(state).mapping_id);
    } else {
        command
            .env_remove("ENZREMOTE_EIS_CONTROL_FD")
            .env_remove("ENZREMOTE_EIS_MAPPING_ID");
    }
    let status = command
        .status()
        .map_err(|error| format!("could not execute {}: {error}", state.helper.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "compositor helper {} exited with {status}",
            state.helper.display()
        ))
    }
}

fn start_input_broker(state: &SessionState) -> Result<UnixStream, String> {
    create_private_dir(&state.runroot)
        .map_err(|error| format!("could not create {}: {error}", state.runroot.display()))?;
    let input = input_endpoint(state);
    if let Err(error) = fs::remove_file(&input.socket)
        && error.kind() != io::ErrorKind::NotFound
    {
        return Err(format!(
            "could not remove stale {}: {error}",
            input.socket.display()
        ));
    }
    let broker = default_input_broker()?;
    if !broker.is_file() {
        return Err(format!("EIS input broker not found: {}", broker.display()));
    }

    let (broker_control, compositor_control) = UnixStream::pair()
        .map_err(|error| format!("could not create EIS control channel: {error}"))?;
    let log_path = state.runroot.join("input-broker.log");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| format!("could not open {}: {error}", log_path.display()))?;
    let mut broker_command = Command::new(&broker);
    if env::var_os("ENZREMOTE_INPUT_BROKER").is_none() {
        broker_command.arg("internal-eis-broker");
    }
    let mut child = broker_command
        .process_group(0)
        .arg(&input.socket)
        .stdin(Stdio::from(OwnedFd::from(broker_control)))
        .stdout(Stdio::null())
        .stderr(Stdio::from(log.try_clone().map_err(|error| {
            format!("could not clone broker log: {error}")
        })?))
        .spawn()
        .map_err(|error| format!("could not execute {}: {error}", broker.display()))?;
    if let Err(error) = write_pid_file(&state.runroot.join("input-broker.pid"), child.id()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    if child
        .try_wait()
        .map_err(|error| format!("could not inspect EIS input broker: {error}"))?
        .is_some()
    {
        return Err(format!(
            "EIS input broker exited during startup; see {}",
            log_path.display()
        ));
    }
    if let Err(error) = fcntl(&compositor_control, FcntlArg::F_SETFD(FdFlag::empty())) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!(
            "could not inherit compositor EIS control fd: {error}"
        ));
    }
    // Detach: a background thread reaps the child to prevent zombies.
    // The broker is managed via PID file by the compositor helper.
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(compositor_control)
}

/// Extract the starttime (field 22) from `/proc/pid/stat`.
///
/// The comm field (field 2) may contain spaces and parentheses,
/// so we find the last `)` and count fields from there.
/// Field 22 in the original stat line becomes field 20 (0-indexed)
/// after stripping the first two fields (pid and comm).
fn parse_starttime(stat: &str) -> Option<&str> {
    let rest = stat.rsplit_once(')')?.1;
    // After the closing paren, fields are 3..N (1-indexed).
    // starttime is field 22 (1-indexed), so index 22-3 = 19 (0-indexed from rest).
    rest.split_whitespace().nth(19)
}

fn write_pid_file(path: &Path, pid: u32) -> Result<(), String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("could not inspect broker process {pid}: {error}"))?;
    let start = parse_starttime(&stat)
        .ok_or_else(|| format!("process {pid} returned invalid stat data"))?;
    fs::write(path, format!("{pid}\n"))
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    let start_path = path.with_extension("pid.start");
    fs::write(&start_path, format!("{start}\n"))
        .map_err(|error| format!("could not write {}: {error}", start_path.display()))
}

/// The app is present: its launched process runs or, for launchers that
/// hand off to a child and exit, the compositor still shows a window.
fn app_present(state: &SessionState) -> bool {
    pid_file_alive(&state.runroot.join("remote-app.pid"))
        || (pid_file_alive(&state.runroot.join("hyprland.pid"))
            && window_count(state).is_ok_and(|count| count > 0))
}

fn window_count(state: &SessionState) -> Result<usize, String> {
    let clients = compositor_query(state, "clients")?;
    let clients: Vec<serde_json::Value> = serde_json::from_str(&clients)
        .map_err(|error| format!("compositor returned invalid client JSON: {error}"))?;
    Ok(clients.len())
}

/// Last meaningful lines the app wrote before exiting (compositor and
/// toolkit noise removed), for telling the user why it did not start.
pub fn app_exit_reason(id: &str) -> Option<String> {
    let state = read_state(id).ok()?;
    let log = fs::read(state.runroot.join("remote-app.log")).ok()?;
    let log = String::from_utf8_lossy(&log);
    let lines: Vec<&str> = log
        .lines()
        .map(str::trim)
        .filter(|line| meaningful_app_log_line(line))
        .collect();
    let tail = lines[lines.len().saturating_sub(2)..].join("\n");
    (!tail.is_empty()).then(|| tail.chars().take(300).collect())
}

fn meaningful_app_log_line(line: &str) -> bool {
    const NOISE: [&str; 9] = [
        "DEBUG",
        "EIS:",
        "xkbcomp",
        "-WARNING **",
        "Gtk-Message",
        "dbus-daemon[",
        "Error Overlay",
        "Fontconfig",
        "MESA",
    ];
    !line.is_empty()
        && !line.starts_with('\u{1b}')
        && !NOISE.iter().any(|noise| line.contains(noise))
}

fn compositor_command(state: &SessionState, command: &str) -> Result<String, String> {
    session_compositor(state)?
        .command(command, COMPOSITOR_TIMEOUT)
        .map_err(|error| format!("session compositor request failed: {error}"))
}

fn compositor_query(state: &SessionState, query: &str) -> Result<String, String> {
    session_compositor(state)?
        .query_json(query, COMPOSITOR_TIMEOUT)
        .map_err(|error| format!("session compositor query failed: {error}"))
}

fn session_compositor(state: &SessionState) -> Result<Compositor, String> {
    let instance_path = state.runroot.join("instance");
    let instance = fs::read_to_string(&instance_path).map_err(|e| {
        format!(
            "could not read Hyprland instance file {}: {e}",
            instance_path.display()
        )
    })?;
    Compositor::for_instance(instance.trim()).map_err(|error| error.to_string())
}

/// Render a Hyprland configuration file for a headless session viewport.
pub(crate) fn render_hyprland_config(viewport: &Viewport) -> String {
    format!(
        r#"-- Generated by enzremote-session. Do not edit.
hl.monitor({{
    output = "",
    mode = "{}x{}@{}",
    position = "0x0",
    scale = {},
}})

hl.config({{
    general = {{ gaps_in = 0, gaps_out = 0, border_size = 0 }},
    decoration = {{ rounding = 0, shadow = {{ enabled = false }} }},
    animations = {{ enabled = false }},
    input = {{ follow_mouse = 1 }},
    misc = {{
        disable_hyprland_logo = true,
        disable_splash_rendering = true,
        force_default_wallpaper = 0,
        disable_scale_notification = true,
    }},
    ecosystem = {{ no_update_news = true, no_donation_nag = true }},
}})

-- Main windows fill the phone. Dialogs float centered at their own size so
-- clicks land where they are drawn; the host zooms out when one is larger
-- than the screen (see fit_windows).
hl.window_rule({{
    name = "remote-app-surface",
    match = {{ class = ".*", float = false }},
    fullscreen = true,
    border_size = 0,
    rounding = 0,
    decorate = false,
    no_anim = true,
    no_shadow = true,
}})

hl.window_rule({{
    name = "remote-app-dialog",
    match = {{ class = ".*", float = true }},
    center = true,
    border_size = 0,
    rounding = 0,
    no_anim = true,
}})
"#,
        viewport.width, viewport.height, viewport.refresh_hz, viewport.scale
    )
}

/// Probe the host's media and graphics capabilities (PipeWire, GPU, encoders).
pub fn probe_capabilities() -> CapabilityProbe {
    let inspect_available = command_exists("gst-inspect-1.0");
    let required = [
        "webrtcbin",
        "pipewiresrc",
        "opusenc",
        "rtpopuspay",
        "rtph264pay",
    ]
    .into_iter()
    .map(|name| probe_element(name, false, inspect_available))
    .collect();
    let h264_encoders = [
        ("vah264enc", true),
        ("vaapih264enc", true),
        ("qsvh264enc", true),
        ("nvh264enc", true),
        ("nvautogpuh264enc", true),
        ("openh264enc", false),
        ("x264enc", false),
    ]
    .into_iter()
    .map(|(name, hardware)| probe_element(name, hardware, inspect_available))
    .collect();
    let av1_encoders = [
        ("vaav1enc", true),
        ("qsvav1enc", true),
        ("nvav1enc", true),
        ("rav1enc", false),
        ("svtav1enc", false),
    ]
    .into_iter()
    .map(|(name, hardware)| probe_element(name, hardware, inspect_available))
    .collect();

    CapabilityProbe {
        schema_version: SCHEMA_VERSION,
        pipewire_running: command_success("pw-cli", ["info", "0"]),
        render_nodes: render_nodes(),
        gstreamer: GStreamerProbe {
            inspect_available,
            required,
            h264_encoders,
            av1_encoders,
        },
    }
}

fn probe_element(name: &str, hardware_candidate: bool, inspect_available: bool) -> ElementProbe {
    ElementProbe {
        name: name.into(),
        available: inspect_available && command_success("gst-inspect-1.0", ["--exists", name]),
        hardware_candidate,
    }
}

fn command_exists(name: &str) -> bool {
    let Some(paths) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&paths).any(|path| path.join(name).is_file())
}

fn command_success<const N: usize>(program: &str, arguments: [&str; N]) -> bool {
    Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn render_nodes() -> Vec<String> {
    let Ok(entries) = fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    let mut nodes: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            name.to_str()
                .filter(|name| name.starts_with("renderD"))
                .map(|_| entry.path().display().to_string())
        })
        .collect();
    nodes.sort();
    nodes
}

fn pid_file_alive(path: &Path) -> bool {
    let Ok(pid_text) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = pid_text.trim().parse::<u32>() else {
        return false;
    };
    let process = PathBuf::from(format!("/proc/{pid}"));
    if !process.exists() {
        return false;
    }
    let start_path = path.with_extension("pid.start");
    let Ok(expected) = fs::read_to_string(start_path) else {
        return true;
    };
    let Ok(stat) = fs::read_to_string(process.join("stat")) else {
        return false;
    };
    parse_starttime(&stat) == Some(expected.trim())
}

fn is_socket(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    // Create the full path with correct permissions. Parents are created
    // with create_dir_all, then the runtime root is restricted to 0o700.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        // Restrict the runtime root (parent) to owner-only if possible.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    match nix::unistd::mkdir(path, nix::sys::stat::Mode::S_IRWXU) {
        Ok(()) => Ok(()),
        Err(nix::Error::EEXIST) => {
            // Already exists; ensure it has correct permissions.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
            Ok(())
        }
        Err(error) => Err(io::Error::from(error)),
    }
}

fn create_exclusive_dir(path: &Path) -> io::Result<()> {
    // Atomically create the directory; fails if it already exists.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        // Restrict the runtime root (parent) to owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
    }
    nix::unistd::mkdir(path, nix::sys::stat::Mode::S_IRWXU).map_err(io::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> SessionManifest {
        SessionManifest {
            schema_version: SCHEMA_VERSION,
            id: "phone-notes".into(),
            source: SessionSource::LaunchedApp {
                command: vec!["/usr/bin/example".into()],
            },
            viewport: Viewport {
                width: 390,
                height: 844,
                scale: 1.0,
                refresh_hz: 60,
            },
            permissions: Permissions::default(),
            media: MediaPreferences::default(),
        }
    }

    #[test]
    fn headless_config_keeps_parent_output_disabled_on_resize() {
        let viewport = valid_manifest().viewport;
        let config = render_output_config(&viewport, true);
        assert!(config.contains("output = \"ENZREMOTE\""));
        assert!(config.contains("output = \"WAYLAND-1\", disabled = true"));
        assert_eq!(
            render_output_config(&viewport, false),
            render_hyprland_config(&viewport)
        );
    }

    #[test]
    fn fractional_scale_is_staged_after_physical_mode() {
        let viewport = Viewport {
            width: 1078,
            height: 2046,
            scale: 2.75,
            refresh_hz: 60,
        };
        assert_eq!(
            viewport_stages(false, viewport),
            vec![
                Viewport {
                    scale: 1.0,
                    ..viewport
                },
                viewport
            ]
        );
        assert_eq!(viewport_stages(true, viewport), vec![viewport]);
        assert_eq!(
            viewport_stages(
                false,
                Viewport {
                    scale: 1.0,
                    ..viewport
                }
            )
            .len(),
            1
        );
    }

    #[test]
    fn viewport_reasserts_overridden_mode_and_waits_for_stable_readback() {
        let requests = std::cell::Cell::new(0);
        let matched_reads = std::cell::Cell::new(0);
        await_viewport(
            || {
                let matched = requests.get() > 0;
                if matched {
                    matched_reads.set(matched_reads.get() + 1);
                }
                Ok(matched)
            },
            || {
                requests.set(requests.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(requests.get(), 1);
        assert_eq!(matched_reads.get(), 3);
    }

    #[test]
    fn viewport_retry_budget_does_not_accept_a_permanent_mismatch() {
        let mut requests = 0;
        assert!(
            await_viewport(
                || Ok(false),
                || {
                    requests += 1;
                    Ok(())
                }
            )
            .is_err()
        );
        assert_eq!(requests, 3);
    }

    #[test]
    fn pointer_requires_manifest_permission() {
        let mut manifest = valid_manifest();
        manifest.permissions.input = false;
        assert_eq!(
            validate_pointer(&manifest, 1, 1),
            Err("session manifest does not grant input".into())
        );
    }

    #[test]
    fn pointer_rejects_viewport_edge() {
        let mut manifest = valid_manifest();
        manifest.permissions.input = true;
        assert!(validate_pointer(&manifest, 389, 843).is_ok());
        assert!(validate_pointer(&manifest, 390, 843).is_err());
        assert!(validate_pointer(&manifest, 389, 844).is_err());
    }

    #[test]
    fn input_endpoint_is_session_scoped() {
        let state = SessionState {
            manifest: valid_manifest(),
            status: LifecycleStatus::Active,
            created_at_unix_ms: 0,
            helper: PathBuf::from("/helper"),
            runroot: PathBuf::from("/runtime/session/compositor"),
            buildroot: PathBuf::from("/build"),
            config: PathBuf::from("/runtime/session/hyprland.lua"),
        };
        assert_eq!(
            input_endpoint(&state).socket,
            PathBuf::from("/runtime/session/compositor/eis-broker.sock")
        );
        assert_eq!(input_endpoint(&state).mapping_id, "enzremote-phone-notes");
    }

    #[test]
    fn desktop_mirror_path_traversal_rejected() {
        let manifest = SessionManifest {
            schema_version: SCHEMA_VERSION,
            id: "mirror-path".into(),
            source: SessionSource::DesktopMirror {
                output: Some("../etc/passwd".into()),
            },
            viewport: Viewport {
                width: 1920,
                height: 1080,
                scale: 1.0,
                refresh_hz: 60,
            },
            permissions: Permissions::default(),
            media: MediaPreferences::default(),
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn fit_scale_keeps_base_until_a_window_is_too_large() {
        // 1078x2090 at 2.75 is a 392x760 logical screen.
        assert_eq!(fit_scale(1078, 2090, 2.75, (0, 0)), 2.75);
        assert_eq!(fit_scale(1078, 2090, 2.75, (392, 760)), 2.75);
        // A 900x1200 dialog needs <= 1.198; only whole-pixel quarter scales qualify.
        assert_eq!(fit_scale(1078, 2090, 2.75, (900, 1200)), 1.0);
        // 1080x2160 allows 1.5 (720x1440 logical).
        assert_eq!(fit_scale(1080, 2160, 2.75, (700, 1300)), 1.5);
        // Larger than even 1.0: 0.5 always yields whole pixels.
        assert_eq!(fit_scale(1078, 2090, 2.75, (1500, 900)), 0.5);
        for q in 2..11u32 {
            let scale = fit_scale(1078, 2090, 2.75, (500 + q * 40, 0));
            assert_eq!((1078.0 / f64::from(scale)).fract(), 0.0);
            assert_eq!((2090.0 / f64::from(scale)).fract(), 0.0);
        }
    }

    #[test]
    fn default_permissions_are_restrictive() {
        let perms = Permissions::default();
        assert!(!perms.input);
        assert!(!perms.clipboard);
        assert!(!perms.audio_playback);
        assert!(!perms.microphone);
    }

    #[test]
    fn all_example_manifests_validate() {
        let examples_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        for entry in std::fs::read_dir(&examples_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                let manifest = load_manifest(&path);
                assert!(
                    manifest.is_ok(),
                    "failed to load {}: {}",
                    path.display(),
                    manifest.unwrap_err()
                );
                let manifest = manifest.unwrap();
                let valid = manifest.validate();
                assert!(
                    valid.is_ok(),
                    "failed to validate {}: {}",
                    path.display(),
                    valid.unwrap_err()
                );
            }
        }
    }

    #[test]
    fn pid_file_alive_with_mismatched_starttime() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("mismatch.pid");
        let pid = std::process::id();
        fs::write(&pid_path, pid.to_string()).unwrap();
        let start_path = pid_path.with_extension("pid.start");
        fs::write(&start_path, "0").unwrap();
        assert!(!pid_file_alive(&pid_path));
    }

    #[test]
    fn create_private_dir_existing_fixes_perms() {
        let base = tempfile::tempdir().unwrap();
        let path = base.path().join("exists");
        fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        create_private_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn create_exclusive_dir_fails_on_existing() {
        let base = tempfile::tempdir().unwrap();
        let path = base.path().join("exists");
        fs::create_dir(&path).unwrap();
        let err = create_exclusive_dir(&path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }
}
