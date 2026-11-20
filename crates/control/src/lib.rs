#![forbid(unsafe_code)]
//! Shared control message types and parsing for the enzremote protocol.
//!
//! This crate defines the JSON control messages exchanged between the Android
//! client (via WebRTC data channel) and the host-side input subsystem.
//!
//! # Message Format
//!
//! All messages are JSON objects with a `"type"` tag and protocol version
//! field `"v"` (currently version 1). The message types cover pointer,
//! keyboard, touch, scroll, button, text input, cursor visibility,
//! viewport resize, keyframe requests, bitrate requests, and clipboard.
//!
//! # Usage
//!
//! ```
//! use enzremote_control::{parse_control_message, ControlMessage, TouchPhase};
//!
//! let msg = parse_control_message(r#"{"v":1,"type":"key","code":30,"pressed":true}"#).unwrap();
//! assert!(matches!(msg, ControlMessage::Key { code: 30, pressed: true, .. }));
//! ```

use serde::Deserialize;

/// Sample control messages for testing.
pub const CONTROL_MESSAGES: &[&str] = &[
    r#"{"v":1,"type":"pointer","x":122,"y":455,"click":false}"#,
    r#"{"v":1,"type":"pointer","x":123,"y":456,"click":true}"#,
    r#"{"v":1,"type":"key","code":30,"pressed":true}"#,
    r#"{"v":1,"type":"key","code":30,"pressed":false}"#,
    r#"{"v":1,"type":"touch","slot":0,"phase":"down","x":100,"y":200}"#,
    r#"{"v":1,"type":"touch","slot":1,"phase":"down","x":140,"y":240}"#,
    r#"{"v":1,"type":"touch","slot":0,"phase":"move","x":110,"y":210}"#,
    r#"{"v":1,"type":"touch","slot":0,"phase":"up","x":110,"y":210}"#,
    r#"{"v":1,"type":"touch","slot":1,"phase":"cancel","x":140,"y":240}"#,
    r#"{"v":1,"type":"button","code":272,"pressed":true}"#,
    r#"{"v":1,"type":"button","code":272,"pressed":false}"#,
    r#"{"v":1,"type":"scroll","dx":0.0,"dy":-3.0}"#,
    r#"{"v":1,"type":"pointer_relative","dx":10.5,"dy":-5.0}"#,
    r#"{"v":1,"type":"text","text":"Hello"}"#,
    r#"{"v":1,"type":"cursor_visibility","visible":false}"#,
    r#"{"v":1,"type":"resize","width":844,"height":390}"#,
    r#"{"v":1,"type":"stream_config"}"#,
    r#"{"v":1,"type":"content_size","width":1078,"height":1144,"serial":1}"#,
    r#"{"v":1,"type":"scroll_pixels","x":10.0,"y":10.0,"dx":0.0,"dy":4.0,"stop":false}"#,
    r#"{"v":1,"type":"keyframe"}"#,
    r#"{"v":1,"type":"bitrate","target_kbps":3000}"#,
    r#"{"v":1,"type":"clipboard","text":"Hello from clipboard"}"#,
    r#"{"v":1,"type":"network_stats","packet_loss":0.02,"rtt_ms":4.5,"available_bandwidth_kbps":15000,"jitter_ms":1.2}"#,
    r#"{"v":1,"type":"list_windows"}"#,
    r#"{"v":1,"type":"focus_window","window_address":"0x12345678"}"#,
    r#"{"v":1,"type":"launch_app","app_id":"terminal"}"#,
    r#"{"v":1,"type":"gamepad","id":0,"buttons":2049,"lx":-0.5,"ly":0.25,"rx":0,"ry":0,"lt":0,"rt":1}"#,
    r#"{"v":1,"type":"gamepad","id":0,"connected":false}"#,
];

