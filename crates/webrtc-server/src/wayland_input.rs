//! Worker-backed input for an explicit Wayland display or borrowed desktop seat.
//! Borrowed seats require the Unicode-capable worker: `wtype` would inject into
//! the desktop's physical seat instead of the leased application.

use enzremote_control::ControlMessage;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

/// Configuration for the Wayland input bridge.
pub struct WaylandInputConfig {
    /// Wayland display name (e.g., `wayland-2`).
    pub wayland_display: String,
    /// XDG_RUNTIME_DIR for the Wayland socket.
    pub xdg_runtime_dir: String,
    /// The helper must implement READY/line-command/OK framing.
    pub helper_path: PathBuf,
    /// Compositor viewport width (e.g., 432).
    pub width: u32,
    /// Compositor viewport height (e.g., 960).
    pub height: u32,
    /// Session width as reported by the client (e.g., 1080).
    /// Used to scale incoming coordinates from session space to viewport space.
    pub session_width: u32,
    /// Session height as reported by the client (e.g., 2400).
    pub session_height: u32,
    pub worker_accepts_unicode: bool,
}

/// Running worker process state.
struct WlWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Serializes worker requests from GStreamer's data-channel callback threads.
pub struct WaylandInputBridge {
    config: WaylandInputConfig,
    worker: Mutex<Option<WlWorker>>,
}

