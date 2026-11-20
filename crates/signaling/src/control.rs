//! Local control socket for host-side pairing and device management.
//!
//! The running server listens on a Unix socket (mode 0600 in a 0700 runtime
//! directory) and only answers peers with the same UID. Each connection sends
//! one JSON request line and receives one JSON response line. The CLI commands
//! `pair`, `approve`, `decline`, `devices`, `unpair` and `send` use it, so the server
//! never needs a restart to pair a phone.

use crate::approval::Decision;
use crate::server::ServerState;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub use enzimos_phone_client::ControlRequest;

/// Start serving the control socket. Errors are logged; the signaling server
/// keeps running without CLI pairing if the socket cannot be created.
pub fn spawn(state: Arc<ServerState>, path: &Path) {
    let listener = match bind(path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("control: socket unavailable at {}: {error}", path.display());
            return;
        }
    };
    eprintln!("control: listening on {}", path.display());
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                if let Err(error) = serve(state, stream).await {
                    eprintln!("control: request failed: {error}");
                }
            });
        }
    });
}

fn bind(path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    // Remove a stale socket only if nothing answers on it.
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "another enzremote server owns the control socket",
            ));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

async fn serve(state: Arc<ServerState>, stream: UnixStream) -> std::io::Result<()> {
    let peer = stream.peer_cred()?;
    if peer.uid() != nix::unistd::getuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "foreign uid",
        ));
    }
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(tokio::io::AsyncReadExt::take(read, 64 * 1024));
    reader.read_line(&mut line).await?;
    let response = match serde_json::from_str::<ControlRequest>(line.trim()) {
        Ok(ControlRequest::Watch) => return watch(&state, write).await,
        Ok(request) => handle(&state, request).await,
        Err(error) => {
            serde_json::json!({"ok": false, "error": format!("invalid request: {error}")})
        }
    };
    let mut out = serde_json::to_vec(&response).unwrap_or_default();
    out.push(b'\n');
    write.write_all(&out).await?;
    write.shutdown().await
}

/// Stream host events as JSON lines until the client goes away.
async fn watch(
    state: &Arc<ServerState>,
    mut write: tokio::net::unix::OwnedWriteHalf,
) -> std::io::Result<()> {
    let mut events = state.subscribe_events();
    write
        .write_all(b"{\"ok\":true,\"event\":\"watching\"}\n")
        .await?;
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                serde_json::json!({"event": "resync"})
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        };
        let mut line = serde_json::to_vec(&event).unwrap_or_default();
        line.push(b'\n');
        write.write_all(&line).await?;
    }
}