/// Client-to-host control message sent over the WebRTC data channel.
///
/// Each variant maps to a JSON message with a `"type"` tag and protocol
/// version field `"v"`. All variants use version 1.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlMessage {
    Pointer {
        v: u32,
        x: f64,
        y: f64,
        click: bool,
    },
    Key {
        v: u32,
        code: u32,
        pressed: bool,
    },
    Touch {
        v: u32,
        slot: u32,
        phase: TouchPhase,
        x: f64,
        y: f64,
    },
    Button {
        v: u32,
        code: u32,
        pressed: bool,
    },
    Scroll {
        v: u32,
        dx: f64,
        dy: f64,
    },
    /// Anchor and deltas use stream pixels, not wheel ticks. Positive deltas scroll right/down.
    /// `stop` ends a continuous finger gesture, including any client-generated inertia.
    ScrollPixels {
        v: u32,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        stop: bool,
    },
    PointerRelative {
        v: u32,
        dx: f64,
        dy: f64,
    },
    Text {
        v: u32,
        text: String,
    },
    CursorVisibility {
        v: u32,
        visible: bool,
    },
    Resize {
        v: u32,
        width: u32,
        height: u32,
    },
    StreamConfig {
        v: u32,
    },
    /// Drain earlier accepted input commands, without acknowledging rendering or document saving.
    InputDrain {
        v: u32,
        serial: u32,
    },
    /// Resize the app work area in capture pixels without changing the capture canvas.
    /// Width must equal the canvas width; height may only shrink it from the bottom.
    ContentSize {
        v: u32,
        width: u32,
        height: u32,
        serial: u32,
    },
    Keyframe {
        v: u32,
    },
    Bitrate {
        v: u32,
        target_kbps: u32,
    },
    Clipboard {
        v: u32,
        text: String,
    },
    /// Request a list of windows from the compositor.
    ///
    /// The server responds with window info via data channel.
    ListWindows {
        v: u32,
    },
    /// Request to focus a specific window by its address/ID.
    FocusWindow {
        v: u32,
        /// Window address from the compositor (e.g., Hyprland window address).
        window_address: String,
    },
    /// Launch an application on the nested compositor.
    ///
    /// The server spawns the command with `WAYLAND_DISPLAY` pointing
    /// to the nested compositor. Only allowlisted commands are accepted.
    LaunchApp {
        v: u32,
        /// Application identifier (e.g., "calculator", "terminal").
        /// Mapped to actual commands server-side for security.
        app_id: String,
    },
    /// Client-reported network statistics for adaptive bitrate.
    ///
    /// Sent periodically by the Android client with WebRTC stats.
    /// The server uses these to adjust encoder bitrate dynamically.
    NetworkStats {
        v: u32,
        /// Fraction of packets lost (0.0 = none, 1.0 = all).
        packet_loss: f64,
        /// Round-trip time in milliseconds.
        rtt_ms: f64,
        /// Available outbound bandwidth estimate in kbps (from WebRTC).
        /// May be 0 if the client cannot determine it.
        #[serde(default)]
        available_bandwidth_kbps: u32,
        /// Jitter in milliseconds.
        #[serde(default)]
        jitter_ms: f64,
    },
    /// Game controller snapshot (`connected:false` unplugs it). Sticks are
    /// -1..1 with +y down, triggers 0..1; `buttons` uses the client's bits
    /// (A B X Y LB RB View Menu Guide L3 R3 Up Down Left Right).
    Gamepad {
        v: u32,
        id: u8,
        #[serde(default = "default_true")]
        connected: bool,
        #[serde(default)]
        buttons: u32,
        #[serde(default)]
        lx: f64,
        #[serde(default)]
        ly: f64,
        #[serde(default)]
        rx: f64,
        #[serde(default)]
        ry: f64,
        #[serde(default)]
        lt: f64,
        #[serde(default)]
        rt: f64,
    },
}

fn default_true() -> bool {
    true
}

/// Touch event lifecycle phase.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

impl TouchPhase {
    /// Return the phase as a lowercase string matching the C worker protocol.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Move => "move",
            Self::Up => "up",
            Self::Cancel => "cancel",
        }
    }
}

