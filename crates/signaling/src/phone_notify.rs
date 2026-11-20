//! Phone notifications mirrored as desktop notifications (KDE Connect style).
//!
//! Talks to `org.freedesktop.Notifications` directly so it can replace and
//! close notifications and learn how the user dealt with them: an action
//! button runs the phone action, dismissing on the computer dismisses on the
//! phone, and reply actions use the server's inline reply (KDE) or a small
//! reply dialog (GNOME and others).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use futures_util::StreamExt;
use tokio::sync::mpsc::UnboundedSender;
use zbus::zvariant::Value;

use crate::protocol::{self, HostMessage, NotificationAction};

const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const REPLY_PREFIX: &str = "reply:";
/// Local action on text-message notifications: open the Messages window.
const OPEN_MESSAGES: &str = "enzremote-open-messages";

/// The desktop Messages app next to this binary, if installed.
fn messages_app() -> Option<std::path::PathBuf> {
    let path = std::env::current_exe().ok()?.with_file_name("enzremote-messages");
    path.is_file().then_some(path)
}

/// Notifications from the phone's SMS app (by its display name).
fn is_sms_app(app_name: &str) -> bool {
    let name = app_name.to_ascii_lowercase();
    ["messages", "messaging", "message", "sms", "textra", "signal"].iter().any(|n| name == *n || name.ends_with(&format!(" {n}")))
}
/// `NotificationClosed` reason: dismissed by the user.
const CLOSED_BY_USER: u32 = 2;

struct Shown {
    device_id: String,
    key: String,
    title: String,
    actions: Vec<NotificationAction>,
}

pub struct PhoneNotifications {
    proxy: zbus::Proxy<'static>,
    inline_reply: bool,
    shown: Mutex<HashMap<u32, Shown>>,
    ids: Mutex<HashMap<(String, String), u32>>,
    to_phone: UnboundedSender<(String, HostMessage)>,
}

/// A notification to show.
pub struct Incoming<'a> {
    pub device_name: &'a str,
    pub key: &'a str,
    pub app_name: &'a str,
    pub title: &'a str,
    pub text: &'a str,
    pub icon: Option<&'a str>,
    pub actions: &'a [NotificationAction],
}

impl PhoneNotifications {
    /// Connect to the session notification server; `to_phone` receives
    /// actions and dismissals for the phones.
    pub async fn start(
        to_phone: UnboundedSender<(String, HostMessage)>,
    ) -> Result<Arc<Self>, String> {
        let connection = zbus::Connection::session()
            .await
            .map_err(|e| e.to_string())?;
        Self::connect(connection, to_phone).await
    }

    async fn connect(
        connection: zbus::Connection,
        to_phone: UnboundedSender<(String, HostMessage)>,
    ) -> Result<Arc<Self>, String> {
        let proxy = zbus::Proxy::new(&connection, DEST, PATH, DEST)
            .await
            .map_err(|e| e.to_string())?;
        let capabilities: Vec<String> = proxy
            .call("GetCapabilities", &())
            .await
            .map_err(|e| e.to_string())?;
        let this = Arc::new(Self {
            proxy,
            inline_reply: capabilities.iter().any(|c| c == "inline-reply"),
            shown: Mutex::new(HashMap::new()),
            ids: Mutex::new(HashMap::new()),
            to_phone,
        });
        this.listen().await.map_err(|e| e.to_string())?;
        Ok(this)
    }

