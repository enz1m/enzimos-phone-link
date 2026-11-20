//! Video from the phone to the computer: screen mirroring in a window, or
//! the phone camera as a PipeWire camera for other apps.
//!
//! The phone offers a send-only WebRTC video stream; `enzremote-uplink`
//! (crates/uplink: webrtcbin + native enzUI window or `pipewiresink`) answers. Input from the
//! mirror window goes back to the phone as `uplink_input`.

use std::collections::HashMap;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, mpsc::UnboundedSender, oneshot};

use crate::protocol::{self, HostMessage};

struct Uplink {
    device_id: String,
    stdin: ChildStdin,
    child: Child,
}

#[derive(Default)]
pub struct Uplinks {
    active: Mutex<HashMap<String, Uplink>>,
}

fn helper() -> std::path::PathBuf {
    std::env::var_os("ENZREMOTE_UPLINK_HELPER")
        .map(Into::into)
        .unwrap_or_else(|| std::path::PathBuf::from("enzremote-uplink"))
}

/// The receiver is installed (absolute path or found on PATH).
pub fn supported() -> bool {
    let helper = helper();
    if helper.is_absolute() {
        return helper.is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(&helper).is_file()))
}

impl Uplinks {
    /// Start receiving; returns the SDP answer.
    pub async fn start(
        &self,
        device_id: &str,
        device_name: &str,
        uplink_id: &str,
        kind: &str,
        offer: &str,
        to_phone: UnboundedSender<(String, HostMessage)>,
    ) -> Result<String, String> {
        let mode = if kind == "camera" { "camera" } else { "view" };
        if self.active.lock().await.contains_key(uplink_id) {
            return Err("uplink already exists".into());
        }
        let mut child = tokio::process::Command::new(helper())
            .args(["--mode", mode, "--title", device_name])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not start the video receiver: {e}"))?;
        let mut stdin = child.stdin.take().ok_or("receiver stdin")?;
        let stdout = child.stdout.take().ok_or("receiver stdout")?;
        let line = serde_json::json!({"type": "offer", "sdp": offer}).to_string() + "\n";
        stdin.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
        let (answer_tx, answer_rx) = oneshot::channel::<String>();
        let (device, id) = (device_id.to_owned(), uplink_id.to_owned());
        tokio::spawn(async move {
            let mut answer_tx = Some(answer_tx);
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                let v = protocol::PROTOCOL_VERSION;
                match message["type"].as_str() {
                    Some("answer") => {
                        if let (Some(tx), Some(sdp)) = (answer_tx.take(), message["sdp"].as_str()) {
                            let _ = tx.send(sdp.to_owned());
                        }
                    }
                    Some("ice") => {
                        let _ = to_phone.send((device.clone(), HostMessage::UplinkIce {
                            v,
                            uplink_id: id.clone(),
                            candidate: message["candidate"].as_str().unwrap_or_default().to_owned(),
                            sdp_m_line_index: message["mline"].as_u64().unwrap_or(0) as u32,
                        }));
                    }
                    Some("input") => {
                        let _ = to_phone.send((device.clone(), HostMessage::UplinkInput { v, uplink_id: id.clone(), event: message["event"].clone() }));
                    }
                    Some("closed") => break,
                    _ => {}
                }
            }
            let _ = to_phone.send((device.clone(), HostMessage::UplinkStopped { v: protocol::PROTOCOL_VERSION, uplink_id: id.clone() }));
        });
        self.active.lock().await.insert(uplink_id.to_owned(), Uplink { device_id: device_id.to_owned(), stdin, child });
        match tokio::time::timeout(std::time::Duration::from_secs(10), answer_rx).await {
            Ok(Ok(answer)) => {
                eprintln!("uplink: {mode} from {device_id} answered ({} bytes)", answer.len());
                Ok(answer)
            }
            _ => {
                self.stop(device_id, uplink_id).await;
                Err("the video receiver did not answer".into())
            }
        }
    }

    pub async fn ice(&self, device_id: &str, uplink_id: &str, candidate: &str, mline: u32) {
        let mut active = self.active.lock().await;
        if let Some(uplink) = active.get_mut(uplink_id).filter(|u| u.device_id == device_id) {
            let line = serde_json::json!({"type": "ice", "candidate": candidate, "mline": mline}).to_string() + "\n";
            let _ = uplink.stdin.write_all(line.as_bytes()).await;
        }
    }

    pub async fn stop(&self, device_id: &str, uplink_id: &str) {
        let removed = {
            let mut active = self.active.lock().await;
            match active.get(uplink_id) {
                Some(u) if u.device_id == device_id => active.remove(uplink_id),
                _ => None,
            }
        };
        if let Some(mut uplink) = removed {
            let _ = uplink.stdin.write_all(b"{\"type\":\"stop\"}\n").await;
            if tokio::time::timeout(std::time::Duration::from_secs(2), uplink.child.wait()).await.is_err() {
                let _ = uplink.child.kill().await;
            }
        }
    }

    pub async fn drop_device(&self, device_id: &str) {
        let ids: Vec<String> = self.active.lock().await.iter().filter(|(_, u)| u.device_id == device_id).map(|(id, _)| id.clone()).collect();
        for id in ids {
            self.stop(device_id, &id).await;
        }
    }
}