impl ControlMessage {
    /// Encode a helper command, or return `None` for in-process handling.
    /// Text newlines are normalized so they cannot form additional worker commands.
    pub fn to_worker_command(&self) -> Option<String> {
        match self {
            Self::Pointer { x, y, click, .. } => {
                let cx = x.clamp(0.0, u32::MAX as f64) as u32;
                let cy = y.clamp(0.0, u32::MAX as f64) as u32;
                Some(format!("pointer {cx} {cy} {}", u8::from(*click)))
            }
            Self::Key { code, pressed, .. } => Some(format!("key {code} {}", u8::from(*pressed))),
            Self::Touch {
                slot, phase, x, y, ..
            } => {
                let cx = x.clamp(0.0, u32::MAX as f64) as u32;
                let cy = y.clamp(0.0, u32::MAX as f64) as u32;
                Some(format!("touch {slot} {} {cx} {cy}", phase.as_str()))
            }
            Self::Button { code, pressed, .. } => {
                Some(format!("button {code} {}", u8::from(*pressed)))
            }
            Self::Scroll { dx, dy, .. } => Some(format!("scroll {dx:.6} {dy:.6}")),
            Self::ScrollPixels {
                x, y, dx, dy, stop, ..
            } => Some(format!(
                "scroll_pixels {x:.6} {y:.6} {dx:.6} {dy:.6} {}",
                u8::from(*stop)
            )),
            Self::PointerRelative { dx, dy, .. } => {
                Some(format!("pointer_relative {dx:.6} {dy:.6}"))
            }
            Self::Text { text, .. } => {
                // Sanitize newlines: the worker protocol is line-delimited, so
                // \n or \r would split the command and could inject arbitrary
                // worker commands. Replace them with spaces (the worker types
                // text character-by-character, so newlines become spaces).
                let safe = text.replace('\n', " ").replace('\r', "");
                Some(format!("text {safe}"))
            }
            Self::CursorVisibility { .. }
            | Self::Resize { .. }
            | Self::StreamConfig { .. }
            | Self::InputDrain { .. }
            | Self::ContentSize { .. }
            | Self::Keyframe { .. }
            | Self::Bitrate { .. }
            | Self::Clipboard { .. }
            | Self::NetworkStats { .. }
            | Self::ListWindows { .. }
            | Self::FocusWindow { .. }
            | Self::LaunchApp { .. }
            | Self::Gamepad { .. } => None,
        }
    }
}

/// Maximum raw message size accepted by [`parse_control_message`].
///
/// This limit is enforced *before* JSON parsing to prevent large
/// allocations from untrusted input. The WebRTC data channel has its
/// own ~256 KB limit, but this provides defense in depth.
const MAX_CONTROL_MESSAGE_SIZE: usize = 128 * 1024; // 128 KB

