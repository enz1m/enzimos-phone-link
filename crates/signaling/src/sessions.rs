use crate::desktop_lease::{DesktopAppSession, DesktopSession};
use crate::protocol::SessionInfo;
use crate::shared_desktop::DesktopTarget;
use enzremote_session::{
    Inspection, MediaPreferences, Permissions, SCHEMA_VERSION, SessionManifest, SessionSource,
    Viewport, create_session, default_helper, inspect_session, resize_session_full, resume_session,
    stop_session, stop_stream,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Owns private app processes and borrowed desktop-window leases.
/// Stopping a borrowed source returns its window without terminating the app.
pub struct SessionManager {
    /// Track known session IDs with creation times for listing and timeout.
    known_sessions: Mutex<HashMap<String, SessionMetadata>>,
    desktop_sessions: Mutex<HashMap<String, Arc<tokio::sync::Mutex<DesktopSession>>>>,
}

struct DesktopReservation<'a> {
    manager: &'a SessionManager,
    id: &'a str,
    committed: bool,
}
impl Drop for DesktopReservation<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.manager.remove(self.id);
        }
    }
}

/// Internal metadata for a tracked session.
struct SessionMetadata {
    /// When the session was created.
    created_at: Instant,
    /// When the last client activity occurred (for idle GC).
    last_activity: Instant,
    /// The device that owns this session (set after authentication).
    device_id: Option<String>,
    /// Current WebRTC SDP/ICE negotiation phase.
    negotiation: NegotiationState,
    /// Viewport width requested by the client.
    width: u32,
    /// Viewport height requested by the client.
    height: u32,
    /// Refresh rate requested by the client (Hz).
    refresh_hz: u32,
}

/// WebRTC SDP/ICE negotiation state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum NegotiationState {
    /// Session created but no stream started.
    Idle,
    /// Stream requested, waiting for SDP offer.
    WaitingForOffer,
    /// SDP offer sent to client, waiting for answer.
    OfferSent,
    /// SDP answer received, exchanging ICE candidates.
    Negotiating,
    /// WebRTC connection established, media flowing.
    Connected,
    /// Negotiation failed.
    Failed(String),
}

impl NegotiationState {
    /// Check whether transitioning from `self` to `next` is a valid state
    /// machine transition.
    ///
    /// Valid forward transitions:
    /// - `Idle` → `WaitingForOffer`
    /// - `WaitingForOffer` → `OfferSent`
    /// - `WaitingForOffer` → `Negotiating` (answer before own offer)
    /// - `OfferSent` → `Negotiating`
    /// - `Negotiating` → `Connected`
    ///
    /// Always-valid transitions:
    /// - Any → `Idle` (reset on stream stop or reconnect)
    /// - Any → `Failed(_)` (error at any stage)
    /// - `Failed(_)` → `WaitingForOffer` (retry after failure)
    pub fn can_transition_to(&self, next: &NegotiationState) -> bool {
        // Reset to Idle is always valid (stream stopped, reconnect, etc.)
        if matches!(next, NegotiationState::Idle) {
            return true;
        }
        // Failure is always valid from any state.
        if matches!(next, NegotiationState::Failed(_)) {
            return true;
        }
        matches!(
            (self, next),
            // Normal forward flow
            (NegotiationState::Idle, NegotiationState::WaitingForOffer)
                | (NegotiationState::WaitingForOffer, NegotiationState::OfferSent)
                | (NegotiationState::WaitingForOffer, NegotiationState::Negotiating)
                | (NegotiationState::OfferSent, NegotiationState::Negotiating)
                | (NegotiationState::Negotiating, NegotiationState::Connected)
                // Connected can arrive from OfferSent if pipeline establishes
                // connection before client sends SDP answer through signaling
                | (NegotiationState::OfferSent, NegotiationState::Connected)
                // Retry after failure (Failed→Idle is already covered by
                // the early return above; only WaitingForOffer needs explicit listing)
                | (NegotiationState::Failed(_), NegotiationState::WaitingForOffer)
        )
    }
}

