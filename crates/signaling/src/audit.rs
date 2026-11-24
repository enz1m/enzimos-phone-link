//! Structured audit logging for signaling server events.
//!
//! Produces machine-parseable key=value log lines on stderr for
//! connection lifecycle events, session operations, and security
//! incidents. All entries include a monotonic sequence number and
//! Unix timestamp for reliable ordering and correlation.
//!
//! # Format
//!
//! ```text
//! audit seq=1 ts=1721300000 event=connect peer=192.168.1.5:54321 device_id=phone-1
//! audit seq=2 ts=1721300001 event=session_create device_id=phone-1 session_id=demo width=390 height=844
//! audit seq=3 ts=1721300060 event=disconnect device_id=phone-1 duration=60 msgs=42 reason=clean_close
//! ```

use std::fmt::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Global sequence counter for audit events.
static AUDIT_SEQ: AtomicU64 = AtomicU64::new(1);

/// Audit event types.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AuditEvent {
    /// New WebSocket connection established.
    Connect,
    /// Connection authenticated via Hello.
    Authenticate,
    /// Authentication rejected (wrong token or duplicate device).
    AuthReject,
    /// Connection disconnected.
    Disconnect,
    /// Session created.
    SessionCreate,
    /// Session stopped.
    SessionStop,
    /// Session resumed by (possibly different) device.
    SessionResume,
    /// Session resized.
    SessionResize,
    /// Stream started.
    StreamStart,
    /// Stream stopped.
    StreamStop,
    /// Rate limit exceeded.
    RateLimit,
    /// Connection rejected (max connections).
    ConnectionRejected,
    /// Session GC removed stale sessions.
    SessionGc,
    /// SDP exchange.
    Sdp,
    /// ICE candidate exchange.
    Ice,
}

impl AuditEvent {
    fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Authenticate => "authenticate",
            Self::AuthReject => "auth_reject",
            Self::Disconnect => "disconnect",
            Self::SessionCreate => "session_create",
            Self::SessionStop => "session_stop",
            Self::SessionResume => "session_resume",
            Self::SessionResize => "session_resize",
            Self::StreamStart => "stream_start",
            Self::StreamStop => "stream_stop",
            Self::RateLimit => "rate_limit",
            Self::ConnectionRejected => "connection_rejected",
            Self::SessionGc => "session_gc",
            Self::Sdp => "sdp",
            Self::Ice => "ice",
        }
    }
}

/// Builder for structured audit log entries.
///
/// Accumulates key=value pairs and emits them as a single line on
/// [`AuditEntry::emit`].
pub struct AuditEntry {
    buf: String,
}

impl AuditEntry {
    /// Start a new audit entry for the given event.
    pub fn new(event: AuditEvent) -> Self {
        let seq = AUDIT_SEQ.fetch_add(1, Ordering::Relaxed);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut buf = String::with_capacity(128);
        let _ = write!(buf, "audit seq={seq} ts={ts} event={}", event.as_str());
        Self { buf }
    }

    /// Add a string key=value pair. Values containing spaces are quoted.
    pub fn kv(mut self, key: &str, value: &str) -> Self {
        if value.contains(' ')
            || value.contains('"')
            || value.contains('\n')
            || value.contains('\r')
            || value.is_empty()
        {
            // Quote and escape for safe parsing. Replace control characters
            // to prevent log injection via newlines in user-supplied values.
            let escaped = value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r");
            let _ = write!(self.buf, " {key}=\"{escaped}\"");
        } else {
            let _ = write!(self.buf, " {key}={value}");
        }
        self
    }

    /// Add a numeric key=value pair.
    pub fn kv_u64(mut self, key: &str, value: u64) -> Self {
        let _ = write!(self.buf, " {key}={value}");
        self
    }

    /// Add a numeric key=value pair (u32).
    pub fn kv_u32(mut self, key: &str, value: u32) -> Self {
        let _ = write!(self.buf, " {key}={value}");
        self
    }

    /// Add a peer address.
    pub fn peer(self, addr: SocketAddr) -> Self {
        self.kv("peer", &addr.to_string())
    }

    /// Add a device ID.
    pub fn device_id(self, id: &str) -> Self {
        self.kv("device_id", id)
    }

    /// Add a session ID.
    pub fn session_id(self, id: &str) -> Self {
        self.kv("session_id", id)
    }

    /// Emit the audit entry to stderr.
    pub fn emit(self) {
        eprintln!("{}", self.buf);
    }

    /// Return the formatted audit line (for testing).
    #[cfg(test)]
    pub fn to_line(&self) -> &str {
        &self.buf
    }
}

/// Convenience: emit a simple audit event with just peer address.
pub fn log_connect(peer: SocketAddr) {
    AuditEntry::new(AuditEvent::Connect).peer(peer).emit();
}

/// Convenience: emit an authentication event.
pub fn log_authenticate(device_id: &str, peer: SocketAddr) {
    AuditEntry::new(AuditEvent::Authenticate)
        .device_id(device_id)
        .peer(peer)
        .emit();
}

/// Convenience: emit an auth rejection event.
pub fn log_auth_reject(device_id: &str, peer: SocketAddr, reason: &str) {
    AuditEntry::new(AuditEvent::AuthReject)
        .device_id(device_id)
        .peer(peer)
        .kv("reason", reason)
        .emit();
}

/// Convenience: emit a disconnect event.
pub fn log_disconnect(
    device_id: &str,
    peer: SocketAddr,
    duration_secs: u64,
    msgs: u64,
    reason: &str,
) {
    AuditEntry::new(AuditEvent::Disconnect)
        .device_id(device_id)
        .peer(peer)
        .kv_u64("duration", duration_secs)
        .kv_u64("msgs", msgs)
        .kv("reason", reason)
        .emit();
}

/// Convenience: emit a session creation event.
pub fn log_session_create(
    device_id: &str,
    session_id: &str,
    width: u32,
    height: u32,
    scale: f32,
    refresh_hz: u32,
) {
    AuditEntry::new(AuditEvent::SessionCreate)
        .device_id(device_id)
        .session_id(session_id)
        .kv_u32("width", width)
        .kv_u32("height", height)
        .kv("scale", &format!("{scale}"))
        .kv_u32("refresh_hz", refresh_hz)
        .emit();
}

/// Convenience: emit a session stop event.
pub fn log_session_stop(device_id: &str, session_id: &str) {
    AuditEntry::new(AuditEvent::SessionStop)
        .device_id(device_id)
        .session_id(session_id)
        .emit();
}

/// Convenience: emit a connection rejected event.
pub fn log_connection_rejected(peer: SocketAddr, reason: &str) {
    AuditEntry::new(AuditEvent::ConnectionRejected)
        .peer(peer)
        .kv("reason", reason)
        .emit();
}

/// Convenience: emit a rate limit event.
pub fn log_rate_limit(device_id: &str) {
    AuditEntry::new(AuditEvent::RateLimit)
        .device_id(device_id)
        .emit();
}

/// Convenience: emit a session GC event.
pub fn log_session_gc(removed: &[String]) {
    let mut entry =
        AuditEntry::new(AuditEvent::SessionGc).kv_u32("removed_count", removed.len() as u32);
    if !removed.is_empty() {
        entry = entry.kv("session_ids", &removed.join(","));
    }
    entry.emit();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_entry_kv_newline_in_value() {
        let entry = AuditEntry::new(AuditEvent::Disconnect).kv("reason", "line1\nline2");
        let line = entry.to_line();
        // Newlines are escaped to prevent log injection.
        assert!(
            line.contains(r#"reason="line1\nline2""#),
            "newline should be escaped: {line}"
        );
    }
}
