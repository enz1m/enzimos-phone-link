use serde::{Deserialize, Serialize};

/// Signaling protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum message size in bytes (64 KiB is generous for signaling).
pub const MAX_MESSAGE_SIZE: usize = 65_536;

/// Messages from client to host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    /// Authenticate with a pairing token and declare capabilities.
    Hello {
        v: u32,
        device_id: String,
        token: String,
        #[serde(default)]
        capabilities: Capabilities,
    },
    /// Request a new session.
    SessionRequest {
        v: u32,
        session_id: String,
        command: Vec<String>,
        width: u32,
        height: u32,
        #[serde(default = "default_scale")]
        scale: f32,
        #[serde(default = "default_refresh_hz")]
        refresh_hz: u32,
    },
    /// Offer SDP for WebRTC negotiation.
    Sdp {
        v: u32,
        session_id: String,
        sdp_type: SdpType,
        sdp: String,
    },
    /// Send an ICE candidate.
    Ice {
        v: u32,
        session_id: String,
        candidate: String,
        sdp_m_line_index: u32,
    },
    /// Request viewport resize.
    SessionResize {
        v: u32,
        session_id: String,
        width: u32,
        height: u32,
        #[serde(default = "default_scale")]
        scale: f32,
        #[serde(default = "default_refresh_hz")]
        refresh_hz: u32,
    },
    /// Resume an interrupted session.
    SessionResume {
        v: u32,
        session_id: String,
    },
    /// Stop a session.
    SessionStop {
        v: u32,
        session_id: String,
    },
    /// Start streaming for a session (triggers WebRTC setup).
    StreamStart {
        v: u32,
        session_id: String,
    },
    /// Stop only the stream for reconnect (keeps compositor and app running).
    StreamStop {
        v: u32,
        session_id: String,
    },
    StreamPause {
        v: u32,
        session_id: String,
    },
    DesktopAttach {
        v: u32,
        session_id: String,
        #[serde(default)]
        output: Option<String>,
    },
    /// List active sessions.
    SessionList {
        v: u32,
    },
    /// Inspect a session's state.
    SessionInspect {
        v: u32,
        session_id: String,
    },
    /// Probe host capabilities.
    Probe {
        v: u32,
    },
    ComputerInfo {
        v: u32,
    },
    AppLaunch {
        v: u32,
        session_id: String,
        app_id: String,
        width: u32,
        height: u32,
        #[serde(default = "default_scale")]
        scale: f32,
        #[serde(default = "default_refresh_hz")]
        refresh_hz: u32,
    },
    /// Application-level ping for RTT measurement.
    Ping {
        v: u32,
        timestamp_ms: u64,
    },
    /// Ask an unpaired host for interactive approval (sent instead of `hello`).
    PairRequest {
        v: u32,
        device_id: String,
        device_name: String,
        /// Random client nonce (16-64 hex chars) bound into the verification code.
        client_nonce: String,
        /// One-time token from a host-displayed QR code. A valid token
        /// approves the request without a desktop prompt.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pairing_token: Option<String>,
    },
    /// Forget this device on the host. The host revokes its credential and
    /// closes the connection after `unpaired`.
    Unpair {
        v: u32,
    },
    /// Request the installed-application catalog (`app_list_result`).
    AppList {
        v: u32,
    },
    /// Request rendered icons for up to 32 catalog IDs (`app_icons_result`).
    AppIcons {
        v: u32,
        app_ids: Vec<String>,
    },
    /// Offer a file to the computer (`file_accepted` or `file_error`).
    FileOffer {
        v: u32,
        transfer_id: String,
        name: String,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
    },
    /// Next sequential piece of an accepted offer (`file_progress`).
    FileChunk {
        v: u32,
        transfer_id: String,
        offset: u64,
        /// Base64, at most `FILE_CHUNK_SIZE` bytes decoded.
        data: String,
    },
    /// All bytes sent; SHA-256 (hex) of the whole file (`file_saved`).
    FileComplete {
        v: u32,
        transfer_id: String,
        sha256: String,
    },
    /// Read part of a file the computer offered (`file_data`).
    FilePull {
        v: u32,
        transfer_id: String,
        offset: u64,
    },
    /// The phone stored a file the computer offered (`file_finished`).
    FileReceived { v: u32, transfer_id: String },
    /// Abort a transfer in either direction (`file_cancelled`).
    FileCancel {
        v: u32,
        transfer_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Opt in or out of receiving the computer's clipboard (`clipboard_ack`).
    ClipboardSync { v: u32, enabled: bool },
    /// Put text on the computer's clipboard (`clipboard_ack`).
    Clipboard { v: u32, text: String },
    /// A notification posted (or updated) on the phone (`ack`).
    PhoneNotification {
        v: u32,
        /// Stable key on the phone (updates reuse it).
        key: String,
        /// App package / bundle ID.
        app: String,
        app_name: String,
        title: String,
        #[serde(default)]
        text: String,
        /// Small PNG app icon, base64.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        icon: Option<String>,
        #[serde(default)]
        actions: Vec<NotificationAction>,
    },
    /// A phone notification went away (`ack`).
    PhoneNotificationRemoved { v: u32, key: String },
    /// Battery level and charging state (`ack`).
    PhoneStatus { v: u32, battery: u8, charging: bool },
    /// The computer's media players and volume (`media_state`).
    MediaStateRequest {
        v: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        player: Option<String>,
        /// Album art the phone already shows (`art_id`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        known_art: Option<String>,
    },
    /// play_pause, next, previous, seek (value: ms), set_volume (value: %),
    /// toggle_mute (`media_state`).
    MediaCommand {
        v: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        player: Option<String>,
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<f64>,
    },
    /// Touchpad / keyboard / presenter input for the computer's desktop (`ack`).
    RemoteInput { v: u32, events: Vec<crate::desktop_input::InputEvent> },
    /// Use the phone as an extra monitor of `width` x `height` pixels
    /// (`session_created`, then the usual `stream_start`). With `view`
    /// (hosts listing `monitors` in `computer_info`), show an existing monitor instead:
    /// `output` names its connector (default: primary) and `width` x
    /// `height` bounds the streamed size; `session_created` has the size used.
    SecondDisplayStart {
        v: u32,
        session_id: String,
        width: u32,
        height: u32,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        view: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// Touch and pointer input on a second display, in its pixels (`ack`).
    DisplayInput { v: u32, session_id: String, events: Vec<crate::second_display::DisplayEvent> },
    /// Send the phone's screen (`screen`) or camera (`camera`) to the
    /// computer; `sdp` is a send-only video offer (`uplink_answer`).
    UplinkStart { v: u32, uplink_id: String, kind: String, sdp: String },
    UplinkIce { v: u32, uplink_id: String, candidate: String, sdp_m_line_index: u32 },
    UplinkStop { v: u32, uplink_id: String },
    /// The phone shares its text messages (`ack`); `changed` reports that
    /// conversations changed since the computer last asked.
    SmsStatus {
        v: u32,
        enabled: bool,
        #[serde(default)]
        changed: bool,
    },
    /// The phone's answer to `sms_request` (`ack`): `{"ok":true,…}` or `{"ok":false,"error"}`.
    SmsResult { v: u32, request_id: String, result: serde_json::Value },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NotificationAction {
    pub id: String,
    pub title: String,
    /// Accepts typed text (messaging reply).
    #[serde(default)]
    pub reply: bool,
}

/// Decoded bytes per `file_chunk` / `file_data` (fits the 64 KiB message limit as base64).
pub const FILE_CHUNK_SIZE: usize = 40 * 1024;
/// Largest clipboard text either side sends.
pub const MAX_CLIPBOARD_TEXT: usize = 48 * 1024;

/// Asynchronous session events pushed from host to client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SessionEventKind {
    /// The session app exited normally.
    AppExited,
    /// The session compositor crashed.
    CompositorCrashed,
    /// The media stream ended.
    StreamEnded,
    /// The session was stopped by another client.
    StoppedExternally,
    /// The session was garbage-collected due to inactivity.
    GarbageCollected,
    /// A new device resumed the session (ownership transferred).
    OwnershipTransferred,
    /// The session's viewport was resized.
    Resized,
}

/// Capabilities that can be negotiated between client and host.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// Supported input types.
    #[serde(default)]
    pub input: Vec<String>,
    /// Supported video codecs.
    #[serde(default)]
    pub video_codecs: Vec<String>,
    /// Whether the client supports viewport resize.
    #[serde(default)]
    pub resize: bool,
    /// Whether the client supports clipboard.
    #[serde(default)]
    pub clipboard: bool,
    /// Whether the client supports audio.
    #[serde(default)]
    pub audio: bool,
}