impl WaylandInputBridge {
    /// Create a new Wayland input bridge from environment variables.
    ///
    /// Returns `None` if `ENZREMOTE_WAYLAND_DISPLAY` is not set or the
    /// helper binary is not found.
    pub fn from_environment() -> Option<Self> {
        let wayland_display = std::env::var("ENZREMOTE_WAYLAND_DISPLAY")
            .or_else(|_| std::env::var("WAYLAND_DISPLAY"))
            .ok()?;
        if wayland_display.is_empty() {
            return None;
        }

        let xdg_runtime_dir =
            std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".to_string());

        let width: u32 = std::env::var("ENZREMOTE_VIDEO_WIDTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(544);

        let height: u32 = std::env::var("ENZREMOTE_VIDEO_HEIGHT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1216);

        // Find the helper binary. Check several locations.
        let helper_path = std::env::var("ENZREMOTE_INPUT_WL_HELPER")
            .ok()
            .map(PathBuf::from)
            .or_else(|| {
                // Look relative to the current binary.
                let exe = std::env::current_exe().ok()?;
                let dir = exe.parent()?;
                // target/debug/enzremote-webrtc-server -> target/input-wl/enzremote-input-wl
                let candidate = dir.parent()?.join("input-wl").join("enzremote-input-wl");
                if candidate.is_file() {
                    Some(candidate)
                } else {
                    None
                }
            })?;

        if !helper_path.is_file() {
            eprintln!("wayland-input: helper not found: {}", helper_path.display());
            return None;
        }

        // Session dimensions default to viewport dimensions if not set
        // (meaning coordinates are already in viewport space).
        let session_width: u32 = std::env::var("ENZREMOTE_SESSION_WIDTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(width);
        let session_height: u32 = std::env::var("ENZREMOTE_SESSION_HEIGHT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(height);

        Some(Self::new(WaylandInputConfig {
            wayland_display,
            xdg_runtime_dir,
            helper_path,
            width,
            height,
            session_width,
            session_height,
            worker_accepts_unicode: std::env::var("ENZREMOTE_INPUT_WORKER_UNICODE").as_deref()
                == Ok("1"),
        }))
    }

    /// Create a new bridge with explicit config.
    pub fn new(config: WaylandInputConfig) -> Self {
        Self {
            config,
            worker: Mutex::new(None),
        }
    }

    /// Start the worker process.
    pub fn start(&self) -> Result<(), String> {
        let mut guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return Err("wayland input worker already running".into());
        }

        let mut child = Command::new(&self.config.helper_path)
            .arg(self.config.width.to_string())
            .arg(self.config.height.to_string())
            .env("WAYLAND_DISPLAY", &self.config.wayland_display)
            .env("XDG_RUNTIME_DIR", &self.config.xdg_runtime_dir)
            .env_remove("DISPLAY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("failed to launch wayland input worker: {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "wayland input worker has no stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "wayland input worker has no stdout".to_string())?;
        let mut stdout = BufReader::new(stdout);

        // Wait for READY line (timeout: 5 seconds).
        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(5);
        let mut ready_line = String::new();
        loop {
            if start.elapsed() > timeout {
                let _ = child.kill();
                return Err("wayland input worker timed out waiting for READY".into());
            }
            match stdout.read_line(&mut ready_line) {
                Ok(0) => {
                    let _ = child.kill();
                    return Err("wayland input worker exited before READY".into());
                }
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                Err(e) => {
                    let _ = child.kill();
                    return Err(format!("failed to read READY from worker: {e}"));
                }
            }
        }

        if !ready_line.trim().starts_with("READY") {
            let _ = child.kill();
            return Err(format!(
                "wayland input worker did not report READY: {}",
                ready_line.trim()
            ));
        }

        eprintln!(
            "wayland-input: worker started (pid={}, display={}, viewport={}x{})",
            child.id(),
            self.config.wayland_display,
            self.config.width,
            self.config.height
        );

        *guard = Some(WlWorker {
            child,
            stdin,
            stdout,
        });
        Ok(())
    }

    /// Scale a control message's coordinates from session space to viewport
    /// space if the session and viewport dimensions differ.
    fn scale_message(&self, message: &ControlMessage) -> ControlMessage {
        let sx = self.config.width as f64 / self.config.session_width as f64;
        let sy = self.config.height as f64 / self.config.session_height as f64;

        // No scaling needed if session == viewport (within rounding).
        if (sx - 1.0).abs() < 0.001 && (sy - 1.0).abs() < 0.001 {
            return message.clone();
        }

        let vw = self.config.width as f64;
        let vh = self.config.height as f64;

        match message {
            ControlMessage::Pointer { v, x, y, click } => ControlMessage::Pointer {
                v: *v,
                x: (x * sx).clamp(0.0, vw),
                y: (y * sy).clamp(0.0, vh),
                click: *click,
            },
            ControlMessage::Touch {
                v,
                slot,
                phase,
                x,
                y,
            } => ControlMessage::Touch {
                v: *v,
                slot: *slot,
                phase: phase.clone(),
                x: (x * sx).clamp(0.0, vw),
                y: (y * sy).clamp(0.0, vh),
            },
            ControlMessage::ScrollPixels {
                v,
                x,
                y,
                dx,
                dy,
                stop,
            } => ControlMessage::ScrollPixels {
                v: *v,
                x: (x * sx).clamp(0.0, vw),
                y: (y * sy).clamp(0.0, vh),
                dx: dx * sx,
                dy: dy * sy,
                stop: *stop,
            },
            // Other message types don't have absolute coordinates.
            other => other.clone(),
        }
    }

    /// Forward a control message to the worker.
    pub fn send(&self, message: &ControlMessage) -> Result<(), String> {
        // Scale coordinates from session space to viewport space.
        let scaled = self.scale_message(message);
        let command = match scaled.to_worker_command() {
            Some(cmd) => cmd,
            None => return Ok(()), // Pipeline control, no action needed.
        };

        // Debug: log touch/pointer events to trace coordinate mapping.
        if cfg!(debug_assertions) {
            match message {
                ControlMessage::Touch {
                    slot, phase, x, y, ..
                } => {
                    eprintln!(
                        "wayland-input: touch slot={slot} phase={} session=({x:.0},{y:.0}) \
                         -> viewport cmd: {command}",
                        phase.as_str()
                    );
                }
                ControlMessage::Pointer { x, y, click, .. } => {
                    eprintln!(
                        "wayland-input: pointer session=({x:.0},{y:.0}) click={click} \
                         -> viewport cmd: {command}"
                    );
                }
                _ => {}
            }
        }

        if let ControlMessage::Text { text, .. } = &scaled
            && !text.is_ascii()
            && !self.config.worker_accepts_unicode
        {
            return self.type_text(text);
        }

        let mut guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());

        // Auto-restart: if the worker is not running, try to restart it.
        if guard.is_none() {
            drop(guard);
            match self.start() {
                Ok(()) => {
                    eprintln!("wayland-input: worker auto-restarted");
                }
                Err(e) => {
                    return Err(format!(
                        "wayland input worker not running and restart failed: {e}"
                    ));
                }
            }
            guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        }

        let worker = guard
            .as_mut()
            .ok_or_else(|| "wayland input worker not running after restart".to_string())?;

        if let Err(e) = writeln!(worker.stdin, "{command}").and_then(|_| worker.stdin.flush()) {
            if let Some(mut dead) = guard.take() {
                let _ = dead.child.kill();
                let _ = dead.child.wait();
            }
            return Err(format!("wayland input worker write failed: {e}"));
        }

        let mut response = String::new();
        let bytes = worker
            .stdout
            .read_line(&mut response)
            .map_err(|e| format!("wayland input worker read failed: {e}"))?;

        if bytes == 0 {
            if let Some(mut dead) = guard.take() {
                let _ = dead.child.kill();
                let _ = dead.child.wait();
            }
            Err("wayland input worker died (EOF)".into())
        } else if response.trim() == "OK" {
            Ok(())
        } else {
            Err("wayland input worker rejected the input request".into())
        }
    }

    fn type_text(&self, text: &str) -> Result<(), String> {
        if text.is_empty() {
            return Ok(());
        }
        let sanitized: String = text
            .chars()
            .filter(|c| !c.is_control() || *c == '\n')
            .collect();
        if sanitized.is_empty() {
            return Ok(());
        }

        let status = Command::new("wtype")
            .arg("--")
            .arg(&sanitized)
            .env("WAYLAND_DISPLAY", &self.config.wayland_display)
            .env("XDG_RUNTIME_DIR", &self.config.xdg_runtime_dir)
            .env_remove("DISPLAY")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("wtype failed to start: {e}"))?;
        if !status.success() {
            return Err(format!("wtype exited with {status}"));
        }
        Ok(())
    }

    /// Stop the worker process.
    pub fn stop(&self) {
        let mut guard = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(mut worker) = guard.take() {
            drop(worker.stdin);
            std::thread::sleep(std::time::Duration::from_millis(100));
            if worker.child.try_wait().ok().flatten().is_none() {
                let _ = worker.child.kill();
                let _ = worker.child.wait();
            }
            eprintln!("wayland-input: worker stopped");
        }
    }
}

impl Drop for WaylandInputBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_scroll_scales_anchor_and_distance_not_wheel_ticks() {
        let bridge = WaylandInputBridge::new(WaylandInputConfig {
            wayland_display: "wayland-test".into(),
            xdg_runtime_dir: "/tmp".into(),
            helper_path: PathBuf::from("/nonexistent"),
            width: 400,
            height: 800,
            session_width: 1000,
            session_height: 2000,
            worker_accepts_unicode: false,
        });
        let message = ControlMessage::ScrollPixels {
            v: 1,
            x: 250.0,
            y: 500.0,
            dx: 2.5,
            dy: 100.0,
            stop: true,
        };
        assert_eq!(
            bridge.scale_message(&message),
            ControlMessage::ScrollPixels {
                v: 1,
                x: 100.0,
                y: 200.0,
                dx: 1.0,
                dy: 40.0,
                stop: true,
            }
        );
    }
}
