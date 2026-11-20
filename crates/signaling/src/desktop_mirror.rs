use crate::shared_desktop::DesktopTarget;
use enzremote_session::{
    CaptureEndpoint, Inspection, LifecycleStatus, MediaPreferences, Permissions, SCHEMA_VERSION,
    SessionManifest, SessionSource, SessionState, Viewport,
};

fn geometry_matches(output: &crate::shared_desktop::DesktopOutput, viewport: Viewport) -> bool {
    output.width == viewport.width
        && output.height == viewport.height
        && (output.scale - f64::from(viewport.scale)).abs() < 0.001
        && output.refresh.round().clamp(15., 120.) as u32 == viewport.refresh_hz
}

pub(crate) struct DesktopMirrorSession {
    target: DesktopTarget,
    output: String,
    pub viewport: Viewport,
    active: bool,
    generation: u64,
    watched: bool,
}

impl DesktopMirrorSession {
    pub async fn new(target: DesktopTarget, output: Option<&str>) -> Result<Self, String> {
        let selected = target.output(output).await?;
        Ok(Self {
            target,
            output: selected.name,
            viewport: Viewport {
                width: selected.width,
                height: selected.height,
                scale: selected.scale as f32,
                refresh_hz: selected.refresh.round().clamp(15., 120.) as u32,
            },
            active: false,
            generation: 0,
            watched: false,
        })
    }
    pub async fn acquire(&mut self) -> Result<(), String> {
        let selected = self.target.output(Some(&self.output)).await?;
        self.viewport = Viewport {
            width: selected.width,
            height: selected.height,
            scale: selected.scale as f32,
            refresh_hz: selected.refresh.round().clamp(15., 120.) as u32,
        };
        self.generation = self.generation.wrapping_add(1);
        self.watched = false;
        self.active = true;
        Ok(())
    }
    pub async fn release(&mut self) -> Result<(), String> {
        self.active = false;
        Ok(())
    }
    pub fn watch_id(&self) -> Option<String> {
        self.active.then(|| format!("mirror-{}", self.generation))
    }
    pub fn claim_watch(&mut self) -> Option<String> {
        if self.watched {
            return None;
        }
        let id = self.watch_id()?;
        self.watched = true;
        Some(id)
    }
    pub async fn is_active(&self) -> bool {
        if !self.active {
            return false;
        }
        self.target
            .output(Some(&self.output))
            .await
            .is_ok_and(|output| geometry_matches(&output, self.viewport))
    }
    pub async fn inspection(&self, id: &str) -> Result<Inspection, String> {
        self.target.output(Some(&self.output)).await?;
        Ok(Inspection {
            state: SessionState {
                manifest: SessionManifest {
                    schema_version: SCHEMA_VERSION,
                    id: id.into(),
                    source: SessionSource::DesktopMirror {
                        output: Some(self.output.clone()),
                    },
                    viewport: self.viewport,
                    permissions: Permissions {
                        input: true,
                        ..Default::default()
                    },
                    media: MediaPreferences::default(),
                },
                status: if self.active {
                    LifecycleStatus::Active
                } else {
                    LifecycleStatus::Stopped
                },
                created_at_unix_ms: 0,
                helper: Default::default(),
                runroot: self.target.runroot.clone(),
                buildroot: self.target.buildroot.clone(),
                config: Default::default(),
            },
            compositor_alive: true,
            app_alive: true,
            stream_alive: false,
            input: None,
            capture: if self.active {
                Some(CaptureEndpoint {
                    output: self.output.clone(),
                    wayland_display: self.target.display.clone(),
                    hyprland_instance: self.target.instance.clone(),
                })
            } else {
                None
            },
        })
    }
    pub fn worker_environment(&self) -> Result<Vec<(&'static str, String)>, String> {
        if !self.active {
            return Err("Desktop stream is inactive".into());
        }
        Ok(vec![
            ("ENZREMOTE_FULL_DESKTOP", "1".into()),
            ("ENZREMOTE_CONTENT_RESIZE", "0".into()),
            ("ENZREMOTE_CAPTURE_CURSOR", "embedded".into()),
            ("ENZREMOTE_INPUT_OUTPUT", self.output.clone()),
            (
                "ENZREMOTE_COMPOSITOR_BUILD_DIR",
                self.target.buildroot.to_string_lossy().into(),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_geometry_changes_invalidate_the_old_input_canvas() {
        let viewport = Viewport {
            width: 1440,
            height: 900,
            scale: 1.,
            refresh_hz: 60,
        };
        let output = crate::shared_desktop::DesktopOutput {
            name: "DP-1".into(),
            width: 1440,
            height: 900,
            scale: 1.,
            refresh: 59.95,
            focused: true,
            transform: 0,
        };
        assert!(geometry_matches(&output, viewport));
        for changed in [
            crate::shared_desktop::DesktopOutput {
                width: 1920,
                ..output.clone()
            },
            crate::shared_desktop::DesktopOutput {
                height: 1080,
                ..output.clone()
            },
            crate::shared_desktop::DesktopOutput {
                scale: 1.5,
                ..output.clone()
            },
            crate::shared_desktop::DesktopOutput {
                refresh: 120.,
                ..output
            },
        ] {
            assert!(!geometry_matches(&changed, viewport));
        }
    }
}
