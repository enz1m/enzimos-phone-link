//! User-facing host settings and per-device preferences, for the enzUI
//! Settings app (or any client of the control socket).
//!
//! Settings live in `$XDG_CONFIG_HOME/enzremote/settings.json` and apply
//! live unless their schema entry says `restart`. An `ENZREMOTE_*`
//! environment variable still wins and locks that setting. Per-device names
//! and permissions live in `<data dir>/devices.json`, beside (not inside) the
//! credential store.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Computer name shown on phones; empty uses the system name.
    pub host_name: String,
    /// Advertise on the local network (mDNS) so phones find this computer.
    pub discoverable: bool,
    /// How new phones are approved: `notification` or `cli`.
    pub approval: String,
    pub clipboard: bool,
    pub phone_notifications: bool,
    pub receive_files: bool,
    /// Where received files go; empty uses the Downloads folder.
    pub downloads_dir: String,
    /// Phones may move the pointer and type (Remote, presenter, second screen).
    pub remote_control: bool,
    /// Phones may show their screen or camera here.
    pub app_streaming: bool,
    pub phone_video: bool,
    /// Desktop apps may read and send a phone's text messages.
    pub sms: bool,
    /// Ceiling for stream bitrate in kbps; 0 lets the host decide.
    pub video_max_kbps: u32,
    pub video_max_fps: u32,
    /// `auto`, `h264` or `h265`.
    pub video_codec: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            host_name: String::new(),
            discoverable: true,
            approval: "notification".into(),
            clipboard: true,
            phone_notifications: true,
            receive_files: true,
            downloads_dir: String::new(),
            remote_control: true,
            app_streaming: true,
            phone_video: true,
            sms: true,
            video_max_kbps: 0,
            video_max_fps: 60,
            video_codec: "auto".into(),
        }
    }
}

/// Describes one setting for a settings UI.
struct Field {
    key: &'static str,
    title: &'static str,
    description: &'static str,
    group: &'static str,
    /// `bool`, `string`, `path`, `choice` or `int`.
    kind: &'static str,
    choices: &'static [(&'static str, &'static str)],
    range: Option<(u32, u32)>,
    restart: bool,
    /// Environment variable that overrides it.
    env: Option<&'static str>,
}

const FIELDS: &[Field] = &[
    Field { key: "host_name", title: "Computer name", description: "Shown on paired phones. Empty uses the system name.", group: "general", kind: "string", choices: &[], range: None, restart: true, env: Some("ENZREMOTE_HOST_NAME") },
    Field { key: "discoverable", title: "Visible on the local network", description: "Phones on the same network list this computer when adding one.", group: "general", kind: "bool", choices: &[], range: None, restart: true, env: Some("ENZREMOTE_NO_MDNS") },
    Field { key: "approval", title: "Approve new phones", description: "How a phone that asks to pair is approved.", group: "general", kind: "choice", choices: &[("notification", "With a notification"), ("cli", "Only from the command line")], range: None, restart: false, env: Some("ENZREMOTE_PAIRING_PROMPT") },
    Field { key: "clipboard", title: "Share clipboard", description: "Copied text follows between this computer and phones that share theirs.", group: "sharing", kind: "bool", choices: &[], range: None, restart: false, env: Some("ENZREMOTE_CLIPBOARD") },
    Field { key: "phone_notifications", title: "Show phone notifications", description: "Notifications from phones appear here, with their buttons and reply.", group: "sharing", kind: "bool", choices: &[], range: None, restart: false, env: None },
    Field { key: "receive_files", title: "Receive files", description: "Phones can send files to this computer.", group: "sharing", kind: "bool", choices: &[], range: None, restart: false, env: None },
    Field { key: "downloads_dir", title: "Save received files in", description: "Empty uses your Downloads folder.", group: "sharing", kind: "path", choices: &[], range: None, restart: false, env: None },
    Field { key: "remote_control", title: "Allow remote control", description: "Phones can move the pointer, type and use this computer as a second screen.", group: "phones", kind: "bool", choices: &[], range: None, restart: false, env: None },
    Field { key: "app_streaming", title: "App and desktop streaming", description: "Allow phones to open and control apps and view the desktop. Apps retain this account's file access.", group: "streaming", kind: "bool", choices: &[], range: None, restart: false, env: None },
    Field { key: "phone_video", title: "Phone screen and camera", description: "Phones can mirror their screen here or be used as a webcam.", group: "phones", kind: "bool", choices: &[], range: None, restart: false, env: None },
    Field { key: "sms", title: "Text messages", description: "The Phone Messages app can read and send texts of phones that allow it.", group: "phones", kind: "bool", choices: &[], range: None, restart: false, env: None },
    Field { key: "video_max_kbps", title: "Maximum stream bitrate", description: "Kilobits per second; 0 adapts to the network.", group: "streaming", kind: "int", choices: &[], range: Some((0, 100_000)), restart: false, env: Some("ENZREMOTE_VIDEO_BITRATE") },
    Field { key: "video_max_fps", title: "Maximum frame rate", description: "Applies to streams started after the change.", group: "streaming", kind: "choice", choices: &[("30", "30"), ("60", "60"), ("90", "90"), ("120", "120")], range: None, restart: false, env: Some("ENZREMOTE_VIDEO_FPS") },
    Field { key: "video_codec", title: "Video codec", description: "H.265 needs about 40% less bandwidth where both sides support it.", group: "streaming", kind: "choice", choices: &[("auto", "Automatic"), ("h264", "H.264"), ("h265", "H.265 (HEVC)")], range: None, restart: false, env: Some("ENZREMOTE_VIDEO_CODEC") },
];