/// One rendered application icon.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppIcon {
    pub app_id: String,
    /// Icon revision (matches `AppEntry.icon`).
    pub icon: String,
    /// Base64-encoded PNG.
    pub png: String,
}

/// Messages from host to client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostMessage {
    /// Authentication accepted with host capabilities.
    HelloAck {
        v: u32,
        capabilities: Capabilities,
        /// Server software version string.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_version: Option<String>,
        /// Permanent device token issued during QR pairing.
        /// Only present when the client authenticates with a one-time
        /// pairing token. The client should store this token and use it
        /// for all future connections instead of the one-time token.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_token: Option<String>,
        /// Stable host identity (32 hex chars), independent of address.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_id: Option<String>,
        /// Human-readable computer name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_name: Option<String>,
    },
    /// Authentication rejected.
    HelloReject {
        v: u32,
        reason: String,
        /// Machine-readable cause. `not_paired` means the credential is
        /// unknown or revoked: the client should offer to pair again.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
    /// A pairing request is waiting for approval on the computer.
    PairPending {
        v: u32,
        host_id: String,
        host_name: String,
        host_nonce: String,
        timeout_secs: u32,
    },
    /// Final pairing decision. On acceptance the connection stays open and
    /// the client continues with `hello` using `device_token`.
    PairResult {
        v: u32,
        accepted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// This device was unpaired; the connection closes next.
    Unpaired { v: u32 },
    /// Installed applications, sorted by name. `revision` changes when the
    /// catalog does (also reported in `computer_info.app_revision`).
    AppListResult {
        v: u32,
        revision: String,
        apps: Vec<crate::launcher::AppEntry>,
    },
    /// Rendered 128 px PNG icons (base64); unknown IDs or icons are omitted.
    AppIconsResult { v: u32, icons: Vec<AppIcon> },
    /// The computer offers a file (pull it with `file_pull`).
    FileOffer {
        v: u32,
        transfer_id: String,
        name: String,
        size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        sha256: String,
    },
    /// Offer accepted; send `file_chunk`s of at most `chunk_size` bytes.
    FileAccepted { v: u32, transfer_id: String, chunk_size: u32 },
    /// Bytes stored so far (acknowledges a `file_chunk`).
    FileProgress { v: u32, transfer_id: String, received: u64 },
    /// File verified and saved on the computer under `name`.
    FileSaved { v: u32, transfer_id: String, name: String },
    /// Requested piece of an offered file.
    FileData {
        v: u32,
        transfer_id: String,
        offset: u64,
        data: String,
        eof: bool,
    },
    /// The computer recorded a completed download.
    FileFinished { v: u32, transfer_id: String },
    FileCancelled {
        v: u32,
        transfer_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    FileError { v: u32, transfer_id: String, reason: String },
    /// The computer's clipboard changed (sent only after `clipboard_sync`).
    Clipboard { v: u32, text: String },
    ClipboardAck { v: u32 },
    /// Generic acknowledgement.
    Ack { v: u32 },
    /// Invoke a phone notification action (optionally with reply text).
    NotificationAction {
        v: u32,
        key: String,
        action_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Dismiss a phone notification (dismissed on the computer).
    NotificationDismiss { v: u32, key: String },
    /// Start or stop ringing the phone (find my phone).
    Ring { v: u32, ringing: bool },
    MediaState { v: u32, state: crate::media::MediaState },
    UplinkAnswer { v: u32, uplink_id: String, sdp: String },
    UplinkIce { v: u32, uplink_id: String, candidate: String, sdp_m_line_index: u32 },
    /// Pointer / keys from the computer's mirror window for the phone.
    UplinkInput { v: u32, uplink_id: String, event: serde_json::Value },
    /// The computer closed the mirror window or the receiver ended.
    UplinkStopped { v: u32, uplink_id: String },
    /// Ask the phone for conversations, a thread, or to send a text
    /// (answered by the phone with `sms_result`).
    SmsRequest { v: u32, request_id: String, request: serde_json::Value },
    /// Session created successfully.
    SessionCreated {
        v: u32,
        session_id: String,
        width: u32,
        height: u32,
        scale: f32,
        refresh_hz: u32,
    },
    /// Session creation failed.
    SessionError {
        v: u32,
        session_id: String,
        reason: String,
    },
    /// SDP answer/offer from host.
    Sdp {
        v: u32,
        session_id: String,
        sdp_type: SdpType,
        sdp: String,
    },
    /// ICE candidate from host.
    Ice {
        v: u32,
        session_id: String,
        candidate: String,
        sdp_m_line_index: u32,
    },
    /// Session resized.
    SessionResized {
        v: u32,
        session_id: String,
        width: u32,
        height: u32,
        scale: f32,
        refresh_hz: u32,
    },
    /// Session resumed.
    SessionResumed {
        v: u32,
        session_id: String,
        width: u32,
        height: u32,
        #[serde(default = "default_scale")]
        scale: f32,
        #[serde(default = "default_refresh_hz")]
        refresh_hz: u32,
    },
    /// Session stopped.
    SessionStopped {
        v: u32,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Stream started (ready for WebRTC negotiation).
    StreamStarted { v: u32, session_id: String },
    /// Stream stopped for reconnect.
    StreamStopped {
        v: u32,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Active sessions list.
    SessionListResult { v: u32, sessions: Vec<SessionInfo> },
    ComputerInfoResult {
        v: u32,
        computer: crate::desktop::ComputerInfo,
    },
    /// Session inspection result.
    SessionInspection {
        v: u32,
        session_id: String,
        width: u32,
        height: u32,
        #[serde(default = "default_scale")]
        scale: f32,
        #[serde(default = "default_refresh_hz")]
        refresh_hz: u32,
        compositor_alive: bool,
        app_alive: bool,
        stream_alive: bool,
        /// The command that was launched for this session (if launched_app).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        command: Vec<String>,
        /// WebRTC negotiation state.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        negotiation_state: Option<String>,
    },
    /// Host capability probe result.
    ProbeResult {
        v: u32,
        pipewire_running: bool,
        render_nodes: Vec<String>,
        h264_encoders: Vec<String>,
        av1_encoders: Vec<String>,
        #[serde(default)]
        active_sessions: u32,
        #[serde(default)]
        active_connections: u32,
        /// Server uptime in seconds.
        #[serde(default)]
        server_uptime_secs: u64,
        /// Maximum sessions allowed (0 = unlimited).
        #[serde(default)]
        max_sessions: u32,
        /// Server hostname for identification.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hostname: Option<String>,
        /// Total connections ever accepted (since server start).
        #[serde(default)]
        total_connections: u64,
        /// Total messages received across all connections.
        #[serde(default)]
        total_messages: u64,
        /// Peak concurrent authenticated connections (high-water mark).
        #[serde(default)]
        peak_connections: u64,
        /// Total authentication failures.
        #[serde(default)]
        total_auth_failures: u64,
        /// Total rate limit violations.
        #[serde(default)]
        total_rate_limits: u64,
        /// Total sessions ever created.
        #[serde(default)]
        total_sessions_created: u64,
        /// Server software version (e.g., "0.1.0").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server_version: Option<String>,
    },
    /// Application-level pong response.
    Pong { v: u32, timestamp_ms: u64 },
    /// Asynchronous session event notification (server-push).
    SessionEvent {
        v: u32,
        session_id: String,
        event: SessionEventKind,
        /// Human-readable cause, e.g. why an app could not start.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// General protocol error.
    Error { v: u32, reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SdpType {
    Offer,
    Answer,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SessionInfo {
    pub session_id: String,
    pub width: u32,
    pub height: u32,
    pub active: bool,
    /// How long the session has been alive, in seconds.
    #[serde(default)]
    pub uptime_secs: u64,
    /// HiDPI scale factor.
    #[serde(default = "default_scale")]
    pub scale: f32,
    /// Refresh rate in Hz.
    #[serde(default = "default_refresh_hz")]
    pub refresh_hz: u32,
    /// Device ID that owns/created this session (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_device_id: Option<String>,
    /// WebRTC negotiation state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiation_state: Option<String>,
    /// Catalog ID of the app this session was launched for, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

// Re-use the canonical defaults from the session crate to prevent divergence.
use enzremote_session::default_refresh_hz;
use enzremote_session::default_scale;

/// Parse and validate a client message from JSON text.
///
/// Enforces the 64 KiB size limit, deserializes the JSON, then validates
/// all fields (version, session IDs, dimensions, capabilities, etc.).
pub fn parse_client_message(text: &str) -> Result<ClientMessage, String> {
    if text.len() > MAX_MESSAGE_SIZE {
        return Err("message exceeds maximum size".into());
    }
    let message: ClientMessage =
        serde_json::from_str(text).map_err(|error| format!("invalid message: {error}"))?;
    validate_client_message(&message)?;
    Ok(message)
}

/// Parse and validate a host message from JSON text.
///
/// Enforces the same size limit, deserialization, and field validation
/// as [`parse_client_message`] but for host-to-client messages.
pub fn parse_host_message(text: &str) -> Result<HostMessage, String> {
    if text.len() > MAX_MESSAGE_SIZE {
        return Err("message exceeds maximum size".into());
    }
    let message: HostMessage =
        serde_json::from_str(text).map_err(|error| format!("invalid message: {error}"))?;
    validate_host_message(&message)?;
    Ok(message)
}

/// Validate all fields of a deserialized [`ClientMessage`].
///
/// Checks version, string lengths, character sets, dimensions, scale/refresh
/// bounds, SDP/ICE size limits, and capability allowlists.
fn validate_client_message(message: &ClientMessage) -> Result<(), String> {
    match message {
        ClientMessage::Hello {
            v,
            device_id,
            token,
            capabilities,
        } => {
            validate_version(*v)?;
            if device_id.is_empty() || device_id.len() > 128 {
                return Err("device_id must be 1-128 characters".into());
            }
            // Restrict device_id to safe characters to prevent log injection.
            if !device_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            {
                return Err(
                    "device_id must contain only ASCII letters, digits, dashes, dots, or underscores"
                        .into(),
                );
            }
            if token.is_empty() || token.len() > 512 {
                return Err("token must be 1-512 characters".into());
            }
            // Reject control characters to prevent log injection.
            if token.bytes().any(|b| b.is_ascii_control()) {
                return Err("token must not contain control characters".into());
            }
            validate_capabilities(capabilities)?;
        }
        ClientMessage::SessionRequest {
            v,
            session_id,
            command,
            width,
            height,
            scale,
            refresh_hz,
        } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            if command.is_empty() {
                return Err("command must not be empty".into());
            }
            if command.len() > 64 {
                return Err("command must have at most 64 elements".into());
            }
            for arg in command {
                if arg.is_empty() {
                    return Err("command arguments must not be empty".into());
                }
                if arg.contains('\0') {
                    return Err("command arguments must not contain NUL bytes".into());
                }
                if arg.len() > 4096 {
                    return Err("command arguments must be at most 4096 bytes".into());
                }
            }
            validate_dimensions(*width, *height)?;
            if !scale.is_finite() || !(0.5..=4.0).contains(scale) {
                return Err("scale must be finite and between 0.5 and 4.0".into());
            }
            if !(15..=240).contains(refresh_hz) {
                return Err("refresh_hz must be between 15 and 240".into());
            }
        }
        ClientMessage::Sdp {
            v, session_id, sdp, ..
        } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            if sdp.is_empty() {
                return Err("SDP must not be empty".into());
            }
            if sdp.len() > 32768 {
                return Err("SDP must be at most 32768 bytes".into());
            }
        }
        ClientMessage::Ice {
            v,
            session_id,
            candidate,
            sdp_m_line_index,
        } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            if candidate.is_empty() {
                return Err("ICE candidate must not be empty".into());
            }
            if candidate.len() > 4096 {
                return Err("ICE candidate must be at most 4096 bytes".into());
            }
            if *sdp_m_line_index > 255 {
                return Err("sdp_m_line_index must be at most 255".into());
            }
        }
        ClientMessage::SessionResize {
            v,
            session_id,
            width,
            height,
            scale,
            refresh_hz,
        } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            validate_dimensions(*width, *height)?;
            if !scale.is_finite() || !(0.5..=4.0).contains(scale) {
                return Err("scale must be finite and between 0.5 and 4.0".into());
            }
            if !(15..=240).contains(refresh_hz) {
                return Err("refresh_hz must be between 15 and 240".into());
            }
        }
        ClientMessage::SessionResume { v, session_id } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
        }
        ClientMessage::SessionStop { v, session_id } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
        }
        ClientMessage::SessionList { v } => {
            validate_version(*v)?;
        }
        ClientMessage::StreamStart { v, session_id } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
        }
        ClientMessage::StreamStop { v, session_id }
        | ClientMessage::StreamPause { v, session_id } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
        }
        ClientMessage::SessionInspect { v, session_id } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
        }
        ClientMessage::Probe { v } | ClientMessage::ComputerInfo { v } => {
            validate_version(*v)?;
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
            validate_version(*v)?;
            validate_session_id(session_id)?;
            validate_session_id(app_id)?;
            validate_dimensions(*width, *height)?;
            if !scale.is_finite() || !(0.5..=4.0).contains(scale) {
                return Err("scale must be finite and between 0.5 and 4.0".into());
            }
            if !(15..=240).contains(refresh_hz) {
                return Err("refresh_hz must be between 15 and 240".into());
            }
        }
        ClientMessage::DesktopAttach {
            v,
            session_id,
            output,
        } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            if output.as_ref().is_some_and(|name| {
                name.is_empty()
                    || name.len() > 80
                    || !name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            }) {
                return Err("Invalid desktop output name".into());
            }
        }
        ClientMessage::PairRequest {
            v,
            device_id,
            device_name,
            client_nonce,
            pairing_token,
        } => {
            validate_version(*v)?;
            if let Some(token) = pairing_token
                && (token.is_empty()
                    || token.len() > 512
                    || token.bytes().any(|b| b.is_ascii_control()))
            {
                return Err("pairing_token must be 1-512 printable characters".into());
            }
            if device_id.is_empty()
                || device_id.len() > 128
                || !device_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            {
                return Err("device_id must be 1-128 safe characters".into());
            }
            if device_name.len() > 256 {
                return Err("device_name too long".into());
            }
            if client_nonce.len() < 16
                || client_nonce.len() > 64
                || !client_nonce.bytes().all(|b| b.is_ascii_hexdigit())
            {
                return Err("client_nonce must be 16-64 hex characters".into());
            }
        }
        ClientMessage::Unpair { v } | ClientMessage::AppList { v } => validate_version(*v)?,
        ClientMessage::AppIcons { v, app_ids } => {
            validate_version(*v)?;
            if app_ids.is_empty() || app_ids.len() > 32 {
                return Err("app_ids must list 1-32 apps".into());
            }
            for id in app_ids {
                validate_session_id(id)?;
            }
        }
        ClientMessage::Ping { v, .. } => {
            validate_version(*v)?;
        }
        ClientMessage::FileOffer { v, transfer_id, name, mime, .. } => {
            validate_version(*v)?;
            validate_transfer_id(transfer_id)?;
            if name.trim().is_empty() || name.len() > 512 {
                return Err("file name must be 1-512 bytes".into());
            }
            if mime.as_ref().is_some_and(|m| m.len() > 255) {
                return Err("mime type too long".into());
            }
        }
        ClientMessage::FileChunk { v, transfer_id, data, .. } => {
            validate_version(*v)?;
            validate_transfer_id(transfer_id)?;
            if data.len() > FILE_CHUNK_SIZE.div_ceil(3) * 4 {
                return Err("file chunk too large".into());
            }
        }
        ClientMessage::FileComplete { v, transfer_id, sha256 } => {
            validate_version(*v)?;
            validate_transfer_id(transfer_id)?;
            if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("sha256 must be 64 hex characters".into());
            }
        }
        ClientMessage::FilePull { v, transfer_id, .. } | ClientMessage::FileReceived { v, transfer_id } => {
            validate_version(*v)?;
            validate_transfer_id(transfer_id)?;
        }
        ClientMessage::FileCancel { v, transfer_id, reason } => {
            validate_version(*v)?;
            validate_transfer_id(transfer_id)?;
            if reason.as_ref().is_some_and(|r| r.len() > 512) {
                return Err("cancel reason too long".into());
            }
        }
        ClientMessage::ClipboardSync { v, .. } => validate_version(*v)?,
        ClientMessage::PhoneNotification { v, key, app, app_name, title, text, icon, actions } => {
            validate_version(*v)?;
            let short = |s: &str, max: usize| s.len() <= max;
            if key.is_empty() || !short(key, 512) || !short(app, 255) || !short(app_name, 255)
                || !short(title, 1024) || !short(text, 8192)
            {
                return Err("notification fields too long".into());
            }
            if icon.as_ref().is_some_and(|i| i.len() > 32 * 1024) {
                return Err("notification icon too large".into());
            }
            if actions.len() > 6 || actions.iter().any(|a| !short(&a.id, 64) || !short(&a.title, 128)) {
                return Err("too many or too long notification actions".into());
            }
        }
        ClientMessage::PhoneNotificationRemoved { v, key } => {
            validate_version(*v)?;
            if key.is_empty() || key.len() > 512 {
                return Err("notification key must be 1-512 bytes".into());
            }
        }
        ClientMessage::MediaStateRequest { v, player, known_art } => {
            validate_version(*v)?;
            if player.as_ref().is_some_and(|p| p.len() > 255) || known_art.as_ref().is_some_and(|a| a.len() > 64) {
                return Err("media request fields too long".into());
            }
        }
        ClientMessage::MediaCommand { v, player, command, value } => {
            validate_version(*v)?;
            if player.as_ref().is_some_and(|p| p.len() > 255)
                || !["play_pause", "next", "previous", "seek", "set_volume", "toggle_mute"].contains(&command.as_str())
                || value.is_some_and(|x| !x.is_finite())
            {
                return Err("invalid media command".into());
            }
        }
        ClientMessage::RemoteInput { v, events } => {
            use crate::desktop_input::InputEvent;
            validate_version(*v)?;
            if events.is_empty() || events.len() > 64 {
                return Err("remote_input needs 1-64 events".into());
            }
            let finite = |x: f64| x.is_finite() && x.abs() <= 10_000.0;
            for event in events {
                let ok = match event {
                    InputEvent::Move { dx, dy } | InputEvent::Scroll { dx, dy, .. } => finite(*dx) && finite(*dy),
                    InputEvent::Key { code, .. } => *code <= 0x2ff,
                    InputEvent::Text { text } => text.len() <= 4096,
                    InputEvent::Button { .. } | InputEvent::Click { .. } => true,
                };
                if !ok {
                    return Err("invalid remote input event".into());
                }
            }
        }
        ClientMessage::SecondDisplayStart { v, session_id, width, height, output, .. } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            if output.as_ref().is_some_and(|o| o.is_empty() || o.len() > 64 || !o.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')) {
                return Err("invalid output name".into());
            }
            if !(320..=4096).contains(width) || !(320..=4096).contains(height) {
                return Err("display size must be 320-4096 pixels".into());
            }
        }
        ClientMessage::DisplayInput { v, session_id, events } => {
            use crate::second_display::DisplayEvent;
            validate_version(*v)?;
            validate_session_id(session_id)?;
            if events.is_empty() || events.len() > 64 {
                return Err("display_input needs 1-64 events".into());
            }
            let ok = |x: f64| x.is_finite() && x.abs() <= 10_000.0;
            if events.iter().any(|e| match e {
                DisplayEvent::Down { slot, x, y } | DisplayEvent::Move { slot, x, y } => *slot > 9 || !ok(*x) || !ok(*y),
                DisplayEvent::Up { slot } => *slot > 9,
                DisplayEvent::Pointer { x, y } | DisplayEvent::Scroll { dx: x, dy: y } => !ok(*x) || !ok(*y),
                DisplayEvent::Button { button, .. } => !(0x110..=0x117).contains(button),
            }) {
                return Err("invalid display input".into());
            }
        }
        ClientMessage::UplinkStart { v, uplink_id, kind, sdp } => {
            validate_version(*v)?;
            validate_transfer_id(uplink_id)?;
            if kind != "screen" && kind != "camera" {
                return Err("uplink kind must be screen or camera".into());
            }
            if sdp.is_empty() || sdp.len() > 32 * 1024 {
                return Err("uplink sdp must be 1-32768 bytes".into());
            }
        }
        ClientMessage::UplinkIce { v, uplink_id, candidate, .. } => {
            validate_version(*v)?;
            validate_transfer_id(uplink_id)?;
            if candidate.len() > 4096 {
                return Err("ice candidate too long".into());
            }
        }
        ClientMessage::UplinkStop { v, uplink_id } => {
            validate_version(*v)?;
            validate_transfer_id(uplink_id)?;
        }
        ClientMessage::SmsStatus { v, .. } => validate_version(*v)?,
        ClientMessage::SmsResult { v, request_id, .. } => {
            validate_version(*v)?;
            validate_transfer_id(request_id)?;
        }
        ClientMessage::PhoneStatus { v, battery, .. } => {
            validate_version(*v)?;
            if *battery > 100 {
                return Err("battery must be 0-100".into());
            }
        }
        ClientMessage::Clipboard { v, text } => {
            validate_version(*v)?;
            if text.len() > MAX_CLIPBOARD_TEXT {
                return Err("clipboard text too long".into());
            }
        }
    }
    Ok(())
}

