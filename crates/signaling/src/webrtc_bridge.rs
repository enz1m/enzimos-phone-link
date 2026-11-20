//! Bridge between the signaling server and per-session WebRTC pipeline
//! processes. Manages launching, IPC communication, and lifecycle of
//! `enzremote-webrtc-server` child processes.
//!
//! Each active streaming session gets its own WebRTC server process that
//! communicates with the signaling server over a Unix domain socket using
//! the `enzremote-webrtc-ipc` protocol.
//!
//! # Lifecycle
//!
//! 1. `start_stream` launches a child process and waits for `READY`.
//! 2. The bridge connects to the child's IPC socket and spawns a reader task.
//! 3. IPC messages from the pipeline are dispatched via the callback.
//! 4. When the IPC reader sees EOF (child exited / crashed), it automatically
//!    removes the session from the map and notifies via the error callback.
//! 5. `stop_stream` sends a `Shutdown` IPC message and waits up to 2 s for
//!    graceful exit before escalating to `kill()`.
//! 6. Socket files are cleaned up on stop or crash.

use enzremote_webrtc_ipc::{
    FromWebRtc, MAX_IPC_MESSAGE_SIZE, ToWebRtc, parse_from_webrtc, serialize_message,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// Timeout for graceful shutdown before escalating to kill.
const GRACEFUL_SHUTDOWN_SECS: u64 = 2;

/// Timeout for IPC write operations.
const IPC_WRITE_TIMEOUT_SECS: u64 = 5;

/// Manages per-session WebRTC server processes and their IPC connections.
pub struct WebRtcBridge {
    /// Active WebRTC processes, keyed by session ID.
    sessions: Arc<Mutex<HashMap<String, SessionSlot>>>,
    /// Path to the `enzremote-webrtc-server` binary.
    binary_path: PathBuf,
    /// Base directory for IPC sockets.
    socket_dir: PathBuf,
    /// Monotonic counter for process generations (prevents stale reader cleanup).
    next_generation: AtomicU64,
}

/// Slot in the sessions map. `Starting` acts as a reservation so other
/// callers know a launch is in progress without holding the lock during I/O.
enum SessionSlot {
    /// A process launch is in progress. The lock is released while waiting
    /// for READY / IPC connect, so other operations see this marker.
    Starting,
    /// A fully running process with an active IPC connection.
    /// Boxed to avoid large size difference between variants.
    Running(Box<WebRtcProcess>),
}

/// A running WebRTC server process for a single session.
struct WebRtcProcess {
    /// The child process handle.
    child: Child,
    /// IPC connection for sending commands.
    ipc_write: tokio::net::unix::OwnedWriteHalf,
    /// Path to the IPC socket file.
    socket_path: PathBuf,
    /// Monotonic generation counter. Reader tasks check this before removing
    /// the entry, to avoid clobbering a newer session with the same ID.
    generation: u64,
}

/// Callback for messages from the WebRTC pipeline (SDP offer, ICE, status).
/// Called on the IPC reader task — implementations must be Send + Sync.
pub type FromWebRtcCallback = Arc<dyn Fn(&str, FromWebRtc) + Send + Sync>;

impl WebRtcBridge {
    /// Maximum length for a session ID (keeps socket paths within
    /// `sockaddr_un.sun_path` limits and prevents abuse).
    const MAX_SESSION_ID_LEN: usize = 64;

    /// Maximum bytes to read when waiting for the READY line from the
    /// child process. Prevents a malicious/buggy child from forcing an
    /// unbounded allocation.
    const MAX_READY_LINE_BYTES: usize = 4096;

    /// Create a new bridge.
    ///
    /// `binary_path` is the path to `enzremote-webrtc-server`.
    /// `socket_dir` is a directory for IPC sockets (created automatically
    /// with 0700 permissions if it does not exist).
    pub fn new(binary_path: PathBuf, socket_dir: PathBuf) -> Self {
        // Ensure the socket directory exists with restricted permissions.
        // Use DirBuilder with mode to avoid a TOCTOU race between
        // create_dir_all and set_permissions.
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            if let Err(e) = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&socket_dir)
            {
                eprintln!(
                    "webrtc-bridge: warning: could not create socket dir {}: {e}",
                    socket_dir.display()
                );
            }
        }
        #[cfg(not(unix))]
        {
            if let Err(e) = std::fs::create_dir_all(&socket_dir) {
                eprintln!(
                    "webrtc-bridge: warning: could not create socket dir {}: {e}",
                    socket_dir.display()
                );
            }
        }
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            binary_path,
            socket_dir,
            next_generation: AtomicU64::new(0),
        }
    }

    /// Find the webrtc-server binary, searching common locations.
    pub fn find_binary() -> Option<PathBuf> {
        let path = enzremote_webrtc_ipc::layout::helper("ENZREMOTE_WEBRTC_HELPER", "enzremote-webrtc-server");
        path.is_file().then_some(path)
    }

    /// Start a WebRTC pipeline process for a session.
    ///
    /// Returns immediately after the process is ready. The `callback` is
    /// invoked asynchronously when the pipeline sends IPC messages (SDP
    /// offer, ICE candidates, connection status).
    ///
    /// `env_vars` are additional environment variables passed to the child
    /// process (e.g., `ENZREMOTE_EIS_BROKER_FD`, `ENZREMOTE_INPUT_EIS_HELPER`).
    pub async fn start_stream(
        &self,
        session_id: &str,
        callback: FromWebRtcCallback,
    ) -> Result<(), String> {
        self.start_stream_with_env(session_id, callback, &[]).await
    }

    /// Like [`Self::start_stream`] but with additional environment variables.
    pub async fn start_stream_with_env(
        &self,
        session_id: &str,
        callback: FromWebRtcCallback,
        env_vars: &[(&str, &str)],
    ) -> Result<(), String> {
        // ── Phase 1: validate + reserve (short lock) ─────────────────
        let socket_path;
        {
            let mut sessions = self.sessions.lock().await;
            if sessions.contains_key(session_id) {
                return Err(format!(
                    "WebRTC process already running for session {session_id}"
                ));
            }

            // Validate session_id: must be non-empty, within length limit, and
            // contain only safe characters (alphanumeric, hyphen, underscore).
            if session_id.is_empty() {
                return Err("session_id must not be empty".to_string());
            }
            if session_id.len() > Self::MAX_SESSION_ID_LEN {
                return Err(format!(
                    "session_id too long ({} > {} chars)",
                    session_id.len(),
                    Self::MAX_SESSION_ID_LEN,
                ));
            }
            if !session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(format!(
                    "session_id contains invalid characters (allowed: a-z A-Z 0-9 - _): {session_id}"
                ));
            }

            socket_path = self.socket_dir.join(format!("webrtc-{session_id}.sock"));
            // Ensure the final path fits within Unix socket address limits
            // (sockaddr_un.sun_path is 108 bytes on Linux).
            if socket_path.as_os_str().len() > 104 {
                return Err(format!(
                    "socket path too long ({} bytes, max 104)",
                    socket_path.as_os_str().len(),
                ));
            }

            // Insert a Starting reservation so concurrent callers know a
            // launch is in progress for this session_id.
            sessions.insert(session_id.to_string(), SessionSlot::Starting);
            // Lock is dropped here — slow I/O happens outside.
        }

        // ── Phase 2: spawn + READY + connect (no lock held) ──────────
        // Helper macro to remove the reservation on error.
        let remove_reservation = |sessions: &mut HashMap<String, SessionSlot>, sid: &str| {
            if let Some(SessionSlot::Starting) = sessions.get(sid) {
                sessions.remove(sid);
            }
        };

        // Clean up stale socket.
        let _ = tokio::fs::remove_file(&socket_path).await;

        // Launch the WebRTC server process.
        let mut cmd = Command::new(&self.binary_path);
        cmd.arg(
            socket_path
                .to_str()
                .ok_or_else(|| "socket path is not valid UTF-8".to_string())?,
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
        for key in [
            "ENZREMOTE_PIPEWIRE_TARGET",
            "ENZREMOTE_PIPEWIRE_FD",
            "ENZREMOTE_GRIM_DISPLAY",
            "ENZREMOTE_WAYLAND_DISPLAY",
            "ENZREMOTE_HYPRLAND_INSTANCE",
            "ENZREMOTE_EIS_BROKER_FD",
        ] {
            cmd.env_remove(key);
        }
        for (key, value) in env_vars {
            cmd.env(key, value);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let mut sessions = self.sessions.lock().await;
                remove_reservation(&mut sessions, session_id);
                return Err(format!("failed to launch webrtc-server: {e}"));
            }
        };

        // Read the READY line from stdout, with a size cap to prevent a
        // malicious child from forcing unbounded memory allocation.
        let stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let mut sessions = self.sessions.lock().await;
                remove_reservation(&mut sessions, session_id);
                return Err("webrtc-server has no stdout".to_string());
            }
        };
        let mut stdout = BufReader::new(tokio::io::AsyncReadExt::take(
            stdout,
            Self::MAX_READY_LINE_BYTES as u64,
        ));
        let mut ready_line = String::new();
        let read_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            stdout.read_line(&mut ready_line),
        )
        .await;
        match read_result {
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = tokio::fs::remove_file(&socket_path).await;
                let mut sessions = self.sessions.lock().await;
                remove_reservation(&mut sessions, session_id);
                return Err("webrtc-server timed out waiting for READY".to_string());
            }
            Ok(Err(e)) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = tokio::fs::remove_file(&socket_path).await;
                let mut sessions = self.sessions.lock().await;
                remove_reservation(&mut sessions, session_id);
                return Err(format!("failed to read READY from webrtc-server: {e}"));
            }
            Ok(Ok(_)) => {}
        }

        if !ready_line.starts_with("READY ") {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = tokio::fs::remove_file(&socket_path).await;
            let mut sessions = self.sessions.lock().await;
            remove_reservation(&mut sessions, session_id);
            return Err(format!(
                "webrtc-server did not report READY: {}",
                ready_line.trim()
            ));
        }

        // Connect to the IPC socket. If this fails, ensure the child
        // process is killed and reaped so it doesn't become an orphan.
        let stream = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            UnixStream::connect(&socket_path),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = tokio::fs::remove_file(&socket_path).await;
                let mut sessions = self.sessions.lock().await;
                remove_reservation(&mut sessions, session_id);
                return Err(format!("failed to connect to webrtc-server IPC: {e}"));
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = tokio::fs::remove_file(&socket_path).await;
                let mut sessions = self.sessions.lock().await;
                remove_reservation(&mut sessions, session_id);
                return Err("timed out connecting to webrtc-server IPC socket".to_string());
            }
        };

        let (read_half, write_half) = stream.into_split();

        // ── Phase 3: promote reservation to Running (short lock) ─────
        let session_id_owned = session_id.to_string();
        let sessions_for_reader = self.sessions.clone();
        let cb = callback;
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);

        {
            let mut sessions = self.sessions.lock().await;
            sessions.insert(
                session_id.to_string(),
                SessionSlot::Running(Box::new(WebRtcProcess {
                    child,
                    ipc_write: write_half,
                    socket_path,
                    generation,
                })),
            );
        }

        // Client recovery depends on the terminal reason; a normal exit must not replace it.
        tokio::spawn(async move {
            let reader = BufReader::new(read_half);
            let mut lines = reader.lines();
            let mut reported_error = false;
            while let Ok(Some(line)) = lines.next_line().await {
                if line.is_empty() {
                    continue;
                }
                // Defence-in-depth: reject absurdly large lines before parsing.
                if line.len() > MAX_IPC_MESSAGE_SIZE + 128 {
                    eprintln!(
                        "webrtc-bridge: IPC line from session {session_id_owned} too large ({} bytes), dropping",
                        line.len()
                    );
                    continue;
                }
                match parse_from_webrtc(&line) {
                    Ok(msg) => {
                        let current=sessions_for_reader.lock().await.get(&session_id_owned).is_some_and(
                            |slot| matches!(slot,SessionSlot::Running(p) if p.generation==generation));
                        if !current {
                            break;
                        }
                        if !reported_error {
                            reported_error = matches!(msg, FromWebRtc::Error { .. });
                            cb(&session_id_owned, msg);
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "webrtc-bridge: invalid IPC message from session {session_id_owned}: {e}"
                        );
                    }
                }
            }
            // IPC reader exited — the child process has closed its end.
            // Clean up the session entry, but ONLY if the generation matches.
            // A rapid stop+start on the same session_id increments the
            // generation, so a stale reader won't clobber the new entry.
            let mut sessions = sessions_for_reader.lock().await;
            let should_remove = sessions.get(&session_id_owned).is_some_and(
                |slot| matches!(slot, SessionSlot::Running(p) if p.generation == generation),
            );
            if should_remove {
                if let Some(SessionSlot::Running(mut process)) = sessions.remove(&session_id_owned)
                {
                    let exit_status = match tokio::time::timeout(
                        std::time::Duration::from_secs(3),
                        process.child.wait(),
                    )
                    .await
                    {
                        Ok(status) => status.ok(),
                        Err(_) => {
                            let _ = process.child.start_kill();
                            tokio::time::timeout(
                                std::time::Duration::from_secs(1),
                                process.child.wait(),
                            )
                            .await
                            .ok()
                            .and_then(Result::ok)
                        }
                    };
                    let status_str = exit_status
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "unknown".into());
                    let _ = tokio::fs::remove_file(&process.socket_path).await;
                    eprintln!(
                        "webrtc-bridge: session {session_id_owned} process exited ({status_str}), cleaned up"
                    );
                    drop(sessions); // release lock before callback
                    if !reported_error {
                        cb(
                            &session_id_owned,
                            FromWebRtc::Error {
                                reason: format!("WebRTC pipeline process exited ({status_str})"),
                            },
                        );
                    }
                }
            } else {
                eprintln!(
                    "webrtc-bridge: IPC reader finished for session {session_id_owned} (generation mismatch or already cleaned up)"
                );
            }
        });

        Ok(())
    }

    /// Send an SDP answer from the client to the WebRTC process.
    pub async fn send_answer(&self, session_id: &str, sdp: &str) -> Result<(), String> {
        let msg = ToWebRtc::Answer {
            sdp: sdp.to_string(),
        };
        self.send_ipc(session_id, &msg).await
    }

    /// Send an ICE candidate from the client to the WebRTC process.
    pub async fn send_ice(
        &self,
        session_id: &str,
        candidate: &str,
        sdp_m_line_index: u32,
    ) -> Result<(), String> {
        let msg = ToWebRtc::Ice {
            candidate: candidate.to_string(),
            sdp_m_line_index,
        };
        self.send_ipc(session_id, &msg).await
    }

    /// Stop the WebRTC process for a session.
    ///
    /// Sends a `Shutdown` IPC command and waits up to 2 seconds
    /// for the process to exit before escalating to `kill()`.
    pub async fn stop_stream(&self, session_id: &str) -> Result<(), String> {
        let mut sessions = self.sessions.lock().await;
        match sessions.remove(session_id) {
            Some(SessionSlot::Running(mut process)) => {
                // Drop lock before potentially slow I/O.
                drop(sessions);
                Self::stop_process(&mut process).await;
                Ok(())
            }
            Some(SessionSlot::Starting) => {
                // A launch is in progress. Put the reservation back and
                // let the caller know.
                sessions.insert(session_id.to_string(), SessionSlot::Starting);
                Err(format!("session {session_id} is still starting"))
            }
            None => Err(format!(
                "no WebRTC process running for session {session_id}"
            )),
        }
    }

    /// Check if a WebRTC process is running (or starting) for a session.
    pub async fn has_stream(&self, session_id: &str) -> bool {
        self.sessions.lock().await.contains_key(session_id)
    }

    /// Stop all WebRTC processes (for server shutdown).
    pub async fn stop_all(&self) {
        let mut sessions = self.sessions.lock().await;
        let entries: Vec<(String, SessionSlot)> = sessions.drain().collect();
        drop(sessions); // release lock before slow I/O

        for (session_id, slot) in entries {
            if let SessionSlot::Running(mut process) = slot {
                Self::stop_process(&mut process).await;
                eprintln!("webrtc-bridge: stopped process for session {session_id}");
            }
            // Starting slots are simply discarded; the launch will see
            // the reservation is gone and clean up its child.
        }
    }

    /// Return the number of active WebRTC processes (Running only).
    pub async fn active_count(&self) -> usize {
        self.sessions
            .lock()
            .await
            .values()
            .filter(|s| matches!(s, SessionSlot::Running(_)))
            .count()
    }

    // ── Internal ──────────────────────────────────────────────────────

    /// Gracefully stop a single process: send Shutdown, wait, then kill.
    async fn stop_process(process: &mut WebRtcProcess) {
        // Send shutdown command (best-effort, with timeout).
        let _ = Self::send_ipc_msg(&mut process.ipc_write, &ToWebRtc::Shutdown).await;

        // Wait for graceful exit with timeout.
        let graceful = tokio::time::timeout(
            std::time::Duration::from_secs(GRACEFUL_SHUTDOWN_SECS),
            process.child.wait(),
        )
        .await;

        if graceful.is_err() {
            // Process didn't exit in time — force kill.
            eprintln!("webrtc-bridge: process did not exit gracefully, sending SIGKILL");
            let _ = process.child.kill().await;
            let _ = process.child.wait().await;
        }

        // Clean up socket file.
        let _ = tokio::fs::remove_file(&process.socket_path).await;
    }

    async fn send_ipc(&self, session_id: &str, msg: &ToWebRtc) -> Result<(), String> {
        let mut sessions = self.sessions.lock().await;
        let process = match sessions.get_mut(session_id) {
            Some(SessionSlot::Running(p)) => p,
            Some(SessionSlot::Starting) => {
                return Err(format!("session {session_id} is still starting"));
            }
            None => {
                return Err(format!(
                    "no WebRTC process running for session {session_id}"
                ));
            }
        };
        let result = Self::send_ipc_msg(&mut process.ipc_write, msg).await;
        if result.is_err() {
            // Writer is potentially poisoned (partial write on timeout).
            // Remove and kill the process to prevent corrupted IPC state.
            if let Some(SessionSlot::Running(mut dead_process)) = sessions.remove(session_id) {
                drop(sessions);
                eprintln!(
                    "webrtc-bridge: IPC write failed for session {session_id}, killing process"
                );
                let _ = dead_process.child.kill().await;
                let _ = dead_process.child.wait().await;
                let _ = tokio::fs::remove_file(&dead_process.socket_path).await;
            }
        }
        result
    }

    async fn send_ipc_msg(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        msg: &ToWebRtc,
    ) -> Result<(), String> {
        let json = serialize_message(msg)?;
        if json.len() > MAX_IPC_MESSAGE_SIZE {
            return Err(format!(
                "outgoing IPC message exceeds size limit ({} > {MAX_IPC_MESSAGE_SIZE} bytes)",
                json.len(),
            ));
        }
        let line = format!("{json}\n");
        tokio::time::timeout(
            std::time::Duration::from_secs(IPC_WRITE_TIMEOUT_SECS),
            writer.write_all(line.as_bytes()),
        )
        .await
        .map_err(|_| "IPC write timed out".to_string())?
        .map_err(|e| format!("IPC write failed: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a Python mock of enzremote-webrtc-server that:
    /// 1. Binds a Unix socket at argv[1]
    /// 2. Prints "READY <path>" on stdout
    /// 3. Accepts one connection
    /// 4. Sends an SDP offer over IPC
    /// 5. Sends ICE candidates
    /// 6. Reads messages (answer, ICE, shutdown) and echoes to stderr
    /// 7. Sends Connected status
    /// 8. Exits on Shutdown or EOF
    fn write_mock_webrtc_server(dir: &std::path::Path) -> PathBuf {
        let script = dir.join("mock-webrtc-server.py");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
"""Mock enzremote-webrtc-server for integration testing."""
import json, os, socket, sys

socket_path = sys.argv[1]

# Clean up stale socket.
try:
    os.unlink(socket_path)
except FileNotFoundError:
    pass

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.bind(socket_path)
sock.listen(1)

# Signal readiness.
print(f"READY {socket_path}", flush=True)

conn, _ = sock.accept()
f = conn.makefile("rw", buffering=1)

# Send an SDP offer.
offer = json.dumps({"type": "offer", "sdp": "v=0\r\no=mock\r\n"})
f.write(offer + "\n")
f.flush()

# Send an ICE candidate.
ice = json.dumps({"type": "ice", "candidate": "candidate:mock 1 udp 2130706431 192.168.1.1 12345 typ host", "sdp_m_line_index": 0})
f.write(ice + "\n")
f.flush()

# Send Connected status.
connected = json.dumps({"type": "connected"})
f.write(connected + "\n")
f.flush()

# Read messages until shutdown or EOF.
try:
    for line in f:
        line = line.strip()
        if not line:
            continue
        msg = json.loads(line)
        sys.stderr.write(f"mock-server received: {msg}\n")
        sys.stderr.flush()
        if msg.get("type") == "shutdown":
            break
except Exception:
    pass

f.close()
conn.close()
sock.close()
try:
    os.unlink(socket_path)
except FileNotFoundError:
    pass
"#,
        )
        .expect("failed to write mock script");
        // Make executable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("failed to set permissions");
        }
        script
    }

    #[tokio::test]
    async fn bridge_full_lifecycle_with_mock() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Notify;

        let test_dir = std::env::temp_dir().join("enzremote-bridge-test");
        let _ = std::fs::create_dir_all(&test_dir);

        let mock_script = write_mock_webrtc_server(&test_dir);

        // Use python3 to run the script (the bridge launches the binary directly,
        // so we wrap it with a shell script that invokes python3).
        let wrapper = test_dir.join("enzremote-webrtc-server");
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec python3 {} \"$@\"\n", mock_script.display()),
        )
        .expect("failed to write wrapper");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
                .expect("failed to set permissions");
        }

        let bridge = WebRtcBridge::new(wrapper.clone(), test_dir.clone());

        // Track IPC messages from the mock server.
        let offer_count = Arc::new(AtomicUsize::new(0));
        let ice_count = Arc::new(AtomicUsize::new(0));
        let connected_count = Arc::new(AtomicUsize::new(0));
        let all_received = Arc::new(Notify::new());

        let oc = offer_count.clone();
        let ic = ice_count.clone();
        let cc = connected_count.clone();
        let notifier = all_received.clone();

        let callback: FromWebRtcCallback = Arc::new(move |session_id, msg| {
            assert_eq!(session_id, "test-session");
            match msg {
                enzremote_webrtc_ipc::FromWebRtc::Offer { sdp } => {
                    assert!(sdp.contains("v=0"), "offer should contain SDP");
                    oc.fetch_add(1, Ordering::Relaxed);
                }
                enzremote_webrtc_ipc::FromWebRtc::Ice {
                    candidate,
                    sdp_m_line_index,
                } => {
                    assert!(
                        candidate.contains("candidate:"),
                        "should be an ICE candidate"
                    );
                    assert_eq!(sdp_m_line_index, 0);
                    ic.fetch_add(1, Ordering::Relaxed);
                }
                enzremote_webrtc_ipc::FromWebRtc::Connected => {
                    cc.fetch_add(1, Ordering::Relaxed);
                    notifier.notify_one();
                }
                _ => {}
            }
        });

        // Start the stream.
        let result = bridge.start_stream("test-session", callback).await;
        assert!(result.is_ok(), "start_stream failed: {:?}", result.err());
        assert!(bridge.has_stream("test-session").await);

        // Wait for the Connected message (with timeout).
        tokio::time::timeout(std::time::Duration::from_secs(10), all_received.notified())
            .await
            .expect("timed out waiting for Connected from mock server");

        // Verify we received all expected messages.
        assert_eq!(offer_count.load(Ordering::Relaxed), 1, "expected 1 offer");
        assert_eq!(
            ice_count.load(Ordering::Relaxed),
            1,
            "expected 1 ICE candidate"
        );
        assert_eq!(
            connected_count.load(Ordering::Relaxed),
            1,
            "expected 1 Connected"
        );

        // Send an answer to the mock server.
        let answer_result = bridge
            .send_answer("test-session", "v=0\r\no=answer\r\n")
            .await;
        assert!(
            answer_result.is_ok(),
            "send_answer failed: {:?}",
            answer_result.err()
        );

        // Send an ICE candidate to the mock server.
        let ice_result = bridge
            .send_ice(
                "test-session",
                "candidate:1 1 udp 2130706431 10.0.0.1 9999 typ host",
                0,
            )
            .await;
        assert!(
            ice_result.is_ok(),
            "send_ice failed: {:?}",
            ice_result.err()
        );

        // Stop the stream.
        let stop_result = bridge.stop_stream("test-session").await;
        assert!(
            stop_result.is_ok(),
            "stop_stream failed: {:?}",
            stop_result.err()
        );
        assert!(!bridge.has_stream("test-session").await);

        // Cleanup.
        let _ = std::fs::remove_dir_all(&test_dir);
    }

    #[tokio::test]
    async fn bridge_stop_all_kills_processes() {
        let test_dir = std::env::temp_dir().join("enzremote-bridge-stopall-test");
        let _ = std::fs::create_dir_all(&test_dir);
        let mock_script = write_mock_webrtc_server(&test_dir);

        let wrapper = test_dir.join("enzremote-webrtc-server");
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec python3 {} \"$@\"\n", mock_script.display()),
        )
        .expect("failed to write wrapper");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
                .expect("failed to set permissions");
        }

        let bridge = WebRtcBridge::new(wrapper, test_dir.clone());
        let cb: FromWebRtcCallback = Arc::new(|_, _| {});

        // Start two sessions.
        bridge.start_stream("sess-a", cb.clone()).await.unwrap();
        bridge.start_stream("sess-b", cb).await.unwrap();
        assert!(bridge.has_stream("sess-a").await);
        assert!(bridge.has_stream("sess-b").await);

        // Stop all.
        bridge.stop_all().await;
        assert!(!bridge.has_stream("sess-a").await);
        assert!(!bridge.has_stream("sess-b").await);

        // Cleanup.
        let _ = std::fs::remove_dir_all(&test_dir);
    }

    /// Write a mock server that crashes (exits immediately after READY + accept).
    fn write_crashing_mock(dir: &std::path::Path) -> PathBuf {
        let script = dir.join("crashing-mock.py");
        std::fs::write(
            &script,
            r#"#!/usr/bin/env python3
"""Mock that crashes after accepting the IPC connection."""
import os, socket, sys

socket_path = sys.argv[1]
try:
    os.unlink(socket_path)
except FileNotFoundError:
    pass

sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.bind(socket_path)
sock.listen(1)
print(f"READY {socket_path}", flush=True)
conn, _ = sock.accept()
# Immediately close and exit (simulating a crash).
conn.close()
sock.close()
sys.exit(1)
"#,
        )
        .expect("failed to write crashing mock");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("failed to set permissions");
        }
        script
    }

    #[tokio::test]
    async fn terminal_reason_is_not_replaced_by_worker_exit() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir_in("/tmp").unwrap();
        let dir = fixture.path();
        let mock = write_crashing_mock(dir);
        let code=std::fs::read_to_string(&mock).unwrap().replace("conn.close()", "conn.sendall(b'{\"type\":\"error\",\"reason\":\"ICE connection failed\"}\\n')\nconn.close()");
        std::fs::write(&mock, code).unwrap();
        let wrapper = dir.join("worker");
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec python3 '{}' \"$@\"\n", mock.display()),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bridge = WebRtcBridge::new(wrapper, dir.to_owned());
        let reasons = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = reasons.clone();
        bridge
            .start_stream(
                "reason",
                Arc::new(move |_, message| {
                    if let FromWebRtc::Error { reason } = message {
                        seen.lock().unwrap().push(reason);
                    }
                }),
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while bridge.has_stream("reason").await {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(*reasons.lock().unwrap(), vec!["ICE connection failed"]);
    }

    #[tokio::test]
    async fn closed_ipc_cannot_leave_a_live_worker_waiting_forever() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir_in("/tmp").unwrap();
        let dir = fixture.path();
        let mock = write_crashing_mock(dir);
        let code = std::fs::read_to_string(&mock)
            .unwrap()
            .replace("sys.exit(1)", "import time\ntime.sleep(30)");
        std::fs::write(&mock, code).unwrap();
        let wrapper = dir.join("worker");
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec python3 '{}' \"$@\"\n", mock.display()),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let bridge = WebRtcBridge::new(wrapper, dir.to_owned());
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        bridge
            .start_stream(
                "reap",
                Arc::new(move |_, message| {
                    let _ = sender.send(message);
                }),
            )
            .await
            .unwrap();
        let message = tokio::time::timeout(std::time::Duration::from_secs(6), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(message, FromWebRtc::Error { .. }));
        assert!(!bridge.has_stream("reap").await);
    }

    #[tokio::test]
    async fn bridge_detects_process_crash() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use tokio::sync::Notify;

        let test_dir = std::env::temp_dir().join("enzremote-bridge-crash-test");
        let _ = std::fs::create_dir_all(&test_dir);
        let mock_script = write_crashing_mock(&test_dir);

        let wrapper = test_dir.join("enzremote-webrtc-server");
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec python3 {} \"$@\"\n", mock_script.display()),
        )
        .expect("failed to write wrapper");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
                .expect("failed to set permissions");
        }

        let bridge = WebRtcBridge::new(wrapper, test_dir.clone());
        let error_received = Arc::new(AtomicBool::new(false));
        let error_notifier = Arc::new(Notify::new());

        let er = error_received.clone();
        let en = error_notifier.clone();
        let callback: FromWebRtcCallback = Arc::new(move |session_id, msg| {
            assert_eq!(session_id, "crash-test");
            if let enzremote_webrtc_ipc::FromWebRtc::Error { reason } = msg {
                assert!(
                    reason.contains("exited"),
                    "error should mention exit: {reason}"
                );
                er.store(true, Ordering::Relaxed);
                en.notify_one();
            }
        });

        let result = bridge.start_stream("crash-test", callback).await;
        assert!(result.is_ok(), "start_stream failed: {:?}", result.err());
        assert!(bridge.has_stream("crash-test").await);

        // Wait for the crash-detection error callback.
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            error_notifier.notified(),
        )
        .await
        .expect("timed out waiting for crash detection");

        assert!(
            error_received.load(Ordering::Relaxed),
            "expected error callback from crash"
        );

        // The session should be auto-cleaned up.
        // Give the cleanup task a moment to complete.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !bridge.has_stream("crash-test").await,
            "crashed session should be auto-removed"
        );

        let _ = std::fs::remove_dir_all(&test_dir);
    }
}
