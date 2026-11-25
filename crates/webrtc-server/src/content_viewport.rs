use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use enzremote_webrtc_ipc::compositor::{Compositor, Error as CompositorError};

const CONTROL_TIMEOUT: Duration = Duration::from_millis(500);

/// Maximizing a main window hides floating windows; refocus open dialogs,
/// most recent last, so they stay above it (Lua, expects `floats`).
const RAISE_FLOATING: &str = "table.sort(floats, function(a, b) return a.focus_history_id > b.focus_history_id end); for _, w in ipairs(floats) do hl.dispatch(hl.dsp.focus({ window = \"address:\" .. w.address })) end;";
/// Re-center dialogs in the work area left above the keyboard (Lua, expects
/// `floats`, `lw` and `lh`; Hyprland's own center ignores the reservation).
const CENTER_FLOATING: &str = "for _, w in ipairs(floats) do hl.dispatch(hl.dsp.window.move({ x = math.max(0, math.floor((lw - w.size.x) / 2)), y = math.max(0, math.floor((lh - w.size.y) / 2)), window = \"address:\" .. w.address })) end";

pub struct ContentViewport {
    pub width: u32,
    pub height: u32,
    scale: f64,
    fps: u32,
    instance: String,
    output: String,
    lease_helper: Option<PathBuf>,
}

impl ContentViewport {
    pub fn from_environment() -> Option<Self> {
        if std::env::var("ENZREMOTE_CONTENT_RESIZE").as_deref() == Ok("0") {
            return None;
        }
        // The host supplies these endpoints; peers cannot choose compositor paths.
        std::env::var("ENZREMOTE_WAYLAND_DISPLAY")
            .ok()
            .filter(|v| !v.is_empty())?;
        let lease_helper = if std::env::var_os("ENZREMOTE_DESKTOP_INPUT_SOCKET").is_some() {
            let helper = PathBuf::from(std::env::var_os("ENZREMOTE_INPUT_WL_HELPER")?);
            if !helper.is_absolute() || !helper.is_file() {
                return None;
            }
            Some(helper)
        } else {
            None
        };
        let viewport = Self {
            width: std::env::var("ENZREMOTE_VIDEO_WIDTH").ok()?.parse().ok()?,
            height: std::env::var("ENZREMOTE_VIDEO_HEIGHT").ok()?.parse().ok()?,
            scale: std::env::var("ENZREMOTE_INPUT_SCALE").ok()?.parse().ok()?,
            fps: std::env::var("ENZREMOTE_VIDEO_FPS").ok()?.parse().ok()?,
            instance: std::env::var("ENZREMOTE_HYPRLAND_INSTANCE").ok()?,
            output: std::env::var("ENZREMOTE_CAPTURE_OUTPUT").ok()?,
            lease_helper,
        };
        if viewport.output.is_empty()
            || viewport.output.len() > 80
            || !viewport
                .output
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
            || viewport.instance.is_empty()
            || !viewport
                .instance
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || viewport.fps == 0
            || viewport.fps > 240
        {
            return None;
        }
        viewport
            .reserved_bottom(viewport.width, viewport.height)
            .ok()?;
        Some(viewport)
    }

    fn reserved_bottom(&self, width: u32, height: u32) -> Result<u32, String> {
        if !(240..=8192).contains(&self.width)
            || !(240..=8192).contains(&self.height)
            || width != self.width
            || !(240..=self.height).contains(&height)
            || !self.scale.is_finite()
            || !(0.5..=4.0).contains(&self.scale)
        {
            return Err("content size is outside the fixed capture canvas".into());
        }
        self.reserved_bottom_at(height, self.scale)
    }

    /// Bottom reservation in logical pixels at `scale`. The host may zoom the
    /// canvas out (smaller scale) while a large dialog is open; then the band
    /// is rounded to whole logical pixels.
    fn reserved_bottom_at(&self, height: u32, scale: f64) -> Result<u32, String> {
        let bottom = f64::from(self.height - height) / scale;
        if (scale - self.scale).abs() > 0.001 {
            return Ok(bottom.round() as u32);
        }
        if (bottom - bottom.round()).abs() > 0.001 {
            return Err("content size must align to compositor logical pixels".into());
        }
        Ok(bottom.round() as u32)
    }

    pub fn initialize(&self) -> Result<(), String> {
        if self.lease_helper.is_some() {
            return self.apply(self.width, self.height);
        }
        let monitors: serde_json::Value = serde_json::from_str(&self.monitors()?)
            .map_err(|_| "invalid content resize monitor description")?;
        if !self.matches_canvas(&monitors) {
            return Err("content resize requires a matching single-output canvas".into());
        }
        // Client fullscreen styling is retained, but internal maximization respects
        // the work area. Future main windows inherit the same policy; floating
        // dialogs keep their own size (the session config centers them).
        self.eval("hl.window_rule({ name = \"remote-app-surface\", match = { class = \".*\", float = false }, fullscreen_state = \"1 2\", border_size = 0, rounding = 0, decorate = false, no_anim = true, no_shadow = true })")?;
        self.apply(self.width, self.height)
    }