/// Parse and validate a JSON control message from the WebRTC data channel.
pub fn parse_control_message(message: &str) -> Result<ControlMessage, String> {
    if message.len() > MAX_CONTROL_MESSAGE_SIZE {
        return Err(format!(
            "control message too large ({} bytes, max {MAX_CONTROL_MESSAGE_SIZE})",
            message.len(),
        ));
    }
    let message: ControlMessage = serde_json::from_str(message)
        .map_err(|error| format!("invalid control message: {error}"))?;
    match &message {
        ControlMessage::Pointer { v, x, y, .. } => {
            validate_version_and_coordinates(*v, *x, *y)?;
        }
        ControlMessage::Key { v, code, .. } => {
            if *v != 1 || *code > 0x2ff {
                return Err("unsupported key message version or keycode".into());
            }
        }
        ControlMessage::Touch { v, slot, x, y, .. } => {
            validate_version_and_coordinates(*v, *x, *y)?;
            if *slot >= 16 {
                return Err("touch slot exceeds protocol bounds".into());
            }
        }
        ControlMessage::Button { v, code, .. } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
            if *code > 0x2ff {
                return Err("unsupported button code".into());
            }
        }
        ControlMessage::Scroll { v, dx, dy } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
            if !dx.is_finite() || !dy.is_finite() {
                return Err("scroll deltas must be finite".into());
            }
            if dx.abs() > 1000.0 || dy.abs() > 1000.0 {
                return Err("scroll deltas exceed protocol bounds".into());
            }
        }
        ControlMessage::ScrollPixels {
            v, x, y, dx, dy, ..
        } => {
            validate_version_and_coordinates(*v, *x, *y)?;
            if !dx.is_finite() || !dy.is_finite() || dx.abs() > 8192.0 || dy.abs() > 8192.0 {
                return Err("pixel scroll deltas must be finite and within 8192 pixels".into());
            }
        }
        ControlMessage::PointerRelative { v, dx, dy } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
            if !dx.is_finite() || !dy.is_finite() {
                return Err("pointer deltas must be finite".into());
            }
            if dx.abs() > 8192.0 || dy.abs() > 8192.0 {
                return Err("pointer deltas exceed protocol bounds".into());
            }
        }
        ControlMessage::Text { v, text } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
            if text.is_empty() || text.len() > 4096 {
                return Err("text must be 1-4096 bytes".into());
            }
            if text.contains('\0') {
                return Err("text must not contain NUL bytes".into());
            }
            // Note: \n and \r are allowed in the JSON protocol (e.g., pasted
            // multiline text). They are sanitized in to_worker_command().
        }
        ControlMessage::CursorVisibility { v, .. } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
        }
        ControlMessage::Resize { v, width, height }
        | ControlMessage::ContentSize {
            v, width, height, ..
        } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
            if !(240..=8192).contains(width) || !(240..=8192).contains(height) {
                return Err("resize dimensions must each be between 240 and 8192".into());
            }
        }
        ControlMessage::InputDrain { v, serial } => {
            if *v != 1 || !(1..=i32::MAX as u32).contains(serial) {
                return Err(
                    "input drain requires version 1 and a positive signed 32-bit serial".into(),
                );
            }
        }
        ControlMessage::Keyframe { v } | ControlMessage::StreamConfig { v } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
        }
        ControlMessage::Bitrate { v, target_kbps } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
            if !(128..=100_000).contains(target_kbps) {
                return Err("target_kbps must be between 128 and 100000".into());
            }
        }
        ControlMessage::Clipboard { v, text } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
            if text.is_empty() || text.len() > 65536 {
                return Err("clipboard text must be 1-65536 bytes".into());
            }
            if text.contains('\0') {
                return Err("clipboard text must not contain NUL bytes".into());
            }
            // Note: \n and \r are allowed (clipboard content can be multiline).
            // They would be sanitized in to_worker_command() if/when clipboard
            // forwarding is wired in.
        }
        ControlMessage::ListWindows { v } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
        }
        ControlMessage::FocusWindow { v, window_address } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
            if window_address.is_empty() || window_address.len() > 256 {
                return Err("window_address must be 1-256 characters".into());
            }
            // Hyprland addresses are hex like "0x56789abc".  We allow
            // `0x` prefix followed by hex digits, or plain hex digits,
            // to prevent any special-character abuse against hyprctl.
            let hex_part = window_address
                .strip_prefix("0x")
                .or_else(|| window_address.strip_prefix("0X"))
                .unwrap_or(window_address);
            if hex_part.is_empty() || !hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("window_address must be a hex address (e.g. '0x12345678')".into());
            }
        }
        ControlMessage::NetworkStats {
            v,
            packet_loss,
            rtt_ms,
            jitter_ms,
            ..
        } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
            if !packet_loss.is_finite() || *packet_loss < 0.0 || *packet_loss > 1.0 {
                return Err("packet_loss must be between 0.0 and 1.0".into());
            }
            if !rtt_ms.is_finite() || *rtt_ms < 0.0 || *rtt_ms > 60_000.0 {
                return Err("rtt_ms must be between 0 and 60000".into());
            }
            if !jitter_ms.is_finite() || *jitter_ms < 0.0 {
                return Err("jitter_ms must be non-negative and finite".into());
            }
        }
        ControlMessage::Gamepad {
            v,
            id,
            buttons,
            lx,
            ly,
            rx,
            ry,
            lt,
            rt,
            ..
        } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
            if *id >= 4 || *buttons >= 1 << 15 {
                return Err("gamepad id or buttons out of range".into());
            }
            let sticks = [lx, ly, rx, ry].iter().all(|v| v.is_finite() && (-1.0..=1.0).contains(*v));
            let triggers = [lt, rt].iter().all(|v| v.is_finite() && (0.0..=1.0).contains(*v));
            if !sticks || !triggers {
                return Err("gamepad sticks must be -1..1 and triggers 0..1".into());
            }
        }
        ControlMessage::LaunchApp { v, app_id } => {
            if *v != 1 {
                return Err("unsupported control message version".into());
            }
            // app_id must be a simple alphanumeric identifier (no shell injection).
            if app_id.is_empty()
                || app_id.len() > 64
                || !app_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err("app_id must be 1-64 alphanumeric/dash/underscore chars".into());
            }
        }
    }
    Ok(message)
}