/// Transfer IDs: 8-64 characters of `[A-Za-z0-9_-]` (client generated, random).
pub fn validate_transfer_id(id: &str) -> Result<(), String> {
    if !(8..=64).contains(&id.len()) || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)) {
        return Err("transfer_id must be 8-64 letters, digits, dashes or underscores".into());
    }
    Ok(())
}

/// Validate all fields of a deserialized [`HostMessage`].
///
/// Mirrors the client-side checks: version, session IDs, dimensions,
/// SDP/ICE limits, and reason string lengths.
fn validate_host_message(message: &HostMessage) -> Result<(), String> {
    match message {
        HostMessage::HelloAck { v, .. } => {
            validate_version(*v)?;
        }
        HostMessage::PairPending { v, host_nonce, .. } => {
            validate_version(*v)?;
            if host_nonce.len() > 64 {
                return Err("host_nonce too long".into());
            }
        }
        HostMessage::PairResult { v, .. }
        | HostMessage::Unpaired { v }
        | HostMessage::AppListResult { v, .. }
        | HostMessage::AppIconsResult { v, .. }
        | HostMessage::FileOffer { v, .. }
        | HostMessage::FileAccepted { v, .. }
        | HostMessage::FileProgress { v, .. }
        | HostMessage::FileSaved { v, .. }
        | HostMessage::FileData { v, .. }
        | HostMessage::FileFinished { v, .. }
        | HostMessage::FileCancelled { v, .. }
        | HostMessage::FileError { v, .. }
        | HostMessage::Clipboard { v, .. }
        | HostMessage::ClipboardAck { v }
        | HostMessage::Ack { v }
        | HostMessage::NotificationAction { v, .. }
        | HostMessage::NotificationDismiss { v, .. }
        | HostMessage::Ring { v, .. }
        | HostMessage::MediaState { v, .. }
        | HostMessage::UplinkAnswer { v, .. }
        | HostMessage::UplinkIce { v, .. }
        | HostMessage::UplinkInput { v, .. }
        | HostMessage::UplinkStopped { v, .. }
        | HostMessage::SmsRequest { v, .. } => {
            validate_version(*v)?;
        }
        HostMessage::HelloReject { v, reason, .. } => {
            validate_version(*v)?;
            if reason.len() > 4096 {
                return Err("rejection reason too long".into());
            }
        }
        HostMessage::SessionCreated {
            v,
            session_id,
            width,
            height,
            scale,
            refresh_hz,
        } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            validate_dimensions(*width, *height)?;
            if !scale.is_finite() || !(0.5..=4.0).contains(scale) {
                return Err("scale must be finite and between 0.5 and 4.0".into());
            }
            if !(15..=240).contains(refresh_hz) {
                return Err("refresh_hz must be between 15 and 240".into());
            }
        }
        HostMessage::SessionError {
            v,
            session_id,
            reason,
        } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            if reason.len() > 4096 {
                return Err("error reason too long".into());
            }
        }
        HostMessage::Sdp {
            v, session_id, sdp, ..
        } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            if sdp.is_empty() {
                return Err("SDP must not be empty".into());
            }
            if sdp.len() > 32768 {
                return Err("SDP must be at most 32768 bytes".into());
            }
        }
        HostMessage::Ice {
            v,
            session_id,
            candidate,
            sdp_m_line_index,
        } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            if candidate.is_empty() {
                return Err("ICE candidate must not be empty".into());
            }
            if candidate.len() > 4096 {
                return Err("ICE candidate must be at most 4096 bytes".into());
            }
            if *sdp_m_line_index > 255 {
                return Err("sdp_m_line_index must be at most 255".into());
            }
        }
        HostMessage::SessionResized {
            v,
            session_id,
            width,
            height,
            scale,
            refresh_hz,
        } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            validate_dimensions(*width, *height)?;
            if !scale.is_finite() || !(0.5..=4.0).contains(scale) {
                return Err("scale must be finite and between 0.5 and 4.0".into());
            }
            if !(15..=240).contains(refresh_hz) {
                return Err("refresh_hz must be between 15 and 240".into());
            }
        }
        HostMessage::SessionResumed {
            v,
            session_id,
            width,
            height,
            scale,
            refresh_hz,
        } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            validate_dimensions(*width, *height)?;
            if !scale.is_finite() || !(0.5..=4.0).contains(scale) {
                return Err("scale must be finite and between 0.5 and 4.0".into());
            }
            if !(15..=240).contains(refresh_hz) {
                return Err("refresh_hz must be between 15 and 240".into());
            }
        }
        HostMessage::SessionStopped { v, session_id, .. } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
        }
        HostMessage::StreamStarted { v, session_id } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
        }
        HostMessage::StreamStopped { v, session_id, .. } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
        }
        HostMessage::SessionListResult { v, .. } => {
            validate_version(*v)?;
        }
        HostMessage::SessionInspection { v, session_id, .. } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
        }
        HostMessage::ProbeResult { v, .. } | HostMessage::ComputerInfoResult { v, .. } => {
            validate_version(*v)?;
        }
        HostMessage::Pong { v, .. } => {
            validate_version(*v)?;
        }
        HostMessage::SessionEvent {
            v,
            session_id,
            detail,
            ..
        } => {
            validate_version(*v)?;
            validate_session_id(session_id)?;
            if detail.as_ref().is_some_and(|d| d.len() > 1024) {
                return Err("session event detail too long".into());
            }
        }
        HostMessage::Error { v, reason } => {
            validate_version(*v)?;
            if reason.len() > 4096 {
                return Err("error reason too long".into());
            }
        }
    }
    Ok(())
}