    pub fn apply(&self, width: u32, height: u32) -> Result<(), String> {
        self.reserved_bottom(width, height)?;
        if let Some(helper) = &self.lease_helper {
            let mut command = Command::new(helper);
            command.args([
                "--content",
                &width.to_string(),
                &height.to_string(),
                &self.scale.to_string(),
            ]);
            return if Self::run_command(command)?.trim() == "OK" {
                Ok(())
            } else {
                Err("Lease rejected content size".into())
            };
        }
        // Keep the scale the host chose (it zooms out for oversized dialogs).
        let scale = self.live_scale().unwrap_or(self.scale);
        let bottom = self.reserved_bottom_at(height, scale)?;
        let output = serde_json::to_string(&self.output).map_err(|e| e.to_string())?;
        let code = format!(
            "local d = hl.dsp and hl.dsp.window or hl.window; local floats = {{}}; for _, w in ipairs(hl.get_windows()) do if w.floating then table.insert(floats, w) else hl.dispatch(d.fullscreen_state({{ internal = 1, client = 2, action = \"set\", window = \"address:\" .. w.address }})) end end; {RAISE_FLOATING} hl.monitor({{ output = {output}, mode = \"{}x{}@{}\", position = \"0x0\", scale = {scale}, reserved = {{ bottom = {bottom} }} }}); local lw, lh = {}, {}; {{CENTER_FLOATING}}",
            self.width, self.height, self.fps,
            (f64::from(self.width) / scale).round(),
            (f64::from(self.height) / scale).round() - f64::from(bottom),
        )
        .replace("{RAISE_FLOATING}", RAISE_FLOATING)
        .replace("{CENTER_FLOATING}", CENTER_FLOATING);
        self.eval(&code)
    }

    fn live_scale(&self) -> Option<f64> {
        let monitors: serde_json::Value = serde_json::from_str(&self.monitors().ok()?).ok()?;
        self.matches_canvas(&monitors).then(|| monitors[0]["scale"].as_f64()).flatten()
    }

    fn matches_canvas(&self, monitors: &serde_json::Value) -> bool {
        monitors.as_array().is_some_and(|items| {
            items.len() == 1 && {
                let m = &items[0];
                m["name"].as_str() == Some(self.output.as_str())
                    && m["width"].as_u64() == Some(u64::from(self.width))
                    && m["height"].as_u64() == Some(u64::from(self.height))
                    && m["scale"]
                        .as_f64()
                        .is_some_and(|s| s >= 0.5 && s <= self.scale + 0.001)
                    && m["refreshRate"]
                        .as_f64()
                        .is_some_and(|r| (r - f64::from(self.fps)).abs() < 0.01)
                    && m["x"].as_i64() == Some(0)
                    && m["y"].as_i64() == Some(0)
                    && m["transform"].as_u64() == Some(0)
            }
        })
    }

    fn eval(&self, code: &str) -> Result<(), String> {
        let code = format!("eval {code}");
        let reply = self.control(|compositor| compositor.command(&code, CONTROL_TIMEOUT))?;
        if reply.trim() != "ok" {
            return Err("compositor rejected content resize".into());
        }
        Ok(())
    }

    fn monitors(&self) -> Result<String, String> {
        self.control(|compositor| compositor.query_json("monitors", CONTROL_TIMEOUT))
    }

    fn control(
        &self,
        request: impl FnOnce(&Compositor) -> Result<String, CompositorError>,
    ) -> Result<String, String> {
        let compositor = Compositor::for_instance(&self.instance)
            .map_err(|e| format!("content resize control unavailable: {e}"))?;
        request(&compositor).map_err(|error| match error {
            CompositorError::Unavailable(e) => format!("content resize control unavailable: {e}"),
            CompositorError::TimedOut => "content resize control timed out".into(),
            CompositorError::Rejected(_) => "compositor rejected content resize".into(),
            CompositorError::InvalidReply => "invalid compositor response".into(),
        })
    }

    fn run_command(mut command: Command) -> Result<String, String> {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("content resize control unavailable: {e}"))?;
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("content resize control timed out".into());
                }
            }
        }
        let output = child.wait_with_output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err("compositor rejected content resize".into());
        }
        String::from_utf8(output.stdout).map_err(|_| "invalid compositor response".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn viewport() -> ContentViewport {
        ContentViewport {
            width: 1078,
            height: 2046,
            scale: 2.75,
            fps: 60,
            instance: "test".into(),
            output: "WAYLAND-1".into(),
            lease_helper: None,
        }
    }
    #[test]
    fn leased_canvas_uses_only_the_capability_helper() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let helper = directory.path().join("content-helper");
        std::fs::write(&helper,"#!/bin/sh\n[ \"$1\" = --content ] && [ \"$2\" = 1078 ] && [ \"$4\" = 2.75 ] || exit 1\nprintf 'OK\\n'\n").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut viewport = viewport();
        viewport.lease_helper = Some(helper.clone());
        viewport.initialize().unwrap();
        viewport.apply(1078, 1144).unwrap();
        std::fs::write(&helper, "#!/bin/sh\nprintf 'ERR rejected\\n'\n").unwrap();
        assert!(viewport.apply(1078, 1144).is_err());
    }
}
