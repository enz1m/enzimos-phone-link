mod clipboard_relay;
use crate::audit::{self, AuditEntry, AuditEvent};
use crate::error::SignalingError;
use crate::protocol::{self, ClientMessage, HostMessage};
use crate::sessions::SessionManager;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Semaphore, broadcast};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};

/// WebSocket-level message size limit (128 KiB — generous for JSON signaling).
const WS_MAX_MESSAGE_SIZE: usize = 128 * 1024;

static SHARED_DESKTOP_LAUNCH: Mutex<()> = Mutex::const_new(());

async fn launch_desktop_app(
    state: &Arc<ServerState>,
    device_id: &str,
    session_id: &str,
    app_id: &str,
    target: crate::shared_desktop::DesktopTarget,
    viewport: enzremote_session::Viewport,
) -> HostMessage {
    let _guard = SHARED_DESKTOP_LAUNCH.lock().await;
    let result = async {
        if state.config.max_sessions > 0
            && state.sessions.session_count() >= state.config.max_sessions
        {
            return Err("Maximum session limit reached".to_owned());
        }
        {
            let mut owners = state.session_owners.lock().await;
            if owners.contains_key(session_id) || state.sessions.get_viewport(session_id).is_some()
            {
                return Err("Session already exists".into());
            }
            owners.insert(session_id.into(), device_id.into());
        }
        let result = state
            .sessions
            .create_desktop_app(
                session_id.into(),
                device_id,
                target,
                app_id.into(),
                viewport,
            )
            .await;
        if result.is_err() {
            state.session_owners.lock().await.remove(session_id);
        }
        result
    }
    .await;
    match result {
        Ok((width, height)) => {
            state
                .metrics
                .total_sessions_created
                .fetch_add(1, Ordering::Relaxed);
            state.sessions.touch(session_id);
            audit::log_session_create(
                device_id,
                session_id,
                width,
                height,
                viewport.scale,
                viewport.refresh_hz,
            );
            state.emit_event(serde_json::json!({"event": "session_started", "session_id": session_id}));
            watch_desktop_control(state, session_id).await;
            HostMessage::SessionCreated {
                v: protocol::PROTOCOL_VERSION,
                session_id: session_id.into(),
                width,
                height,
                scale: viewport.scale,
                refresh_hz: viewport.refresh_hz,
            }
        }
        Err(reason) => HostMessage::SessionError {
            v: protocol::PROTOCOL_VERSION,
            session_id: session_id.into(),
            reason,
        },
    }
}

async fn attach_desktop(
    state: &Arc<ServerState>,
    device_id: &str,
    session_id: &str,
    output: Option<&str>,
) -> HostMessage {
    let _guard = SHARED_DESKTOP_LAUNCH.lock().await;
    let result = async {
        if state.config.demo_mode {
            return Err("Desktop control requires a real desktop host".to_owned());
        }
        let target = crate::shared_desktop::DesktopTarget::from_environment()
            .await?
            .ok_or("Desktop control is not configured")?;
        if state.config.max_sessions > 0
            && state.sessions.session_count() >= state.config.max_sessions
        {
            return Err("Maximum session limit reached".into());
        }
        {
            let mut owners = state.session_owners.lock().await;
            if owners.contains_key(session_id) || state.sessions.get_viewport(session_id).is_some()
            {
                return Err("Session already exists".into());
            }
            owners.insert(session_id.into(), device_id.into());
        }
        let result = state
            .sessions
            .create_desktop_mirror(session_id.into(), device_id, target, output)
            .await;
        if result.is_err() {
            state.session_owners.lock().await.remove(session_id);
        }
        result
    }
    .await;
    match result {
        Ok(viewport) => {
            watch_desktop_control(state, session_id).await;
            HostMessage::SessionCreated {
                v: protocol::PROTOCOL_VERSION,
                session_id: session_id.into(),
                width: viewport.width,
                height: viewport.height,
                scale: viewport.scale,
                refresh_hz: viewport.refresh_hz,
            }
        }
        Err(reason) => HostMessage::SessionError {
            v: protocol::PROTOCOL_VERSION,
            session_id: session_id.into(),
            reason,
        },
    }
}

// PipeWire can keep producing black frames after a lease ends; media EOF is not its lifecycle signal.
async fn watch_desktop_control(state: &Arc<ServerState>, id: &str) {
    let Some(desktop) = state.sessions.desktop_session(id) else {
        return;
    };
    let Some(generation) = desktop.lock().await.claim_watch() else {
        return;
    };
    let interval = desktop.lock().await.watch_interval();
    let mut shutdown = state.shutdown_tx.subscribe();
    let state = Arc::downgrade(state);
    let id = id.to_owned();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _=shutdown.recv()=>return,
                _=tokio::time::sleep(interval)=>{}
            }
            let Some(state) = state.upgrade() else { return };
            let mut desktop = desktop.lock().await;
            if desktop.watch_id().as_deref() != Some(generation.as_str()) {
                return;
            }
            if desktop.is_active().await {
                continue;
            }
            state
                .sessions
                .set_negotiation_state(&id, crate::sessions::NegotiationState::Idle);
            state.broadcast_event(HostMessage::SessionEvent {
                v: protocol::PROTOCOL_VERSION,
                session_id: id.clone(),
                event: protocol::SessionEventKind::StoppedExternally,
                detail: None,
            });
            let _ = state.webrtc_bridge.stop_stream(&id).await;
            let _ = desktop.release().await;
            return;
        }
    });
}

/// How long an app may be missing (no process, no window) before its private
/// session ends. Covers launchers that hand off to a child that maps later.
const APP_GONE_GRACE: std::time::Duration = std::time::Duration::from_secs(3);
/// Exits within this time after launch are reported as failed starts.
const APP_START_WINDOW: std::time::Duration = std::time::Duration::from_secs(20);

/// Ends a private session whose app exited, crashed or never started, so the
/// phone shows why instead of an empty compositor (and Home lists no ghost).
/// While the app runs it also zooms the screen out for dialogs larger than it.
fn watch_private_app(state: &Arc<ServerState>, id: &str) {
    if state.config.demo_mode || state.sessions.desktop_session(id).is_some() {
        return;
    }
    if !state
        .watched_apps
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.to_owned())
    {
        return;
    }
    let mut shutdown = state.shutdown_tx.subscribe();
    let weak = Arc::downgrade(state);
    let id = id.to_owned();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut missing_since: Option<std::time::Instant> = None;
        let outcome = loop {
            let interval = if started.elapsed() < APP_START_WINDOW {
                std::time::Duration::from_millis(250)
            } else {
                std::time::Duration::from_millis(500)
            };
            tokio::select! {
                _ = shutdown.recv() => break None,
                _ = tokio::time::sleep(interval) => {}
            }
            let Some(state) = weak.upgrade() else { break None };
            if state.sessions.negotiation_state(&id).is_none() {
                break None;
            }
            let Ok(inspection) = state.sessions.inspect(id.clone()).await else {
                break None;
            };
            if inspection.app_alive {
                missing_since = None;
                let fit_id = id.clone();
                if let Ok(Ok(Some(scale))) =
                    tokio::task::spawn_blocking(move || enzremote_session::fit_windows(&fit_id)).await
                {
                    eprintln!("session {id}: screen scale {scale} to fit its windows");
                }
                continue;
            }
            let since = *missing_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= APP_GONE_GRACE {
                break Some((state, inspection.compositor_alive));
            }
        };
        let Some((state, compositor_alive)) = outcome else {
            if let Some(state) = weak.upgrade() {
                state.watched_apps.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            }
            return;
        };
        let failed_start = started.elapsed() < APP_START_WINDOW + APP_GONE_GRACE;
        let reason_id = id.clone();
        let reason = tokio::task::spawn_blocking(move || enzremote_session::app_exit_reason(&reason_id))
            .await
            .ok()
            .flatten();
        eprintln!(
            "session {id}: app is gone ({}){}",
            if failed_start { "failed to start" } else { "exited" },
            reason.as_deref().map(|r| format!(": {r}")).unwrap_or_default()
        );
        let _ = state.webrtc_bridge.stop_stream(&id).await;
        let _ = state.sessions.stop(id.clone()).await;
        state.session_owners.lock().await.remove(&id);
        state.session_apps.lock().await.remove(&id);
        state.watched_apps.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        state.broadcast_event(HostMessage::SessionEvent {
            v: protocol::PROTOCOL_VERSION,
            session_id: id,
            event: if compositor_alive {
                protocol::SessionEventKind::AppExited
            } else {
                protocol::SessionEventKind::CompositorCrashed
            },
            detail: if failed_start { reason } else { None },
        });
    });
}

/// 24 hex characters from the OS random source (transfer IDs).
fn random_id() -> String {
    let mut bytes = [0u8; 12];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes))
        .is_err()
    {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        bytes.copy_from_slice(&nanos.to_le_bytes()[..12]);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(WS_MAX_MESSAGE_SIZE))
        .max_frame_size(Some(WS_MAX_MESSAGE_SIZE))
}

/// Compute encoder-friendly capture dimensions from the client's viewport.
///
/// Returns `(width, height)` where both are multiples of 16 (H.264 macroblock
/// alignment) and the aspect ratio matches the client as closely as possible.
/// The capture resolution is scaled down to keep grim capture + encoding fast
/// (~400-500px wide for portrait, ~800-960px wide for landscape).
fn encoder_friendly_viewport(client_w: u32, client_h: u32) -> (u32, u32) {
    if client_w == 0 || client_h == 0 {
        return (544, 1200);
    }

    // Target the shorter dimension to ~540px (portrait) or ~672px (landscape).
    // 540x1200 is a good balance: ~7 Mbps at 30fps, ~24% CPU with openh264.
    let is_portrait = client_h > client_w;
    let target_short = if is_portrait { 544u32 } else { 672 };

    // Scale factor to bring the shorter dimension to the target.
    let (short, long) = if is_portrait {
        (client_w, client_h)
    } else {
        (client_h, client_w)
    };
    let scale = target_short as f64 / short as f64;
    let scaled_short = (short as f64 * scale).round() as u32;
    let scaled_long = (long as f64 * scale).round() as u32;

    // Round both to the nearest multiple of 16.
    let aligned_short = ((scaled_short + 8) / 16) * 16;
    let aligned_long = ((scaled_long + 8) / 16) * 16;

    // Clamp to valid range (240-4096).
    let aligned_short = aligned_short.clamp(240, 4096);
    let aligned_long = aligned_long.clamp(240, 4096);

    if is_portrait {
        (aligned_short, aligned_long)
    } else {
        (aligned_long, aligned_short)
    }
}

/// Server configuration.
#[derive(Clone)]
pub struct ServerConfig {
    /// Address to bind the WebSocket server.
    pub bind_addr: SocketAddr,
    /// Maximum number of concurrent connections.
    pub max_connections: usize,
    /// Maximum messages per second per connection.
    pub rate_limit_per_second: u32,
    /// Ping interval for keepalive (0 = disabled).
    pub ping_interval_secs: u64,
    /// Session idle timeout in seconds (0 = disabled).
    pub session_timeout_secs: u64,
    /// WebSocket connection idle timeout in seconds (0 = disabled).
    /// Connections that don't send any messages within this period are closed.
    pub connection_timeout_secs: u64,
    /// Maximum number of active sessions (0 = unlimited).
    pub max_sessions: usize,
    /// Session garbage collection interval in seconds (0 = use default 300).
    /// Controls how frequently stale sessions are checked and removed.
    /// Only takes effect when `session_timeout_secs > 0`.
    pub gc_interval_secs: u64,
    /// Required auth token. When non-empty, the Hello message's `token`
    /// field must match exactly. Empty string disables token validation.
    pub auth_token: String,
    /// Demo mode: session_request registers metadata only (no compositor
    /// launch). Allows end-to-end streaming tests with `videotestsrc`.
    pub demo_mode: bool,
    /// Require an active unlocked local graphical login.
    pub require_active_session: bool,
    /// Accept legacy client-supplied commands in an explicitly configured development host.
    pub allow_legacy_commands: bool,
    /// Require a paired device credential (or `auth_token`) for every hello.
    pub require_pairing: bool,
    /// With `require_pairing`, still admit unauthenticated loopback peers
    /// (developer tooling only; `adb reverse` also appears as loopback).
    pub allow_anonymous_loopback: bool,
    /// Accept interactive `pair_request` approvals from unpaired phones.
    pub pairing_requests: bool,
    /// Accept plaintext WebSocket from non-loopback peers while TLS is on.
    pub allow_lan_plaintext: bool,
    /// Optional TLS configuration.
    #[cfg(feature = "tls")]
    pub tls: Option<crate::tls::TlsConfig>,
}

/// Manual `Debug` impl that redacts `auth_token` to prevent accidental
/// leakage to logs or crash reports.
impl std::fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerConfig")
            .field("bind_addr", &self.bind_addr)
            .field("max_connections", &self.max_connections)
            .field("rate_limit_per_second", &self.rate_limit_per_second)
            .field("ping_interval_secs", &self.ping_interval_secs)
            .field("session_timeout_secs", &self.session_timeout_secs)
            .field("connection_timeout_secs", &self.connection_timeout_secs)
            .field("max_sessions", &self.max_sessions)
            .field("gc_interval_secs", &self.gc_interval_secs)
            .field(
                "auth_token",
                &if self.auth_token.is_empty() {
                    "(disabled)"
                } else {
                    "(set)"
                },
            )
            .field("demo_mode", &self.demo_mode)
            .field("require_pairing", &self.require_pairing)
            .field("pairing_requests", &self.pairing_requests)
            .finish()
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: ([127, 0, 0, 1], 9470).into(),
            max_connections: 16,
            rate_limit_per_second: 60,
            ping_interval_secs: 30,
            session_timeout_secs: 3600,   // 1 hour default.
            connection_timeout_secs: 300, // 5 minutes default.
            max_sessions: 8,
            gc_interval_secs: 300,     // 5 minutes default.
            auth_token: String::new(), // Empty = no token required.
            demo_mode: false,
            require_active_session: false,
            allow_legacy_commands: false,
            require_pairing: false,
            allow_anonymous_loopback: false,
            pairing_requests: true,
            allow_lan_plaintext: false,
            #[cfg(feature = "tls")]
            tls: None,
        }
    }
}

impl ServerConfig {
    /// Validate configuration values and return errors for invalid settings.
    pub fn validate(&self) -> Result<(), SignalingError> {
        if self.max_connections == 0 {
            return Err(SignalingError::ConfigError(
                "max_connections must be at least 1".into(),
            ));
        }
        if self.rate_limit_per_second == 0 {
            return Err(SignalingError::ConfigError(
                "rate_limit_per_second must be at least 1".into(),
            ));
        }
        if self.connection_timeout_secs > 0
            && self.ping_interval_secs > 0
            && self.connection_timeout_secs <= self.ping_interval_secs
        {
            return Err(SignalingError::ConfigError(format!(
                "connection_timeout_secs ({}) must be greater than ping_interval_secs ({})",
                self.connection_timeout_secs, self.ping_interval_secs
            )));
        }
        if self.gc_interval_secs > 0
            && self.session_timeout_secs > 0
            && self.gc_interval_secs > self.session_timeout_secs
        {
            return Err(SignalingError::ConfigError(format!(
                "gc_interval_secs ({}) should not exceed session_timeout_secs ({})",
                self.gc_interval_secs, self.session_timeout_secs
            )));
        }
        Ok(())
    }
}

/// Server-wide metrics tracked with atomics (lock-free).
///
/// All counters use `Relaxed` ordering — they are advisory statistics,
/// not synchronization primitives.
pub struct ServerMetrics {
    /// Total connections ever accepted (including rejected/failed).
    pub total_connections: AtomicU64,
    /// Total WebSocket messages received across all connections.
    pub total_messages: AtomicU64,
    /// Total authentication failures (wrong token, duplicate device).
    pub total_auth_failures: AtomicU64,
    /// Total rate limit violations.
    pub total_rate_limits: AtomicU64,
    /// Total sessions created.
    pub total_sessions_created: AtomicU64,
    /// Peak concurrent authenticated connections (high-water mark).
    pub peak_connections: AtomicU64,
}

impl ServerMetrics {
    fn new() -> Self {
        Self {
            total_connections: AtomicU64::new(0),
            total_messages: AtomicU64::new(0),
            total_auth_failures: AtomicU64::new(0),
            total_rate_limits: AtomicU64::new(0),
            total_sessions_created: AtomicU64::new(0),
            peak_connections: AtomicU64::new(0),
        }
    }

    /// Update the peak connections high-water mark if `current` exceeds it.
    fn update_peak(&self, current: u64) {
        self.peak_connections.fetch_max(current, Ordering::Relaxed);
    }
}

/// Shared server state.
pub struct ServerState {
    session_access: tokio::sync::watch::Sender<bool>,
    /// Active authenticated connections by device ID.
    connections: Mutex<HashMap<String, ConnectionInfo>>,
    /// Session-to-device binding: maps session_id -> device_id of the controller.
    session_owners: Mutex<HashMap<String, String>>,
    /// Session manager backed by enzremote-session.
    sessions: SessionManager,
    /// Broadcast channel for shutdown signaling.
    shutdown_tx: broadcast::Sender<()>,
    /// Broadcast channel for session events (pushed to all connections).
    session_events: broadcast::Sender<HostMessage>,
    config: ServerConfig,
    /// Server start time for uptime calculation.
    started_at: std::time::Instant,
    /// Semaphore that limits concurrent connections (acquired before
    /// WebSocket handshake, released on disconnect).
    connection_semaphore: Arc<Semaphore>,
    /// Server hostname for probe responses.
    hostname: Option<String>,
    /// Server-wide metrics (atomic, lock-free).
    metrics: ServerMetrics,
    /// Bridge to per-session WebRTC pipeline processes.
    webrtc_bridge: crate::webrtc_bridge::WebRtcBridge,
    /// Pairing token store for QR code-based device pairing.
    pairing: Mutex<crate::pairing::PairingStore>,
    /// Stable host ID and display name.
    identity: crate::identity::HostIdentity,
    /// SHA-256 of the TLS certificate clients pin, when TLS is enabled.
    cert_fingerprint: Option<String>,
    /// Pending interactive pairing approvals.
    approvals: Arc<crate::approval::Approvals>,
    /// Peers whose pairing request was recently declined (anti-spam).
    recent_declines: Mutex<HashMap<std::net::IpAddr, std::time::Instant>>,
    /// Catalog app each launched session belongs to (reported in session lists).
    session_apps: Mutex<HashMap<String, String>>,
    /// Private sessions whose app process is being watched.
    watched_apps: std::sync::Mutex<std::collections::HashSet<String>>,
    /// File transfers with paired phones.
    transfers: crate::share::Transfers,
    /// Desktop clipboard bridge (started by `run`).
    clipboard: std::sync::OnceLock<crate::clipboard::Clipboard>,
    shell_clipboard: bool,
    clipboard_relay: Mutex<clipboard_relay::Relay>,
    /// Mirrored phone notifications (started by `start_phone_features`).
    notifications: std::sync::OnceLock<Arc<crate::phone_notify::PhoneNotifications>>,
    /// Desktop media players (created on first use).
    media: tokio::sync::OnceCell<Option<crate::media::Media>>,
    /// Remote input into the computer's desktop (created on first use).
    desktop_input: std::sync::OnceLock<Arc<crate::desktop_input::DesktopInput>>,
    /// Phones used as extra monitors, by session ID (with their device).
    second_displays: tokio::sync::Mutex<HashMap<String, (String, Arc<crate::second_display::VirtualDisplay>)>>,
    /// Phone screens / cameras streaming to this computer.
    uplinks: crate::uplink::Uplinks,
    sms: crate::sms::Sms,
    /// Host events for settings UIs (`watch` on the control socket).
    events: tokio::sync::broadcast::Sender<serde_json::Value>,
    /// Host-initiated messages for phones from background helpers.
    to_phone: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<(String, HostMessage)>>,
    /// Devices that sent desktop input (the session closes when they leave).
    input_devices: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Battery level, charging, and whether the low-battery alert was shown.
    phone_status: std::sync::Mutex<HashMap<String, (u8, bool, bool)>>,
}

/// Per-connection metadata tracked after successful authentication.
struct ConnectionInfo {
    /// When the WebSocket handshake completed.
    connected_at: std::time::Instant,
    /// Sender side of an eviction channel. When a new connection arrives with
    /// the same `device_id`, the old connection is evicted by sending `true`.
    /// The old connection's message loop watches the receiver and exits.
    evict_tx: tokio::sync::watch::Sender<bool>,
    /// Signalled when this device is unpaired from the host.
    revoked: Arc<tokio::sync::Notify>,
    /// Remote address, for device listings.
    peer: SocketAddr,
    /// Host-initiated messages (file offers, clipboard) for this device.
    outbox: tokio::sync::mpsc::UnboundedSender<HostMessage>,
    /// The device asked for the computer's clipboard (`clipboard_sync`).
    clipboard: Arc<std::sync::atomic::AtomicBool>,
    /// Video codecs the device can decode (`hello` capabilities).
    video_codecs: Vec<String>,
}