    async fn listen(self: &Arc<Self>) -> zbus::Result<()> {
        // Actions and their close signal must retain their order on the bus.
        let mut signals = self.proxy.receive_all_signals().await?;
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Some(signal) = signals.next().await {
                let Some(this) = weak.upgrade() else { return };
                match signal.header().member().map(|member| member.as_str()) {
                    Some("ActionInvoked") => {
                        if let Ok((id, action)) = signal.body().deserialize::<(u32, String)>() {
                            this.action_invoked(id, &action);
                        }
                    }
                    Some("NotificationClosed") => {
                        if let Ok((id, reason)) = signal.body().deserialize::<(u32, u32)>() {
                            this.closed(id, reason);
                        }
                    }
                    Some("NotificationReplied") => {
                        if let Ok((id, text)) = signal.body().deserialize::<(u32, String)>() {
                            this.replied(id, text);
                        }
                    }
                    _ => {}
                }
            }
        });
        Ok(())
    }

    /// Show or update a phone notification.
    pub async fn show(&self, device_id: &str, n: Incoming<'_>) -> Result<(), String> {
        let replaces = self.ids.lock().unwrap_or_else(|e| e.into_inner()).get(&(device_id.into(), n.key.into())).copied().unwrap_or(0);
        let mut actions: Vec<String> = Vec::new();
        for action in n.actions {
            if action.reply && self.inline_reply {
                actions.extend(["inline-reply".into(), action.title.clone()]);
            } else if action.reply {
                actions.extend([format!("{REPLY_PREFIX}{}", action.id), format!("{}…", action.title)]);
            } else {
                actions.extend([action.id.clone(), action.title.clone()]);
            }
        }
        if is_sms_app(n.app_name)
            && messages_app().is_some()
            && crate::host_settings::allowed(device_id, crate::host_settings::Feature::Sms)
        {
            actions.extend([OPEN_MESSAGES.to_string(), "Open in Messages".to_string()]);
        }
        let mut hints: HashMap<&str, Value<'_>> = HashMap::new();
        hints.insert("category", Value::from("im.received"));
        hints.insert("x-kde-origin-name", Value::from(n.device_name.to_string()));
        if let Some(image) = n.icon.and_then(image_data) {
            hints.insert("image-data", image);
        }
        let app_name = if n.app_name.is_empty() { n.device_name.to_string() } else { format!("{} · {}", n.app_name, n.device_name) };
        let id: u32 = self
            .proxy
            .call("Notify", &(app_name.as_str(), replaces, "phone", n.title, escape(n.text).as_str(), actions, hints, -1i32))
            .await
            .map_err(|e| e.to_string())?;
        self.ids.lock().unwrap_or_else(|e| e.into_inner()).insert((device_id.into(), n.key.into()), id);
        self.shown.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id,
            Shown { device_id: device_id.into(), key: n.key.into(), title: n.title.into(), actions: n.actions.to_vec() },
        );
        Ok(())
    }

    /// The phone removed a notification.
    pub async fn remove(&self, device_id: &str, key: &str) {
        let id = self.ids.lock().unwrap_or_else(|e| e.into_inner()).remove(&(device_id.into(), key.into()));
        if let Some(id) = id {
            self.shown.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            let _: Result<(), _> = self.proxy.call("CloseNotification", &(id,)).await;
        }
    }

    /// The phone disconnected: its notifications are stale.
    pub async fn drop_device(&self, device_id: &str) {
        let ids: Vec<u32> = {
            let mut ids = self.ids.lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<_> = ids.keys().filter(|(d, _)| d == device_id).cloned().collect();
            keys.into_iter().filter_map(|k| ids.remove(&k)).collect()
        };
        for id in ids {
            self.shown.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            let _: Result<(), _> = self.proxy.call("CloseNotification", &(id,)).await;
        }
    }

    fn action_invoked(&self, id: u32, action: &str) {
        let Some((device_id, key, title, actions)) = self
            .shown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .map(|s| (s.device_id.clone(), s.key.clone(), s.title.clone(), s.actions.clone()))
        else {
            return;
        };
        if action == OPEN_MESSAGES {
            if let Some(app) = messages_app() {
                // The sender: a contact name or number, matched by the app.
                let _ = std::process::Command::new(app).args(["--to", &title]).spawn();
            }
            return;
        }
        if let Some(action_id) = action.strip_prefix(REPLY_PREFIX) {
            let to_phone = self.to_phone.clone();
            let action_id = action_id.to_string();
            tokio::spawn(async move {
                if let Some(text) = ask_reply(&title).await {
                    let _ = to_phone.send((device_id, HostMessage::NotificationAction {
                        v: protocol::PROTOCOL_VERSION, key, action_id, text: Some(text),
                    }));
                }
            });
        } else if actions.iter().any(|a| a.id == action) {
            let _ = self.to_phone.send((device_id, HostMessage::NotificationAction {
                v: protocol::PROTOCOL_VERSION, key, action_id: action.into(), text: None,
            }));
        }
    }

    fn replied(&self, id: u32, text: String) {
        let shown = self.shown.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = shown.get(&id) else { return };
        if let Some(reply) = s.actions.iter().find(|a| a.reply) {
            let _ = self.to_phone.send((s.device_id.clone(), HostMessage::NotificationAction {
                v: protocol::PROTOCOL_VERSION, key: s.key.clone(), action_id: reply.id.clone(), text: Some(text),
            }));
        }
    }

    fn closed(&self, id: u32, reason: u32) {
        let Some(s) = self.shown.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) else { return };
        self.ids.lock().unwrap_or_else(|e| e.into_inner()).remove(&(s.device_id.clone(), s.key.clone()));
        if reason == CLOSED_BY_USER {
            let _ = self.to_phone.send((s.device_id, HostMessage::NotificationDismiss { v: protocol::PROTOCOL_VERSION, key: s.key }));
        }
    }
}

