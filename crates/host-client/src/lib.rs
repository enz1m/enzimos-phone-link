//! Same-user, bounded local control of the Phone Link host.

use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::{
    fmt, io,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Control request.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case", deny_unknown_fields)]
pub enum ControlRequest {
    /// Mint a one-time QR pairing payload.
    PairCode,
    /// List pending interactive requests.
    Pending,
    /// Approve a pending request by code (or the only one when empty).
    Approve {
        #[serde(default)]
        target: String,
    },
    /// Decline a pending request.
    Decline {
        #[serde(default)]
        target: String,
    },
    /// List paired devices.
    Devices,
    /// Revoke a paired device.
    Unpair { device_id: String },
    /// Host identity and listener summary.
    Status,
    /// Ring a phone (find my phone), or stop ringing.
    Ring {
        #[serde(default)]
        device_id: Option<String>,
        #[serde(default)]
        stop: bool,
    },
    /// Publish freshly collected, non-sensitive text from the shell clipboard.
    ClipboardPublish { text: String },
    /// Fetch fresh phone text; the first call acknowledges older content.
    ClipboardPoll { since: Option<u64> },
    /// Host settings with their schema (titles, kinds, choices, restart needs).
    SettingsGet,
    /// Change settings: `{"values":{"clipboard":false,…}}`.
    SettingsSet {
        values: serde_json::Map<String, serde_json::Value>,
    },
    /// Rename a paired device on this computer and/or change what it may do:
    /// `{"device_id","name"?,"permissions"?:{"files":false,…}}`.
    DeviceUpdate {
        device_id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        permissions: Option<serde_json::Value>,
    },
    /// App sessions phones opened on this computer.
    Sessions,
    /// Close one (its app quits; unsaved work in it is lost).
    SessionStop { session_id: String },
    /// Keep the connection open and receive one JSON line per host event
    /// (`device_connected`, `device_disconnected`, `pairing_pending`,
    /// `device_unpaired`, `device_updated`, `settings_changed`).
    Watch,
    /// Phones that share text messages, with a revision that increases when
    /// their conversations change.
    SmsDevices,
    /// Text-message request for a phone: `{"op":"conversations"}`,
    /// `{"op":"thread","thread_id"}`, `{"op":"send","address","text"}`.
    Sms {
        #[serde(default)]
        device_id: Option<String>,
        request: serde_json::Value,
    },
    /// Offer files to a connected phone (absolute paths).
    SendFiles {
        #[serde(default)]
        device_id: Option<String>,
        paths: Vec<String>,
    },
}

impl fmt::Debug for ControlRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ControlRequest([redacted])")
    }
}

/// A host reply whose debug output excludes pairing payloads and private data.
#[derive(Clone)]
pub struct Reply(pub serde_json::Value);

impl fmt::Debug for Reply {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PhoneReply([redacted])")
    }
}

/// Resolve the control endpoint in this graphical session's runtime directory.
pub fn socket_path() -> Option<PathBuf> {
    std::env::var_os("ENZREMOTE_CONTROL_SOCKET")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(|root| PathBuf::from(root).join("enzremote/control.sock"))
        })
        .filter(|path| path.is_absolute())
}

/// Exchange one JSON request with an owner-only same-user socket within three seconds.
///
/// # Errors
/// Returns an I/O error for unavailable, unsafe, oversized or invalid host replies.
/// Remote refusals remain in the reply's `ok` and `error` fields.
pub async fn request(path: &Path, request: &ControlRequest) -> io::Result<Reply> {
    tokio::time::timeout(Duration::from_secs(3), exchange(path, request))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Phone Link did not respond"))?
}

async fn exchange(path: &Path, request: &ControlRequest) -> io::Result<Reply> {
    let uid = rustix::process::geteuid().as_raw();
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Phone socket needs a parent")
    })?;
    let directory = tokio::fs::symlink_metadata(parent).await?;
    if !directory.is_dir() || directory.uid() != uid || directory.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unsafe phone runtime directory",
        ));
    }
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if !path.is_absolute()
        || metadata.uid() != uid
        || metadata.mode() & 0o777 != 0o600
        || !std::os::unix::fs::FileTypeExt::is_socket(&metadata.file_type())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unsafe Phone Link endpoint",
        ));
    }
    let mut stream = UnixStream::connect(path).await?;
    if stream.peer_cred()?.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Foreign Phone Link host",
        ));
    }
    let mut bytes = serde_json::to_vec(request).map_err(io::Error::other)?;
    if bytes.len() >= 65536 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Phone request is too large",
        ));
    }
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    let mut reply = String::new();
    BufReader::new(stream.take(262145))
        .read_line(&mut reply)
        .await?;
    if reply.len() > 262144 || !reply.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Incomplete or oversized phone reply",
        ));
    }
    let value: serde_json::Value = serde_json::from_str(&reply).map_err(io::Error::other)?;
    if !value.get("ok").is_some_and(serde_json::Value::is_boolean) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid phone reply",
        ));
    }
    Ok(Reply(value))
}