impl ServerState {
    /// Create a new server state from the given configuration.
    pub fn new(config: ServerConfig) -> Arc<Self> {
        let (shutdown_tx, _) = broadcast::channel(1);
        let session_access = tokio::sync::watch::channel(!config.require_active_session).0;
        let (session_events_tx, _) = broadcast::channel(64);
        let sem = Arc::new(Semaphore::new(config.max_connections));

        // Find the WebRTC server binary.
        let webrtc_binary =
            crate::webrtc_bridge::WebRtcBridge::find_binary().unwrap_or_else(|| {
                eprintln!(
                    "warning: enzremote-webrtc-server binary not found; streaming will not work"
                );
                std::path::PathBuf::from("enzremote-webrtc-server")
            });
        let socket_dir = std::env::temp_dir().join("enzremote");
        let _ = std::fs::create_dir_all(&socket_dir);

        let data_dir = crate::identity::data_dir();
        let identity = crate::identity::HostIdentity::load(data_dir.as_deref());
        #[cfg(feature = "tls")]
        let cert_fingerprint = config.tls.as_ref().and_then(|tls| {
            crate::tls::certificate_fingerprint(&tls.cert_path)
                .map_err(|e| eprintln!("tls: {e}"))
                .ok()
        });
        #[cfg(not(feature = "tls"))]
        let cert_fingerprint: Option<String> = None;
        // Load pairing tokens from environment and set up persistence.
        let pairing_store = {
            let mut store = crate::pairing::PairingStore::new();

            // Enable device token persistence if a data directory is available.
            if let Some(dir_path) = data_dir.clone() {
                let _ = std::fs::create_dir_all(&dir_path);
                let token_file = dir_path.join("device_tokens");
                let existing = store.device_token_count();
                store = store.with_persistence(token_file.clone());
                let loaded = store.device_token_count() - existing;
                if loaded > 0 {
                    eprintln!(
                        "loaded {loaded} device token(s) from {}",
                        token_file.display()
                    );
                }
            }

            let pairing_count = store.load_from_env("ENZREMOTE_PAIRING_TOKENS");
            if pairing_count > 0 {
                eprintln!("loaded {pairing_count} pairing token(s)");
            }
            store
        };

        Arc::new(Self {
            session_access,
            connections: Mutex::new(HashMap::new()),
            session_owners: Mutex::new(HashMap::new()),
            sessions: SessionManager::new(),
            shutdown_tx,
            session_events: session_events_tx,
            config,
            started_at: std::time::Instant::now(),
            connection_semaphore: sem,
            hostname: hostname::get().ok().and_then(|h| h.into_string().ok()),
            metrics: ServerMetrics::new(),
            webrtc_bridge: crate::webrtc_bridge::WebRtcBridge::new(webrtc_binary, socket_dir),
            pairing: Mutex::new(pairing_store),
            identity,
            cert_fingerprint,
            approvals: Arc::new(crate::approval::Approvals::new(
                crate::approval::PromptMode::from_env(),
            )),
            recent_declines: Mutex::new(HashMap::new()),
            session_apps: Mutex::new(HashMap::new()),
            watched_apps: std::sync::Mutex::new(std::collections::HashSet::new()),
            transfers: crate::share::Transfers::default(),
            clipboard: std::sync::OnceLock::new(),
            shell_clipboard: std::env::var("ENZREMOTE_CLIPBOARD_BACKEND").as_deref() == Ok("shell") && std::env::var("ENZREMOTE_CLIPBOARD").as_deref() != Ok("0"),
            clipboard_relay: Mutex::new(clipboard_relay::Relay::default()),
            notifications: std::sync::OnceLock::new(),
            phone_status: std::sync::Mutex::new(HashMap::new()),
            media: tokio::sync::OnceCell::new(),
            desktop_input: std::sync::OnceLock::new(),
            second_displays: tokio::sync::Mutex::new(HashMap::new()),
            uplinks: crate::uplink::Uplinks::default(),
            sms: crate::sms::Sms::default(),
            events: tokio::sync::broadcast::channel(64).0,
            to_phone: std::sync::OnceLock::new(),
            input_devices: std::sync::Mutex::new(std::collections::HashSet::new()),
        })
    }

    /// Configured listen address.
    pub fn bind_addr(&self) -> SocketAddr {
        self.config.bind_addr
    }

    /// Stable host identity.
    pub fn identity(&self) -> &crate::identity::HostIdentity {
        &self.identity
    }

    /// Pinned TLS certificate fingerprint, if TLS is enabled.
    pub fn cert_fingerprint(&self) -> Option<&str> {
        self.cert_fingerprint.as_deref()
    }

    /// Pending interactive pairing approvals.
    pub fn approvals(&self) -> &Arc<crate::approval::Approvals> {
        &self.approvals
    }

    /// Mint a one-time pairing token for a host-displayed QR code.
    pub async fn mint_pairing_token(&self) -> Result<(String, u64), String> {
        let mut pairing = self.pairing.lock().await;
        let token = pairing.mint_pairing_token()?;
        Ok((token, pairing.token_ttl().as_secs()))
    }

    /// Paired devices with their current connection, if any.
    pub async fn paired_devices(&self) -> Vec<(crate::pairing::DeviceRecord, Option<SocketAddr>)> {
        let devices = self.pairing.lock().await.list_devices();
        let connections = self.connections.lock().await;
        devices
            .into_iter()
            .map(|d| {
                let peer = connections.get(&d.device_id).map(|c| c.peer);
                (d, peer)
            })
            .collect()
    }

    async fn device_name(&self, device_id: &str) -> String {
        let alias = crate::host_settings::device(device_id).alias;
        if !alias.is_empty() {
            return alias;
        }
        self.pairing
            .lock()
            .await
            .list_devices()
            .into_iter()
            .find(|d| d.device_id == device_id)
            .map(|d| d.name)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "your phone".into())
    }

    /// Queue a host-initiated message for a connected device.
    async fn send_to_device(&self, device_id: &str, message: HostMessage) -> bool {
        self.connections
            .lock()
            .await
            .get(device_id)
            .is_some_and(|c| c.outbox.send(message).is_ok())
    }

    /// Offer local files to a connected phone (`send` CLI, "Send to Phone").
    /// `device_id` may be omitted when exactly one phone is connected.
    pub async fn offer_files(
        self: &Arc<Self>,
        device_id: Option<String>,
        paths: Vec<std::path::PathBuf>,
    ) -> Result<(String, Vec<String>), String> {
        let connected: Vec<String> = self.connections.lock().await.keys().cloned().collect();
        let device_id = match device_id {
            Some(id) if connected.contains(&id) => id,
            Some(id) => return Err(format!("{id} is not connected")),
            None => match connected.as_slice() {
                [only] => only.clone(),
                [] => return Err("no phone is connected; open enzimOS Remote on the phone".into()),
                _ => return Err(format!("several phones are connected; choose one with --device ({})", connected.join(", "))),
            },
        };
        let mut offered = Vec::new();
        for path in paths {
            let transfer_id = random_id();
            let state = Arc::clone(self);
            let (id, device) = (transfer_id.clone(), device_id.clone());
            let outgoing = tokio::task::spawn_blocking(move || state.transfers.register_outgoing(&device, &id, &path))
                .await
                .map_err(|e| e.to_string())??;
            let offer = HostMessage::FileOffer {
                v: protocol::PROTOCOL_VERSION,
                transfer_id: transfer_id.clone(),
                name: outgoing.name.clone(),
                size: outgoing.size,
                mime: None,
                sha256: outgoing.sha256,
            };
            if !self.send_to_device(&device_id, offer).await {
                self.transfers.finish_outgoing(&device_id, &transfer_id);
                return Err("the phone disconnected".into());
            }
            offered.push(outgoing.name);
        }
        Ok((self.device_name(&device_id).await, offered))
    }

    /// Share the desktop clipboard with phones that opted in.
    pub fn start_clipboard(self: &Arc<Self>) {
        if self.shell_clipboard || std::env::var("ENZREMOTE_CLIPBOARD").as_deref() == Ok("0") {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        match crate::clipboard::start(move |text| {
            let _ = tx.send(text);
        }) {
            Ok(bridge) => {
                let _ = self.clipboard.set(bridge);
            }
            Err(error) => {
                eprintln!("clipboard: sharing unavailable: {error}");
                return;
            }
        }
        let state = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                let Some(state) = state.upgrade() else { return };
                if !*state.session_access.borrow() || text.len() > protocol::MAX_CLIPBOARD_TEXT {
                    continue;
                }
                for (device_id, info) in state.connections.lock().await.iter() {
                    if info.clipboard.load(Ordering::Relaxed)
                        && crate::host_settings::allowed(device_id, crate::host_settings::Feature::Clipboard)
                    {
                        let _ = info.outbox.send(HostMessage::Clipboard { v: protocol::PROTOCOL_VERSION, text: text.clone() });
                    }
                }
            }
        });
    }

    /// Desktop notifications for phone notifications, plus the channel that
    /// carries their actions and dismissals back to the phones.
    pub fn start_phone_features(self: &Arc<Self>) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, HostMessage)>();
        let _ = self.to_phone.set(tx.clone());
        let state = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some((device_id, message)) = rx.recv().await {
                let Some(state) = state.upgrade() else { return };
                state.send_to_device(&device_id, message).await;
            }
        });
        let state = Arc::downgrade(self);
        tokio::spawn(async move {
            while state.strong_count() > 0 {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    crate::phone_notify::PhoneNotifications::start(tx.clone()),
                )
                .await
                {
                    Ok(Ok(notifications)) => {
                        if let Some(state) = state.upgrade() {
                            let _ = state.notifications.set(notifications);
                        }
                        return;
                    }
                    _ => tokio::time::sleep(std::time::Duration::from_secs(3)).await,
                }
            }
        });
    }

    /// Battery level and charging state of a connected phone.
    pub fn phone_battery(&self, device_id: &str) -> Option<(u8, bool)> {
        self.phone_status.lock().unwrap_or_else(|e| e.into_inner()).get(device_id).map(|s| (s.0, s.1))
    }

    /// Ring (or stop ringing) one phone, or every connected phone.
    pub async fn ring(&self, device_id: Option<String>, ringing: bool) -> Result<Vec<String>, String> {
        let targets: Vec<String> = match device_id {
            Some(id) => vec![id],
            None => self.connections.lock().await.keys().cloned().collect(),
        };
        let mut rung = Vec::new();
        for id in targets {
            if self.send_to_device(&id, HostMessage::Ring { v: protocol::PROTOCOL_VERSION, ringing }).await {
                rung.push(self.device_name(&id).await);
            }
        }
        if rung.is_empty() {
            return Err("no phone is connected; it rings only while enzimOS Remote is running".into());
        }
        Ok(rung)
    }

    /// Phones sharing texts that this computer allows to.
    fn sms_sharing(&self) -> Vec<(String, u64)> {
        self.sms
            .devices()
            .into_iter()
            .filter(|(id, _)| crate::host_settings::allowed(id, crate::host_settings::Feature::Sms))
            .collect()
    }

    /// Phones sharing text messages: (device ID, name, revision).
    pub async fn sms_devices(&self) -> Vec<(String, String, u64)> {
        let mut list = Vec::new();
        for (id, revision) in self.sms_sharing() {
            list.push((id.clone(), self.device_name(&id).await, revision));
        }
        list
    }

    /// Forward a text-message request to a phone (the only sharing phone
    /// when `device_id` is empty) and return its answer.
    pub async fn sms_request(
        &self,
        device_id: Option<String>,
        request: serde_json::Value,
    ) -> Result<(String, serde_json::Value), String> {
        let device_id = match device_id {
            Some(id) => id,
            None => {
                let devices = self.sms_sharing();
                match devices.as_slice() {
                    [(id, _)] => id.clone(),
                    [] => return Err("no connected phone shares text messages".into()),
                    _ => return Err("several phones share text messages; choose one with device_id".into()),
                }
            }
        };
        if !crate::host_settings::allowed(&device_id, crate::host_settings::Feature::Sms) {
            return Err("text messages are turned off for that phone on this computer".into());
        }
        let to_phone = self.to_phone.get().ok_or("phone features are not running")?;
        let result = self.sms.request(&device_id, request, to_phone).await?;
        Ok((device_id, result))
    }

    /// Revoke a device's credential and disconnect it.
    pub async fn unpair_device(&self, device_id: &str) -> Result<bool, String> {
        let removed = self.pairing.lock().await.revoke_device(device_id)?;
        if removed {
            if let Some(info) = self.connections.lock().await.get(device_id) {
                info.revoked.notify_one();
            }
            AuditEntry::new(AuditEvent::Disconnect)
                .device_id(device_id)
                .kv("reason", "unpaired")
                .emit();
            crate::host_settings::forget_device(device_id);
            self.emit_event(serde_json::json!({"event": "device_unpaired", "device_id": device_id}));
        }
        Ok(removed)
    }

    /// Withdraw a phone's active media and input while retaining its apps and pairing.
    pub async fn pause_device_media(&self, device_id: &str) {
        self.uplinks.drop_device(device_id).await;
        let displays: Vec<_> = {
            let mut displays = self.second_displays.lock().await;
            let ids: Vec<_> = displays.iter().filter(|(_, (owner, _))| owner == device_id).map(|(id, _)| id.clone()).collect();
            ids.into_iter().filter_map(|id| displays.remove(&id).map(|(_, display)| (id, display))).collect()
        };
        for (id, display) in displays {
            let _ = self.webrtc_bridge.stop_stream(&id).await;
            display.stop().await;
            self.sessions.remove(&id);
        }
        if self.input_devices.lock().unwrap_or_else(|error| error.into_inner()).remove(device_id)
            && let Some(input) = self.desktop_input.get() { input.close().await; }
        let owned: Vec<_> = self.session_owners.lock().await.iter().filter(|(_, owner)| owner.as_str() == device_id).map(|(id, _)| id.clone()).collect();
        for id in owned {
            let _ = self.webrtc_bridge.stop_stream(&id).await;
            if self.sessions.is_desktop(&id) { let _ = self.sessions.stop_stream_only(id.clone()).await; }
            self.send_to_device(device_id, HostMessage::StreamStopped { v: protocol::PROTOCOL_VERSION, session_id: id, reason: Some("Phone access changed".into()) }).await;
        }
    }

    /// Publish an event to `watch` subscribers (dropped when nobody listens).
    pub fn emit_event(&self, event: serde_json::Value) {
        let _ = self.events.send(event);
    }

    pub fn subscribe_events(&self) -> tokio::sync::broadcast::Receiver<serde_json::Value> {
        self.events.subscribe()
    }

    /// App sessions phones opened here: (session, app id, owner name, uptime s, streaming).
    pub async fn phone_sessions(&self) -> Vec<serde_json::Value> {
        let sessions = self.sessions.list().await;
        let displays: Vec<String> = self.second_displays.lock().await.keys().cloned().collect();
        let apps = self.session_apps.lock().await.clone();
        let owners = self.session_owners.lock().await.clone();
        let names: HashMap<String, String> = tokio::task::spawn_blocking(|| {
            crate::launcher::catalog().apps.into_iter().map(|a| (a.id, a.name)).collect()
        })
        .await
        .unwrap_or_default();
        let mut list = Vec::new();
        for session in sessions.into_iter().filter(|s| !displays.contains(&s.session_id)) {
            let owner = owners.get(&session.session_id).cloned();
            // Launched sessions are named `<app id>-<suffix>`.
            let app_id = apps.get(&session.session_id).cloned().or_else(|| {
                session.session_id.rsplit_once('-').map(|(id, _)| id.to_string()).filter(|id| names.contains_key(id))
            });
            let owner_name = match &owner {
                Some(id) => Some(self.device_name(id).await),
                None => None,
            };
            list.push(serde_json::json!({
                "session_id": session.session_id,
                "app_name": app_id.as_ref().and_then(|id| names.get(id)),
                "app_id": app_id,
                "device_id": owner,
                "device_name": owner_name,
                "uptime_secs": session.uptime_secs,
                "streaming": self.webrtc_bridge.has_stream(&session.session_id).await,
            }));
        }
        list
    }

    /// Close a phone's app session from this computer (unsaved work in it is lost).
    pub async fn stop_phone_session(self: &Arc<Self>, session_id: &str) -> Result<(), String> {
        let owner = self.session_owners.lock().await.get(session_id).cloned().ok_or("no such app session")?;
        let message = ClientMessage::SessionStop { v: protocol::PROTOCOL_VERSION, session_id: session_id.to_string() };
        match handle_client_message(self, &owner, &message).await {
            HostMessage::SessionStopped { .. } => {
                self.emit_event(serde_json::json!({"event": "session_stopped", "session_id": session_id}));
                Ok(())
            }
            HostMessage::SessionError { reason, .. } => Err(reason),
            _ => Err("could not stop the session".into()),
        }
    }

    /// Whether a connected device said it decodes `codec` (hello capabilities).
    pub async fn device_decodes(&self, device_id: &str, codec: &str) -> bool {
        self.connections
            .lock()
            .await
            .get(device_id)
            .is_some_and(|info| info.video_codecs.iter().any(|c| c.eq_ignore_ascii_case(codec)))
    }

    /// Whether a paired device is connected now.
    pub async fn device_connected(&self, device_id: &str) -> bool {
        self.connections.lock().await.contains_key(device_id)
    }

    /// Whether every hello must present a paired credential.
    fn auth_required(&self, peer: SocketAddr) -> bool {
        if !self.config.auth_token.is_empty() {
            return true;
        }
        self.config.require_pairing
            && !(self.config.allow_anonymous_loopback && peer.ip().is_loopback())
    }

    /// Return the number of currently authenticated connections.
    pub async fn connection_count(&self) -> usize {
        self.connections.lock().await.len()
    }

    /// Return a reference to the server-wide metrics.
    pub fn metrics(&self) -> &ServerMetrics {
        &self.metrics
    }

    /// Signal all connections to shut down gracefully.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(());
    }

    /// Broadcast a session event to all connected clients.
    pub fn broadcast_event(&self, event: HostMessage) {
        // It's OK if no one is listening (returns Err with the value).
        let _ = self.session_events.send(event);
    }
}