/// Reply text from a small dialog (servers without inline reply).
async fn ask_reply(title: &str) -> Option<String> {
    let output = tokio::process::Command::new("zenity")
        .args(["--entry", "--width=420", "--ok-label=Send"])
        .arg(format!("--title=Reply to {title}"))
        .arg(format!("--text=Reply to {}", escape(title)))
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim_end_matches('\n').to_string();
    (output.status.success() && !text.trim().is_empty()).then_some(text)
}

/// Notification bodies may contain markup; show phone text literally.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// `image-data` hint (iiibiiay, straight RGBA) from a base64 PNG.
fn image_data(icon: &str) -> Option<Value<'static>> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(icon).ok()?;
    let pixmap = tiny_skia::Pixmap::decode_png(&bytes).ok()?;
    let (width, height) = (pixmap.width() as i32, pixmap.height() as i32);
    let mut rgba = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.pixels() {
        let c = pixel.demultiply();
        rgba.extend([c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Some(Value::from((width, height, width * 4, true, 8i32, 4i32, rgba)))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Bus(std::process::Child);
    impl Drop for Bus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    struct Desktop;
    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl Desktop {
        fn get_capabilities(&self) -> Vec<&str> {
            vec!["actions", "inline-reply"]
        }
        #[allow(clippy::too_many_arguments, reason = "The freedesktop notification method has a fixed wire signature")]
        fn notify(
            &self,
            _app: &str,
            replaces: u32,
            _icon: &str,
            _summary: &str,
            _body: &str,
            _actions: Vec<String>,
            _hints: HashMap<String, zbus::zvariant::OwnedValue>,
            _timeout: i32,
        ) -> u32 {
            replaces.max(1)
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn desktop_actions_and_replies_survive_their_close_signal() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        use tokio::time::{Duration, timeout};
        let directory = tempfile::tempdir().unwrap();
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .arg(format!(
                "--address=unix:path={}",
                directory.path().join("bus").display()
            ))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        let _bus = Bus(child);
        let desktop = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .name(DEST)
            .unwrap()
            .serve_at(PATH, Desktop)
            .unwrap()
            .build()
            .await
            .unwrap();
        let client = zbus::connection::Builder::address(address.trim())
            .unwrap()
            .build()
            .await
            .unwrap();
        let (send, mut received) = tokio::sync::mpsc::unbounded_channel();
        let notifications = PhoneNotifications::connect(client, send).await.unwrap();
        let actions = vec![
            NotificationAction {
                id: "done".into(),
                title: "Done".into(),
                reply: false,
            },
            NotificationAction {
                id: "reply".into(),
                title: "Reply".into(),
                reply: true,
            },
        ];
        for sequence in 0..64 {
            let key = format!("fixture-{sequence}");
            notifications
                .show(
                    "phone",
                    Incoming {
                        device_name: "QA phone",
                        key: &key,
                        app_name: "QA fixture",
                        title: "Synthetic",
                        text: "Fixture",
                        icon: None,
                        actions: &actions,
                    },
                )
                .await
                .unwrap();
            if sequence % 2 == 0 {
                desktop
                    .emit_signal(
                        None::<&str>,
                        PATH,
                        DEST,
                        "NotificationReplied",
                        &(1u32, "fixture reply"),
                    )
                    .await
                    .unwrap();
            } else {
                desktop
                    .emit_signal(None::<&str>, PATH, DEST, "ActionInvoked", &(1u32, "done"))
                    .await
                    .unwrap();
            }
            desktop
                .emit_signal(
                    None::<&str>,
                    PATH,
                    DEST,
                    "NotificationClosed",
                    &(1u32, CLOSED_BY_USER),
                )
                .await
                .unwrap();
            let (device, message) = timeout(Duration::from_secs(3), received.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(device, "phone");
            match message {
                HostMessage::NotificationAction {
                    key: actual,
                    action_id,
                    text,
                    ..
                } => {
                    assert_eq!(actual, key);
                    assert_eq!(action_id, if sequence % 2 == 0 { "reply" } else { "done" });
                    assert_eq!(
                        text.as_deref(),
                        (sequence % 2 == 0).then_some("fixture reply")
                    );
                }
                _ => panic!("Notification close overtook its action"),
            }
            assert!(
                matches!(timeout(Duration::from_secs(3), received.recv()).await.unwrap(),
                Some((_, HostMessage::NotificationDismiss { key: actual, .. })) if actual == key)
            );
        }
    }

    #[test]
    fn phone_text_is_not_markup() {
        assert_eq!(escape("<b>hi</b> & bye"), "&lt;b&gt;hi&lt;/b&gt; &amp; bye");
    }
}