impl std::fmt::Display for NegotiationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "Idle"),
            Self::WaitingForOffer => write!(f, "WaitingForOffer"),
            Self::OfferSent => write!(f, "OfferSent"),
            Self::Negotiating => write!(f, "Negotiating"),
            Self::Connected => write!(f, "Connected"),
            Self::Failed(reason) => write!(f, "Failed({reason})"),
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    /// Create an empty session manager with no known sessions.
    pub fn new() -> Self {
        Self {
            known_sessions: Mutex::new(HashMap::new()),
            desktop_sessions: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn desktop_session(
        &self,
        id: &str,
    ) -> Option<Arc<tokio::sync::Mutex<DesktopSession>>> {
        self.desktop_sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(id)
            .cloned()
    }

    pub(crate) fn is_desktop(&self, id: &str) -> bool {
        self.desktop_session(id).is_some()
    }

    pub(crate) async fn desktop_lease_id(&self, id: &str) -> Option<String> {
        let desktop = self.desktop_session(id)?;
        desktop.lock().await.lease_id().map(str::to_owned)
    }

    pub(crate) fn failure_stamp(&self, id: &str) -> Option<Instant> {
        let sessions = self
            .known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let meta = sessions.get(id)?;
        matches!(meta.negotiation, NegotiationState::Failed(_)).then_some(meta.last_activity)
    }

    pub(crate) async fn release_desktop_failure(
        &self,
        id: &str,
        generation: &str,
        stamp: Option<Instant>,
    ) {
        if let Some(desktop) = self.desktop_session(id) {
            let mut desktop = desktop.lock().await;
            if stamp.is_some()
                && self.failure_stamp(id) == stamp
                && desktop.lease_id() == Some(generation)
            {
                let _ = desktop.release().await;
            }
        }
    }

    pub(crate) async fn ensure_desktop_active(&self, id: &str) -> Result<(), String> {
        if let Some(desktop) = self.desktop_session(id) {
            let mut desktop = desktop.lock().await;
            if !desktop.is_active().await {
                let _ = desktop.release().await;
                desktop.acquire().await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn desktop_worker_environment(
        &self,
        id: &str,
    ) -> Result<Option<Vec<(&'static str, String)>>, String> {
        if let Some(desktop) = self.desktop_session(id) {
            return desktop.lock().await.worker_environment().map(Some);
        }
        Ok(None)
    }

    pub(crate) async fn create_desktop_app(
        &self,
        id: String,
        device_id: &str,
        target: DesktopTarget,
        app_id: String,
        viewport: Viewport,
    ) -> Result<(u32, u32), String> {
        SessionManifest {
            schema_version: SCHEMA_VERSION,
            id: id.clone(),
            source: SessionSource::RemoteWorkspace,
            viewport,
            permissions: Permissions::default(),
            media: MediaPreferences::default(),
        }
        .validate()?;
        {
            let mut sessions = self
                .known_sessions
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if sessions.contains_key(&id) {
                return Err("Session already exists".into());
            }
            let now = Instant::now();
            sessions.insert(
                id.clone(),
                SessionMetadata {
                    created_at: now,
                    last_activity: now,
                    device_id: Some(device_id.into()),
                    negotiation: NegotiationState::Idle,
                    width: viewport.width,
                    height: viewport.height,
                    refresh_hz: viewport.refresh_hz,
                },
            );
        }
        let mut reservation = DesktopReservation {
            manager: self,
            id: &id,
            committed: false,
        };
        let window_id = target.find_or_launch_app(&app_id).await?;
        let mut desktop = DesktopAppSession::new(target, window_id, viewport);
        desktop.acquire().await?;
        self.desktop_sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                id.clone(),
                Arc::new(tokio::sync::Mutex::new(DesktopSession::App(Box::new(
                    desktop,
                )))),
            );
        reservation.committed = true;
        Ok((viewport.width, viewport.height))
    }

    pub(crate) async fn create_desktop_mirror(
        &self,
        id: String,
        device_id: &str,
        target: DesktopTarget,
        output: Option<&str>,
    ) -> Result<Viewport, String> {
        let mut desktop = crate::desktop_mirror::DesktopMirrorSession::new(target, output).await?;
        desktop.acquire().await?;
        let viewport = desktop.viewport;
        SessionManifest {
            schema_version: SCHEMA_VERSION,
            id: id.clone(),
            source: SessionSource::DesktopMirror {
                output: output.map(str::to_owned),
            },
            viewport,
            permissions: Permissions::default(),
            media: MediaPreferences::default(),
        }
        .validate()?;
        {
            let mut sessions = self
                .known_sessions
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if sessions.contains_key(&id) {
                return Err("Session already exists".into());
            }
            let now = Instant::now();
            sessions.insert(
                id.clone(),
                SessionMetadata {
                    created_at: now,
                    last_activity: now,
                    device_id: Some(device_id.into()),
                    negotiation: NegotiationState::Idle,
                    width: viewport.width,
                    height: viewport.height,
                    refresh_hz: viewport.refresh_hz,
                },
            );
        }
        self.desktop_sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                id,
                Arc::new(tokio::sync::Mutex::new(DesktopSession::Mirror(desktop))),
            );
        Ok(viewport)
    }

    /// Create a launched-app session.
    ///
    /// Returns `Err` if a session with the same ID already exists.
    pub async fn create(
        &self,
        session_id: String,
        command: Vec<String>,
        width: u32,
        height: u32,
        scale: f32,
        refresh_hz: u32,
    ) -> Result<(u32, u32), String> {
        // Atomically reserve the session ID to prevent TOCTOU races where
        // two concurrent create() calls both pass the duplicate check and
        // then one silently overwrites the other's metadata (leaking the
        // first process).
        let now = Instant::now();
        {
            let mut sessions = self
                .known_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if sessions.contains_key(&session_id) {
                return Err(format!(
                    "session '{session_id}' already exists; use session_resume to reconnect"
                ));
            }
            // Insert a placeholder so no concurrent create() can claim this ID.
            sessions.insert(
                session_id.clone(),
                SessionMetadata {
                    created_at: now,
                    last_activity: now,
                    device_id: None,
                    negotiation: NegotiationState::Idle,
                    width,
                    height,
                    refresh_hz,
                },
            );
        }

        let manifest = SessionManifest {
            schema_version: SCHEMA_VERSION,
            id: session_id.clone(),
            source: SessionSource::LaunchedApp { command },
            viewport: Viewport {
                width,
                height,
                scale,
                refresh_hz,
            },
            permissions: Permissions {
                input: true,
                clipboard: false,
                audio_playback: false,
                microphone: false,
            },
            media: MediaPreferences::default(),
        };
        if let Err(e) = manifest.validate() {
            // Remove the placeholder on validation failure.
            self.known_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&session_id);
            return Err(e);
        }

        let sid_for_task = session_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            let helper = default_helper()?;
            let state = create_session(manifest, &helper)?;
            Ok::<_, String>((
                state.manifest.viewport.width,
                state.manifest.viewport.height,
            ))
        })
        .await
        .map_err(|e| format!("session task panicked: {e}"));

        match result {
            Ok(Ok(dims)) => Ok(dims),
            Ok(Err(e)) | Err(e) => {
                // Process creation failed — remove the placeholder so the ID
                // can be reused.
                self.known_sessions
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .remove(&sid_for_task);
                Err(e)
            }
        }
    }

    /// Create a demo session that is registered in the session manager but
    /// does **not** launch a compositor or application. This allows
    /// `stream_start` to work with `enzremote-webrtc-server`'s built-in
    /// `videotestsrc` pattern for end-to-end testing without a real desktop
    /// session.
    ///
    /// Returns `Err` if a session with the same ID already exists.
    pub fn create_demo(
        &self,
        session_id: String,
        width: u32,
        height: u32,
    ) -> Result<(u32, u32), String> {
        let now = Instant::now();
        let mut sessions = self
            .known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if sessions.contains_key(&session_id) {
            return Err(format!(
                "session '{session_id}' already exists; use session_resume to reconnect"
            ));
        }
        sessions.insert(
            session_id,
            SessionMetadata {
                created_at: now,
                last_activity: now,
                device_id: None,
                negotiation: NegotiationState::Idle,
                width,
                height,
                refresh_hz: 60, // Default for import/resume
            },
        );
        Ok((width, height))
    }

    /// Resume an interrupted session. Returns (width, height, scale, refresh_hz).
    ///
    /// Ensures the session is registered in `known_sessions` so it is visible
    /// in `list()`, eligible for `gc_stale()`, and trackable via
    /// `touch()`/`set_device_id()`/`set_negotiation_state()`.
    pub async fn resume(&self, session_id: String) -> Result<(u32, u32, f32, u32), String> {
        if let Some(desktop) = self.desktop_session(&session_id) {
            let mut desktop = desktop.lock().await;
            if !desktop.is_active().await {
                let _ = desktop.release().await;
                desktop.acquire().await?;
            }
            let viewport = desktop.viewport();
            self.record_viewport(
                &session_id,
                viewport.width,
                viewport.height,
                Some(viewport.refresh_hz),
            );
            self.touch(&session_id);
            return Ok((
                viewport.width,
                viewport.height,
                viewport.scale,
                viewport.refresh_hz,
            ));
        }
        let sid = session_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            let state = resume_session(&sid)?;
            Ok::<_, String>((
                state.manifest.viewport.width,
                state.manifest.viewport.height,
                state.manifest.viewport.scale,
                state.manifest.viewport.refresh_hz,
            ))
        })
        .await
        .map_err(|e| format!("resume task panicked: {e}"))??;

        // Register (or refresh) the session in known_sessions so it appears
        // in list() and is eligible for GC, touch tracking, etc.
        let now = Instant::now();
        let mut sessions = self
            .known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (rw, rh, _, _) = result;
        sessions
            .entry(session_id)
            .or_insert_with(|| SessionMetadata {
                created_at: now,
                last_activity: now,
                device_id: None,
                negotiation: NegotiationState::Idle,
                width: rw,
                height: rh,
                refresh_hz: 60,
            })
            .last_activity = now;

        Ok(result)
    }

    /// Inspect a session.
    pub async fn inspect(&self, session_id: String) -> Result<Inspection, String> {
        if let Some(desktop) = self.desktop_session(&session_id) {
            return desktop.lock().await.inspection(&session_id).await;
        }
        tokio::task::spawn_blocking(move || inspect_session(&session_id))
            .await
            .map_err(|e| format!("inspect task panicked: {e}"))?
    }

    /// Resize an active session's viewport with optional scale and refresh rate.
    pub async fn resize(
        &self,
        session_id: String,
        width: u32,
        height: u32,
        scale: Option<f32>,
        refresh_hz: Option<u32>,
    ) -> Result<(u32, u32), String> {
        if let Some(desktop) = self.desktop_session(&session_id) {
            let mut desktop = desktop.lock().await;
            let viewport = Viewport {
                width,
                height,
                scale: scale.unwrap_or(desktop.viewport().scale),
                refresh_hz: refresh_hz.unwrap_or(desktop.viewport().refresh_hz),
            };
            desktop.resize(viewport).await?;
            self.record_viewport(&session_id, width, height, Some(viewport.refresh_hz));
            return Ok((width, height));
        }
        let resize_id = session_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            let state = resize_session_full(&session_id, width, height, scale, refresh_hz)?;
            Ok::<_, String>((
                state.manifest.viewport.width,
                state.manifest.viewport.height,
            ))
        })
        .await
        .map_err(|e| format!("resize task panicked: {e}"))??;
        self.record_viewport(&resize_id, result.0, result.1, refresh_hz);
        Ok(result)
    }

    fn record_viewport(&self, session_id: &str, width: u32, height: u32, refresh_hz: Option<u32>) {
        if let Some(meta) = self
            .known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(session_id)
        {
            meta.width = width;
            meta.height = height;
            if let Some(fps) = refresh_hz {
                meta.refresh_hz = fps;
            }
        }
    }

    /// Terminates owned private apps, but only releases borrowed desktop windows.
    pub async fn stop(&self, session_id: String) -> Result<(), String> {
        if let Some(desktop) = self.desktop_session(&session_id) {
            let result = desktop.lock().await.release().await;
            self.remove(&session_id);
            return result;
        }
        let id = session_id.clone();
        tokio::task::spawn_blocking(move || {
            stop_session(&id)?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|e| format!("stop task panicked: {e}"))??;

        self.known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&session_id);

        Ok(())
    }

    /// Get the number of tracked sessions.
    pub fn session_count(&self) -> usize {
        self.known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Remove a session by ID. Called when the WebRTC pipeline process
    /// dies so the client can create a new session with the same ID.
    pub fn remove(&self, session_id: &str) {
        self.desktop_sessions
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(session_id);
        let removed = self
            .known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        if removed.is_some() {
            eprintln!("session-cleanup: removed dead session '{session_id}'");
        }
    }

    /// Stops media while retaining the app; borrowed windows return to the desktop.
    pub async fn stop_stream_only(&self, session_id: String) -> Result<(), String> {
        if let Some(desktop) = self.desktop_session(&session_id) {
            return desktop.lock().await.release().await;
        }
        tokio::task::spawn_blocking(move || {
            stop_stream(&session_id)?;
            Ok::<_, String>(())
        })
        .await
        .map_err(|e| format!("stop-stream task panicked: {e}"))?
    }

    /// Get the viewport dimensions for a session.
    ///
    /// Returns `(width, height)` if the session exists, `None` otherwise.
    pub fn get_viewport(&self, session_id: &str) -> Option<(u32, u32)> {
        self.known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .map(|meta| (meta.width, meta.height))
    }

    /// Get the refresh rate (Hz) for a session.
    pub fn get_refresh_hz(&self, session_id: &str) -> Option<u32> {
        self.known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .map(|meta| meta.refresh_hz)
    }

    /// Get the current negotiation state for a session.
    pub fn negotiation_state(&self, session_id: &str) -> Option<NegotiationState> {
        self.known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .map(|meta| meta.negotiation.clone())
    }

    /// Transition the negotiation state for a session.
    ///
    /// Validates that the transition is allowed by the state machine. Invalid
    /// transitions are logged and ignored to prevent inconsistent state.
    pub fn set_negotiation_state(&self, session_id: &str, state: NegotiationState) {
        let mut sessions = self
            .known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let Some(meta) = sessions.get_mut(session_id) else {
            eprintln!("sessions: set_negotiation_state for unknown session {session_id}, ignoring");
            return;
        };
        if meta.negotiation.can_transition_to(&state) {
            meta.negotiation = state;
        } else {
            eprintln!(
                "sessions: invalid negotiation transition for {session_id}: {:?} → {state:?}, ignoring",
                meta.negotiation
            );
        }
    }

    /// Force-set the negotiation state without validation.
    /// Only available in tests for setting up specific test scenarios.
    #[cfg(test)]
    pub fn force_negotiation_state(&self, session_id: &str, state: NegotiationState) {
        if let Some(meta) = self
            .known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(session_id)
        {
            meta.negotiation = state;
        }
    }

    /// Set the owner device ID for a session.
    pub fn set_device_id(&self, session_id: &str, device_id: &str) {
        if let Some(meta) = self
            .known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(session_id)
        {
            meta.device_id = Some(device_id.into());
        }
    }

    /// Record activity for a session (updates the last-activity timestamp).
    pub fn touch(&self, session_id: &str) {
        if let Some(meta) = self
            .known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(session_id)
        {
            meta.last_activity = Instant::now();
        }
    }

    /// Remove sessions that have been inactive for longer than `max_idle`.
    ///
    /// Sessions with active WebRTC negotiation (OfferSent, Negotiating,
    /// Connected) are skipped even if idle, because media may be flowing
    /// peer-to-peer without signaling messages.
    ///
    /// Returns the session IDs that were garbage-collected.
    pub async fn gc_stale(&self, max_idle: std::time::Duration) -> Vec<String> {
        let now = Instant::now();
        let stale: Vec<String> = self
            .known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, meta)| {
                if now.duration_since(meta.last_activity) <= max_idle {
                    return false;
                }
                // Skip sessions with active WebRTC negotiation or media flow.
                !matches!(
                    meta.negotiation,
                    NegotiationState::OfferSent
                        | NegotiationState::Negotiating
                        | NegotiationState::Connected
                )
            })
            .map(|(id, _)| id.clone())
            .collect();

        let mut removed = Vec::new();
        for id in stale {
            if self.is_desktop(&id) {
                let _ = self.stop(id.clone()).await;
                removed.push(id);
                continue;
            }
            let id_clone = id.clone();
            let stop_result = tokio::task::spawn_blocking(move || stop_session(&id_clone)).await;

            match stop_result {
                Ok(Ok(_)) | Ok(Err(_)) => {
                    // Re-check: session may have been touched during stop_session.
                    let mut sessions = self
                        .known_sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(meta) = sessions.get(&id) {
                        if meta.last_activity.elapsed() > max_idle {
                            sessions.remove(&id);
                            removed.push(id);
                        } else {
                            eprintln!("gc: session {id} was touched during cleanup, keeping");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("gc: failed to stop session {id}: {e}");
                }
            }
        }

        removed
    }

    /// List known sessions with their current state.
    pub async fn list(&self) -> Vec<SessionInfo> {
        let snapshot: Vec<(String, u64, Option<String>, Option<String>)> = self
            .known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, meta)| {
                (
                    id.clone(),
                    meta.created_at.elapsed().as_secs(),
                    meta.device_id.clone(),
                    Some(meta.negotiation.to_string()),
                )
            })
            .collect();

        let mut sessions = Vec::new();
        for (id, uptime_secs, owner_device_id, negotiation_state) in snapshot {
            let inspection = self.inspect(id.clone()).await;
            match inspection {
                Ok(info) => {
                    sessions.push(SessionInfo {
                        session_id: id,
                        width: info.state.manifest.viewport.width,
                        height: info.state.manifest.viewport.height,
                        active: info.compositor_alive && info.app_alive,
                        uptime_secs,
                        scale: info.state.manifest.viewport.scale,
                        refresh_hz: info.state.manifest.viewport.refresh_hz,
                        owner_device_id,
                        negotiation_state,
                        app_id: None,
                    });
                }
                _ => {
                    // Session may have been cleaned up externally.
                    sessions.push(SessionInfo {
                        session_id: id,
                        width: 0,
                        height: 0,
                        active: false,
                        uptime_secs,
                        scale: enzremote_session::default_scale(),
                        refresh_hz: enzremote_session::default_refresh_hz(),
                        owner_device_id,
                        negotiation_state,
                        app_id: None,
                    });
                }
            }
        }
        sessions
    }

    /// Insert a fake session for testing (does not launch any process).
    #[cfg(test)]
    pub(crate) fn insert_fake(&self, session_id: &str) {
        let now = Instant::now();
        self.known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                session_id.into(),
                SessionMetadata {
                    refresh_hz: 60,
                    created_at: now,
                    last_activity: now,
                    device_id: None,
                    negotiation: NegotiationState::Idle,
                    width: 0,
                    height: 0,
                },
            );
    }

    /// Insert a fake session with old activity for GC testing.
    #[cfg(test)]
    pub(crate) fn insert_fake_stale(&self, session_id: &str, age: std::time::Duration) {
        let now = Instant::now();
        self.known_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                session_id.into(),
                SessionMetadata {
                    refresh_hz: 60,
                    created_at: now - age,
                    last_activity: now - age,
                    device_id: None,
                    negotiation: NegotiationState::Idle,
                    width: 0,
                    height: 0,
                },
            );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resized_viewport_is_used_by_the_next_stream() {
        let manager = SessionManager::new();
        manager.insert_fake("rotated");
        manager.record_viewport("rotated", 2222, 1078, Some(30));
        assert_eq!(manager.get_viewport("rotated"), Some((2222, 1078)));
        assert_eq!(manager.get_refresh_hz("rotated"), Some(30));
    }

    #[test]
    fn failure_timeout_is_fenced_by_subsequent_session_activity() {
        let manager = SessionManager::new();
        manager.insert_fake("failure-timer");
        assert_eq!(manager.failure_stamp("failure-timer"), None);
        manager.force_negotiation_state(
            "failure-timer",
            NegotiationState::Failed("ICE connection failed".into()),
        );
        let old = manager.failure_stamp("failure-timer").unwrap();
        manager.force_negotiation_state("failure-timer", NegotiationState::Connected);
        assert_eq!(manager.failure_stamp("failure-timer"), None);
        manager.touch("failure-timer");
        manager.force_negotiation_state(
            "failure-timer",
            NegotiationState::Failed("ICE connection failed".into()),
        );
        assert_ne!(manager.failure_stamp("failure-timer"), Some(old));
    }

    #[tokio::test]
    async fn gc_skips_connected_sessions() {
        let manager = SessionManager::new();
        // Insert a stale session that is in Connected state.
        manager.insert_fake_stale("gc-connected", std::time::Duration::from_secs(7200));
        manager.force_negotiation_state("gc-connected", NegotiationState::Connected);
        // Insert a stale session in Idle state (should be GC'd if possible).
        manager.insert_fake_stale("gc-idle", std::time::Duration::from_secs(7200));

        assert_eq!(manager.session_count(), 2);

        let removed = manager.gc_stale(std::time::Duration::from_secs(3600)).await;

        // The Connected session should NOT be in the removed list.
        assert!(
            !removed.contains(&"gc-connected".to_string()),
            "Connected sessions should be skipped by GC"
        );
        // The Connected session should still be tracked.
        assert_eq!(
            manager.negotiation_state("gc-connected"),
            Some(NegotiationState::Connected)
        );
    }
}
