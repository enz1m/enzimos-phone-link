//! Interactive pairing approval.
//!
//! A phone that is not yet paired sends `pair_request`. The host keeps the
//! request pending until a person approves it on the computer, through a
//! desktop notification action or `enzremote-signaling approve`. Requests are
//! bounded in number, time out, and are cancelled when the phone disconnects.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, oneshot, watch};

/// How long a pairing request waits for a decision.
pub const PAIR_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum simultaneously pending requests (limits notification spam).
const MAX_PENDING: usize = 3;

/// Decision for a pending pairing request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accept,
    Decline,
}

/// Summary of a pending request for CLI listings.
#[derive(Debug, Clone)]
pub struct PendingSummary {
    pub id: u64,
    pub device_id: String,
    pub device_name: String,
    pub code: String,
    pub age_secs: u64,
}

struct Pending {
    device_id: String,
    device_name: String,
    code: String,
    created: Instant,
    responder: oneshot::Sender<Decision>,
}

/// Registry of pending pairing requests.
pub struct Approvals {
    pending: Mutex<HashMap<u64, Pending>>,
    next_id: AtomicU64,
    prompt: PromptMode,
}

/// How the host asks a person for approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    /// Desktop notification with Pair/Decline actions (`notify-send --wait`).
    Notification,
    /// CLI only (`enzremote-signaling approve`).
    None,
}

impl PromptMode {
    /// `ENZREMOTE_PAIRING_PROMPT`, else the host setting (read again for
    /// each request, so changing it applies at once).
    pub fn from_env() -> Self {
        match std::env::var("ENZREMOTE_PAIRING_PROMPT").as_deref() {
            Ok("none" | "cli" | "off") => Self::None,
            Ok(_) => Self::Notification,
            Err(_) if crate::host_settings::current().approval == "cli" => Self::None,
            Err(_) => Self::Notification,
        }
    }
}

