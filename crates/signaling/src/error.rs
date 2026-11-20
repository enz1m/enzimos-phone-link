use std::fmt;

/// Signaling protocol errors with structured error codes.
///
/// Used throughout the signaling server for both protocol-level errors
/// (sent back to clients) and connection-level errors (logged on disconnect).
#[derive(Debug, Clone, PartialEq)]
pub enum SignalingError {
    /// Protocol version mismatch.
    UnsupportedVersion { got: u32, expected: u32 },
    /// Invalid session ID format.
    InvalidSessionId(String),
    /// Invalid viewport dimensions.
    InvalidDimensions { width: u32, height: u32 },
    /// Message too large.
    MessageTooLarge { size: usize, max: usize },
    /// Malformed message.
    MalformedMessage(String),
    /// Authentication failed.
    AuthenticationFailed(String),
    /// Session operation failed.
    SessionError { session_id: String, reason: String },
    /// Rate limit exceeded.
    RateLimitExceeded,
    /// Connection error (WebSocket transport failures, send errors).
    ConnectionError(String),
    /// Hello timeout.
    HelloTimeout,
    /// Connection idle timeout.
    IdleTimeout,
    /// Duplicate device ID.
    DuplicateDeviceId(String),
    /// Not the owner of the session.
    NotSessionOwner {
        session_id: String,
        device_id: String,
    },
    /// Maximum sessions reached.
    MaxSessionsReached(usize),
    /// Invalid capabilities.
    InvalidCapabilities(String),
    /// Server configuration is invalid.
    ConfigError(String),
    /// JSON serialization failed.
    Serialization(String),
    /// Client sent an unexpected message (e.g. binary, or non-hello first).
    UnexpectedMessage(String),
    /// Server could not bind to the requested address.
    BindError(String),
}

impl fmt::Display for SignalingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion { got, expected } => {
                write!(f, "unsupported protocol version {got}; expected {expected}")
            }
            Self::InvalidSessionId(id) => write!(f, "invalid session ID: {id}"),
            Self::InvalidDimensions { width, height } => {
                write!(f, "invalid dimensions: {width}x{height}")
            }
            Self::MessageTooLarge { size, max } => {
                write!(f, "message too large: {size} bytes (max {max})")
            }
            Self::MalformedMessage(reason) => write!(f, "malformed message: {reason}"),
            Self::AuthenticationFailed(reason) => {
                write!(f, "authentication failed: {reason}")
            }
            Self::SessionError { session_id, reason } => {
                write!(f, "session {session_id}: {reason}")
            }
            Self::RateLimitExceeded => write!(f, "rate limit exceeded"),
            Self::ConnectionError(reason) => write!(f, "connection error: {reason}"),
            Self::HelloTimeout => write!(f, "hello timeout"),
            Self::IdleTimeout => write!(f, "connection idle timeout"),
            Self::DuplicateDeviceId(id) => write!(f, "duplicate device_id: {id}"),
            Self::NotSessionOwner {
                session_id,
                device_id,
            } => {
                write!(
                    f,
                    "device {device_id} is not the owner of session {session_id}"
                )
            }
            Self::MaxSessionsReached(max) => {
                write!(f, "maximum session limit reached ({max})")
            }
            Self::InvalidCapabilities(reason) => {
                write!(f, "invalid capabilities: {reason}")
            }
            Self::ConfigError(reason) => write!(f, "configuration error: {reason}"),
            Self::Serialization(reason) => write!(f, "serialization failed: {reason}"),
            Self::UnexpectedMessage(reason) => write!(f, "unexpected message: {reason}"),
            Self::BindError(reason) => write!(f, "could not bind: {reason}"),
        }
    }
}

impl std::error::Error for SignalingError {}