fn config_path() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("enzremote/settings.json")
}

fn store() -> &'static RwLock<Settings> {
    static STORE: OnceLock<RwLock<Settings>> = OnceLock::new();
    STORE.get_or_init(|| {
        let settings = std::fs::read_to_string(config_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        RwLock::new(settings)
    })
}

/// Current settings (as saved; see [`locked`] for environment overrides).
pub fn current() -> Settings {
    store().read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Settings fixed by an environment variable.
pub fn locked() -> Vec<&'static str> {
    FIELDS.iter().filter(|f| f.env.is_some_and(|var| std::env::var_os(var).is_some())).map(|f| f.key).collect()
}

/// Schema for a settings UI: groups, titles, kinds, choices, restart needs.
pub fn schema() -> Value {
    let locked = locked();
    Value::Array(
        FIELDS
            .iter()
            .map(|f| {
                let mut entry = json!({
                    "key": f.key, "title": f.title, "description": f.description, "group": f.group,
                    "kind": f.kind, "restart": f.restart, "locked": locked.contains(&f.key),
                });
                if !f.choices.is_empty() {
                    entry["choices"] = f.choices.iter().map(|(value, label)| json!({"value": value, "label": label})).collect();
                }
                if let Some((min, max)) = f.range {
                    entry["min"] = json!(min);
                    entry["max"] = json!(max);
                }
                entry
            })
            .collect(),
    )
}

/// Validate and save changed values. Returns whether a restart is needed.
pub fn update(values: &serde_json::Map<String, Value>) -> Result<bool, String> {
    let mut store_guard = store().write().unwrap_or_else(|error| error.into_inner());
    let previous = serde_json::to_value(&*store_guard).map_err(|error| error.to_string())?;
    let mut next = previous.clone();
    let locked = locked();
    let mut restart = false;
    for (key, value) in values {
        let field = FIELDS.iter().find(|f| f.key == key).ok_or_else(|| format!("unknown setting {key}"))?;
        if locked.contains(&field.key) {
            return Err(format!("{key} is set by {} in the host environment", field.env.unwrap_or_default()));
        }
        let valid = match field.kind {
            "bool" => value.is_boolean(),
            "string" => value.as_str().is_some_and(|s| s.chars().count() <= 64 && !s.chars().any(char::is_control)),
            "path" => value.as_str().is_some_and(|s| s.is_empty() || (s.starts_with('/') && s.len() < 4096)),
            "int" => value
                .as_u64()
                .is_some_and(|n| field.range.is_none_or(|(min, max)| (u64::from(min)..=u64::from(max)).contains(&n))),
            "choice" => {
                let text = value.as_str().map(str::to_string).or_else(|| value.as_u64().map(|n| n.to_string()));
                text.is_some_and(|t| field.choices.iter().any(|(choice, _)| *choice == t))
            }
            _ => false,
        };
        if !valid {
            return Err(format!("invalid value for {key}"));
        }
        // Frame rates are choices but stored as numbers.
        next[key.as_str()] = if key == "video_max_fps" {
            json!(value.as_str().and_then(|s| s.parse::<u32>().ok()).or(value.as_u64().map(|n| n as u32)))
        } else {
            value.clone()
        };
        restart |= field.restart && next[key.as_str()] != previous[key.as_str()];
    }
    let settings: Settings = serde_json::from_value(next).map_err(|e| e.to_string())?;
    save(&settings)?;
    *store_guard = settings;
    Ok(restart)
}