async fn handle(state: &Arc<ServerState>, request: ControlRequest) -> serde_json::Value {
    use serde_json::json;
    let identity = state.identity();
    match request {
        ControlRequest::Status => json!({
            "ok": true,
            "host_id": identity.id,
            "name": identity.name,
            "port": state.bind_addr().port(),
            "tls": state.cert_fingerprint().is_some(),
            "fp": state.cert_fingerprint(),
            "addrs": crate::discovery::lan_addresses(),
        }),
        ControlRequest::PairCode => match state.mint_pairing_token().await {
            Ok((token, ttl)) => json!({
                "ok": true,
                "expires_in": ttl,
                "payload": pairing_payload(state, &token),
            }),
            Err(error) => json!({"ok": false, "error": error}),
        },
        ControlRequest::Pending => {
            let pending: Vec<_> = state
                .approvals()
                .list()
                .await
                .into_iter()
                .map(|p| json!({"id": p.id, "device_id": p.device_id, "name": p.device_name, "code": p.code, "age_secs": p.age_secs}))
                .collect();
            json!({"ok": true, "pending": pending})
        }
        ControlRequest::Approve { target } => resolve(state, &target, Decision::Accept).await,
        ControlRequest::Decline { target } => resolve(state, &target, Decision::Decline).await,
        ControlRequest::ClipboardPublish { text } => match state.publish_shell_clipboard(text).await {
            Ok(()) => json!({"ok":true}),
            Err(error) => json!({"ok":false,"error":error}),
        },
        ControlRequest::ClipboardPoll { since } => state.poll_shell_clipboard(since).await,
        ControlRequest::Devices => {
            let devices: Vec<_> = state
                .paired_devices()
                .await
                .into_iter()
                .map(|(d, peer)| {
                    let prefs = crate::host_settings::device(&d.device_id);
                    json!({
                        "device_id": d.device_id,
                        "name": if prefs.alias.is_empty() { d.name.clone() } else { prefs.alias.clone() },
                        "device_name": d.name,
                        "permissions": prefs.permissions,
                        "paired_at": d.paired_at,
                        "connected": peer.is_some(),
                        "battery": state.phone_battery(&d.device_id).map(|(level, charging)| json!({"level": level, "charging": charging})),
                        "address": peer.map(|p| p.ip().to_string()),
                    })
                })
                .collect();
            json!({"ok": true, "devices": devices})
        }
        ControlRequest::Ring { device_id, stop } => match state.ring(device_id, !stop).await {
            Ok(devices) => json!({"ok": true, "devices": devices}),
            Err(error) => json!({"ok": false, "error": error}),
        },
        ControlRequest::SendFiles { device_id, paths } => {
            if paths.is_empty() || paths.len() > 256 || paths.iter().any(|p| !p.starts_with('/')) {
                return json!({"ok": false, "error": "give 1-256 absolute file paths"});
            }
            match state
                .offer_files(device_id, paths.into_iter().map(Into::into).collect())
                .await
            {
                Ok((device, files)) => json!({"ok": true, "device": device, "files": files}),
                Err(error) => json!({"ok": false, "error": error}),
            }
        }
        ControlRequest::SettingsGet => json!({
            "ok": true,
            "settings": crate::host_settings::current(),
            "schema": crate::host_settings::schema(),
        }),
        ControlRequest::SettingsSet { values } => {
            match crate::host_settings::update(&values) {
                Ok(restart) => {
                    if ["app_streaming", "remote_control", "phone_video"].iter().any(|key| values.get(*key) == Some(&json!(false))) {
                        for (device, _) in state.paired_devices().await { state.pause_device_media(&device.device_id).await; }
                    }
                    state.emit_event(json!({"event": "settings_changed", "keys": values.keys().collect::<Vec<_>>()}));
                    json!({"ok": true, "settings": crate::host_settings::current(), "restart_required": restart})
                }
                Err(error) => json!({"ok": false, "error": error}),
            }
        }
        ControlRequest::DeviceUpdate {
            device_id,
            name,
            permissions,
        } => {
            if !state
                .paired_devices()
                .await
                .iter()
                .any(|(d, _)| d.device_id == device_id)
            {
                return json!({"ok": false, "error": "no paired device with that ID"});
            }
            match crate::host_settings::update_device(&device_id, name, permissions.as_ref()) {
                Ok(prefs) => {
                    if permissions.as_ref().is_some_and(|permissions| ["app_streaming", "remote_control", "phone_video"].iter().any(|key| permissions.get(*key) == Some(&json!(false)))) {
                    state.pause_device_media(&device_id).await;
                }
                state.emit_event(json!({"event": "device_updated", "device_id": device_id}));
                    json!({"ok": true, "device_id": device_id, "name": prefs.alias, "permissions": prefs.permissions})
                }
                Err(error) => json!({"ok": false, "error": error}),
            }
        }
        ControlRequest::Watch => json!({"ok": false, "error": "watch keeps its own connection"}),
        ControlRequest::Sessions => json!({"ok": true, "sessions": state.phone_sessions().await}),
        ControlRequest::SessionStop { session_id } => {
            match state.stop_phone_session(&session_id).await {
                Ok(()) => json!({"ok": true}),
                Err(error) => json!({"ok": false, "error": error}),
            }
        }
        ControlRequest::SmsDevices => {
            let devices: Vec<_> = state
                .sms_devices()
                .await
                .into_iter()
                .map(|(id, name, revision)| json!({"device_id": id, "name": name, "revision": revision}))
                .collect();
            json!({"ok": true, "devices": devices})
        }
        ControlRequest::Sms { device_id, request } => {
            match state.sms_request(device_id, request).await {
                Ok((device, result)) => json!({"ok": true, "device_id": device, "result": result}),
                Err(error) => json!({"ok": false, "error": error}),
            }
        }
        ControlRequest::Unpair { device_id } => {
            match state.unpair_device(&device_id).await {
                Ok(true) => json!({"ok": true}),
                Ok(false) => json!({"ok": false, "error": "no paired device with that ID"}),
                Err(error) => json!({"ok": false, "error": error}),
            }
        }
    }
}

async fn resolve(state: &Arc<ServerState>, target: &str, decision: Decision) -> serde_json::Value {
    match state.approvals().resolve(target, decision).await {
        Ok(summary) => {
            serde_json::json!({"ok": true, "device_id": summary.device_id, "name": summary.device_name})
        }
        Err(error) => serde_json::json!({"ok": false, "error": error}),
    }
}

/// JSON payload encoded in the pairing QR code.
///
/// `v:2` payloads identify the host by ID and certificate fingerprint and list
/// every LAN address; the phone pins `fp` and remembers `id`.
pub fn pairing_payload(state: &ServerState, token: &str) -> serde_json::Value {
    let identity = state.identity();
    let addrs = crate::discovery::lan_addresses();
    serde_json::json!({
        "v": 2,
        "id": identity.id,
        "name": identity.name,
        "host": addrs.first().cloned().unwrap_or_else(|| "127.0.0.1".into()),
        "addrs": addrs,
        "port": state.bind_addr().port(),
        "token": token,
        "tls": state.cert_fingerprint().is_some(),
        "fp": state.cert_fingerprint(),
    })
}

/// Send one request to the running server (CLI side).
pub async fn request(path: &Path, request: &ControlRequest) -> Result<serde_json::Value, String> {
    enzimos_phone_client::request(path, request)
        .await
        .map(|reply| reply.0)
        .map_err(|error| error.to_string())
}