/// Run the signaling server.
pub async fn run(state: Arc<ServerState>) -> Result<(), SignalingError> {
    if state.config.require_active_session {
        let weak = Arc::downgrade(&state);
        tokio::spawn(async move {
            let gate = crate::session_access::Gate::open().await;
            loop {
                let Some(state) = weak.upgrade() else { return };
                let allowed = match &gate { Some(gate) => gate.allowed().await, None => false };
                let previous = *state.session_access.borrow();
                if previous != allowed { state.session_access.send_replace(allowed); }
                if previous && !allowed { state.webrtc_bridge.stop_all().await; }
                drop(state);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        });
    }

    // Use SO_REUSEADDR to avoid "Address already in use" errors from
    // TIME_WAIT sockets after rapid restart cycles.
    let socket = tokio::net::TcpSocket::new_v4()
        .map_err(|e| SignalingError::BindError(format!("socket creation failed: {e}")))?;
    socket
        .set_reuseaddr(true)
        .map_err(|e| SignalingError::BindError(format!("set_reuseaddr failed: {e}")))?;
    socket
        .bind(state.config.bind_addr)
        .map_err(|e| SignalingError::BindError(format!("{}: {e}", state.config.bind_addr)))?;
    let listener = socket
        .listen(128)
        .map_err(|e| SignalingError::BindError(format!("listen failed: {e}")))?;

    #[cfg(feature = "tls")]
    let tls_acceptor = match &state.config.tls {
        Some(tls_config) => {
            let acceptor = crate::tls::load_acceptor(tls_config)
                .await
                .map_err(SignalingError::ConfigError)?;
            eprintln!(
                "signaling server v{} listening on {} (TLS)",
                env!("CARGO_PKG_VERSION"),
                state.config.bind_addr
            );
            Some(acceptor)
        }
        None => {
            eprintln!(
                "signaling server v{} listening on {} (plaintext)",
                env!("CARGO_PKG_VERSION"),
                state.config.bind_addr
            );
            None
        }
    };

    #[cfg(not(feature = "tls"))]
    eprintln!(
        "signaling server v{} listening on {}",
        env!("CARGO_PKG_VERSION"),
        state.config.bind_addr
    );

    // Compute effective GC interval before logging.
    let gc_secs = if state.config.gc_interval_secs > 0 {
        state.config.gc_interval_secs
    } else {
        300 // default fallback
    };

    let encoder_setting = std::env::var("ENZREMOTE_ENCODER").unwrap_or_else(|_| "auto".into());
    eprintln!(
        "config: max_connections={} max_sessions={} rate_limit={}/s session_timeout={}s connection_timeout={}s ping_interval={}s gc_interval={}s auth={} demo={} encoder={}",
        state.config.max_connections,
        state.config.max_sessions,
        state.config.rate_limit_per_second,
        state.config.session_timeout_secs,
        state.config.connection_timeout_secs,
        state.config.ping_interval_secs,
        gc_secs,
        if !state.config.auth_token.is_empty() {
            "static-token"
        } else if state.config.require_pairing && state.config.allow_anonymous_loopback {
            "paired-devices+anonymous-loopback"
        } else if state.config.require_pairing {
            "paired-devices"
        } else {
            "disabled"
        },
        if state.config.demo_mode {
            "enabled"
        } else {
            "disabled"
        },
        encoder_setting,
    );

    let mut shutdown = state.shutdown_tx.subscribe();

    // Session GC runs periodically if a timeout is configured.
    let gc_interval = if state.config.session_timeout_secs > 0 {
        Some(tokio::time::interval(std::time::Duration::from_secs(
            gc_secs,
        )))
    } else {
        None
    };
    tokio::pin!(gc_interval);

    loop {
        tokio::select! {
            accept = listener.accept() => {
                let (stream, peer) = match accept {
                    Ok(conn) => conn,
                    Err(e) => {
                        eprintln!("accept error (will retry): {e}");
                        // Brief sleep to avoid busy-looping on persistent
                        // errors like fd exhaustion.
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };

                state.metrics.total_connections.fetch_add(1, Ordering::Relaxed);

                // Acquire a connection slot before the handshake. The permit
                // is held for the entire connection lifetime and released on
                // disconnect, preventing the TOCTOU gap of count-then-spawn.
                let permit = match state.connection_semaphore.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        audit::log_connection_rejected(peer);
                        drop(stream);
                        continue;
                    }
                };

                let state = Arc::clone(&state);

                #[cfg(feature = "tls")]
                let tls_acceptor = tls_acceptor.clone();

                tokio::spawn(async move {
                    let result = {
                        #[cfg(feature = "tls")]
                        {
                            // One port serves both: TLS for phones, plaintext
                            // only for loopback tooling (status, adb reverse)
                            // unless LAN plaintext is explicitly allowed.
                            let wants_tls = tls_acceptor.is_some() && {
                                let mut first = [0u8; 1];
                                matches!(
                                    tokio::time::timeout(std::time::Duration::from_secs(5), stream.peek(&mut first)).await,
                                    Ok(Ok(1))
                                ) && first[0] == 0x16
                            };
                            if tls_acceptor.is_some()
                                && !wants_tls
                                && !peer.ip().is_loopback()
                                && !state.config.allow_lan_plaintext
                            {
                                audit::log_connection_rejected(peer);
                                Err(SignalingError::ConnectionError(
                                    "plaintext connection refused; use TLS".into(),
                                ))
                            } else if let (true, Some(acceptor)) = (wants_tls, tls_acceptor.as_ref()) {
                                match acceptor.accept(stream).await {
                                    Ok(tls_stream) => {
                                        handle_connection_tls(state, tls_stream, peer).await
                                    }
                                    Err(e) => Err(SignalingError::ConnectionError(
                                        format!("TLS handshake failed: {e}"),
                                    )),
                                }
                            } else {
                                handle_connection(state, stream, peer).await
                            }
                        }
                        #[cfg(not(feature = "tls"))]
                        {
                            handle_connection(state, stream, peer).await
                        }
                    };
                    // The permit is dropped here, releasing the connection slot.
                    drop(permit);
                    if let Err(error) = result {
                        AuditEntry::new(AuditEvent::Disconnect)
                            .peer(peer)
                            .kv("reason", &error.to_string())
                            .emit();
                    }
                });
            }
            // Session garbage collection timer.
            _ = async {
                if let Some(ref mut interval) = *gc_interval {
                    interval.tick().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                let timeout = std::time::Duration::from_secs(state.config.session_timeout_secs);
                let removed = state.sessions.gc_stale(timeout).await;
                if !removed.is_empty() {
                    // Clean up session ownership and WebRTC bridges for GC'd sessions.
                    let mut owners = state.session_owners.lock().await;
                    for id in &removed {
                        owners.remove(id);
                        // Stop the associated WebRTC bridge process (best-effort).
                        let _ = state.webrtc_bridge.stop_stream(id).await;
                        // Notify clients about the GC.
                        state.broadcast_event(HostMessage::SessionEvent {
                            v: protocol::PROTOCOL_VERSION,
                            session_id: id.clone(),
                            event: protocol::SessionEventKind::GarbageCollected,
                            detail: None,
                        });
                    }
                    audit::log_session_gc(&removed);
                }
            }
            _ = shutdown.recv() => {
                eprintln!("signaling server shutting down");
                break;
            }
        }
    }

    // Clean up all WebRTC bridge processes before exiting.
    let count = state.webrtc_bridge.active_count().await;
    if count > 0 {
        eprintln!("stopping {count} WebRTC pipeline process(es)...");
        state.webrtc_bridge.stop_all().await;
    }

    Ok(())
}

#[cfg(feature = "tls")]
async fn handle_connection_tls(
    state: Arc<ServerState>,
    stream: tokio_rustls::server::TlsStream<TcpStream>,
    peer: SocketAddr,
) -> Result<(), SignalingError> {
    let ws = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config()))
        .await
        .map_err(|e| SignalingError::ConnectionError(format!("WebSocket handshake failed: {e}")))?;

    handle_ws(state, ws, peer, true).await
}

async fn handle_connection(
    state: Arc<ServerState>,
    stream: TcpStream,
    peer: SocketAddr,
) -> Result<(), SignalingError> {
    let ws = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config()))
        .await
        .map_err(|e| SignalingError::ConnectionError(format!("WebSocket handshake failed: {e}")))?;

    handle_ws(state, ws, peer, false).await
}

async fn handle_ws<S>(
    state: Arc<ServerState>,
    ws: tokio_tungstenite::WebSocketStream<S>,
    peer: SocketAddr,
    tls: bool,
) -> Result<(), SignalingError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if !*state.session_access.borrow() {
        return Err(SignalingError::AuthenticationFailed("Unlock the computer to connect a phone".into()));
    }
    let (mut sink, mut source) = ws.split();
    audit::log_connect(peer);

    // Expect a Hello message first.
    let hello_deadline = tokio::time::sleep(std::time::Duration::from_secs(5));
    tokio::pin!(hello_deadline);

    let (device_id, (evict_rx, revoked, outbox_rx)) = loop {
        tokio::select! {
            message = source.next() => {
                let Some(message) = message else {
                    return Err(SignalingError::ConnectionError(
                        "connection closed before hello".into(),
                    ));
                };
                let message = message.map_err(|e| {
                    SignalingError::ConnectionError(format!("WebSocket error: {e}"))
                })?;
                match message {
                    Message::Text(text) => {
                        match protocol::parse_client_message(&text) {
                            Ok(ClientMessage::PairRequest {
                                device_id,
                                device_name,
                                client_nonce,
                                pairing_token,
                                ..
                            }) => {
                                handle_pair_request(
                                    &state,
                                    &mut sink,
                                    &mut source,
                                    peer,
                                    &device_id,
                                    &device_name,
                                    &client_nonce,
                                    pairing_token.as_deref(),
                                    tls,
                                )
                                .await?;
                                // Approved: the same connection continues with hello.
                                hello_deadline
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
                                continue;
                            }
                            Ok(ClientMessage::Hello { device_id, token, capabilities, .. }) => {
                                let video_codecs = capabilities.video_codecs.clone();
                                // Note: device_id length (max 128) and character set are
                                // already validated by protocol::parse_client_message().

                                // Authentication priority:
                                // 1. Static ENZREMOTE_AUTH_TOKEN (exact match)
                                // 2. Legacy one-time pairing token in hello
                                // 3. Permanent device credential, bound to its device_id
                                // Without any requirement (development loopback), allow.
                                let mut device_token_for_ack: Option<String> = None;

                                let auth_required = state.auth_required(peer);
                                let token_matches_auth = !state.config.auth_token.is_empty()
                                    && token == state.config.auth_token;

                                if auth_required && !token_matches_auth {
                                    let mut pairing = state.pairing.lock().await;
                                    let pairing_result = pairing.consume_pairing_token(&token);
                                    let reject_reason = match pairing_result {
                                        Ok(Some(new_device_token)) => {
                                            match pairing.register_device(new_device_token.clone(), device_id.clone(), String::new()) {
                                                Ok(()) => { device_token_for_ack = Some(new_device_token); None }
                                                Err(_) => Some("could not save pairing; check the computer storage and retry"),
                                            }
                                        }
                                        Err(e) => {
                                            drop(pairing);
                                            eprintln!("pairing: token generation failed for '{}': {e}", device_id);
                                            let reject = HostMessage::HelloReject {
                                                v: protocol::PROTOCOL_VERSION,
                                                reason: "internal server error during pairing".into(),
                                                code: None,
                                            };
                                            let json = serde_json::to_string(&reject)
                                                .map_err(|e| SignalingError::Serialization(e.to_string()))?;
                                            let _ = sink.send(Message::Text(json.into())).await;
                                            return Err(SignalingError::AuthenticationFailed(
                                                format!("token generation failed for device_id: {device_id}"),
                                            ));
                                        }
                                        Ok(None) => match pairing.device_for_token(&token) {
                                            Some(record) if record.device_id == device_id => None,
                                            Some(_) => Some("credential belongs to another device"),
                                            None => Some("invalid authentication token: this phone is not paired with this computer"),
                                        },
                                    };
                                    drop(pairing);
                                    if let Some(reason) = reject_reason {
                                        let reject = HostMessage::HelloReject {
                                            v: protocol::PROTOCOL_VERSION,
                                            reason: reason.into(),
                                            code: Some("not_paired".into()),
                                        };
                                        let json = serde_json::to_string(&reject)
                                            .map_err(|e| SignalingError::Serialization(e.to_string()))?;
                                        let _ = sink.send(Message::Text(json.into())).await;
                                        state.metrics.total_auth_failures.fetch_add(1, Ordering::Relaxed);
                                        audit::log_auth_reject(&device_id, peer, "invalid_token");
                                        return Err(SignalingError::AuthenticationFailed(
                                            format!("rejected device_id: {device_id}"),
                                        ));
                                    }
                                }
                                // Atomically check for duplicates and register.
                                // If a stale connection exists with the same
                                // device_id, evict it (the old message loop
                                // watches the evict channel and exits).
                                let evict_rx_and_revoked = {
                                    let mut connections = state.connections.lock().await;
                                    if let Some(old) = connections.remove(&device_id) {
                                        // Signal the old connection to exit.
                                        let _ = old.evict_tx.send(true);
                                        eprintln!(
                                            "signaling: evicting stale connection for device_id={device_id}"
                                        );
                                        audit::log_auth_reject(
                                            &device_id,
                                            peer,
                                            "evicted_stale_connection",
                                        );
                                    }
                                    let (evict_tx, evict_rx) =
                                        tokio::sync::watch::channel(false);
                                    // Register immediately to prevent TOCTOU races.
                                    let revoked = Arc::new(tokio::sync::Notify::new());
                                    let (outbox, outbox_rx) = tokio::sync::mpsc::unbounded_channel();
                                    connections.insert(
                                        device_id.clone(),
                                        ConnectionInfo {
                                            connected_at: std::time::Instant::now(),
                                            evict_tx,
                                            revoked: Arc::clone(&revoked),
                                            peer,
                                            outbox,
                                            clipboard: Arc::default(),
                                            video_codecs,
                                        },
                                    );
                                    // Update peak connections high-water mark.
                                    state.metrics.update_peak(connections.len() as u64);
                                    state.emit_event(serde_json::json!({"event": "device_connected", "device_id": device_id}));
                                    (evict_rx, revoked, outbox_rx)
                                };
                                // Accept the connection.
                                let ack = HostMessage::HelloAck {
                                    v: protocol::PROTOCOL_VERSION,
                                    capabilities: protocol::Capabilities {
                                        input: protocol::KNOWN_INPUT_TYPES
                                            .iter()
                                            .map(|s| (*s).into())
                                            .collect(),
                                        video_codecs: vec!["h264".into()],
                                        resize: true,
                                        clipboard: state.clipboard_available(),
                                        audio: false,
                                    },
                                    server_version: Some(env!("CARGO_PKG_VERSION").into()),
                                    device_token: device_token_for_ack.clone(),
                                    host_id: Some(state.identity.id.clone()),
                                    host_name: Some(state.identity.name.clone()),
                                };
                                let json = serde_json::to_string(&ack)
                                    .map_err(|e| SignalingError::Serialization(e.to_string()))?;
                                if let Err(e) = sink.send(Message::Text(json.into())).await {
                                    // HelloAck send failed — clean up.
                                    // If we issued a device token via pairing, revoke it
                                    // so the user can generate a new pairing token and retry.
                                    if let Some(ref dt) = device_token_for_ack {
                                        let mut pairing = state.pairing.lock().await;
                                        if pairing.revoke_device_token(dt).is_err() { eprintln!("pairing: could not persist failed-handshake credential removal"); }
                                        eprintln!(
                                            "pairing: revoked device token for '{}' (HelloAck send failed)",
                                            device_id
                                        );
                                    }
                                    // Remove the connection registration.
                                    state.connections.lock().await.remove(&device_id);
                                    return Err(SignalingError::ConnectionError(
                                        format!("could not send hello ack: {e}"),
                                    ));
                                }
                                break (device_id, evict_rx_and_revoked);
                            }
                            Ok(_) => {
                                let error = HostMessage::Error {
                                    v: protocol::PROTOCOL_VERSION,
                                    reason: "expected hello message first".into(),
                                };
                                let json = serde_json::to_string(&error)
                                    .map_err(|e| SignalingError::Serialization(e.to_string()))?;
                                let _ = sink.send(Message::Text(json.into())).await;
                                return Err(SignalingError::UnexpectedMessage(
                                    "non-hello message before authentication".into(),
                                ));
                            }
                            Err(error) => {
                                let error_msg = HostMessage::Error {
                                    v: protocol::PROTOCOL_VERSION,
                                    reason: error.clone(),
                                };
                                let json = serde_json::to_string(&error_msg)
                                    .map_err(|e| SignalingError::Serialization(e.to_string()))?;
                                let _ = sink.send(Message::Text(json.into())).await;
                                return Err(SignalingError::MalformedMessage(error));
                            }
                        }
                    }
                    Message::Close(_) => return Ok(()),
                    Message::Ping(data) => {
                        let _ = sink.send(Message::Pong(data)).await;
                    }
                    Message::Binary(_) => {
                        let error = HostMessage::Error {
                            v: protocol::PROTOCOL_VERSION,
                            reason: "binary messages are not supported".into(),
                        };
                        let json = serde_json::to_string(&error)
                            .map_err(|e| SignalingError::Serialization(e.to_string()))?;
                        let _ = sink.send(Message::Text(json.into())).await;
                        return Err(SignalingError::UnexpectedMessage(
                            "binary message".into(),
                        ));
                    }
                    _ => continue, // Pong, Frame — ignore.
                }
            }
            _ = &mut hello_deadline => {
                let _ = sink.send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: "hello timeout".into(),
                }))).await;
                return Err(SignalingError::HelloTimeout);
            }
        }
    };

    // Connection was already registered atomically during hello handling.
    audit::log_authenticate(&device_id, peer);

    // Message loop.
    let msg_counter = Arc::new(AtomicU64::new(0));
    let result = message_loop(
        &state,
        &mut sink,
        &mut source,
        &device_id,
        &msg_counter,
        ConnectionEvents { evict_rx, revoked, outbox_rx },
    )
    .await;

    // Cleanup: remove connection and release owned sessions.
    // If we were evicted, the new connection already owns the device_id
    // entry in the connections map — don't remove it.
    let was_evicted = matches!(
        &result,
        Err(SignalingError::ConnectionError(msg)) if msg.contains("evicted")
    );
    let msg_count = msg_counter.load(Ordering::Relaxed);
    let conn_duration = if was_evicted {
        // Don't touch the connections map — the new connection owns it.
        0
    } else {
        let mut connections = state.connections.lock().await;
        let info = connections.remove(&device_id);
        info.map_or(0, |info| info.connected_at.elapsed().as_secs())
    };
    if !was_evicted {
        state.emit_event(serde_json::json!({"event": "device_disconnected", "device_id": device_id}));
        state.transfers.drop_device(&device_id);
        state.uplinks.drop_device(&device_id).await;
        state.sms.drop_device(&device_id);
        state.phone_status.lock().unwrap_or_else(|e| e.into_inner()).remove(&device_id);
        let displays: Vec<(String, Arc<crate::second_display::VirtualDisplay>)> = {
            let mut displays = state.second_displays.lock().await;
            let ids: Vec<String> = displays.iter().filter(|(_, (d, _))| d == &device_id).map(|(id, _)| id.clone()).collect();
            ids.into_iter().filter_map(|id| displays.remove(&id).map(|(_, display)| (id, display))).collect()
        };
        for (id, display) in displays {
            let _ = state.webrtc_bridge.stop_stream(&id).await;
            display.stop().await;
            state.sessions.remove(&id);
        }
        let used_input = state.input_devices.lock().unwrap_or_else(|e| e.into_inner()).remove(&device_id);
        if used_input && let Some(input) = state.desktop_input.get() {
            input.close().await;
        }
        if let Some(notifications) = state.notifications.get() {
            notifications.drop_device(&device_id).await;
        }
        // Stop captures and release borrowed desktop windows on transport loss.
        // Retained private apps remain bound to their authenticated device.
        let owners = state.session_owners.lock().await;
        let mut released = Vec::new();
        for (session_id, owner) in owners.iter() {
            if owner == &device_id { released.push(session_id.clone()); }
        }
        drop(owners);
        if !released.is_empty() {
            for sid in &released {
                let _ = state.webrtc_bridge.stop_stream(sid).await;
                if state.sessions.is_desktop(sid) {
                    let _ = state.sessions.stop_stream_only(sid.clone()).await;
                }
                AuditEntry::new(AuditEvent::StreamStop)
                    .device_id(&device_id)
                    .session_id(sid)
                    .kv("trigger", "disconnect")
                    .emit();
            }
        }
    }
    let reason = match &result {
        Ok(()) => "clean_close".to_string(),
        Err(err) => err.to_string(),
    };
    audit::log_disconnect(&device_id, peer, conn_duration, msg_count, &reason);

    result
}

struct ConnectionEvents {
    evict_rx: tokio::sync::watch::Receiver<bool>,
    revoked: Arc<tokio::sync::Notify>,
    outbox_rx: tokio::sync::mpsc::UnboundedReceiver<HostMessage>,
}