/// Reject messages with a protocol version other than [`PROTOCOL_VERSION`].
fn validate_version(v: u32) -> Result<(), String> {
    if v != PROTOCOL_VERSION {
        return Err(format!(
            "unsupported protocol version {v}; expected {PROTOCOL_VERSION}"
        ));
    }
    Ok(())
}

/// Validate that a session ID is 1–64 lowercase ASCII alphanumeric characters,
/// dashes, or underscores.
fn validate_session_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_".contains(&b))
    {
        return Err(
            "session_id must be 1-64 lowercase ASCII letters, digits, dashes, or underscores"
                .into(),
        );
    }
    Ok(())
}

/// Validate that width and height are each between 240 and 8192 pixels.
fn validate_dimensions(width: u32, height: u32) -> Result<(), String> {
    if !(240..=8192).contains(&width) || !(240..=8192).contains(&height) {
        return Err("dimensions must each be between 240 and 8192 pixels".into());
    }
    Ok(())
}

/// Known input types for capability negotiation.
pub const KNOWN_INPUT_TYPES: &[&str] =
    &["pointer", "pointer_relative", "touch", "keyboard", "scroll"];

/// Known video codecs for capability negotiation.
pub const KNOWN_VIDEO_CODECS: &[&str] = &["h264", "h265", "av1", "vp8"];

