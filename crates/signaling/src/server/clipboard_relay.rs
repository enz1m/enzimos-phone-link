use super::{HostMessage, ServerState, protocol};
use crate::host_settings::{Feature, allowed};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(super) struct Relay {
    revision: u64,
    latest: Option<(String, String, Instant)>,
}

impl Default for Relay {
    fn default() -> Self {
        Self {
            revision: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64,
            latest: None,
        }
    }
}

impl ServerState {
    pub(super) fn clipboard_available(&self) -> bool {
        self.shell_clipboard || self.clipboard.get().is_some()
    }

    pub(super) async fn relay_phone_clipboard(&self, device_id: &str, text: &str) {
        if !*self.session_access.borrow()
            || text.is_empty()
            || text.len() > protocol::MAX_CLIPBOARD_TEXT
            || !allowed(device_id, Feature::Clipboard)
            || !self.connections.lock().await.contains_key(device_id)
        {
            return;
        }
        let mut relay = self.clipboard_relay.lock().await;
        relay.revision = relay.revision.wrapping_add(1);
        relay.latest = Some((device_id.to_owned(), text.to_owned(), Instant::now()));
    }

    pub(crate) async fn publish_shell_clipboard(&self, text: String) -> Result<(), &'static str> {
        if !self.shell_clipboard || !*self.session_access.borrow() {
            return Err("Shell clipboard sharing is unavailable");
        }
        if text.is_empty() || text.len() > protocol::MAX_CLIPBOARD_TEXT {
            return Err("Clipboard text exceeds the sharing limit");
        }
        for (device_id, info) in self.connections.lock().await.iter() {
            if info.clipboard.load(Ordering::Relaxed) && allowed(device_id, Feature::Clipboard) {
                let _ = info.outbox.send(HostMessage::Clipboard {
                    v: protocol::PROTOCOL_VERSION,
                    text: text.clone(),
                });
            }
        }
        Ok(())
    }

    pub(crate) async fn poll_shell_clipboard(&self, since: Option<u64>) -> Value {
        let connections = self.connections.lock().await;
        let mut relay = self.clipboard_relay.lock().await;
        let unlocked = self.shell_clipboard && *self.session_access.borrow();
        let available = unlocked && connections.keys().any(|id| allowed(id, Feature::Clipboard));
        let enabled = unlocked
            && connections.iter().any(|(id, info)| {
                info.clipboard.load(Ordering::Relaxed) && allowed(id, Feature::Clipboard)
            });
        let incoming = relay.latest.take().filter(|(id, _, copied)| {
            unlocked
                && since.is_some_and(|since| since != relay.revision)
                && copied.elapsed() < Duration::from_secs(5)
                && connections.contains_key(id)
                && allowed(id, Feature::Clipboard)
        });
        json!({"ok":true,"available":available,"enabled":enabled,"revision":relay.revision,"text":incoming.map(|(_, text, _)| text)})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{ClientMessage, ConnectionInfo, ServerConfig, handle_client_message};
    use std::sync::{Arc, atomic::AtomicBool};

    #[tokio::test]
    async fn shell_clipboard_delivers_only_fresh_authorized_text_and_stops_on_lock() {
        let mut state = ServerState::new(ServerConfig::default());
        Arc::get_mut(&mut state).unwrap().shell_clipboard = true;
        let (outbox, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (evict_tx, _) = tokio::sync::watch::channel(false);
        state.connections.lock().await.insert(
            "clipboard-fixture".into(),
            ConnectionInfo {
                connected_at: Instant::now(),
                evict_tx,
                revoked: Arc::default(),
                peer: "127.0.0.1:1".parse().unwrap(),
                outbox,
                clipboard: Arc::new(AtomicBool::new(false)),
                video_codecs: Vec::new(),
            },
        );
        state
            .publish_shell_clipboard("not opted in".into())
            .await
            .unwrap();
        assert!(received.try_recv().is_err());
        let _ = handle_client_message(
            &state,
            "clipboard-fixture",
            &ClientMessage::ClipboardSync {
                v: 1,
                enabled: true,
            },
        )
        .await;
        state
            .publish_shell_clipboard("desktop fixture".into())
            .await
            .unwrap();
        assert!(
            matches!(received.try_recv().unwrap(), HostMessage::Clipboard { text, .. } if text == "desktop fixture")
        );
        let _ = handle_client_message(
            &state,
            "clipboard-fixture",
            &ClientMessage::Clipboard {
                v: 1,
                text: "old fixture".into(),
            },
        )
        .await;
        let first = state.poll_shell_clipboard(None).await;
        assert!(
            first["text"].is_null(),
            "a newly started shell must not restore old phone text"
        );
        assert_eq!(first["enabled"], true);
        let _ = handle_client_message(
            &state,
            "clipboard-fixture",
            &ClientMessage::Clipboard {
                v: 1,
                text: "phone fixture".into(),
            },
        )
        .await;
        let delivered = state.poll_shell_clipboard(first["revision"].as_u64()).await;
        assert_eq!(delivered["text"], "phone fixture");
        assert!(
            state.poll_shell_clipboard(first["revision"].as_u64()).await["text"].is_null(),
            "phone text is delivered once"
        );
        state.session_access.send_replace(false);
        assert!(
            state
                .publish_shell_clipboard("locked fixture".into())
                .await
                .is_err()
        );
        let _ = handle_client_message(
            &state,
            "clipboard-fixture",
            &ClientMessage::Clipboard {
                v: 1,
                text: "locked phone".into(),
            },
        )
        .await;
        assert!(
            state
                .poll_shell_clipboard(delivered["revision"].as_u64())
                .await["text"]
                .is_null()
        );
        assert!(received.try_recv().is_err());
        state.session_access.send_replace(true);
        state
            .relay_phone_clipboard("clipboard-fixture", "disconnected fixture")
            .await;
        state.connections.lock().await.clear();
        let after_disconnect = state
            .poll_shell_clipboard(delivered["revision"].as_u64())
            .await;
        assert!(after_disconnect["text"].is_null());
        assert_eq!(after_disconnect["enabled"], false);
    }
}
