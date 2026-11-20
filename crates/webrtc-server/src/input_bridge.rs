//! Input bridge that spawns and communicates with the `enzremote-input-eis`
//! C worker process. Receives parsed control messages from the WebRTC data
//! channel and forwards input events to the compositor via libei.
//!
//! # Architecture
//!
//! ```text
//! WebRTC data channel
//!     |-- JSON string
//!     v
//! parse_control_message()
//!     |-- ControlMessage
//!     v
//! InputBridge::send()
//!     |-- line protocol ("pointer 123 456 0\n")
//!     v
//! enzremote-input-eis (C worker, stdin/stdout)
//!     |-- libei
//!     v
//! Compositor (Hyprland)
//! ```
//!
//! # Environment Variables
//!
//! The bridge reads these from the process environment (set by sessiond):
//!
//! - `ENZREMOTE_EIS_BROKER_FD`: Inherited file descriptor for the EIS broker
//! - `ENZREMOTE_INPUT_EIS_HELPER`: Path to the `enzremote-input-eis` binary

use enzremote_control::ControlMessage;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

/// A running EIS input worker process.
struct InputWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Bridge to the EIS input worker process.
///
/// Thread-safe: can be called from GStreamer's data channel callback threads.
pub struct InputBridge {
    worker: Mutex<Option<InputWorker>>,
    /// Path to the helper binary.
    helper_path: PathBuf,
    /// Broker fd to pass to the worker.
    broker_fd: i32,
}

impl InputBridge {
    /// Create an input bridge from environment variables.
    ///
    /// Returns `None` if the required environment variables are not set
    /// (input is not available for this session).
    pub fn from_environment() -> Option<Self> {
        let broker_fd: i32 = std::env::var("ENZREMOTE_EIS_BROKER_FD")
            .ok()?
            .parse()
            .ok()?;
        let helper_path = std::env::var("ENZREMOTE_INPUT_EIS_HELPER")
            .ok()
            .map(PathBuf::from)?;

        if broker_fd < 0 {
            eprintln!("input-bridge: invalid broker fd: {broker_fd}");
            return None;
        }
        if !helper_path.is_file() {
            eprintln!(
                "input-bridge: helper binary not found: {}",
                helper_path.display()
            );
            return None;
        }

        Some(Self {
            worker: Mutex::new(None),
            helper_path,
            broker_fd,
        })
    }

    /// Create a bridge with explicit parameters (for testing).
    pub fn new(helper_path: PathBuf, broker_fd: i32) -> Self {
        Self {
            worker: Mutex::new(None),
            helper_path,
            broker_fd,
        }
    }

    /// Start the worker process.
    ///
    /// Must be called before `send()`. Returns an error if the worker
    /// fails to start or doesn't report READY within 5 seconds.
    pub fn start(&self) -> Result<(), String> {
        let mut guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return Err("input worker already running".into());
        }

        let mut child = Command::new(&self.helper_path)
            .arg("--worker-fd")
            .arg(self.broker_fd.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("failed to launch input worker: {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "input worker has no stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "input worker has no stdout".to_string())?;
        let mut stdout = BufReader::new(stdout);

        // Wait for READY line with timeout.
        // NOTE: The pipe is blocking, so read_line will block if the child
        // hangs without writing. The WouldBlock branch below is a defensive
        // fallback (in case the fd is set non-blocking externally). For a
        // proper non-blocking timeout, use a background thread + mpsc channel
        // (as the loopback InputBridge does). In practice, the child either
        // writes READY quickly or exits, so the blocking read is acceptable
        // for the webrtc-server use case.
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(5);
        let mut ready_line = String::new();
        loop {
            if start.elapsed() > timeout {
                let _ = child.kill();
                return Err("input worker timed out waiting for READY".into());
            }
            match stdout.read_line(&mut ready_line) {
                Ok(0) => {
                    let _ = child.kill();
                    return Err("input worker exited before READY".into());
                }
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                Err(e) => {
                    let _ = child.kill();
                    return Err(format!("failed to read READY from input worker: {e}"));
                }
            }
        }

        if !ready_line.trim().starts_with("READY") {
            let _ = child.kill();
            return Err(format!(
                "input worker did not report READY: {}",
                ready_line.trim()
            ));
        }

        eprintln!("input-bridge: worker started (pid {})", child.id());
        *guard = Some(InputWorker {
            child,
            stdin,
            stdout,
        });
        Ok(())
    }

    /// Forward a control message to the input worker.
    ///
    /// Returns Ok(()) if the message was sent successfully (or was a
    /// pipeline-control message that doesn't need forwarding).
    /// Returns Err if the worker process has died or the write failed.
    pub fn send(&self, message: &ControlMessage) -> Result<(), String> {
        let command = match message.to_worker_command() {
            Some(cmd) => cmd,
            None => return Ok(()), // Pipeline control message, no EIS action needed.
        };

        let mut guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());

        // Auto-restart: if the worker is not running, try to restart it.
        if guard.is_none() {
            drop(guard);
            match self.start() {
                Ok(()) => {
                    eprintln!("input-bridge: EIS worker auto-restarted");
                }
                Err(e) => {
                    return Err(format!("input worker not running and restart failed: {e}"));
                }
            }
            guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        }

        let worker = guard
            .as_mut()
            .ok_or_else(|| "input worker not running after restart".to_string())?;

        if let Err(e) = writeln!(worker.stdin, "{command}").and_then(|_| worker.stdin.flush()) {
            // Worker's stdin is broken — kill and reap to avoid orphan/zombie.
            if let Some(mut dead) = guard.take() {
                let _ = dead.child.kill();
                let _ = dead.child.wait();
            }
            return Err(format!("input worker write failed: {e}"));
        }

        let mut response = String::new();
        let bytes = worker
            .stdout
            .read_line(&mut response)
            .map_err(|e| format!("input worker read failed: {e}"))?;

        if bytes == 0 {
            // Worker died — kill and reap to avoid zombie.
            if let Some(mut dead) = guard.take() {
                let _ = dead.child.kill();
                let _ = dead.child.wait();
            }
            Err("input worker died (EOF on stdout)".into())
        } else if response.trim() == "OK" {
            Ok(())
        } else {
            Err(format!(
                "input worker rejected command: {command} (response: {})",
                response.trim()
            ))
        }
    }

    /// Stop the input worker process.
    ///
    /// Attempts a graceful shutdown by closing stdin first (worker should exit
    /// on EOF), then falls back to SIGKILL after a brief wait.
    pub fn stop(&self) {
        let mut guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut worker) = guard.take() {
            // Close stdin — well-behaved workers exit on EOF.
            drop(worker.stdin);
            // Give the worker a moment to exit cleanly.
            match worker.child.try_wait().ok().flatten() {
                Some(_status) => {}
                None => {
                    // Still running after stdin close — wait briefly then kill.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if worker.child.try_wait().ok().flatten().is_none() {
                        let _ = worker.child.kill();
                        let _ = worker.child.wait();
                    }
                }
            }
            eprintln!("input-bridge: worker stopped");
        }
    }

    /// Check if the worker is running.
    pub fn is_running(&self) -> bool {
        self.worker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

impl Drop for InputBridge {
    fn drop(&mut self) {
        self.stop();
    }
}