/// Validate protocol version (must be 1) and absolute coordinates (must be < 8192).
fn validate_version_and_coordinates(v: u32, x: f64, y: f64) -> Result<(), String> {
    if v != 1 {
        return Err("unsupported control message version".into());
    }
    if !x.is_finite() || !y.is_finite() {
        return Err("input coordinates must be finite".into());
    }
    if x < 0.0 || y < 0.0 || x >= 8192.0 || y >= 8192.0 {
        return Err("input coordinates exceed protocol bounds".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamepad_messages_are_bounded() {
        assert!(parse_control_message(r#"{"v":1,"type":"gamepad","id":4,"buttons":0}"#).is_err());
        assert!(parse_control_message(r#"{"v":1,"type":"gamepad","id":0,"lx":1.5}"#).is_err());
        assert!(parse_control_message(r#"{"v":1,"type":"gamepad","id":0,"lt":-0.1}"#).is_err());
        assert!(parse_control_message(r#"{"v":1,"type":"gamepad","id":0,"buttons":32768}"#).is_err());
        let unplug = parse_control_message(r#"{"v":1,"type":"gamepad","id":2,"connected":false}"#).unwrap();
        assert!(matches!(unplug, ControlMessage::Gamepad { id: 2, connected: false, .. }));
        assert!(unplug.to_worker_command().is_none());
    }

    #[test]
    fn worker_command_format_matches_c_worker() {
        let cases = [
            (
                ControlMessage::Pointer {
                    v: 1,
                    x: 123.0,
                    y: 456.0,
                    click: true,
                },
                Some("pointer 123 456 1"),
            ),
            (
                ControlMessage::Key {
                    v: 1,
                    code: 30,
                    pressed: true,
                },
                Some("key 30 1"),
            ),
            (
                ControlMessage::Touch {
                    v: 1,
                    slot: 0,
                    phase: TouchPhase::Down,
                    x: 100.0,
                    y: 200.0,
                },
                Some("touch 0 down 100 200"),
            ),
            (
                ControlMessage::Button {
                    v: 1,
                    code: 272,
                    pressed: true,
                },
                Some("button 272 1"),
            ),
            (
                ControlMessage::Scroll {
                    v: 1,
                    dx: 0.0,
                    dy: -3.0,
                },
                Some("scroll 0.000000 -3.000000"),
            ),
            (
                ControlMessage::PointerRelative {
                    v: 1,
                    dx: 10.5,
                    dy: -5.0,
                },
                Some("pointer_relative 10.500000 -5.000000"),
            ),
            (
                ControlMessage::Text {
                    v: 1,
                    text: "Hello".into(),
                },
                Some("text Hello"),
            ),
            (
                ControlMessage::CursorVisibility {
                    v: 1,
                    visible: false,
                },
                None,
            ),
            (
                ControlMessage::Resize {
                    v: 1,
                    width: 844,
                    height: 390,
                },
                None,
            ),
            (ControlMessage::Keyframe { v: 1 }, None),
            (
                ControlMessage::Bitrate {
                    v: 1,
                    target_kbps: 3000,
                },
                None,
            ),
            (
                ControlMessage::Clipboard {
                    v: 1,
                    text: "test".into(),
                },
                None,
            ),
            (
                ControlMessage::NetworkStats {
                    v: 1,
                    packet_loss: 0.02,
                    rtt_ms: 4.5,
                    available_bandwidth_kbps: 15000,
                    jitter_ms: 1.2,
                },
                None,
            ),
            (ControlMessage::ListWindows { v: 1 }, None),
            (
                ControlMessage::FocusWindow {
                    v: 1,
                    window_address: "0x12345678".into(),
                },
                None,
            ),
        ];
        for (msg, expected) in cases {
            let cmd = msg.to_worker_command();
            assert_eq!(
                cmd.as_deref(),
                expected,
                "worker command mismatch for {msg:?}"
            );
        }
    }

    #[test]
    fn android_float_pointer_coordinates() {
        // Android ControlMessageSerializer sends x/y as Double.
        let msg =
            parse_control_message(r#"{"type":"pointer","v":1,"x":100.5,"y":200.75,"click":false}"#)
                .unwrap();
        match msg {
            ControlMessage::Pointer { x, y, .. } => {
                assert_eq!(x, 100.5);
                assert_eq!(y, 200.75);
            }
            _ => panic!("expected Pointer"),
        }
        // Worker command should truncate to integer.
        assert_eq!(msg.to_worker_command().unwrap(), "pointer 100 200 0");
    }

    #[test]
    fn text_mixed_newlines_sanitized() {
        let msg = parse_control_message(r#"{"v":1,"type":"text","text":"a\r\nb\nc\rd"}"#).unwrap();
        let cmd = msg.to_worker_command().unwrap();
        assert!(!cmd.contains('\n'), "no newlines in worker command");
        assert!(!cmd.contains('\r'), "no carriage returns in worker command");
        assert_eq!(cmd, "text a b cd");
    }

    #[test]
    fn text_worker_command_preserves_quotes() {
        // Quotes in text should be preserved in the worker command
        // (the worker doesn't parse JSON, it reads the raw text after "text ")
        let msg =
            parse_control_message(r#"{"v":1,"type":"text","text":"he said \"hi\""}"#).unwrap();
        let cmd = msg.to_worker_command().unwrap();
        assert_eq!(cmd, r#"text he said "hi""#);
    }

    #[test]
    fn content_size_is_validated_and_never_sent_to_input_workers() {
        let message = parse_control_message(
            r#"{"v":1,"type":"content_size","width":1078,"height":1144,"serial":7}"#,
        )
        .unwrap();
        assert!(message.to_worker_command().is_none());
        for invalid in [
            r#"{"v":2,"type":"stream_config"}"#,
            r#"{"v":1,"type":"content_size","width":1078,"height":0,"serial":7}"#,
            r#"{"v":1,"type":"content_size","width":1078,"height":1144,"serial":-1}"#,
            r#"{"v":1,"type":"content_size","width":1078,"height":1144}"#,
            r#"{"v":1,"type":"content_size","width":1078,"height":1144,"serial":7,"extra":true}"#,
        ] {
            assert!(parse_control_message(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn clipboard_over_max_rejected() {
        let text = "a".repeat(65537);
        let json = format!(r#"{{"v":1,"type":"clipboard","text":"{}"}}"#, text);
        assert!(parse_control_message(&json).is_err());
    }

    #[test]
    fn oversized_message_rejected() {
        // Build a valid-looking JSON that exceeds the size limit.
        let huge_text = "a".repeat(MAX_CONTROL_MESSAGE_SIZE + 1);
        let json = format!(r#"{{"type":"text","v":1,"text":"{huge_text}"}}"#);
        let result = parse_control_message(&json);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too large"));
    }

    #[test]
    fn all_pipeline_control_messages_return_none_worker_command() {
        let pipeline_msgs = [
            r#"{"v":1,"type":"keyframe"}"#,
            r#"{"v":1,"type":"resize","width":1080,"height":2400}"#,
            r#"{"v":1,"type":"bitrate","target_kbps":5000}"#,
            r#"{"v":1,"type":"cursor_visibility","visible":true}"#,
            r#"{"v":1,"type":"network_stats","packet_loss":0.0,"rtt_ms":5.0}"#,
            r#"{"v":1,"type":"list_windows"}"#,
            r#"{"v":1,"type":"focus_window","window_address":"0xabc"}"#,
            r#"{"v":1,"type":"clipboard","text":"hello"}"#,
        ];
        for json in &pipeline_msgs {
            let msg = parse_control_message(json)
                .unwrap_or_else(|e| panic!("failed to parse {json}: {e}"));
            assert!(
                msg.to_worker_command().is_none(),
                "pipeline control message should not have worker command: {json}"
            );
        }
    }

    #[test]
    fn pixel_scroll_preserves_fractional_deltas_and_stop() {
        let message = parse_control_message(
            r#"{"v":1,"type":"scroll_pixels","x":200,"y":300,"dx":0.25,"dy":-2.5,"stop":true}"#,
        )
        .unwrap();
        assert_eq!(
            message.to_worker_command().as_deref(),
            Some("scroll_pixels 200.000000 300.000000 0.250000 -2.500000 1")
        );
    }

    #[test]
    fn pixel_scroll_rejects_invalid_coordinates_deltas_and_version() {
        for input in [
            r#"{"v":2,"type":"scroll_pixels","x":0,"y":0,"dx":0,"dy":0,"stop":true}"#,
            r#"{"v":1,"type":"scroll_pixels","x":-1,"y":0,"dx":0,"dy":0,"stop":true}"#,
            r#"{"v":1,"type":"scroll_pixels","x":0,"y":0,"dx":8193,"dy":0,"stop":false}"#,
            r#"{"v":1,"type":"scroll_pixels","x":0,"y":0,"dx":1e999,"dy":0,"stop":false}"#,
            r#"{"v":1,"type":"scroll_pixels","x":0,"y":0,"dx":0,"dy":0}"#,
        ] {
            assert!(parse_control_message(input).is_err(), "{input}");
        }
    }
}