impl Approvals {
    pub fn new(prompt: PromptMode) -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            prompt,
        }
    }

    /// Register a request. Returns its ID and a receiver for the decision.
    pub async fn request(
        &self,
        device_id: &str,
        device_name: &str,
        code: &str,
    ) -> Result<(u64, oneshot::Receiver<Decision>), String> {
        let mut pending = self.pending.lock().await;
        pending
            .retain(|_, p| p.created.elapsed() < PAIR_REQUEST_TIMEOUT && !p.responder.is_closed());
        if pending.values().any(|p| p.device_id == device_id) {
            // A repeated request from the same phone replaces the old one.
            pending.retain(|_, p| p.device_id != device_id);
        }
        if pending.len() >= MAX_PENDING {
            return Err("too many pairing requests are waiting; try again shortly".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        pending.insert(
            id,
            Pending {
                device_id: device_id.into(),
                device_name: device_name.into(),
                code: code.into(),
                created: Instant::now(),
                responder: tx,
            },
        );
        Ok((id, rx))
    }

    /// Resolve by numeric ID or by verification code. With an empty target,
    /// resolves the only pending request (and fails if there are several).
    pub async fn resolve(
        &self,
        target: &str,
        decision: Decision,
    ) -> Result<PendingSummary, String> {
        let mut pending = self.pending.lock().await;
        pending.retain(|_, p| !p.responder.is_closed());
        let target = target.trim().replace([' ', '-'], "");
        let id = if target.is_empty() {
            match pending.len() {
                0 => return Err("no pairing request is waiting".into()),
                1 => *pending.keys().next().unwrap_or(&0),
                _ => {
                    return Err(
                        "several requests are waiting; pass the code shown on the phone".into(),
                    );
                }
            }
        } else if let Some((id, _)) = pending.iter().find(|(_, p)| p.code == target) {
            *id
        } else if let Ok(id) = target.parse::<u64>()
            && pending.contains_key(&id)
        {
            id
        } else {
            return Err("no pending request matches that code".into());
        };
        let Some(entry) = pending.remove(&id) else {
            return Err("request no longer pending".into());
        };
        let summary = PendingSummary {
            id,
            device_id: entry.device_id,
            device_name: entry.device_name,
            code: entry.code,
            age_secs: entry.created.elapsed().as_secs(),
        };
        entry
            .responder
            .send(decision)
            .map_err(|_| "the phone already cancelled this request".to_string())?;
        Ok(summary)
    }

    /// Drop a request (phone disconnected or timed out).
    pub async fn cancel(&self, id: u64) {
        self.pending.lock().await.remove(&id);
    }

    /// Pending requests for display.
    pub async fn list(&self) -> Vec<PendingSummary> {
        let mut pending = self.pending.lock().await;
        pending
            .retain(|_, p| !p.responder.is_closed() && p.created.elapsed() < PAIR_REQUEST_TIMEOUT);
        let mut out: Vec<_> = pending
            .iter()
            .map(|(id, p)| PendingSummary {
                id: *id,
                device_id: p.device_id.clone(),
                device_name: p.device_name.clone(),
                code: p.code.clone(),
                age_secs: p.created.elapsed().as_secs(),
            })
            .collect();
        out.sort_by_key(|p| p.id);
        out
    }

    /// Show the desktop prompt for request `id` until `done` changes.
    /// A notification action resolves the request; dismissing it does not.
    pub fn spawn_prompt(
        self: &std::sync::Arc<Self>,
        id: u64,
        device_name: String,
        code: String,
        mut done: watch::Receiver<bool>,
    ) {
        if self.prompt == PromptMode::None || PromptMode::from_env() == PromptMode::None {
            eprintln!(
                "pairing: a phone is waiting; review it in Settings > Phone Link"
            );
            return;
        }
        let approvals = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let spaced = format!(
                "{} {}",
                &code[..3.min(code.len())],
                &code[3.min(code.len())..]
            );
            let child = tokio::process::Command::new("notify-send")
                .arg("--app-name=enzimOS Remote")
                .arg("--icon=phone")
                .arg("--urgency=critical")
                .arg(format!("--expire-time={}", PAIR_REQUEST_TIMEOUT.as_millis()))
                .arg("--action=accept=Pair")
                .arg("--action=decline=Decline")
                .arg("--wait")
                .arg(format!("Pair with {device_name}?"))
                .arg(format!(
                    "Make sure the phone shows the code {spaced}. Paired phones can open and control your apps."
                ))
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn();
            let child = match child {
                Ok(child) => child,
                Err(error) => {
                    eprintln!(
                        "pairing: could not show desktop prompt ({error}); review the request in Settings > Phone Link"
                    );
                    return;
                }
            };
            eprintln!(
                "pairing: a phone is waiting for desktop approval"
            );
            tokio::select! {
                output = child.wait_with_output() => {
                    let Ok(output) = output else { return };
                    let action = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    let decision = match action.as_str() {
                        "accept" => Decision::Accept,
                        "decline" => Decision::Decline,
                        _ => return,
                    };
                    if let Err(error) = approvals.resolve(&id.to_string(), decision).await {
                        eprintln!("pairing: notification action ignored: {error}");
                    }
                }
                _ = done.changed() => {
                    // Request resolved elsewhere or cancelled: dropping the
                    // child closes the notification process.
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_target_requires_single_request() {
        let approvals = Approvals::new(PromptMode::None);
        assert!(approvals.resolve("", Decision::Accept).await.is_err());
        let (_a, _ra) = approvals.request("a", "A", "111111").await.unwrap();
        let (_b, _rb) = approvals.request("b", "B", "222222").await.unwrap();
        assert!(approvals.resolve("", Decision::Accept).await.is_err());
        assert!(approvals.resolve("222222", Decision::Decline).await.is_ok());
        assert!(approvals.resolve("", Decision::Accept).await.is_ok());
    }

    #[tokio::test]
    async fn pending_requests_are_bounded_and_replaced_per_device() {
        let approvals = Approvals::new(PromptMode::None);
        let mut keep = Vec::new();
        for i in 0..MAX_PENDING {
            keep.push(
                approvals
                    .request(&format!("d{i}"), "D", "000000")
                    .await
                    .unwrap(),
            );
        }
        assert!(approvals.request("extra", "E", "999999").await.is_err());
        // Same device replaces its own request rather than consuming a slot.
        keep.push(approvals.request("d0", "D", "123123").await.unwrap());
        assert_eq!(approvals.list().await.len(), MAX_PENDING);
    }
}
