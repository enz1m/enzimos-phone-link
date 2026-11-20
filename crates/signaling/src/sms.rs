//! Text messages from a connected phone, for desktop apps on this computer.
//!
//! The phone opts in (`sms_status`), then answers `sms_request` pushes with
//! `sms_result`. Desktop apps reach it through the control socket (`sms`,
//! `sms_devices`); each phone keeps a revision that increases whenever it
//! reports changed conversations, so apps can poll cheaply.

use crate::protocol::{self, HostMessage};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// How long a phone gets to answer one request.
const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Default)]
pub struct Sms {
    /// Phones sharing messages → revision.
    devices: Mutex<HashMap<String, u64>>,
    pending: Mutex<HashMap<String, (String, oneshot::Sender<serde_json::Value>)>>,
    next: std::sync::atomic::AtomicU64,
}

impl Sms {
    /// `sms_status` from a phone.
    pub fn status(&self, device_id: &str, enabled: bool, changed: bool) {
        let mut devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
        if !enabled {
            devices.remove(device_id);
            return;
        }
        let revision = devices.entry(device_id.to_owned()).or_insert(1);
        if changed {
            *revision += 1;
        }
    }

    /// Phones sharing messages, with their revisions.
    pub fn devices(&self) -> Vec<(String, u64)> {
        let mut list: Vec<_> = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, revision)| (id.clone(), *revision))
            .collect();
        list.sort();
        list
    }

    pub fn revision(&self, device_id: &str) -> Option<u64> {
        self.devices.lock().unwrap_or_else(|e| e.into_inner()).get(device_id).copied()
    }

    /// Ask a phone and wait for its answer.
    pub async fn request(
        &self,
        device_id: &str,
        request: serde_json::Value,
        to_phone: &mpsc::UnboundedSender<(String, HostMessage)>,
    ) -> Result<serde_json::Value, String> {
        if self.revision(device_id).is_none() {
            return Err("that phone does not share text messages (turn it on in the app's settings)".into());
        }
        let request_id = format!("sms-{:08}", self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(request_id.clone(), (device_id.to_owned(), tx));
        let message = HostMessage::SmsRequest { v: protocol::PROTOCOL_VERSION, request_id: request_id.clone(), request };
        if to_phone.send((device_id.to_owned(), message)).is_err() {
            self.forget(&request_id);
            return Err("the host is shutting down".into());
        }
        match tokio::time::timeout(TIMEOUT, rx).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err("the phone disconnected".into()),
            Err(_) => {
                self.forget(&request_id);
                Err("the phone did not answer".into())
            }
        }
    }

    /// `sms_result` from a phone (only the phone that was asked may answer).
    pub fn resolve(&self, device_id: &str, request_id: &str, result: serde_json::Value) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.get(request_id).is_some_and(|(device, _)| device == device_id)
            && let Some((_, tx)) = pending.remove(request_id)
        {
            let _ = tx.send(result);
        }
    }

    /// A phone disconnected: drop its state and fail its requests.
    pub fn drop_device(&self, device_id: &str) {
        self.devices.lock().unwrap_or_else(|e| e.into_inner()).remove(device_id);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (device, _)| device != device_id);
    }

    fn forget(&self, request_id: &str) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(request_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_round_trip_and_revisions() {
        let sms = std::sync::Arc::new(Sms::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(sms.request("phone", serde_json::json!({"op": "conversations"}), &tx).await.is_err());

        sms.status("phone", true, false);
        assert_eq!(sms.revision("phone"), Some(1));
        sms.status("phone", true, true);
        assert_eq!(sms.revision("phone"), Some(2));

        let asking = {
            let sms = sms.clone();
            tokio::spawn(async move { sms.request("phone", serde_json::json!({"op": "conversations"}), &tx).await })
        };
        let (device, message) = rx.recv().await.unwrap();
        assert_eq!(device, "phone");
        let HostMessage::SmsRequest { request_id, .. } = message else { panic!("not an sms request") };
        sms.resolve("other", &request_id, serde_json::json!({"ok": false}));
        sms.resolve("phone", &request_id, serde_json::json!({"ok": true}));
        assert_eq!(asking.await.unwrap().unwrap(), serde_json::json!({"ok": true}));

        sms.status("phone", false, false);
        assert!(sms.devices().is_empty());
    }
}