async fn message_loop<S>(
    state: &Arc<ServerState>,
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    source: &mut futures_util::stream::SplitStream<tokio_tungstenite::WebSocketStream<S>>,
    device_id: &str,
    msg_counter: &AtomicU64,
    events: ConnectionEvents,
) -> Result<(), SignalingError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let ConnectionEvents { mut evict_rx, revoked, mut outbox_rx } = events;
    let rate_limit = state.config.rate_limit_per_second;
    let mut messages_this_second: u32 = 0;
    let mut rate_window = tokio::time::Instant::now();
    let mut last_message = tokio::time::Instant::now();
    // Consecutive seconds with rate limit violations.
    // Disconnect if the client keeps flooding for too long.
    let mut consecutive_rate_limit_seconds: u32 = 0;
    const MAX_RATE_LIMIT_SECONDS: u32 = 5;
    // msg_counter is an AtomicU64 shared with handle_ws for disconnect stats.
    let connection_timeout = if state.config.connection_timeout_secs > 0 {
        Some(std::time::Duration::from_secs(
            state.config.connection_timeout_secs,
        ))
    } else {
        None
    };
    let mut event_rx = state.session_events.subscribe();
    let mut session_access = state.session_access.subscribe();

    let ping_interval = if state.config.ping_interval_secs > 0 {
        Some(tokio::time::interval(std::time::Duration::from_secs(
            state.config.ping_interval_secs,
        )))
    } else {
        None
    };
    // Pin the optional interval so we can use it in select!.
    tokio::pin!(ping_interval);

    loop {
        if !*state.session_access.borrow() {
            let _ = sink.send(Message::Close(Some(CloseFrame { code: CloseCode::Policy, reason: "computer locked or session ended".into() }))).await;
            return Ok(());
        }
        let next_message = source.next();
        tokio::pin!(next_message);

        tokio::select! {
            _ = session_access.changed() => { continue; }
            message = &mut next_message => {
                let Some(message) = message else { break };
                let message = match message {
                    Ok(m) => m,
                    Err(e) => return Err(SignalingError::ConnectionError(
                        format!("WebSocket error: {e}"),
                    )),
                };
                last_message = tokio::time::Instant::now();

                // Track message count (atomic; read by handle_ws on disconnect).
                msg_counter.fetch_add(1, Ordering::Relaxed);
                state.metrics.total_messages.fetch_add(1, Ordering::Relaxed);

                // Rate limiting. File transfer pieces are bounded by the
                // offered size instead (and are many per second by design).
                let bulk = matches!(&message, Message::Text(text) if is_transfer_piece(text));
                let now = tokio::time::Instant::now();
                if now.duration_since(rate_window) >= std::time::Duration::from_secs(1) {
                    // New window: check if previous window was abusive.
                    if messages_this_second > rate_limit {
                        consecutive_rate_limit_seconds += 1;
                    } else {
                        consecutive_rate_limit_seconds = 0;
                    }
                    messages_this_second = 0;
                    rate_window = now;
                }
                if !bulk {
                    messages_this_second += 1;
                }
                if messages_this_second > rate_limit {
                    state.metrics.total_rate_limits.fetch_add(1, Ordering::Relaxed);
                    audit::log_rate_limit(device_id);

                    // Disconnect after sustained abuse.
                    if consecutive_rate_limit_seconds >= MAX_RATE_LIMIT_SECONDS {
                        let close_msg = HostMessage::Error {
                            v: protocol::PROTOCOL_VERSION,
                            reason: "rate limit exceeded: disconnected after sustained abuse".into(),
                        };
                        let json = serde_json::to_string(&close_msg)
                            .map_err(|e| SignalingError::Serialization(e.to_string()))?;
                        let _ = sink.send(Message::Text(json.into())).await;
                        return Err(SignalingError::RateLimitExceeded);
                    }

                    let error_msg = HostMessage::Error {
                        v: protocol::PROTOCOL_VERSION,
                        reason: "rate limit exceeded".into(),
                    };
                    let json = serde_json::to_string(&error_msg)
                        .map_err(|e| SignalingError::Serialization(e.to_string()))?;
                    sink.send(Message::Text(json.into()))
                        .await
                        .map_err(|e| SignalingError::ConnectionError(
                            format!("could not send error: {e}"),
                        ))?;
                    continue;
                }

                match message {
                    Message::Text(text) => {
                        let parsed = protocol::parse_client_message(&text);
                        match parsed {
                            Ok(ClientMessage::Unpair { .. }) => {
                                if let Err(reason) = state.pairing.lock().await.revoke_device(device_id) {
                                    send_host_message(sink, &HostMessage::Error { v: protocol::PROTOCOL_VERSION, reason }).await;
                                    continue;
                                }
                                eprintln!("pairing: device '{device_id}' unpaired itself");
                                send_host_message(sink, &HostMessage::Unpaired { v: protocol::PROTOCOL_VERSION }).await;
                                let _ = sink.send(Message::Close(Some(CloseFrame {
                                    code: CloseCode::Normal,
                                    reason: "device unpaired".into(),
                                }))).await;
                                return Ok(());
                            }
                            Ok(client_msg) => {
                                let response = if !*state.session_access.borrow() {
                                    HostMessage::Error { v: protocol::PROTOCOL_VERSION, reason: "Unlock the computer to use Phone Link".into() }
                                } else if matches!(&client_msg, ClientMessage::SessionRequest { .. }) && !state.config.allow_legacy_commands {
                                    HostMessage::Error { v: protocol::PROTOCOL_VERSION, reason: "Choose an installed application; arbitrary remote commands are disabled".into() }
                                } else {
                                    handle_client_message(state, device_id, &client_msg).await
                                };
                                let json = serde_json::to_string(&response)
                                    .map_err(|e| SignalingError::Serialization(e.to_string()))?;
                                sink.send(Message::Text(json.into()))
                                    .await
                                    .map_err(|e| SignalingError::ConnectionError(
                                        format!("could not send response: {e}"),
                                    ))?;
                            }
                            Err(error) => {
                                let error_msg = HostMessage::Error {
                                    v: protocol::PROTOCOL_VERSION,
                                    reason: error,
                                };
                                let json = serde_json::to_string(&error_msg)
                                    .map_err(|e| SignalingError::Serialization(e.to_string()))?;
                                sink.send(Message::Text(json.into()))
                                    .await
                                    .map_err(|e| SignalingError::ConnectionError(
                                        format!("could not send error: {e}"),
                                    ))?;
                            }
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(data) => {
                        let _ = sink.send(Message::Pong(data)).await;
                    }
                    Message::Binary(_) => {
                        let error = HostMessage::Error {
                            v: protocol::PROTOCOL_VERSION,
                            reason: "binary messages are not supported".into(),
                        };
                        let json = serde_json::to_string(&error)
                            .map_err(|e| SignalingError::Serialization(e.to_string()))?;
                        sink.send(Message::Text(json.into()))
                            .await
                            .map_err(|e| SignalingError::ConnectionError(
                                format!("could not send error: {e}"),
                            ))?;
                    }
                    _ => {} // Pong, Frame — ignore.
                }
            }
            // File offers and clipboard changes for this device.
            Some(message) = outbox_rx.recv() => {
                if !send_host_message(sink, &message).await {
                    return Err(SignalingError::ConnectionError("could not send to device".into()));
                }
            }
            // Session events broadcast to all clients.
            event = event_rx.recv() => {
                match event {
                    Ok(event) => {
                        let private_session = match &event {
                            HostMessage::Sdp { session_id, .. } | HostMessage::Ice { session_id, .. }
                            | HostMessage::SessionError { session_id, .. } => Some(session_id),
                            _ => None,
                        };
                        if let Some(session_id) = private_session
                            && state.session_owners.lock().await.get(session_id).map(String::as_str) != Some(device_id)
                        {
                            continue;
                        }
                        let json = serde_json::to_string(&event)
                            .map_err(|e| SignalingError::Serialization(e.to_string()))?;
                        if let Err(e) = sink.send(Message::Text(json.into())).await {
                            return Err(SignalingError::ConnectionError(
                                format!("could not send event: {e}"),
                            ));
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        AuditEntry::new(AuditEvent::Disconnect)
                            .device_id(device_id)
                            .kv("warning", "broadcast_lag")
                            .kv_u64("dropped_events", n)
                            .emit();
                        // Notify the client so it can re-sync state (e.g. re-fetch
                        // the session list). Best-effort — ignore send errors.
                        let warning = serde_json::to_string(
                            &protocol::HostMessage::Error {
                                v: protocol::PROTOCOL_VERSION,
                                reason: format!("{n} event(s) were dropped; re-sync recommended"),
                            },
                        )
                        .unwrap_or_default();
                        let _ = sink.send(Message::Text(warning.into())).await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break; // Server shutting down, broadcast channel closed.
                    }
                }
            }
            // This device was unpaired on the host.
            _ = revoked.notified() => {
                send_host_message(sink, &HostMessage::Unpaired { v: protocol::PROTOCOL_VERSION }).await;
                let _ = sink.send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: "device unpaired".into(),
                }))).await;
                return Ok(());
            }
            // A new connection with the same device_id has evicted us.
            result = evict_rx.changed() => {
                if result.is_ok() && *evict_rx.borrow_and_update() {
                    let _ = sink.send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Away,
                        reason: "evicted by new connection".into(),
                    }))).await;
                    return Err(SignalingError::ConnectionError(
                        "evicted by new connection with same device_id".into(),
                    ));
                }
            }
            _ = async {
                if let Some(ref mut interval) = *ping_interval {
                    interval.tick().await
                } else {
                    // Never completes if pings are disabled.
                    std::future::pending::<tokio::time::Instant>().await
                }
            } => {
                // Check connection idle timeout.
                if let Some(timeout) = connection_timeout
                    && last_message.elapsed() > timeout
                {
                    AuditEntry::new(AuditEvent::Disconnect)
                        .device_id(device_id)
                        .kv("reason", "idle_timeout")
                        .emit();
                    let _ = sink.send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Away,
                        reason: "idle timeout".into(),
                    }))).await;
                    return Err(SignalingError::IdleTimeout);
                }
                // Send a WebSocket ping for keepalive.
                if let Err(e) = sink.send(Message::Ping(Vec::new().into())).await {
                    return Err(SignalingError::ConnectionError(
                        format!("keepalive ping failed: {e}"),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// `file_chunk` / `file_pull`: transfer pieces exempt from message-rate limits.
fn is_transfer_piece(text: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct Tag<'a> {
        #[serde(rename = "type", borrow)]
        kind: &'a str,
    }
    text.len() <= protocol::MAX_MESSAGE_SIZE
        && serde_json::from_str::<Tag<'_>>(text).is_ok_and(|t| matches!(t.kind, "file_chunk" | "file_pull" | "remote_input" | "display_input"))
}

async fn send_host_message<S>(
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    message: &HostMessage,
) -> bool
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match serde_json::to_string(message) {
        Ok(json) => sink.send(Message::Text(json.into())).await.is_ok(),
        Err(_) => false,
    }
}

/// How long a declined peer must wait before requesting again.
const PAIR_DECLINE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

/// Handle an unauthenticated `pair_request`. Returns `Ok(())` once the device
/// holds a new credential (sent in `pair_result`); any other outcome closes
/// the connection with an error.
#[allow(clippy::too_many_arguments)]
async fn handle_pair_request<S>(
    state: &Arc<ServerState>,
    sink: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    source: &mut futures_util::stream::SplitStream<tokio_tungstenite::WebSocketStream<S>>,
    peer: SocketAddr,
    device_id: &str,
    device_name: &str,
    client_nonce: &str,
    pairing_token: Option<&str>,
    tls: bool,
) -> Result<(), SignalingError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let device_name = {
        let name = crate::pairing::sanitize_device_name(device_name);
        if name.is_empty() {
            device_id.to_string()
        } else {
            name
        }
    };
    let refuse = |reason: &str| HostMessage::PairResult {
        v: protocol::PROTOCOL_VERSION,
        accepted: false,
        device_token: None,
        reason: Some(reason.into()),
    };

    // QR flow: a valid host-issued one-time token approves immediately.
    let approved_by_token = match pairing_token {
        Some(token) => match state.pairing.lock().await.consume_pairing_token(token) {
            Ok(Some(_)) => true,
            Ok(None) => {
                send_host_message(
                    sink,
                    &refuse("this pairing code has expired; show a new one on the computer"),
                )
                .await;
                state
                    .metrics
                    .total_auth_failures
                    .fetch_add(1, Ordering::Relaxed);
                audit::log_auth_reject(device_id, peer, "expired_pairing_token");
                return Err(SignalingError::AuthenticationFailed(
                    "expired pairing token".into(),
                ));
            }
            Err(e) => return Err(SignalingError::AuthenticationFailed(e)),
        },
        None => false,
    };

    if !approved_by_token {
        if !state.config.pairing_requests {
            send_host_message(
                sink,
                &refuse("pairing requests are disabled on this computer"),
            )
            .await;
            return Err(SignalingError::AuthenticationFailed(
                "pairing requests disabled".into(),
            ));
        }
        {
            let mut declines = state.recent_declines.lock().await;
            declines.retain(|_, at| at.elapsed() < PAIR_DECLINE_COOLDOWN);
            if declines.contains_key(&peer.ip()) {
                drop(declines);
                send_host_message(
                    sink,
                    &refuse("the computer declined recently; wait a moment and try again"),
                )
                .await;
                return Err(SignalingError::AuthenticationFailed(
                    "pairing cooldown".into(),
                ));
            }
        }
        let host_nonce = crate::identity::random_hex(16);
        // The fingerprint the client observed: none over plaintext loopback.
        let observed = if tls {
            state.cert_fingerprint.as_deref().unwrap_or("")
        } else {
            ""
        };
        let code = crate::identity::verification_code(observed, client_nonce, &host_nonce);
        let (request_id, decision_rx) = match state
            .approvals
            .request(device_id, &device_name, &code)
            .await
        {
            Ok(pending) => pending,
            Err(reason) => {
                send_host_message(sink, &refuse(&reason)).await;
                return Err(SignalingError::AuthenticationFailed(reason));
            }
        };
        state.emit_event(serde_json::json!({"event": "pairing_pending", "device_id": device_id, "name": device_name, "code": code}));
        let pending = HostMessage::PairPending {
            v: protocol::PROTOCOL_VERSION,
            host_id: state.identity.id.clone(),
            host_name: state.identity.name.clone(),
            host_nonce,
            timeout_secs: crate::approval::PAIR_REQUEST_TIMEOUT.as_secs() as u32,
        };
        if !send_host_message(sink, &pending).await {
            state.approvals.cancel(request_id).await;
            return Err(SignalingError::ConnectionError(
                "could not send pair_pending".into(),
            ));
        }
        AuditEntry::new(AuditEvent::Connect)
            .device_id(device_id)
            .peer(peer)
            .kv("event", "pair_request")
            .emit();
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        state
            .approvals
            .spawn_prompt(request_id, device_name.clone(), code, done_rx);

        let timeout = tokio::time::sleep(crate::approval::PAIR_REQUEST_TIMEOUT);
        tokio::pin!(timeout);
        tokio::pin!(decision_rx);
        let decision = loop {
            tokio::select! {
                decision = &mut decision_rx => break decision.ok(),
                _ = &mut timeout => break None,
                message = source.next() => match message {
                    Some(Ok(Message::Ping(data))) => { let _ = sink.send(Message::Pong(data)).await; }
                    Some(Ok(Message::Pong(_))) => {}
                    // The phone cancelled, closed, or sent something unexpected.
                    _ => {
                        state.approvals.cancel(request_id).await;
                        let _ = done_tx.send(true);
                        return Err(SignalingError::ConnectionError("pairing cancelled by client".into()));
                    }
                },
            }
        };
        state.approvals.cancel(request_id).await;
        let _ = done_tx.send(true);
        match decision {
            Some(crate::approval::Decision::Accept) => {}
            Some(crate::approval::Decision::Decline) => {
                state
                    .recent_declines
                    .lock()
                    .await
                    .insert(peer.ip(), std::time::Instant::now());
                send_host_message(sink, &refuse("pairing was declined on the computer")).await;
                audit::log_auth_reject(device_id, peer, "pairing_declined");
                return Err(SignalingError::AuthenticationFailed(
                    "pairing declined".into(),
                ));
            }
            None => {
                send_host_message(
                    sink,
                    &refuse("no one approved the request on the computer in time"),
                )
                .await;
                return Err(SignalingError::AuthenticationFailed(
                    "pairing timed out".into(),
                ));
            }
        }
    }

    let token = {
        let mut pairing = state.pairing.lock().await;
        let token = pairing
            .issue_device_token()
            .map_err(SignalingError::AuthenticationFailed)?;
        if let Err(e) =
            pairing.register_device(token.clone(), device_id.to_string(), device_name.clone())
        {
            drop(pairing);
            send_host_message(sink, &refuse(&e)).await;
            return Err(SignalingError::AuthenticationFailed(e));
        }
        token
    };
    eprintln!(
        "pairing: paired '{device_name}' ({device_id}) via {}",
        if approved_by_token {
            "QR code"
        } else {
            "desktop approval"
        }
    );
    let accepted = HostMessage::PairResult {
        v: protocol::PROTOCOL_VERSION,
        accepted: true,
        device_token: Some(token),
        reason: None,
    };
    if !send_host_message(sink, &accepted).await {
        if state.pairing.lock().await.revoke_device(device_id).is_err() { eprintln!("pairing: could not persist failed-handshake credential removal"); }
        return Err(SignalingError::ConnectionError(
            "could not send pair_result".into(),
        ));
    }
    Ok(())
}

/// Record which catalog app a newly created session runs.
async fn remember_session_app(state: &Arc<ServerState>, response: &HostMessage, app_id: &str) {
    if let HostMessage::SessionCreated { session_id, .. } = response {
        state
            .session_apps
            .lock()
            .await
            .insert(session_id.clone(), app_id.to_string());
    }
}

/// Wrap a desktop entry's command with Wayland hints for toolkits that do
/// not pick Wayland on their own (Electron, Firefox, SDL, Java) and its
/// working directory. The compositor helper already sets GTK/Qt backends.
async fn media_state(state: &Arc<ServerState>, player: Option<&str>, known_art: Option<&str>) -> crate::media::MediaState {
    match state.media.get_or_init(|| async { crate::media::Media::connect().await.ok() }).await {
        Some(media) => media.state(player, known_art).await,
        None => crate::media::MediaState::default(),
    }
}

/// Battery level (%) at which the computer warns once (until charging).
const LOW_BATTERY: u8 = 15;

fn file_error(transfer_id: &str, reason: String) -> HostMessage {
    HostMessage::FileError { v: protocol::PROTOCOL_VERSION, transfer_id: transfer_id.to_owned(), reason }
}

fn desktop_app_command(
    command: Vec<String>,
    working_dir: Option<std::path::PathBuf>,
) -> Vec<String> {
    let mut wrapped = vec!["/usr/bin/env".to_string()];
    if let Some(dir) = working_dir.filter(|d| d.is_dir()) {
        wrapped.push("-C".into());
        wrapped.push(dir.to_string_lossy().into_owned());
    }
    wrapped.extend(
        [
            "ELECTRON_OZONE_PLATFORM_HINT=auto",
            "MOZ_ENABLE_WAYLAND=1",
            "SDL_VIDEODRIVER=wayland,x11",
            "_JAVA_AWT_WM_NONREPARENTING=1",
        ]
        .map(String::from),
    );
    wrapped.extend(command);
    wrapped
}

/// The host setting / device permission a phone message needs, if any.
fn required_feature(message: &ClientMessage) -> Option<crate::host_settings::Feature> {
    use crate::host_settings::Feature;
    match message {
        ClientMessage::FileOffer { .. } | ClientMessage::FileChunk { .. } | ClientMessage::FileComplete { .. } => Some(Feature::Files),
        ClientMessage::Clipboard { .. } => Some(Feature::Clipboard),
        ClientMessage::PhoneNotification { .. } => Some(Feature::Notifications),
        ClientMessage::RemoteInput { .. } | ClientMessage::SecondDisplayStart { .. } | ClientMessage::DisplayInput { .. } | ClientMessage::DesktopAttach { .. } => {
            Some(Feature::RemoteControl)
        }
        ClientMessage::UplinkStart { .. } => Some(Feature::PhoneVideo),
        ClientMessage::AppLaunch { .. } | ClientMessage::SessionRequest { .. } | ClientMessage::StreamStart { .. } | ClientMessage::SessionResume { .. } => Some(Feature::AppStreaming),
        _ => None,
    }
}