/// Validate capabilities against the known input types and video codecs allowlists.
fn validate_capabilities(caps: &Capabilities) -> Result<(), String> {
    if caps.input.len() > 32 {
        return Err("capabilities input list too long (max 32)".into());
    }
    if caps.video_codecs.len() > 32 {
        return Err("capabilities video_codecs list too long (max 32)".into());
    }
    for input in &caps.input {
        if !KNOWN_INPUT_TYPES.contains(&input.as_str()) {
            return Err(format!("unknown input type: {input}"));
        }
    }
    for codec in &caps.video_codecs {
        if !KNOWN_VIDEO_CODECS.contains(&codec.as_str()) {
            return Err(format!("unknown video codec: {codec}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_attach_does_not_accept_client_geometry() {
        let valid =
            r#"{"type":"desktop_attach","v":1,"session_id":"desktop-test","output":"WAYLAND-1"}"#;
        assert!(matches!(
            parse_client_message(valid).unwrap(),
            ClientMessage::DesktopAttach { .. }
        ));
        assert!(parse_client_message(&valid.replace("WAYLAND-1", "../desktop")).is_err());
        assert!(parse_client_message(&valid.replace("\"v\":1", "\"v\":2")).is_err());
        assert!(
            parse_client_message(
                r#"{"type":"desktop_attach","v":1,"session_id":"desktop-test","width":1080}"#
            )
            .is_err()
        );
    }

    #[test]
    fn media_pause_has_its_own_validated_intent() {
        assert!(matches!(
            parse_client_message(r#"{"type":"stream_pause","v":1,"session_id":"text-test"}"#)
                .unwrap(),
            ClientMessage::StreamPause { .. }
        ));
        assert!(
            parse_client_message(r#"{"type":"stream_pause","v":1,"session_id":"../other"}"#)
                .is_err()
        );
    }

    #[test]
    fn resize_defaults_scale_and_refresh_hz() {
        let json =
            r#"{"type":"session_resize","v":1,"session_id":"test","width":844,"height":390}"#;
        let message = parse_client_message(json).unwrap();
        assert_eq!(
            message,
            ClientMessage::SessionResize {
                v: 1,
                session_id: "test".into(),
                width: 844,
                height: 390,
                scale: 1.0,
                refresh_hz: 60,
            }
        );
    }

    #[test]
    fn rejects_oversized_sdp() {
        let sdp = "v=".to_string() + &"0".repeat(32769);
        let json = format!(
            r#"{{"type":"sdp","v":1,"session_id":"test","sdp_type":"offer","sdp":"{sdp}"}}"#
        );
        assert!(parse_client_message(&json).is_err());
    }

    #[test]
    fn rejects_oversized_ice_candidate() {
        let candidate = "c:".to_string() + &"0".repeat(4097);
        let json = format!(
            r#"{{"type":"ice","v":1,"session_id":"test","candidate":"{candidate}","sdp_m_line_index":0}}"#
        );
        assert!(parse_client_message(&json).is_err());
    }

    #[test]
    fn all_host_message_types_roundtrip() {
        let messages: Vec<HostMessage> = vec![
            HostMessage::HelloAck {
                v: 1,
                capabilities: Capabilities {
                    input: vec!["pointer".into(), "touch".into()],
                    video_codecs: vec!["h264".into()],
                    resize: true,
                    clipboard: false,
                    audio: false,
                },
                server_version: Some("0.1.0".into()),
                device_token: None,
                host_id: Some("0123456789abcdef0123456789abcdef".into()),
                host_name: Some("Desk".into()),
            },
            HostMessage::HelloReject {
                v: 1,
                reason: "denied".into(),
                code: Some("not_paired".into()),
            },
            HostMessage::PairPending {
                v: 1,
                host_id: "h".into(),
                host_name: "Desk".into(),
                host_nonce: "00ff".into(),
                timeout_secs: 120,
            },
            HostMessage::PairResult {
                v: 1,
                accepted: true,
                device_token: Some("t".into()),
                reason: None,
            },
            HostMessage::Unpaired { v: 1 },
            HostMessage::SessionCreated {
                v: 1,
                session_id: "s".into(),
                width: 390,
                height: 844,
                scale: 2.0,
                refresh_hz: 120,
            },
            HostMessage::SessionError {
                v: 1,
                session_id: "s".into(),
                reason: "failed".into(),
            },
            HostMessage::SessionResumed {
                v: 1,
                session_id: "s".into(),
                width: 390,
                height: 844,
                scale: 2.0,
                refresh_hz: 120,
            },
            HostMessage::SessionStopped {
                v: 1,
                session_id: "s".into(),
                reason: None,
            },
            HostMessage::StreamStarted {
                v: 1,
                session_id: "s".into(),
            },
            HostMessage::StreamStopped {
                v: 1,
                session_id: "s".into(),
                reason: None,
            },
            HostMessage::Sdp {
                v: 1,
                session_id: "s".into(),
                sdp_type: SdpType::Answer,
                sdp: "v=0".into(),
            },
            HostMessage::Ice {
                v: 1,
                session_id: "s".into(),
                candidate: "c:1".into(),
                sdp_m_line_index: 0,
            },
            HostMessage::SessionResized {
                v: 1,
                session_id: "s".into(),
                width: 844,
                height: 390,
                scale: 1.5,
                refresh_hz: 90,
            },
            HostMessage::SessionInspection {
                v: 1,
                session_id: "s".into(),
                width: 390,
                height: 844,
                scale: 1.5,
                refresh_hz: 90,
                compositor_alive: true,
                app_alive: true,
                stream_alive: false,
                command: vec!["/usr/bin/test".into()],
                negotiation_state: Some("Connected".into()),
            },
            HostMessage::SessionListResult {
                v: 1,
                sessions: vec![SessionInfo {
                    session_id: "test".into(),
                    width: 390,
                    height: 844,
                    active: true,
                    uptime_secs: 120,
                    scale: 1.0,
                    refresh_hz: 60,
                    owner_device_id: None,
                    negotiation_state: Some("Idle".into()),
                    app_id: None,
                }],
            },
            HostMessage::ProbeResult {
                v: 1,
                pipewire_running: true,
                render_nodes: vec!["/dev/dri/renderD128".into()],
                h264_encoders: vec!["openh264enc".into()],
                av1_encoders: Vec::new(),
                active_sessions: 3,
                active_connections: 2,
                server_uptime_secs: 600,
                max_sessions: 8,
                hostname: Some("test-host".into()),
                total_connections: 0,
                total_messages: 0,
                peak_connections: 0,
                total_auth_failures: 0,
                total_rate_limits: 0,
                total_sessions_created: 0,
                server_version: None,
            },
            HostMessage::SessionEvent {
                v: 1,
                session_id: "s".into(),
                event: SessionEventKind::AppExited,
                detail: None,
            },
            HostMessage::Pong {
                v: 1,
                timestamp_ms: 1000,
            },
            HostMessage::Error {
                v: 1,
                reason: "oops".into(),
            },
        ];
        for message in &messages {
            let json = serde_json::to_string(message).unwrap();
            let parsed = parse_host_message(&json);
            assert!(
                parsed.is_ok(),
                "roundtrip failed for {message:?}: {}",
                parsed.unwrap_err()
            );
            assert_eq!(&parsed.unwrap(), message);
        }
    }

    #[test]
    fn all_client_message_types_roundtrip() {
        let messages = [
            r#"{"type":"hello","v":1,"device_id":"phone","token":"abc"}"#,
            r#"{"type":"session_request","v":1,"session_id":"test","command":["/bin/x"],"width":390,"height":844}"#,
            r#"{"type":"sdp","v":1,"session_id":"test","sdp_type":"offer","sdp":"v=0..."}"#,
            r#"{"type":"ice","v":1,"session_id":"test","candidate":"c:1","sdp_m_line_index":0}"#,
            r#"{"type":"session_resize","v":1,"session_id":"test","width":844,"height":390}"#,
            r#"{"type":"session_resume","v":1,"session_id":"test"}"#,
            r#"{"type":"session_stop","v":1,"session_id":"test"}"#,
            r#"{"type":"stream_start","v":1,"session_id":"test"}"#,
            r#"{"type":"stream_stop","v":1,"session_id":"test"}"#,
            r#"{"type":"session_inspect","v":1,"session_id":"test"}"#,
            r#"{"type":"session_list","v":1}"#,
            r#"{"type":"probe","v":1}"#,
            r#"{"type":"ping","v":1,"timestamp_ms":1000}"#,
        ];
        for json in messages {
            let parsed = parse_client_message(json);
            assert!(parsed.is_ok(), "failed to parse: {json}");
            // Serialize and re-parse to verify roundtrip.
            let serialized = serde_json::to_string(&parsed.unwrap()).unwrap();
            let reparsed = parse_client_message(&serialized);
            assert!(reparsed.is_ok(), "roundtrip failed for: {json}");
        }
    }

    #[test]
    fn device_id_rejects_unicode_and_path_traversal() {
        // Unicode emoji
        let json = r#"{"type":"hello","v":1,"device_id":"📱phone","token":"abc"}"#;
        assert!(parse_client_message(json).is_err());

        // Path traversal
        let json = r#"{"type":"hello","v":1,"device_id":"../etc/passwd","token":"abc"}"#;
        assert!(parse_client_message(json).is_err());
    }
}