fn save(settings: &Settings) -> Result<(), String> {
    let path = config_path();
    let parent = path.parent().ok_or("Missing settings directory")?;
    enzui_security::secure_dir::ensure_private_dir(parent).map_err(|error| error.to_string())?;
    let mut stage =
        enzui_security::tempstage::TempStage::create(&path).map_err(|error| error.to_string())?;
    stage
        .write_all(&serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    stage.commit().map_err(|error| error.to_string())
}

// ── Per-device preferences ──

/// What one phone may do on this computer (everything by default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Permissions {
    pub files: bool,
    pub clipboard: bool,
    pub notifications: bool,
    pub remote_control: bool,
    pub app_streaming: bool,
    pub phone_video: bool,
    pub sms: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self { files: true, clipboard: true, notifications: true, remote_control: true, app_streaming: true, phone_video: true, sms: true }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DevicePrefs {
    /// Name chosen on this computer, shown instead of the phone's own.
    pub alias: String,
    pub permissions: Permissions,
}

/// A capability gated by both the host setting and the device permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Files,
    Clipboard,
    Notifications,
    RemoteControl,
    PhoneVideo,
    AppStreaming,
    Sms,
}

fn devices_path() -> Option<PathBuf> {
    crate::identity::data_dir().map(|dir| dir.join("devices.json"))
}

fn devices() -> &'static RwLock<BTreeMap<String, DevicePrefs>> {
    static DEVICES: OnceLock<RwLock<BTreeMap<String, DevicePrefs>>> = OnceLock::new();
    DEVICES.get_or_init(|| {
        let map = devices_path()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        RwLock::new(map)
    })
}

pub fn device(device_id: &str) -> DevicePrefs {
    devices().read().unwrap_or_else(|e| e.into_inner()).get(device_id).cloned().unwrap_or_default()
}

/// Whether `device_id` may use `feature` right now.
pub fn allowed(device_id: &str, feature: Feature) -> bool {
    let settings = current();
    let permissions = device(device_id).permissions;
    match feature {
        Feature::Files => settings.receive_files && permissions.files,
        Feature::Clipboard => settings.clipboard && permissions.clipboard,
        Feature::Notifications => settings.phone_notifications && permissions.notifications,
        Feature::RemoteControl => settings.remote_control && permissions.remote_control,
        Feature::PhoneVideo => settings.phone_video && permissions.phone_video,
        Feature::AppStreaming => settings.app_streaming && permissions.app_streaming,
        Feature::Sms => settings.sms && permissions.sms,
    }
}

/// Change a device's alias and/or permissions (merged into the current ones).
pub fn update_device(device_id: &str, alias: Option<String>, permissions: Option<&Value>) -> Result<DevicePrefs, String> {
    let mut map = devices().write().unwrap_or_else(|e| e.into_inner());
    let mut prefs = map.get(device_id).cloned().unwrap_or_default();
    if let Some(alias) = alias {
        let alias = crate::pairing::sanitize_device_name(&alias);
        prefs.alias = alias;
    }
    if let Some(changes) = permissions {
        let mut merged = serde_json::to_value(prefs.permissions).map_err(|e| e.to_string())?;
        let changes = changes.as_object().ok_or("permissions must be an object")?;
        for (key, value) in changes {
            if merged.get(key).is_none() || !value.is_boolean() {
                return Err(format!("unknown or invalid permission {key}"));
            }
            merged[key.as_str()] = value.clone();
        }
        prefs.permissions = serde_json::from_value(merged).map_err(|e| e.to_string())?;
    }
    store_device_locked(&mut map, device_id, (prefs != DevicePrefs::default()).then(|| prefs.clone()))?;
    Ok(prefs)
}

/// Forget a removed device's preferences.
pub fn forget_device(device_id: &str) {
    let _ = store_device(device_id, None);
}

fn store_device(device_id: &str, prefs: Option<DevicePrefs>) -> Result<(), String> {
    let mut map = devices().write().unwrap_or_else(|e| e.into_inner());
    store_device_locked(&mut map, device_id, prefs)
}

fn store_device_locked(map: &mut BTreeMap<String, DevicePrefs>, device_id: &str, prefs: Option<DevicePrefs>) -> Result<(), String> {
    let mut next = map.clone();
    match prefs {
        Some(prefs) => next.insert(device_id.to_string(), prefs),
        None => next.remove(device_id),
    };
    if let Some(path) = devices_path() {
        let text = serde_json::to_vec_pretty(&next).map_err(|e| e.to_string())?;
        crate::pairing::write_private_file(&path, &text).map_err(|e| e.to_string())?;
    }
    *map = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_matches_settings_fields() {
        let defaults = serde_json::to_value(Settings::default()).unwrap();
        let keys: Vec<&str> = FIELDS.iter().map(|f| f.key).collect();
        assert_eq!(keys.len(), defaults.as_object().unwrap().len());
        for key in keys {
            assert!(defaults.get(key).is_some(), "{key} missing from Settings");
        }
        assert!(schema().as_array().unwrap().iter().all(|f| f["title"].is_string()));
    }
}