async fn handle_client_message(
    state: &Arc<ServerState>,
    device_id: &str,
    message: &ClientMessage,
) -> HostMessage {
    if matches!(message, ClientMessage::DesktopAttach { .. }) && !crate::host_settings::allowed(device_id, crate::host_settings::Feature::AppStreaming) {
        return HostMessage::Error { v: protocol::PROTOCOL_VERSION, reason: "Streaming is disabled for this phone".into() };
    }
    if let Some(feature) = required_feature(message)
        && !crate::host_settings::allowed(device_id, feature)
    {
        return match message {
            // Quietly ignored: the phone keeps working without them.
            ClientMessage::Clipboard { .. } | ClientMessage::PhoneNotification { .. } => HostMessage::Ack { v: protocol::PROTOCOL_VERSION },
            ClientMessage::FileOffer { transfer_id, .. } | ClientMessage::FileChunk { transfer_id, .. } | ClientMessage::FileComplete { transfer_id, .. } => {
                file_error(transfer_id, "receiving files is turned off on this computer".into())
            }
            ClientMessage::SecondDisplayStart { session_id, .. } | ClientMessage::DisplayInput { session_id, .. }
            | ClientMessage::AppLaunch { session_id, .. } | ClientMessage::SessionRequest { session_id, .. }
            | ClientMessage::StreamStart { session_id, .. } | ClientMessage::SessionResume { session_id, .. } => HostMessage::SessionError {
                v: protocol::PROTOCOL_VERSION,
                session_id: session_id.clone(),
                reason: "streaming or remote control is turned off on this computer".into(),
            },
            _ => HostMessage::Error { v: protocol::PROTOCOL_VERSION, reason: "this is turned off in the computer's settings".into() },
        };
    }
    match message {
        ClientMessage::FileOffer { transfer_id, name, size, .. } => {
            let dir = crate::share::downloads_dir();
            match state.transfers.offer(device_id, transfer_id, name, *size, &dir) {
                Ok(()) => HostMessage::FileAccepted {
                    v: protocol::PROTOCOL_VERSION,
                    transfer_id: transfer_id.clone(),
                    chunk_size: protocol::FILE_CHUNK_SIZE as u32,
                },
                Err(reason) => file_error(transfer_id, reason),
            }
        }
        ClientMessage::FileChunk { transfer_id, offset, data, .. } => {
            match state.transfers.chunk(device_id, transfer_id, *offset, data) {
                Ok(received) => HostMessage::FileProgress {
                    v: protocol::PROTOCOL_VERSION,
                    transfer_id: transfer_id.clone(),
                    received,
                },
                Err(reason) => {
                    state.transfers.cancel(device_id, transfer_id);
                    file_error(transfer_id, reason)
                }
            }
        }
        ClientMessage::FileComplete { transfer_id, sha256, .. } => {
            match state.transfers.complete(device_id, transfer_id, sha256) {
                Ok(path) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    eprintln!("share: received {name} from {device_id}");
                    crate::share::notify_received(path, state.device_name(device_id).await);
                    HostMessage::FileSaved { v: protocol::PROTOCOL_VERSION, transfer_id: transfer_id.clone(), name }
                }
                Err(reason) => file_error(transfer_id, reason),
            }
        }
        ClientMessage::FilePull { transfer_id, offset, .. } => {
            match state.transfers.pull(device_id, transfer_id, *offset) {
                Ok((data, eof)) => HostMessage::FileData {
                    v: protocol::PROTOCOL_VERSION,
                    transfer_id: transfer_id.clone(),
                    offset: *offset,
                    data,
                    eof,
                },
                Err(reason) => file_error(transfer_id, reason),
            }
        }
        ClientMessage::FileReceived { transfer_id, .. } => {
            if let Some(sent) = state.transfers.finish_outgoing(device_id, transfer_id) {
                eprintln!("share: {device_id} saved {}", sent.name);
            }
            HostMessage::FileFinished { v: protocol::PROTOCOL_VERSION, transfer_id: transfer_id.clone() }
        }
        ClientMessage::FileCancel { transfer_id, .. } => {
            state.transfers.cancel(device_id, transfer_id);
            HostMessage::FileCancelled { v: protocol::PROTOCOL_VERSION, transfer_id: transfer_id.clone(), reason: None }
        }
        ClientMessage::ClipboardSync { enabled, .. } => {
            if let Some(info) = state.connections.lock().await.get(device_id) {
                info.clipboard.store(*enabled && state.clipboard_available(), Ordering::Relaxed);
            }
            HostMessage::ClipboardAck { v: protocol::PROTOCOL_VERSION }
        }
        ClientMessage::Clipboard { text, .. } => {
            if state.shell_clipboard {
                state.relay_phone_clipboard(device_id, text).await;
            } else if let Some(clipboard) = state.clipboard.get() {
                clipboard.set(text.clone());
            }
            HostMessage::ClipboardAck { v: protocol::PROTOCOL_VERSION }
        }
        ClientMessage::PhoneNotification { key, app_name, title, text, icon, actions, .. } => {
            if let Some(notifications) = state.notifications.get() {
                let device_name = state.device_name(device_id).await;
                let shown = notifications
                    .show(device_id, crate::phone_notify::Incoming {
                        device_name: &device_name, key, app_name, title, text, icon: icon.as_deref(), actions,
                    })
                    .await;
                if let Err(error) = shown {
                    eprintln!("notifications: could not show: {error}");
                }
            }
            HostMessage::Ack { v: protocol::PROTOCOL_VERSION }
        }
        ClientMessage::PhoneNotificationRemoved { key, .. } => {
            if let Some(notifications) = state.notifications.get() {
                notifications.remove(device_id, key).await;
            }
            HostMessage::Ack { v: protocol::PROTOCOL_VERSION }
        }
        ClientMessage::MediaStateRequest { player, known_art, .. } => {
            HostMessage::MediaState { v: protocol::PROTOCOL_VERSION, state: media_state(state, player.as_deref(), known_art.as_deref()).await }
        }
        ClientMessage::MediaCommand { player, command, value, .. } => {
            let media = state.media.get_or_init(|| async { crate::media::Media::connect().await.ok() }).await;
            match media {
                Some(media) => match media.command(player.as_deref(), command, *value).await {
                    Ok(()) => {
                        // Let the player apply it before reporting.
                        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                        HostMessage::MediaState { v: protocol::PROTOCOL_VERSION, state: media.state(player.as_deref(), None).await }
                    }
                    Err(reason) => HostMessage::Error { v: protocol::PROTOCOL_VERSION, reason },
                },
                None => HostMessage::Error { v: protocol::PROTOCOL_VERSION, reason: "media control unavailable".into() },
            }
        }
        ClientMessage::RemoteInput { events, .. } => {
            let input = state.desktop_input.get_or_init(crate::desktop_input::DesktopInput::new);
            state.input_devices.lock().unwrap_or_else(|e| e.into_inner()).insert(device_id.to_owned());
            match input.apply(events).await {
                Ok(()) => HostMessage::Ack { v: protocol::PROTOCOL_VERSION },
                Err(reason) => HostMessage::Error { v: protocol::PROTOCOL_VERSION, reason: format!("remote input: {reason}") },
            }
        }
        ClientMessage::SecondDisplayStart { session_id, width, height, view, output, .. } => {
            if state.sessions.negotiation_state(session_id).is_some() {
                return HostMessage::SessionError { v: protocol::PROTOCOL_VERSION, session_id: session_id.clone(), reason: "session already exists".into() };
            }
            // Even pixels for the H.264 encoder.
            let (width, height) = (*width & !1, *height & !1);
            let display = if *view {
                crate::second_display::VirtualDisplay::view_monitor(output.as_deref(), (width, height)).await
            } else {
                crate::second_display::VirtualDisplay::create(width, height).await
            };
            match display {
                Ok(display) => {
                    let (width, height) = (display.width, display.height);
                    if let Err(reason) = state.sessions.create_demo(session_id.clone(), width, height) {
                        display.stop().await;
                        return HostMessage::SessionError { v: protocol::PROTOCOL_VERSION, session_id: session_id.clone(), reason };
                    }
                    state.sessions.set_device_id(session_id, device_id);
                    state.session_owners.lock().await.insert(session_id.clone(), device_id.into());
                    eprintln!(
                        "{}: {width}x{height} (source {}x{}) for {device_id} (PipeWire node {})",
                        if *view { "desktop view" } else { "second display" },
                        display.source_width,
                        display.source_height,
                        display.node
                    );
                    state.second_displays.lock().await.insert(session_id.clone(), (device_id.to_owned(), Arc::new(display)));
                    HostMessage::SessionCreated {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        width,
                        height,
                        scale: 1.0,
                        refresh_hz: 60,
                    }
                }
                Err(reason) => HostMessage::SessionError { v: protocol::PROTOCOL_VERSION, session_id: session_id.clone(), reason },
            }
        }
        ClientMessage::DisplayInput { session_id, events, .. } => {
            let display = state.second_displays.lock().await.get(session_id)
                .filter(|(owner, _)| owner == device_id)
                .map(|(_, display)| Arc::clone(display));
            match display {
                Some(display) => match display.input(events).await {
                    Ok(()) => HostMessage::Ack { v: protocol::PROTOCOL_VERSION },
                    Err(reason) => HostMessage::Error { v: protocol::PROTOCOL_VERSION, reason },
                },
                None => HostMessage::SessionError { v: protocol::PROTOCOL_VERSION, session_id: session_id.clone(), reason: "no such display".into() },
            }
        }
        ClientMessage::UplinkStart { uplink_id, kind, sdp, .. } => {
            let Some(to_phone) = state.to_phone.get().cloned() else {
                return HostMessage::Error { v: protocol::PROTOCOL_VERSION, reason: "phone video is unavailable".into() };
            };
            let name = state.device_name(device_id).await;
            match state.uplinks.start(device_id, &name, uplink_id, kind, sdp, to_phone).await {
                Ok(sdp) => HostMessage::UplinkAnswer { v: protocol::PROTOCOL_VERSION, uplink_id: uplink_id.clone(), sdp },
                Err(reason) => HostMessage::Error { v: protocol::PROTOCOL_VERSION, reason },
            }
        }
        ClientMessage::UplinkIce { uplink_id, candidate, sdp_m_line_index, .. } => {
            state.uplinks.ice(device_id, uplink_id, candidate, *sdp_m_line_index).await;
            HostMessage::Ack { v: protocol::PROTOCOL_VERSION }
        }
        ClientMessage::UplinkStop { uplink_id, .. } => {
            state.uplinks.stop(device_id, uplink_id).await;
            HostMessage::UplinkStopped { v: protocol::PROTOCOL_VERSION, uplink_id: uplink_id.clone() }
        }
        ClientMessage::SmsStatus { enabled, changed, .. } => {
            state.sms.status(device_id, *enabled, *changed);
            HostMessage::Ack { v: protocol::PROTOCOL_VERSION }
        }
        ClientMessage::SmsResult { request_id, result, .. } => {
            state.sms.resolve(device_id, request_id, result.clone());
            HostMessage::Ack { v: protocol::PROTOCOL_VERSION }
        }
        ClientMessage::PhoneStatus { battery, charging, .. } => {
            let alert = {
                let mut status = state.phone_status.lock().unwrap_or_else(|e| e.into_inner());
                let alerted = status.get(device_id).is_some_and(|s| s.2);
                let low = *battery <= LOW_BATTERY && !*charging;
                status.insert(device_id.to_owned(), (*battery, *charging, low && alerted || low));
                low && !alerted
            };
            if alert {
                let name = state.device_name(device_id).await;
                let battery = *battery;
                tokio::spawn(async move {
                    let _ = tokio::process::Command::new("notify-send")
                        .args(["--app-name=enzimOS Remote", "--icon=battery-caution"])
                        .arg(format!("{name} battery low"))
                        .arg(format!("{battery}% remaining."))
                        .status()
                        .await;
                });
            }
            HostMessage::Ack { v: protocol::PROTOCOL_VERSION }
        }
        ClientMessage::AppList { .. } => {
            match tokio::task::spawn_blocking(crate::launcher::catalog).await {
                Ok(catalog) => HostMessage::AppListResult {
                    v: protocol::PROTOCOL_VERSION,
                    revision: catalog.revision,
                    apps: catalog.apps,
                },
                Err(_) => HostMessage::Error {
                    v: protocol::PROTOCOL_VERSION,
                    reason: "The application list is unavailable".into(),
                },
            }
        }
        ClientMessage::AppIcons { app_ids, .. } => {
            let ids = app_ids.clone();
            let icons = tokio::task::spawn_blocking(move || {
                ids.iter()
                    .filter_map(|id| {
                        crate::launcher::icon_png(id).map(|(icon, png)| protocol::AppIcon {
                            app_id: id.clone(),
                            icon,
                            png,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            HostMessage::AppIconsResult {
                v: protocol::PROTOCOL_VERSION,
                icons,
            }
        }
        ClientMessage::PairRequest { .. } | ClientMessage::Unpair { .. } => HostMessage::Error {
            v: protocol::PROTOCOL_VERSION,
            reason: "already authenticated".into(),
        },
        ClientMessage::DesktopAttach {
            session_id, output, ..
        } => attach_desktop(state, device_id, session_id, output.as_deref()).await,
        ClientMessage::ComputerInfo { .. } => {
            match tokio::task::spawn_blocking(crate::desktop::computer_info).await {
                Ok(computer) => HostMessage::ComputerInfoResult {
                    v: protocol::PROTOCOL_VERSION,
                    computer,
                },
                Err(_) => HostMessage::Error {
                    v: protocol::PROTOCOL_VERSION,
                    reason: "Computer information is unavailable".into(),
                },
            }
        }
        ClientMessage::AppLaunch {
            v,
            session_id,
            app_id,
            width,
            height,
            scale,
            refresh_hz,
        } => {
            let id = app_id.clone();
            let resolved = tokio::task::spawn_blocking(move || {
                crate::launcher::launch_plan(&id).map(|plan| {
                    let already_open = plan
                        .dbus_name
                        .as_deref()
                        .is_some_and(crate::launcher::already_running);
                    (plan, already_open)
                })
            })
            .await
            .unwrap_or_else(|_| Err("The application list is unavailable".into()));
            let (plan, already_open) = match resolved {
                Ok(resolved) => resolved,
                Err(reason) => {
                    return HostMessage::SessionError {
                        v: *v,
                        session_id: session_id.clone(),
                        reason,
                    };
                }
            };
            if state.config.demo_mode {
                return HostMessage::SessionError {
                    v: *v,
                    session_id: session_id.clone(),
                    reason: "App launch requires the real session server, not demo mode".into(),
                };
            }
            if already_open && !plan.iced {
                return HostMessage::SessionError {
                    v: *v,
                    session_id: session_id.clone(),
                    reason: format!(
                        "{} is already open on the computer. Close it there to open it on your phone.",
                        plan.name
                    ),
                };
            }
            // Shared-desktop hosts borrow/open ICED apps in the real desktop;
            // other installed apps always get a private app compositor.
            let iced = plan.iced;
            let shared_target = if iced {
                crate::shared_desktop::DesktopTarget::from_environment().await
            } else {
                Ok(None)
            };
            match shared_target {
                Ok(Some(target)) => {
                    let response = launch_desktop_app(
                        state,
                        device_id,
                        session_id,
                        app_id,
                        target,
                        enzremote_session::Viewport {
                            width: *width,
                            height: *height,
                            scale: *scale,
                            refresh_hz: *refresh_hz,
                        },
                    )
                    .await;
                    remember_session_app(state, &response, app_id).await;
                    return response;
                }
                Err(reason) => {
                    return HostMessage::SessionError {
                        v: *v,
                        session_id: session_id.clone(),
                        reason,
                    };
                }
                Ok(None) => {}
            }
            let command = if iced {
                plan.command
            } else {
                desktop_app_command(plan.command, plan.working_dir)
            };
            let response = Box::pin(handle_client_message(
                state,
                device_id,
                &ClientMessage::SessionRequest {
                    v: *v,
                    session_id: session_id.clone(),
                    command,
                    width: *width,
                    height: *height,
                    scale: *scale,
                    refresh_hz: *refresh_hz,
                },
            ))
            .await;
            remember_session_app(state, &response, app_id).await;
            response
        }
        ClientMessage::Hello { .. } => {
            // Already authenticated; reject duplicate hello.
            HostMessage::Error {
                v: protocol::PROTOCOL_VERSION,
                reason: "already authenticated".into(),
            }
        }
        ClientMessage::SessionRequest {
            session_id,
            command,
            width,
            height,
            scale,
            refresh_hz,
            ..
        } => {
            audit::log_session_create(device_id, session_id, *width, *height, *scale, *refresh_hz);
            state.emit_event(serde_json::json!({"event": "session_started", "session_id": session_id}));
            // Check session limit. Note: there's a small TOCTOU window between
            // this check and the create() call below (another handler could
            // create a session concurrently). With Tokio cooperative scheduling
            // and max_sessions typically <=8, this is extremely unlikely.
            if state.config.max_sessions > 0
                && state.sessions.session_count() >= state.config.max_sessions
            {
                return HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason: format!(
                        "maximum session limit reached ({})",
                        state.config.max_sessions
                    ),
                };
            }
            let result = if state.config.demo_mode {
                // Demo mode: register metadata only, no compositor launch.
                state
                    .sessions
                    .create_demo(session_id.clone(), *width, *height)
            } else {
                state
                    .sessions
                    .create(
                        session_id.clone(),
                        command.clone(),
                        *width,
                        *height,
                        *scale,
                        *refresh_hz,
                    )
                    .await
            };
            match result {
                Ok((w, h)) => {
                    state
                        .metrics
                        .total_sessions_created
                        .fetch_add(1, Ordering::Relaxed);
                    // Touch after successful creation (not before, to avoid
                    // timestamp leaks for sessions that fail to create).
                    state.sessions.touch(session_id);
                    // Bind this session to the requesting device.
                    state
                        .session_owners
                        .lock()
                        .await
                        .insert(session_id.clone(), device_id.into());
                    state.sessions.set_device_id(session_id, device_id);
                    watch_private_app(state, session_id);
                    HostMessage::SessionCreated {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        width: w,
                        height: h,
                        scale: *scale,
                        refresh_hz: *refresh_hz,
                    }
                }
                Err(reason) => HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason,
                },
            }
        }
        ClientMessage::Sdp {
            session_id,
            sdp_type,
            sdp,
            ..
        } => {
            // Verify session ownership.
            let owner = state.session_owners.lock().await.get(session_id).cloned();
            if owner.as_deref() != Some(device_id) {
                return HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason: "not the owner of this session".into(),
                };
            }
            // Enforce negotiation state machine.
            //
            // In server-offer mode, the pipeline generates the SDP offer
            // asynchronously after StreamStart. The client only sends answers.
            // Offers from the client are rejected.
            //
            // Valid transitions:
            //   WaitingForOffer → (pipeline sends offer) → OfferSent
            //   OfferSent → client sends Answer → Negotiating
            //   WaitingForOffer → client sends Answer → Negotiating
            //     (allow this as a race: the offer was broadcast but the
            //      state hasn't been updated yet by the IPC callback)
            let negotiation = state.sessions.negotiation_state(session_id);
            match sdp_type {
                protocol::SdpType::Offer => {
                    // Server generates offers; client should not send offers.
                    return HostMessage::SessionError {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        reason: "server generates SDP offers; send an answer instead".into(),
                    };
                }
                protocol::SdpType::Answer => match negotiation.as_ref() {
                    Some(
                        crate::sessions::NegotiationState::OfferSent
                        | crate::sessions::NegotiationState::WaitingForOffer,
                    ) => {
                        state.sessions.set_negotiation_state(
                            session_id,
                            crate::sessions::NegotiationState::Negotiating,
                        );
                    }
                    _ => {
                        return HostMessage::SessionError {
                            v: protocol::PROTOCOL_VERSION,
                            session_id: session_id.clone(),
                            reason: format!(
                                "SDP answer not allowed in negotiation state {:?}; call stream_start first",
                                negotiation
                                    .as_ref()
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| "unknown".into())
                            ),
                        };
                    }
                },
            }
            state.sessions.touch(session_id);
            let new_state = state
                .sessions
                .negotiation_state(session_id)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".into());
            AuditEntry::new(AuditEvent::Sdp)
                .device_id(device_id)
                .session_id(session_id)
                .kv("sdp_type", &format!("{sdp_type:?}"))
                .kv("negotiation", &new_state)
                .emit();

            // Forward SDP answer to the WebRTC pipeline via the bridge.
            // (Offers were rejected above; only answers reach this point.)
            // Best-effort forward: if no pipeline is running (e.g., in
            // tests or when the binary is unavailable), this is a no-op.
            if let Err(e) = state.webrtc_bridge.send_answer(session_id, sdp).await {
                eprintln!("warning: could not forward SDP answer for session {session_id}: {e}");
            }

            // Echo the SDP back as acknowledgment.
            HostMessage::Sdp {
                v: protocol::PROTOCOL_VERSION,
                session_id: session_id.clone(),
                sdp_type: sdp_type.clone(),
                sdp: sdp.clone(),
            }
        }
        ClientMessage::Ice {
            session_id,
            candidate,
            sdp_m_line_index,
            ..
        } => {
            // Verify session ownership.
            let owner = state.session_owners.lock().await.get(session_id).cloned();
            if owner.as_deref() != Some(device_id) {
                return HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason: "not the owner of this session".into(),
                };
            }
            // ICE candidates are only valid after SDP exchange has begun.
            let negotiation = state.sessions.negotiation_state(session_id);
            match negotiation.as_ref() {
                Some(
                    crate::sessions::NegotiationState::OfferSent
                    | crate::sessions::NegotiationState::Negotiating
                    | crate::sessions::NegotiationState::Connected,
                ) => {}
                _ => {
                    return HostMessage::SessionError {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        reason: format!(
                            "ICE candidate not allowed in negotiation state {:?}; SDP exchange must begin first",
                            negotiation
                                .as_ref()
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| "unknown".into())
                        ),
                    };
                }
            }
            state.sessions.touch(session_id);
            AuditEntry::new(AuditEvent::Ice)
                .device_id(device_id)
                .session_id(session_id)
                .emit();

            // Forward ICE candidate to the WebRTC pipeline (best-effort).
            if let Err(e) = state
                .webrtc_bridge
                .send_ice(session_id, candidate, *sdp_m_line_index)
                .await
            {
                eprintln!("warning: could not forward ICE for session {session_id}: {e}");
            }

            HostMessage::Ice {
                v: protocol::PROTOCOL_VERSION,
                session_id: session_id.clone(),
                candidate: candidate.clone(),
                sdp_m_line_index: *sdp_m_line_index,
            }
        }
        ClientMessage::SessionResume { session_id, .. } => {
            // NOTE: resume intentionally allows any authenticated device to
            // take over a session. This supports reconnection from a different
            // device (e.g. after a phone restart). The previous owner is
            // notified via an OwnershipTransferred session event broadcast.
            AuditEntry::new(AuditEvent::SessionResume)
                .device_id(device_id)
                .session_id(session_id)
                .emit();
            match state.sessions.resume(session_id.clone()).await {
                Ok((w, h, sc, hz)) => {
                    // Touch after successful resume (not before, to avoid
                    // updating the timestamp for sessions that fail to resume,
                    // which would delay garbage collection).
                    state.sessions.touch(session_id);
                    // Rebind session ownership to the resuming device.
                    state
                        .session_owners
                        .lock()
                        .await
                        .insert(session_id.clone(), device_id.into());
                    state.sessions.set_device_id(session_id, device_id);
                    // Stop any existing WebRTC bridge for this session so
                    // the new owner can start a fresh stream. Without this,
                    // the old bridge stays registered and stream_start fails
                    // with "already running".
                    let _ = state.webrtc_bridge.stop_stream(session_id).await;
                    // Reset negotiation state: the new owner must start fresh.
                    // Any in-progress WebRTC negotiation from the previous owner
                    // is orphaned and cannot continue under new ownership.
                    state
                        .sessions
                        .set_negotiation_state(session_id, crate::sessions::NegotiationState::Idle);
                    // Notify other clients about the ownership transfer.
                    state.broadcast_event(HostMessage::SessionEvent {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        event: protocol::SessionEventKind::OwnershipTransferred,
                        detail: None,
                    });
                    watch_desktop_control(state, session_id).await;
                    watch_private_app(state, session_id);
                    HostMessage::SessionResumed {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        width: w,
                        height: h,
                        scale: sc,
                        refresh_hz: hz,
                    }
                }
                Err(reason) => HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason,
                },
            }
        }
        ClientMessage::SessionResize {
            session_id,
            width,
            height,
            scale,
            refresh_hz,
            ..
        } => {
            // Verify session ownership for resize.
            let owner = state.session_owners.lock().await.get(session_id).cloned();
            if owner.as_deref() != Some(device_id) {
                return HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason: "not the owner of this session".into(),
                };
            }
            AuditEntry::new(AuditEvent::SessionResize)
                .device_id(device_id)
                .session_id(session_id)
                .kv_u32("width", *width)
                .kv_u32("height", *height)
                .kv("scale", &format!("{scale}"))
                .kv_u32("refresh_hz", *refresh_hz)
                .emit();
            state.sessions.touch(session_id);
            match state
                .sessions
                .resize(
                    session_id.clone(),
                    *width,
                    *height,
                    Some(*scale),
                    Some(*refresh_hz),
                )
                .await
            {
                Ok((w, h)) => {
                    // Notify clients about the resize.
                    state.broadcast_event(HostMessage::SessionEvent {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        event: protocol::SessionEventKind::Resized,
                        detail: None,
                    });
                    HostMessage::SessionResized {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        width: w,
                        height: h,
                        scale: *scale,
                        refresh_hz: *refresh_hz,
                    }
                }
                Err(reason) => HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason,
                },
            }
        }
        ClientMessage::SessionStop { session_id, .. } => {
            audit::log_session_stop(device_id, session_id);
            // Verify session ownership.
            let owner = state.session_owners.lock().await.get(session_id).cloned();
            if owner.as_deref() != Some(device_id) {
                return HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason: "not the owner of this session".into(),
                };
            }
            // Stop the WebRTC bridge process first (if running) to prevent
            // orphaned child processes that keep streaming after the session
            // is torn down.
            if state.webrtc_bridge.has_stream(session_id).await {
                let _ = state.webrtc_bridge.stop_stream(session_id).await;
                state
                    .sessions
                    .set_negotiation_state(session_id, crate::sessions::NegotiationState::Idle);
            }
            let display = state.second_displays.lock().await.remove(session_id);
            if let Some((_, display)) = display {
                display.stop().await;
                state.sessions.remove(session_id);
                state.session_owners.lock().await.remove(session_id);
                return HostMessage::SessionStopped { v: protocol::PROTOCOL_VERSION, session_id: session_id.clone(), reason: None };
            }
            match state.sessions.stop(session_id.clone()).await {
                Ok(()) => {
                    // Unbind session ownership.
                    state.session_owners.lock().await.remove(session_id);
                    // Notify other clients about the stop.
                    state.broadcast_event(HostMessage::SessionEvent {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        event: protocol::SessionEventKind::StoppedExternally,
                        detail: None,
                    });
                    HostMessage::SessionStopped {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        reason: None,
                    }
                }
                Err(reason) => HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason,
                },
            }
        }
        ClientMessage::StreamStart { session_id, .. } => {
            let owner = state.session_owners.lock().await.get(session_id).cloned();
            if owner.as_deref() != Some(device_id) {
                return HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason: "not the owner of this session".into(),
                };
            }
            // Reject StreamStart if the session is unknown or if a stream
            // is already active (not in Idle). The client must call
            // StreamStop first to reset the state.
            let negotiation = state.sessions.negotiation_state(session_id);
            match negotiation {
                None => {
                    return HostMessage::SessionError {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        reason: "session not found".into(),
                    };
                }
                Some(ref n) if *n != crate::sessions::NegotiationState::Idle => {
                    return HostMessage::SessionError {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        reason: format!(
                            "stream already active (state: {n}); call stream_stop first",
                        ),
                    };
                }
                _ => {} // Idle — proceed
            }
            if let Err(reason) = state.sessions.ensure_desktop_active(session_id).await {
                return HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason,
                };
            }
            state.sessions.touch(session_id);
            // Transition negotiation state to WaitingForOffer.
            state.sessions.set_negotiation_state(
                session_id,
                crate::sessions::NegotiationState::WaitingForOffer,
            );
            AuditEntry::new(AuditEvent::StreamStart)
                .device_id(device_id)
                .session_id(session_id)
                .kv("negotiation", "WaitingForOffer")
                .emit();

            // Launch the WebRTC server process for this session.
            let session_id_for_cb = session_id.clone();
            let desktop_generation = state.sessions.desktop_lease_id(session_id).await;
            let events_tx = state.session_events.clone();
            let state_for_cb = Arc::clone(state);
            let callback: crate::webrtc_bridge::FromWebRtcCallback = Arc::new(move |_sid, msg| {
                use enzremote_webrtc_ipc::FromWebRtc;
                match msg {
                    FromWebRtc::Offer { sdp } => {
                        // Update negotiation state to OfferSent.
                        state_for_cb.sessions.set_negotiation_state(
                            &session_id_for_cb,
                            crate::sessions::NegotiationState::OfferSent,
                        );
                        let _ = events_tx.send(HostMessage::Sdp {
                            v: protocol::PROTOCOL_VERSION,
                            session_id: session_id_for_cb.clone(),
                            sdp_type: protocol::SdpType::Offer,
                            sdp,
                        });
                    }
                    FromWebRtc::Ice {
                        candidate,
                        sdp_m_line_index,
                    } => {
                        let _ = events_tx.send(HostMessage::Ice {
                            v: protocol::PROTOCOL_VERSION,
                            session_id: session_id_for_cb.clone(),
                            candidate,
                            sdp_m_line_index,
                        });
                    }
                    FromWebRtc::Connected => {
                        state_for_cb.sessions.set_negotiation_state(
                            &session_id_for_cb,
                            crate::sessions::NegotiationState::Connected,
                        );
                        eprintln!("webrtc-bridge: session {} connected", session_id_for_cb);
                    }
                    FromWebRtc::Disconnected => {
                        eprintln!(
                            "webrtc-bridge: session {} ICE disconnected (timeout pending)",
                            session_id_for_cb
                        );
                        // Informational — the pipeline will escalate to
                        // Error if ICE does not recover within 10 seconds.
                    }
                    FromWebRtc::Error { reason } => {
                        if desktop_generation.is_some()
                            && state_for_cb.sessions.negotiation_state(&session_id_for_cb)
                                == Some(crate::sessions::NegotiationState::Idle)
                        {
                            return;
                        }
                        state_for_cb.sessions.set_negotiation_state(
                            &session_id_for_cb,
                            crate::sessions::NegotiationState::Failed(reason.clone()),
                        );
                        if let Some(generation) = desktop_generation.clone() {
                            let state = state_for_cb.clone();
                            let id = session_id_for_cb.clone();
                            let stamp = state.sessions.failure_stamp(&id);
                            let retryable = matches!(
                                reason.trim().to_ascii_lowercase().as_str(),
                                "ice connection failed" | "peer connection failed"
                            );
                            tokio::spawn(async move {
                                if retryable {
                                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                                }
                                state
                                    .sessions
                                    .release_desktop_failure(&id, &generation, stamp)
                                    .await;
                            });
                        }
                        let _ = events_tx.send(HostMessage::SessionError {
                            v: protocol::PROTOCOL_VERSION,
                            session_id: session_id_for_cb.clone(),
                            reason,
                        });
                        // Media failure must not orphan a still-running app/compositor.
                        // Keep the session registered for resume, stop, and idle GC.
                    }
                    FromWebRtc::Ready => {}
                }
            });

            // Compute encoder-friendly dimensions from the session's viewport.
            // H.264 macroblocks are 16x16, so both dimensions should be multiples of 16
            // to avoid encoder padding that causes aspect ratio mismatch on the client.
            let (session_w, session_h) = state
                .sessions
                .get_viewport(session_id)
                .unwrap_or((544, 1200));

            // Scale down to a reasonable capture resolution while preserving
            // the phone's exact aspect ratio. Target ~544px wide (portrait) for
            // a good balance of quality and CPU usage.
            // ENZREMOTE_VIEWPORT_OVERRIDE="WxH" bypasses the auto-scaling
            // (useful for testing higher resolutions).
            let (capture_w, capture_h) = if !state.config.demo_mode {
                (session_w, session_h)
            } else if let Ok(ovr) = std::env::var("ENZREMOTE_VIEWPORT_OVERRIDE") {
                let parts: Vec<&str> = ovr.split('x').collect();
                if parts.len() == 2 {
                    if let (Ok(w), Ok(h)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                        let w = ((w + 8) / 16) * 16; // align to 16
                        let h = ((h + 8) / 16) * 16;
                        eprintln!("viewport override: {w}x{h} (from ENZREMOTE_VIEWPORT_OVERRIDE)");
                        (w, h)
                    } else {
                        encoder_friendly_viewport(session_w, session_h)
                    }
                } else {
                    encoder_friendly_viewport(session_w, session_h)
                }
            } else {
                encoder_friendly_viewport(session_w, session_h)
            };

            // Dynamically resize the compositor to match the capture dimensions.
            // This makes the Wayland clients reflow to the phone's aspect ratio.
            if state.config.demo_mode
                && let Ok(instance) = std::env::var("ENZREMOTE_HYPRLAND_INSTANCE")
            {
                let mode = format!("{}x{}", capture_w, capture_h);
                eprintln!(
                    "resizing compositor to {mode} for session {session_id} (client {}x{})",
                    session_w, session_h
                );
                let eval_cmd = format!(
                    "hl.monitor({{ output = \"\", mode = \"{mode}\", position = \"0x0\", scale = 1 }})"
                );
                match tokio::process::Command::new("hyprctl")
                    .args(["--instance", &instance, "eval", &eval_cmd])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await
                {
                    Ok(status) if !status.success() => {
                        eprintln!("warning: hyprctl resize failed with exit code: {status}");
                    }
                    Err(e) => {
                        eprintln!("warning: hyprctl resize failed: {e}");
                    }
                    _ => {}
                }
            }

            // Pass capture and input environment variables to the webrtc-server.
            let mut env_vars: Vec<(&str, String)> = Vec::new();
            // Capture mode priority:
            // 1. ENZREMOTE_PIPEWIRE_TARGET — PipeWire/portal DMA-BUF capture
            //    (zero-copy when combined with VA-API encoder + vapostproc)
            // 2. ENZREMOTE_GRIM_DISPLAY — wlr-screencopy capture via grim/screencopy
            // 3. WAYLAND_DISPLAY — fallback to grim on the server's display
            //
            // The webrtc-server checks GRIM_DISPLAY before PIPEWIRE_TARGET,
            // so we must NOT set GRIM_DISPLAY when portal capture is active.
            let use_portal_capture = std::env::var("ENZREMOTE_PIPEWIRE_TARGET").is_ok();
            if use_portal_capture {
                // Portal DMA-BUF capture: pass the PipeWire target node.
                if let Ok(target) = std::env::var("ENZREMOTE_PIPEWIRE_TARGET") {
                    env_vars.push(("ENZREMOTE_PIPEWIRE_TARGET", target));
                }
                if let Ok(fd) = std::env::var("ENZREMOTE_PIPEWIRE_FD") {
                    env_vars.push(("ENZREMOTE_PIPEWIRE_FD", fd));
                }
                // Still need the Wayland display for input injection.
                if let Ok(display) = std::env::var("ENZREMOTE_GRIM_DISPLAY")
                    .or_else(|_| std::env::var("WAYLAND_DISPLAY"))
                {
                    env_vars.push(("ENZREMOTE_WAYLAND_DISPLAY", display));
                }
            } else if let Ok(grim_display) = std::env::var("ENZREMOTE_GRIM_DISPLAY")
                .or_else(|_| std::env::var("WAYLAND_DISPLAY"))
            {
                // Standard screencopy capture.
                env_vars.push(("ENZREMOTE_WAYLAND_DISPLAY", grim_display.clone()));
                env_vars.push(("ENZREMOTE_GRIM_DISPLAY", grim_display));
            }
            // Use session's refresh_hz as the video FPS, falling back to
            // ENZREMOTE_VIDEO_FPS env var, then to 30 as default.
            let settings = crate::host_settings::current();
            let video_fps = if let Ok(fps) = std::env::var("ENZREMOTE_VIDEO_FPS") {
                fps
            } else {
                state
                    .sessions
                    .get_refresh_hz(session_id)
                    .unwrap_or(30)
                    .min(settings.video_max_fps.max(15))
                    .to_string()
            };
            env_vars.push(("ENZREMOTE_VIDEO_FPS", video_fps));
            if settings.video_max_kbps > 0 {
                env_vars.push(("ENZREMOTE_VIDEO_MAX_KBPS", settings.video_max_kbps.to_string()));
            }
            // The app's own audio sink, sent to the phone as Opus.
            let audio_id = session_id.clone();
            if let Ok(Some(sink)) = tokio::task::spawn_blocking(move || enzremote_session::audio_sink(&audio_id)).await {
                env_vars.push(("ENZREMOTE_AUDIO_SINK", sink));
            }
            if let Ok(codec) = std::env::var("ENZREMOTE_VIDEO_CODEC") {
                env_vars.push(("ENZREMOTE_VIDEO_CODEC", codec));
            } else if settings.video_codec == "h265"
                || (settings.video_codec == "auto"
                    && state.device_decodes(device_id, "h265").await)
            {
                env_vars.push(("ENZREMOTE_VIDEO_CODEC", "hevc".into()));
            }
            if let Ok(instance) = std::env::var("ENZREMOTE_HYPRLAND_INSTANCE") {
                env_vars.push(("ENZREMOTE_HYPRLAND_INSTANCE", instance));
            }
            // Use session-derived dimensions, not static env vars.
            env_vars.push(("ENZREMOTE_VIDEO_WIDTH", capture_w.to_string()));
            env_vars.push(("ENZREMOTE_VIDEO_HEIGHT", capture_h.to_string()));
            if let Ok(helper) = std::env::var("ENZREMOTE_INPUT_WL_HELPER") {
                env_vars.push(("ENZREMOTE_INPUT_WL_HELPER", helper));
            }
            if let Ok(encoder) = std::env::var("ENZREMOTE_ENCODER") {
                env_vars.push(("ENZREMOTE_ENCODER", encoder));
            }
            if let Ok(bitrate) = std::env::var("ENZREMOTE_VIDEO_BITRATE") {
                env_vars.push(("ENZREMOTE_VIDEO_BITRATE", bitrate));
            }
            // Pass session dimensions so the input bridge can scale
            // coordinates from session space (phone resolution) to
            // viewport space (compositor resolution).
            env_vars.push(("ENZREMOTE_SESSION_WIDTH", session_w.to_string()));
            env_vars.push(("ENZREMOTE_SESSION_HEIGHT", session_h.to_string()));
            let second_display = state.second_displays.lock().await.get(session_id).map(|(_, d)| Arc::clone(d));
            if let Some(display) = &second_display {
                env_vars.retain(|(key, _)| {
                    !matches!(
                        *key,
                        "ENZREMOTE_PIPEWIRE_TARGET" | "ENZREMOTE_PIPEWIRE_FD" | "ENZREMOTE_WAYLAND_DISPLAY"
                            | "ENZREMOTE_GRIM_DISPLAY" | "ENZREMOTE_HYPRLAND_INSTANCE" | "ENZREMOTE_INPUT_WL_HELPER"
                    )
                });
                env_vars.push(("ENZREMOTE_PIPEWIRE_TARGET", display.serial.to_string()));
                env_vars.push(("ENZREMOTE_PIPEWIRE_SIZE", format!("{}x{}", display.source_width, display.source_height)));
                if (display.width, display.height) != (display.source_width, display.source_height) {
                    env_vars.push(("ENZREMOTE_OUTPUT_SIZE", format!("{}x{}", display.width, display.height)));
                }
                // Input arrives over the signaling link (display_input).
                env_vars.push(("ENZREMOTE_NO_INPUT", "1".into()));
            } else if !state.config.demo_mode {
                let inspection = match state.sessions.inspect(session_id.clone()).await {
                    Ok(inspection) if inspection.app_alive && inspection.compositor_alive => {
                        inspection
                    }
                    _ => {
                        state.sessions.set_negotiation_state(
                            session_id,
                            crate::sessions::NegotiationState::Idle,
                        );
                        return HostMessage::SessionError {
                            v: protocol::PROTOCOL_VERSION,
                            session_id: session_id.clone(),
                            reason: "stream launch failed: the application is no longer running"
                                .into(),
                        };
                    }
                };
                let Some(capture) = inspection.capture else {
                    state
                        .sessions
                        .set_negotiation_state(session_id, crate::sessions::NegotiationState::Idle);
                    return HostMessage::SessionError {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        reason: "Session capture is unavailable".into(),
                    };
                };
                env_vars.retain(|(key, _)| {
                    !matches!(
                        *key,
                        "ENZREMOTE_PIPEWIRE_TARGET"
                            | "ENZREMOTE_PIPEWIRE_FD"
                            | "ENZREMOTE_WAYLAND_DISPLAY"
                            | "ENZREMOTE_GRIM_DISPLAY"
                            | "ENZREMOTE_HYPRLAND_INSTANCE"
                    )
                });
                env_vars.push((
                    "ENZREMOTE_INPUT_SCALE",
                    inspection.state.manifest.viewport.scale.to_string(),
                ));
                env_vars.push(("ENZREMOTE_CAPTURE_OUTPUT", capture.output));
                env_vars.push(("ENZREMOTE_WAYLAND_DISPLAY", capture.wayland_display.clone()));
                env_vars.push(("ENZREMOTE_GRIM_DISPLAY", capture.wayland_display));
                env_vars.push(("ENZREMOTE_HYPRLAND_INSTANCE", capture.hyprland_instance));
            }
            match state.sessions.desktop_worker_environment(session_id).await {
                Ok(Some(desktop_env)) => {
                    env_vars.retain(|(key, _)| {
                        !desktop_env
                            .iter()
                            .any(|(replacement, _)| replacement == key)
                    });
                    env_vars.extend(desktop_env);
                }
                Ok(None) => {}
                Err(reason) => {
                    state
                        .sessions
                        .set_negotiation_state(session_id, crate::sessions::NegotiationState::Idle);
                    return HostMessage::SessionError {
                        v: protocol::PROTOCOL_VERSION,
                        session_id: session_id.clone(),
                        reason,
                    };
                }
            }
            let env_refs: Vec<(&str, &str)> =
                env_vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
            if let Err(reason) = state
                .webrtc_bridge
                .start_stream_with_env(session_id, callback, &env_refs)
                .await
            {
                // Reset negotiation state so the client can retry. Without
                // this, the session is stuck in WaitingForOffer with no
                // pipeline to generate an offer.
                state
                    .sessions
                    .set_negotiation_state(session_id, crate::sessions::NegotiationState::Idle);
                if state.sessions.is_desktop(session_id) {
                    let _ = state.sessions.stop_stream_only(session_id.clone()).await;
                }
                eprintln!(
                    "warning: WebRTC pipeline launch failed for session {session_id}: {reason}"
                );
                return HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason: format!("stream launch failed: {reason}"),
                };
            }

            HostMessage::StreamStarted {
                v: protocol::PROTOCOL_VERSION,
                session_id: session_id.clone(),
            }
        }
        ClientMessage::StreamStop { session_id, .. }
        | ClientMessage::StreamPause { session_id, .. } => {
            let owner = state.session_owners.lock().await.get(session_id).cloned();
            if owner.as_deref() != Some(device_id) {
                return HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason: "not the owner of this session".into(),
                };
            }
            let prev_state = state
                .sessions
                .negotiation_state(session_id)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "unknown".into());
            AuditEntry::new(AuditEvent::StreamStop)
                .device_id(device_id)
                .session_id(session_id)
                .kv("from_state", &prev_state)
                .kv("negotiation", "Idle")
                .emit();
            state.sessions.touch(session_id);
            // Reset negotiation state.
            state
                .sessions
                .set_negotiation_state(session_id, crate::sessions::NegotiationState::Idle);
            // Stop the WebRTC pipeline process (best-effort).
            let _ = state.webrtc_bridge.stop_stream(session_id).await;
            let result = if matches!(message, ClientMessage::StreamPause { .. })
                && state.sessions.is_desktop(session_id)
            {
                Ok(())
            } else {
                state.sessions.stop_stream_only(session_id.clone()).await
            };
            match result {
                Ok(()) => HostMessage::StreamStopped {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason: None,
                },
                Err(reason) => HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason,
                },
            }
        }
        ClientMessage::SessionInspect { session_id, .. } => {
            match state.sessions.inspect(session_id.clone()).await {
                Ok(info) => HostMessage::SessionInspection {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    width: info.state.manifest.viewport.width,
                    height: info.state.manifest.viewport.height,
                    scale: info.state.manifest.viewport.scale,
                    refresh_hz: info.state.manifest.viewport.refresh_hz,
                    compositor_alive: info.compositor_alive,
                    app_alive: info.app_alive,
                    stream_alive: info.stream_alive,
                    command: match &info.state.manifest.source {
                        enzremote_session::SessionSource::LaunchedApp { command } => {
                            command.clone()
                        }
                        _ => Vec::new(),
                    },
                    negotiation_state: state
                        .sessions
                        .negotiation_state(session_id)
                        .map(|s| s.to_string()),
                },
                Err(reason) => HostMessage::SessionError {
                    v: protocol::PROTOCOL_VERSION,
                    session_id: session_id.clone(),
                    reason,
                },
            }
        }
        ClientMessage::SessionList { .. } => {
            let mut sessions = state.sessions.list().await;
            {
                // Extra monitors are not apps to resume.
                let displays = state.second_displays.lock().await;
                sessions.retain(|s| !displays.contains_key(&s.session_id));
            }
            {
                let mut apps = state.session_apps.lock().await;
                apps.retain(|id, _| sessions.iter().any(|s| &s.session_id == id));
                for session in &mut sessions {
                    session.app_id = apps.get(&session.session_id).cloned();
                }
            }
            HostMessage::SessionListResult {
                v: protocol::PROTOCOL_VERSION,
                sessions,
            }
        }
        ClientMessage::Probe { .. } => {
            let active_sessions = state.sessions.session_count() as u32;
            let active_connections = state.connection_count().await as u32;
            let server_uptime_secs = state.started_at.elapsed().as_secs();
            tokio::task::spawn_blocking(enzremote_session::probe_capabilities)
                .await
                .map(|p| HostMessage::ProbeResult {
                    v: protocol::PROTOCOL_VERSION,
                    pipewire_running: p.pipewire_running,
                    render_nodes: p.render_nodes,
                    h264_encoders: p
                        .gstreamer
                        .h264_encoders
                        .iter()
                        .filter(|e| e.available)
                        .map(|e| e.name.clone())
                        .collect(),
                    av1_encoders: p
                        .gstreamer
                        .av1_encoders
                        .iter()
                        .filter(|e| e.available)
                        .map(|e| e.name.clone())
                        .collect(),
                    active_sessions,
                    active_connections,
                    server_uptime_secs,
                    max_sessions: state.config.max_sessions as u32,
                    hostname: state.hostname.clone(),
                    total_connections: state.metrics.total_connections.load(Ordering::Relaxed),
                    total_messages: state.metrics.total_messages.load(Ordering::Relaxed),
                    peak_connections: state.metrics.peak_connections.load(Ordering::Relaxed),
                    total_auth_failures: state.metrics.total_auth_failures.load(Ordering::Relaxed),
                    total_rate_limits: state.metrics.total_rate_limits.load(Ordering::Relaxed),
                    total_sessions_created: state
                        .metrics
                        .total_sessions_created
                        .load(Ordering::Relaxed),
                    server_version: Some(env!("CARGO_PKG_VERSION").into()),
                })
                .unwrap_or_else(|e| HostMessage::Error {
                    v: protocol::PROTOCOL_VERSION,
                    reason: format!("probe failed: {e}"),
                })
        }
        ClientMessage::Ping { timestamp_ms, .. } => HostMessage::Pong {
            v: protocol::PROTOCOL_VERSION,
            timestamp_ms: *timestamp_ms,
        },
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_transfer_pieces_skip_the_message_rate_limit() {
        assert!(is_transfer_piece(r#"{"type":"file_chunk","v":1,"transfer_id":"abcdefgh","offset":0,"data":""}"#));
        assert!(is_transfer_piece(r#"{"v":1,"type":"file_pull","transfer_id":"abcdefgh","offset":0}"#));
        assert!(!is_transfer_piece(r#"{"type":"ping","v":1,"note":"\"type\":\"file_chunk\""}"#));
        assert!(!is_transfer_piece(r#"{"type":"file_offer","v":1}"#));
        assert!(!is_transfer_piece("not json"));
    }

    #[test]
    fn transfer_messages_validate_ids_sizes_and_hashes() {
        let ok = r#"{"type":"file_offer","v":1,"transfer_id":"t-123456","name":"a.jpg","size":3}"#;
        assert!(protocol::parse_client_message(ok).is_ok());
        let bad_id = r#"{"type":"file_offer","v":1,"transfer_id":"../x","name":"a.jpg","size":3}"#;
        assert!(protocol::parse_client_message(bad_id).is_err());
        let bad_hash = r#"{"type":"file_complete","v":1,"transfer_id":"t-123456","sha256":"zz"}"#;
        assert!(protocol::parse_client_message(bad_hash).is_err());
        let long_clip = format!(r#"{{"type":"clipboard","v":1,"text":"{}"}}"#, "x".repeat(protocol::MAX_CLIPBOARD_TEXT + 1));
        assert!(protocol::parse_client_message(&long_clip).is_err());
        let chunk = format!(r#"{{"type":"file_chunk","v":1,"transfer_id":"t-123456","offset":0,"data":"{}"}}"#, "A".repeat(60_000));
        assert!(protocol::parse_client_message(&chunk).is_err(), "chunks above 40 KiB are refused");
    }

    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn server_full_session_lifecycle() {
        let config = ServerConfig {
            ping_interval_secs: 0, // Disable pings for test determinism.
            ..Default::default()
        };
        let state = ServerState::new(config);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = Arc::clone(&state);

        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_connection(server_state, stream, peer).await
        });

        let url = format!("ws://{addr}");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Helper to send and receive.
        async fn send_recv(
            ws: &mut tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            message: &str,
        ) -> HostMessage {
            ws.send(Message::Text(message.into())).await.unwrap();
            let response = ws.next().await.unwrap().unwrap();
            let text = match response {
                Message::Text(t) => t.to_string(),
                other => panic!("expected text, got {other:?}"),
            };
            serde_json::from_str(&text).unwrap()
        }

        // 1. Hello
        let ack = send_recv(
            &mut ws,
            r#"{"type":"hello","v":1,"device_id":"lifecycle","token":"t"}"#,
        )
        .await;
        assert!(matches!(ack, HostMessage::HelloAck { .. }));

        // 2. Session list (empty)
        let list = send_recv(&mut ws, r#"{"type":"session_list","v":1}"#).await;
        match &list {
            HostMessage::SessionListResult { sessions, .. } => {
                assert!(sessions.is_empty());
            }
            other => panic!("expected SessionListResult, got {other:?}"),
        }

        // 3. Application-level ping
        let pong = send_recv(&mut ws, r#"{"type":"ping","v":1,"timestamp_ms":12345}"#).await;
        assert!(matches!(
            pong,
            HostMessage::Pong {
                timestamp_ms: 12345,
                ..
            }
        ));

        // 4. Probe
        let probe = send_recv(&mut ws, r#"{"type":"probe","v":1}"#).await;
        assert!(matches!(probe, HostMessage::ProbeResult { .. }));

        // 5. Duplicate hello (should error)
        let dup = send_recv(
            &mut ws,
            r#"{"type":"hello","v":1,"device_id":"dup","token":"t"}"#,
        )
        .await;
        assert!(matches!(dup, HostMessage::Error { .. }));

        let _ = ws.close(None).await;
        let _ = server.await;
    }

    #[tokio::test]
    async fn eviction_does_not_release_sessions() {
        // After eviction, the old connection's sessions should NOT be
        // released so the new connection can resume them.
        let config = ServerConfig {
            ping_interval_secs: 0,
            ..Default::default()
        };
        let state = ServerState::new(config);

        // Pre-populate a session owned by "phone".
        state.sessions.insert_fake("my-session");
        state
            .session_owners
            .lock()
            .await
            .insert("my-session".into(), "phone".into());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = Arc::clone(&state);

        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, peer) = listener.accept().await.unwrap();
                let s = Arc::clone(&server_state);
                tokio::spawn(async move {
                    let _ = handle_connection(s, stream, peer).await;
                });
            }
        });

        let url = format!("ws://{addr}");

        // First connection as "phone".
        let (mut ws1, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws1.send(Message::Text(
            r#"{"type":"hello","v":1,"device_id":"phone","token":"t"}"#.into(),
        ))
        .await
        .unwrap();
        let _ = ws1.next().await; // drain HelloAck

        // Second connection with same device_id evicts the first.
        let (mut ws2, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws2.send(Message::Text(
            r#"{"type":"hello","v":1,"device_id":"phone","token":"t"}"#.into(),
        ))
        .await
        .unwrap();
        let _ = ws2.next().await; // drain HelloAck

        // Wait a moment for the eviction to propagate.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Session should still be owned by "phone" (not released by the
        // evicted connection's cleanup).
        let owners = state.session_owners.lock().await;
        let owner = owners.get("my-session");
        assert_eq!(
            owner.map(|s| s.as_str()),
            Some("phone"),
            "session ownership should survive eviction"
        );

        let _ = ws1.close(None).await;
        let _ = ws2.close(None).await;
        let _ = server.await;
    }

    #[tokio::test]
    async fn server_admits_only_unlocked_control_and_rejects_arbitrary_commands() {
        let config = ServerConfig {
            ping_interval_secs: 0,
            ..Default::default()
        };
        let state = ServerState::new(config);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = Arc::clone(&state);

        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_connection(server_state, stream, peer).await
        });

        let url = format!("ws://{addr}");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Authenticate.
        ws.send(Message::Text(
            r#"{"type":"hello","v":1,"device_id":"phone","token":"t"}"#.into(),
        ))
        .await
        .unwrap();
        let _ = ws.next().await; // hello ack

        // Create session. In test environments without a compositor, this
        // returns SessionError instead of SessionCreated, which is correct.
        ws.send(Message::Text(
            r#"{"type":"session_request","v":1,"session_id":"notes","command":["/usr/bin/app"],"width":390,"height":844}"#.into()
        )).await.unwrap();

        let response = ws.next().await.unwrap().unwrap();
        let text = match response {
            Message::Text(t) => t.to_string(),
            other => panic!("expected text, got {other:?}"),
        };
        let msg: HostMessage = serde_json::from_str(&text).unwrap();
        assert!(matches!(&msg, HostMessage::Error { .. }), "arbitrary command reached session creation: {msg:?}");
        assert_eq!(state.sessions.session_count(), 0);
        // Normal authorized traffic remains usable after a refused launch.
        ws.send(Message::Text(r#"{"type":"ping","v":1,"timestamp_ms":123}"#.into())).await.unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap();
        let Message::Text(response) = response else { panic!("expected application pong") };
        assert!(matches!(serde_json::from_str::<HostMessage>(&response).unwrap(), HostMessage::Pong { .. }));
        // A lock must also end an already authenticated connection without
        // waiting for a phone message or its next keepalive.
        state.session_access.send_replace(false);
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await.unwrap().unwrap().unwrap();
        assert!(matches!(response, Message::Close(Some(frame)) if frame.code == CloseCode::Policy));

        let _ = ws.close(None).await;
        let _ = server.await;
    }

    #[tokio::test]
    async fn server_rejects_sdp_from_non_owner() {
        let config = ServerConfig {
            ping_interval_secs: 0,
            session_timeout_secs: 0,
            ..Default::default()
        };
        let state = ServerState::new(config);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = Arc::clone(&state);

        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_connection(server_state, stream, peer).await
        });

        let url = format!("ws://{addr}");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Authenticate.
        ws.send(Message::Text(
            r#"{"type":"hello","v":1,"device_id":"intruder","token":"t"}"#.into(),
        ))
        .await
        .unwrap();
        let _ = ws.next().await; // hello ack

        // Try sending SDP for a session we don't own.
        ws.send(Message::Text(
            r#"{"type":"sdp","v":1,"session_id":"not-my-session","sdp_type":"offer","sdp":"v=0..."}"#.into(),
        ))
        .await
        .unwrap();

        let response = ws.next().await.unwrap().unwrap();
        let text = match response {
            Message::Text(t) => t.to_string(),
            other => panic!("expected text, got {other:?}"),
        };
        let msg: HostMessage = serde_json::from_str(&text).unwrap();
        assert!(
            matches!(&msg, HostMessage::SessionError { reason, .. } if reason.contains("not the owner")),
            "expected SessionError for non-owner SDP, got {msg:?}"
        );

        // Also try ICE for a session we don't own.
        ws.send(Message::Text(
            r#"{"type":"ice","v":1,"session_id":"not-my-session","candidate":"c:1","sdp_m_line_index":0}"#.into(),
        ))
        .await
        .unwrap();

        let response = ws.next().await.unwrap().unwrap();
        let text = match response {
            Message::Text(t) => t.to_string(),
            other => panic!("expected text, got {other:?}"),
        };
        let msg: HostMessage = serde_json::from_str(&text).unwrap();
        assert!(
            matches!(&msg, HostMessage::SessionError { reason, .. } if reason.contains("not the owner")),
            "expected SessionError for non-owner ICE, got {msg:?}"
        );

        let _ = ws.close(None).await;
        let _ = server.await;
    }

    async fn recv_host(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    ) -> HostMessage {
        loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Text(t) => return serde_json::from_str(&t).unwrap(),
                Message::Ping(_) | Message::Pong(_) => continue,
                other => panic!("expected text, got {other:?}"),
            }
        }
    }

    async fn paired_server(
        config: ServerConfig,
    ) -> (
        Arc<ServerState>,
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
    ) {
        let state = ServerState::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = Arc::clone(&state);
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, peer)) = listener.accept().await else {
                    break;
                };
                let state = Arc::clone(&server_state);
                tokio::spawn(async move {
                    let _ = handle_connection(state, stream, peer).await;
                });
            }
        });
        (state, addr, server)
    }

    fn pairing_config() -> ServerConfig {
        ServerConfig {
            ping_interval_secs: 0,
            require_pairing: true,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn interactive_pairing_issues_bound_credential() {
        let (state, addr, server) = paired_server(pairing_config()).await;
        let url = format!("ws://{addr}");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws.send(Message::Text(
            r#"{"type":"pair_request","v":1,"device_id":"phone-a","device_name":"Test Phone","client_nonce":"00112233445566778899aabbccddeeff"}"#.into(),
        ))
        .await
        .unwrap();
        let HostMessage::PairPending {
            host_id,
            host_nonce,
            ..
        } = recv_host(&mut ws).await
        else {
            panic!("expected pair_pending");
        };
        assert_eq!(host_id, state.identity().id);
        let expected =
            crate::identity::verification_code("", "00112233445566778899aabbccddeeff", &host_nonce);
        let pending = state.approvals().list().await;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].code, expected);
        assert_eq!(pending[0].device_name, "Test Phone");
        state
            .approvals()
            .resolve(&expected, crate::approval::Decision::Accept)
            .await
            .unwrap();
        let HostMessage::PairResult {
            accepted: true,
            device_token: Some(token),
            ..
        } = recv_host(&mut ws).await
        else {
            panic!("expected accepted pair_result");
        };
        // Same connection continues with hello.
        ws.send(Message::Text(
            format!(r#"{{"type":"hello","v":1,"device_id":"phone-a","token":"{token}"}}"#).into(),
        ))
        .await
        .unwrap();
        let HostMessage::HelloAck {
            host_id: Some(ack_id),
            host_name: Some(_),
            ..
        } = recv_host(&mut ws).await
        else {
            panic!("expected hello_ack with identity");
        };
        assert_eq!(ack_id, state.identity().id);
        let _ = ws.close(None).await;

        // The credential is bound to its device ID.
        let (mut other, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        other
            .send(Message::Text(
                format!(r#"{{"type":"hello","v":1,"device_id":"phone-b","token":"{token}"}}"#)
                    .into(),
            ))
            .await
            .unwrap();
        let HostMessage::HelloReject { code, .. } = recv_host(&mut other).await else {
            panic!("expected rejection for another device id");
        };
        assert_eq!(code.as_deref(), Some("not_paired"));
        server.abort();
    }

    #[tokio::test]
    async fn declined_pairing_is_refused_and_cooled_down() {
        let (state, addr, server) = paired_server(pairing_config()).await;
        let url = format!("ws://{addr}");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        ws.send(Message::Text(
            r#"{"type":"pair_request","v":1,"device_id":"phone-d","device_name":"D","client_nonce":"00112233445566778899aabbccddeeff"}"#.into(),
        ))
        .await
        .unwrap();
        assert!(matches!(
            recv_host(&mut ws).await,
            HostMessage::PairPending { .. }
        ));
        state
            .approvals()
            .resolve("", crate::approval::Decision::Decline)
            .await
            .unwrap();
        assert!(matches!(
            recv_host(&mut ws).await,
            HostMessage::PairResult {
                accepted: false,
                ..
            }
        ));
        let (mut again, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        again
            .send(Message::Text(
                r#"{"type":"pair_request","v":1,"device_id":"phone-d","device_name":"D","client_nonce":"00112233445566778899aabbccddeeff"}"#.into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            recv_host(&mut again).await,
            HostMessage::PairResult {
                accepted: false,
                ..
            }
        ));
        assert_eq!(state.pairing.lock().await.device_token_count(), 0);
        server.abort();
    }

    #[tokio::test]
    async fn qr_token_pair_request_needs_no_prompt() {
        let (state, addr, server) = paired_server(pairing_config()).await;
        let (token, _) = state.mint_pairing_token().await.unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .unwrap();
        ws.send(Message::Text(
            format!(r#"{{"type":"pair_request","v":1,"device_id":"phone-q","device_name":"Q","client_nonce":"00112233445566778899aabbccddeeff","pairing_token":"{token}"}}"#).into(),
        ))
        .await
        .unwrap();
        assert!(matches!(
            recv_host(&mut ws).await,
            HostMessage::PairResult { accepted: true, .. }
        ));
        assert!(state.approvals().list().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn unpair_from_host_disconnects_device() {
        let (state, addr, server) = paired_server(pairing_config()).await;
        state
            .pairing
            .lock()
            .await
            .register_device("tok-unpair".into(), "phone-u".into(), "U".into())
            .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .unwrap();
        ws.send(Message::Text(
            r#"{"type":"hello","v":1,"device_id":"phone-u","token":"tok-unpair"}"#.into(),
        ))
        .await
        .unwrap();
        assert!(matches!(
            recv_host(&mut ws).await,
            HostMessage::HelloAck { .. }
        ));
        let devices = state.paired_devices().await;
        assert!(
            devices
                .iter()
                .any(|(d, peer)| d.device_id == "phone-u" && peer.is_some())
        );
        assert!(state.unpair_device("phone-u").await.unwrap());
        assert!(matches!(
            recv_host(&mut ws).await,
            HostMessage::Unpaired { .. }
        ));
        server.abort();
    }

    #[tokio::test]
    async fn client_unpair_revokes_its_credential() {
        let (state, addr, server) = paired_server(pairing_config()).await;
        state
            .pairing
            .lock()
            .await
            .register_device("tok-self".into(), "phone-s".into(), "S".into())
            .unwrap();
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .unwrap();
        ws.send(Message::Text(
            r#"{"type":"hello","v":1,"device_id":"phone-s","token":"tok-self"}"#.into(),
        ))
        .await
        .unwrap();
        assert!(matches!(
            recv_host(&mut ws).await,
            HostMessage::HelloAck { .. }
        ));
        ws.send(Message::Text(r#"{"type":"unpair","v":1}"#.into()))
            .await
            .unwrap();
        assert!(matches!(
            recv_host(&mut ws).await,
            HostMessage::Unpaired { .. }
        ));
        assert!(!state.pairing.lock().await.validate_device_token("tok-self"));
        server.abort();
    }

    #[tokio::test]
    async fn anonymous_loopback_only_when_allowed() {
        let (_state, addr, server) = paired_server(pairing_config()).await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .unwrap();
        ws.send(Message::Text(
            r#"{"type":"hello","v":1,"device_id":"anon","token":"anonymous"}"#.into(),
        ))
        .await
        .unwrap();
        assert!(matches!(
            recv_host(&mut ws).await,
            HostMessage::HelloReject { .. }
        ));
        server.abort();
        let (_state, addr, server) = paired_server(ServerConfig {
            allow_anonymous_loopback: true,
            ..pairing_config()
        })
        .await;
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .unwrap();
        ws.send(Message::Text(
            r#"{"type":"hello","v":1,"device_id":"anon","token":"anonymous"}"#.into(),
        ))
        .await
        .unwrap();
        assert!(matches!(
            recv_host(&mut ws).await,
            HostMessage::HelloAck { .. }
        ));
        server.abort();
    }

    #[tokio::test]
    async fn session_stop_requires_ownership() {
        let config = ServerConfig {
            ping_interval_secs: 0,
            ..Default::default()
        };
        let state = ServerState::new(config);

        // Pre-populate a session owned by "device-owner".
        state.sessions.insert_fake("owned-session");
        state
            .session_owners
            .lock()
            .await
            .insert("owned-session".into(), "device-owner".into());

        // A different device should not be able to stop it.
        let msg = ClientMessage::SessionStop {
            v: protocol::PROTOCOL_VERSION,
            session_id: "owned-session".into(),
        };
        let response = handle_client_message(&state, "other-device", &msg).await;
        match response {
            HostMessage::SessionError { reason, .. } => {
                assert!(
                    reason.contains("not the owner"),
                    "expected ownership error, got: {reason}"
                );
            }
            other => panic!("expected SessionError, got {other:?}"),
        }

        // The owner should be able to stop it (will fail at sessiond level
        // because it's a fake session, but won't get the ownership error).
        let response = handle_client_message(&state, "device-owner", &msg).await;
        assert!(
            !matches!(
                &response,
                HostMessage::SessionError { reason, .. }
                    if reason.contains("not the owner")
            ),
            "owner should not get ownership error, got {response:?}"
        );
    }

    #[tokio::test]
    async fn connection_idle_timeout_disconnects() {
        let config = ServerConfig {
            ping_interval_secs: 1,      // Short interval needed for timeout check.
            connection_timeout_secs: 3, // 3-second timeout (enough headroom for CI).
            ..Default::default()
        };
        let state = ServerState::new(config);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = Arc::clone(&state);

        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_connection(server_state, stream, peer).await
        });

        let url = format!("ws://{addr}");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Authenticate.
        ws.send(Message::Text(
            r#"{"type":"hello","v":1,"device_id":"idle-test","token":"t"}"#.into(),
        ))
        .await
        .unwrap();
        let _ = ws.next().await; // HelloAck

        // Don't send any messages — wait for idle timeout (3s + 1s ping interval).
        // The server should close with a CloseCode::Away.
        let _result = tokio::time::timeout(std::time::Duration::from_secs(7), ws.next()).await;

        // We should either get a close frame, a ping, or connection end.
        // The connection should eventually be closed.
        let server_result = server.await.unwrap();
        assert!(
            server_result.is_err(),
            "expected server error from idle timeout"
        );
    }

    #[tokio::test]
    async fn hello_timeout_closes_connection() {
        let config = ServerConfig {
            ping_interval_secs: 0,
            ..Default::default()
        };
        let state = ServerState::new(config);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = Arc::clone(&state);

        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_connection(server_state, stream, peer).await
        });

        let url = format!("ws://{addr}");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Don't send hello — wait for the server to time out (5s).
        // The server should close with a Policy close frame.
        let result = tokio::time::timeout(std::time::Duration::from_secs(7), ws.next()).await;

        match result {
            Ok(Some(Ok(Message::Close(Some(frame))))) => {
                assert_eq!(frame.code, CloseCode::Policy);
                assert!(
                    frame.reason.contains("hello timeout"),
                    "expected 'hello timeout' reason, got: {}",
                    frame.reason
                );
            }
            Ok(Some(Ok(Message::Close(None)))) => {
                // Some WebSocket implementations omit the close frame body.
            }
            Ok(None) => {
                // Stream ended — server closed the connection.
            }
            _other => {
                // Any result that indicates the connection was closed is acceptable.
                // The server should have returned an error.
                let server_result = server.await.unwrap();
                assert!(
                    server_result.is_err(),
                    "expected server error from timeout, got {server_result:?}"
                );
                return;
            }
        }

        let server_result = server.await.unwrap();
        assert!(
            server_result.is_err(),
            "expected server error from timeout, got {server_result:?}"
        );
    }

    #[tokio::test]
    async fn disconnect_preserves_session_ownership() {
        let config = ServerConfig {
            ping_interval_secs: 0,
            ..Default::default()
        };
        let state = ServerState::new(config);

        // Pre-populate a session owner.
        state
            .session_owners
            .lock()
            .await
            .insert("test-session".into(), "cleanup-device".into());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = Arc::clone(&state);

        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_connection(server_state, stream, peer).await
        });

        let url = format!("ws://{addr}");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

        // Authenticate as the same device that owns the session.
        ws.send(Message::Text(
            r#"{"type":"hello","v":1,"device_id":"cleanup-device","token":"t"}"#.into(),
        ))
        .await
        .unwrap();
        let _ = ws.next().await; // HelloAck

        // Verify the session is still owned.
        assert!(
            state
                .session_owners
                .lock()
                .await
                .contains_key("test-session")
        );

        // Disconnect.
        let _ = ws.close(None).await;
        let _ = server.await;

        // A disconnected phone retains its apps, but another phone cannot adopt them.
        let request = ClientMessage::StreamStart { v: 1, session_id: "test-session".into() };
        let foreign = handle_client_message(&state, "other-phone", &request).await;
        assert!(matches!(foreign, HostMessage::SessionError { reason, .. } if reason.contains("not the owner")));
        let owner = handle_client_message(&state, "cleanup-device", &request).await;
        assert!(matches!(owner, HostMessage::SessionError { reason, .. } if reason == "session not found"));

        // Connection should also be cleaned up.
        assert_eq!(state.connection_count().await, 0);
    }

    #[tokio::test]
    async fn stream_start_requires_ownership() {
        let config = ServerConfig {
            ping_interval_secs: 0,
            ..Default::default()
        };
        let state = ServerState::new(config);

        // Pre-populate a session owned by "device-owner".
        state.sessions.insert_fake("stream-session");
        state
            .session_owners
            .lock()
            .await
            .insert("stream-session".into(), "device-owner".into());

        // A different device should not be able to start the stream.
        let msg = ClientMessage::StreamStart {
            v: protocol::PROTOCOL_VERSION,
            session_id: "stream-session".into(),
        };
        let response = handle_client_message(&state, "other-device", &msg).await;
        assert!(
            matches!(
                &response,
                HostMessage::SessionError { reason, .. }
                    if reason.contains("not the owner")
            ),
            "expected ownership error, got {response:?}"
        );
    }

    #[tokio::test]
    async fn computer_home_does_not_create_sessions_or_media() {
        let state = ServerState::new(ServerConfig::default());
        let response =
            handle_client_message(&state, "phone", &ClientMessage::ComputerInfo { v: 1 }).await;
        assert!(matches!(response, HostMessage::ComputerInfoResult { .. }));
        assert_eq!(state.sessions.session_count(), 0);
        assert_eq!(state.webrtc_bridge.active_count().await, 0);
    }

    #[tokio::test]
    async fn session_resume_rebinds_ownership() {
        let config = ServerConfig {
            ping_interval_secs: 0,
            ..Default::default()
        };
        let state = ServerState::new(config);

        // Pre-populate a session owned by "device-a".
        state.sessions.insert_fake("resume-session");
        state
            .session_owners
            .lock()
            .await
            .insert("resume-session".into(), "device-a".into());

        // A different device resumes the session.
        // It will fail at the sessiond level (fake session), but ownership
        // logic is exercised regardless of the resume outcome.
        let msg = ClientMessage::SessionResume {
            v: protocol::PROTOCOL_VERSION,
            session_id: "resume-session".into(),
        };
        let _response = handle_client_message(&state, "device-b", &msg).await;

        // Whether resume succeeded or not at sessiond, the ownership should
        // be rebound to device-b only if resume succeeded. Since it's a fake
        // session and resume fails, the original owner should be unchanged.
        let owners = state.session_owners.lock().await;
        let owner = owners.get("resume-session");
        // With a fake session, resume fails so ownership stays with device-a.
        assert_eq!(
            owner.map(|s| s.as_str()),
            Some("device-a"),
            "ownership should not change on failed resume"
        );
    }

    #[tokio::test]
    async fn full_negotiation_lifecycle_through_handler() {
        // Server-offer model lifecycle:
        // Idle -> StreamStart -> WaitingForOffer -> (server sends offer
        // via pipeline) -> client sends Answer -> Negotiating -> StreamStop -> Idle
        let config = ServerConfig {
            ping_interval_secs: 0,
            ..Default::default()
        };
        let state = ServerState::new(config);

        state.sessions.insert_fake("neg-lifecycle");
        state
            .session_owners
            .lock()
            .await
            .insert("neg-lifecycle".into(), "device-a".into());

        // 1. Initial state: Idle.
        assert_eq!(
            state.sessions.negotiation_state("neg-lifecycle"),
            Some(crate::sessions::NegotiationState::Idle)
        );

        // 2. StreamStart -> WaitingForOffer (or Idle if binary missing).
        let msg = ClientMessage::StreamStart {
            v: protocol::PROTOCOL_VERSION,
            session_id: "neg-lifecycle".into(),
        };
        let response = handle_client_message(&state, "device-a", &msg).await;
        let binary_available = matches!(response, HostMessage::StreamStarted { .. });
        if binary_available {
            assert_eq!(
                state.sessions.negotiation_state("neg-lifecycle"),
                Some(crate::sessions::NegotiationState::WaitingForOffer)
            );

            // 3. Client sends SDP Answer (after receiving server's offer via
            //    broadcast) -> Negotiating.
            let msg = ClientMessage::Sdp {
                v: protocol::PROTOCOL_VERSION,
                session_id: "neg-lifecycle".into(),
                sdp_type: protocol::SdpType::Answer,
                sdp: "v=0\r\n".into(),
            };
            let _ = handle_client_message(&state, "device-a", &msg).await;
            assert_eq!(
                state.sessions.negotiation_state("neg-lifecycle"),
                Some(crate::sessions::NegotiationState::Negotiating)
            );

            // 4. StreamStop -> Idle (reset).
            let msg = ClientMessage::StreamStop {
                v: protocol::PROTOCOL_VERSION,
                session_id: "neg-lifecycle".into(),
            };
            let _ = handle_client_message(&state, "device-a", &msg).await;
        }
        // Whether binary is available or not, should end at Idle.
        assert_eq!(
            state.sessions.negotiation_state("neg-lifecycle"),
            Some(crate::sessions::NegotiationState::Idle)
        );
    }

    /// Helper: connect a WebSocket client, send Hello, and return the
    /// authenticated stream. Panics on failure.
    async fn connect_and_auth(
        addr: SocketAddr,
        device_id: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
        let url = format!("ws://{addr}");
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let hello = format!(
            r#"{{"type":"hello","v":{},"device_id":"{device_id}","token":"t"}}"#,
            protocol::PROTOCOL_VERSION
        );
        ws.send(Message::Text(hello.into())).await.unwrap();
        // Drain HelloAck.
        let resp = ws.next().await.unwrap().unwrap();
        if let Message::Text(t) = &resp {
            let ack: HostMessage = serde_json::from_str(t).unwrap();
            assert!(
                matches!(ack, HostMessage::HelloAck { .. }),
                "expected HelloAck, got {ack:?}"
            );
        } else {
            panic!("expected text, got {resp:?}");
        }
        ws
    }

    #[tokio::test]
    async fn handler_resize_by_non_owner_preserves_state() {
        let config = ServerConfig {
            ping_interval_secs: 0,
            ..Default::default()
        };
        let state = ServerState::new(config);
        state.sessions.insert_fake("resize-pres");
        state
            .session_owners
            .lock()
            .await
            .insert("resize-pres".into(), "device-a".into());
        state
            .sessions
            .force_negotiation_state("resize-pres", crate::sessions::NegotiationState::Connected);

        let msg = ClientMessage::SessionResize {
            v: protocol::PROTOCOL_VERSION,
            session_id: "resize-pres".into(),
            width: 844,
            height: 390,
            scale: 1.0,
            refresh_hz: 60,
        };
        let response = handle_client_message(&state, "device-b", &msg).await;
        assert!(
            matches!(&response, HostMessage::SessionError { reason, .. } if reason.contains("not the owner")),
            "expected ownership error, got {response:?}"
        );
        // Negotiation state should be untouched.
        assert_eq!(
            state.sessions.negotiation_state("resize-pres"),
            Some(crate::sessions::NegotiationState::Connected)
        );
    }

    /// End-to-end test: pair via one-time token, reconnect with device token,
    /// then ensure the static auth token still works.
    #[tokio::test]
    async fn end_to_end_pairing_then_reconnect() {
        let config = ServerConfig {
            ping_interval_secs: 0,
            auth_token: "static-secret".into(),
            ..Default::default()
        };
        let state = ServerState::new(config);

        // Add a one-time pairing token.
        let otp = "end2end-pair-token";
        {
            let mut pairing = state.pairing.lock().await;
            pairing.add_pairing_token(otp.into());
        }

        // Step 1: Pair via one-time token.
        let device_token = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let s = Arc::clone(&state);

            let server = tokio::spawn(async move {
                let (stream, peer) = listener.accept().await.unwrap();
                handle_connection(s, stream, peer).await
            });

            let url = format!("ws://{addr}");
            let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

            ws.send(Message::Text(
                format!(r#"{{"type":"hello","v":1,"device_id":"pairing-device","token":"{otp}"}}"#)
                    .into(),
            ))
            .await
            .unwrap();

            let response = ws.next().await.unwrap().unwrap();
            let text = match response {
                Message::Text(t) => t.to_string(),
                other => panic!("expected text, got {other:?}"),
            };
            let msg: HostMessage = serde_json::from_str(&text).unwrap();
            let dt = match msg {
                HostMessage::HelloAck { device_token, .. } => {
                    device_token.expect("should get device_token from pairing")
                }
                other => panic!("expected HelloAck, got {other:?}"),
            };

            let _ = ws.close(None).await;
            let _ = server.await;
            dt
        };

        assert_eq!(
            device_token.len(),
            64,
            "device token should be 64 hex chars"
        );

        // Step 2: Reconnect using the permanent device token.
        {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let s = Arc::clone(&state);

            let server = tokio::spawn(async move {
                let (stream, peer) = listener.accept().await.unwrap();
                handle_connection(s, stream, peer).await
            });

            let url = format!("ws://{addr}");
            let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

            ws.send(Message::Text(
                format!(
                    r#"{{"type":"hello","v":1,"device_id":"pairing-device","token":"{device_token}"}}"#
                )
                .into(),
            ))
            .await
            .unwrap();

            let response = ws.next().await.unwrap().unwrap();
            let text = match response {
                Message::Text(t) => t.to_string(),
                other => panic!("expected text, got {other:?}"),
            };
            let msg: HostMessage = serde_json::from_str(&text).unwrap();
            match msg {
                HostMessage::HelloAck {
                    device_token: dt, ..
                } => {
                    assert!(
                        dt.is_none(),
                        "returning device should NOT get a new device_token"
                    );
                }
                other => panic!("expected HelloAck for reconnect, got {other:?}"),
            }

            let _ = ws.close(None).await;
            let _ = server.await;
        }

        // Step 3: Static auth token still works.
        {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let s = Arc::clone(&state);

            let server = tokio::spawn(async move {
                let (stream, peer) = listener.accept().await.unwrap();
                handle_connection(s, stream, peer).await
            });

            let url = format!("ws://{addr}");
            let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

            ws.send(Message::Text(
                r#"{"type":"hello","v":1,"device_id":"static-auth-device","token":"static-secret"}"#.into(),
            ))
            .await
            .unwrap();

            let response = ws.next().await.unwrap().unwrap();
            let text = match response {
                Message::Text(t) => t.to_string(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(
                text.contains("hello_ack"),
                "static auth token should still work, got: {text}"
            );

            let _ = ws.close(None).await;
            let _ = server.await;
        }

        // Step 4: One-time token is consumed.
        {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let s = Arc::clone(&state);

            let server = tokio::spawn(async move {
                let (stream, peer) = listener.accept().await.unwrap();
                handle_connection(s, stream, peer).await
            });

            let url = format!("ws://{addr}");
            let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

            ws.send(Message::Text(
                format!(r#"{{"type":"hello","v":1,"device_id":"late-pairer","token":"{otp}"}}"#)
                    .into(),
            ))
            .await
            .unwrap();

            let response = ws.next().await.unwrap().unwrap();
            let text = match response {
                Message::Text(t) => t.to_string(),
                other => panic!("expected text, got {other:?}"),
            };
            assert!(
                text.contains("hello_reject"),
                "consumed pairing token should be rejected, got: {text}"
            );

            let _ = ws.close(None).await;
            let _ = server.await;
        }
    }

    /// Sustained rate-limit abuse should disconnect the client.
    #[tokio::test]
    async fn sustained_rate_limit_abuse_disconnects() {
        use tokio::time::{Duration, sleep};

        let config = ServerConfig {
            rate_limit_per_second: 2, // Very low for fast testing.
            ping_interval_secs: 0,
            connection_timeout_secs: 0,
            ..Default::default()
        };
        let state = ServerState::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_state = Arc::clone(&state);

        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_connection(server_state, stream, peer).await
        });

        let mut ws = connect_and_auth(addr, "abusive-client").await;

        // Flood messages for multiple seconds to trigger sustained abuse
        // disconnect (MAX_RATE_LIMIT_SECONDS = 5, rate_limit = 2).
        // We need 5+ consecutive seconds with >2 messages each.
        let mut disconnected = false;
        for _second in 0..8 {
            // Send 5 messages in rapid succession (exceeds limit of 2).
            for _burst in 0..5 {
                let msg = format!(
                    r#"{{"type":"session_list","v":{}}}"#,
                    protocol::PROTOCOL_VERSION,
                );
                if ws.send(Message::Text(msg.into())).await.is_err() {
                    disconnected = true;
                    break;
                }
                // Drain any response.
                match tokio::time::timeout(Duration::from_millis(100), ws.next()).await {
                    Ok(Some(Ok(Message::Text(t)))) => {
                        if t.contains("sustained abuse") {
                            disconnected = true;
                            break;
                        }
                    }
                    Ok(Some(Ok(Message::Close(_)))) | Ok(None) => {
                        disconnected = true;
                        break;
                    }
                    _ => {}
                }
            }
            if disconnected {
                break;
            }
            // Wait ~1 second for the rate window to rotate.
            sleep(Duration::from_millis(1050)).await;
        }

        assert!(
            disconnected,
            "client should have been disconnected after sustained abuse"
        );

        let result = server.await.unwrap();
        assert!(
            result.is_err(),
            "server handler should return Err(RateLimitExceeded)"
        );

        let limits = state.metrics.total_rate_limits.load(Ordering::Relaxed);
        assert!(
            limits >= 5,
            "expected at least 5 rate limit violations, got {limits}"
        );
    }
}
