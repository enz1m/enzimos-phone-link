//! Media remote control: the desktop's MPRIS players and system volume.

use std::collections::HashMap;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use zbus::zvariant::OwnedValue;

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const PLAYER_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
/// Album art is scaled down to this edge for the phone.
const ART_SIZE: u32 = 640;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MediaState {
    /// Available players: (id, display name).
    pub players: Vec<MediaPlayer>,
    /// Player the state below describes (`None` when nothing plays).
    pub player: Option<String>,
    /// `Playing`, `Paused` or `Stopped`.
    pub status: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub length_ms: u64,
    pub position_ms: u64,
    pub can_seek: bool,
    /// Identifies the album art; `art` is only sent when it changes.
    pub art_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art: Option<String>,
    /// System output volume, 0-150.
    pub volume: u32,
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MediaPlayer {
    pub id: String,
    pub name: String,
}

pub struct Media {
    connection: zbus::Connection,
}

impl Media {
    pub async fn connect() -> Result<Self, String> {
        Ok(Self { connection: zbus::Connection::session().await.map_err(|e| e.to_string())? })
    }

    async fn player_ids(&self) -> Vec<String> {
        let Ok(dbus) = zbus::fdo::DBusProxy::new(&self.connection).await else { return Vec::new() };
        let Ok(names) = dbus.list_names().await else { return Vec::new() };
        let mut ids: Vec<String> = names
            .iter()
            .filter_map(|n| n.as_str().strip_prefix(MPRIS_PREFIX).map(str::to_owned))
            .filter(|id| id != "playerctld")
            .collect();
        ids.sort();
        ids
    }

    async fn proxy(&self, id: &str, iface: &'static str) -> Option<zbus::Proxy<'static>> {
        zbus::Proxy::new(&self.connection, format!("{MPRIS_PREFIX}{id}"), PLAYER_PATH, iface).await.ok()
    }

    async fn status_of(&self, id: &str) -> String {
        match self.proxy(id, PLAYER_IFACE).await {
            Some(p) => p.get_property::<String>("PlaybackStatus").await.unwrap_or_default(),
            None => String::new(),
        }
    }

    /// The chosen player, else the first playing one, else the first.
    async fn pick(&self, ids: &[String], wanted: Option<&str>) -> Option<String> {
        if let Some(w) = wanted.filter(|w| ids.iter().any(|id| id == w)) {
            return Some(w.to_owned());
        }
        for id in ids {
            if self.status_of(id).await == "Playing" {
                return Some(id.clone());
            }
        }
        ids.first().cloned()
    }

    /// Current state; `known_art` avoids resending unchanged album art.
    pub async fn state(&self, wanted: Option<&str>, known_art: Option<&str>) -> MediaState {
        let ids = self.player_ids().await;
        let mut players = Vec::new();
        for id in &ids {
            let name = match self.proxy(id, "org.mpris.MediaPlayer2").await {
                Some(p) => p.get_property::<String>("Identity").await.unwrap_or_else(|_| id.clone()),
                None => id.clone(),
            };
            players.push(MediaPlayer { id: id.clone(), name });
        }
        let (volume, muted) = system_volume().await;
        let mut state = MediaState { players, volume, muted, ..Default::default() };
        let Some(id) = self.pick(&ids, wanted).await else { return state };
        let Some(player) = self.proxy(&id, PLAYER_IFACE).await else { return state };
        state.status = player.get_property::<String>("PlaybackStatus").await.unwrap_or_default();
        state.can_seek = player.get_property::<bool>("CanSeek").await.unwrap_or(false);
        state.position_ms = player.get_property::<i64>("Position").await.map_or(0, |us| (us.max(0) / 1000) as u64);
        let metadata: HashMap<String, OwnedValue> = player.get_property("Metadata").await.unwrap_or_default();
        let text = |key: &str| -> String {
            metadata.get(key).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default()
        };
        state.title = text("xesam:title");
        state.album = text("xesam:album");
        state.artist = metadata
            .get("xesam:artist")
            .and_then(|v| Vec::<String>::try_from(v.clone()).ok())
            .map(|a| a.join(", "))
            .unwrap_or_default();
        state.length_ms = metadata
            .get("mpris:length")
            .and_then(|v| i64::try_from(v.clone()).ok().or_else(|| u64::try_from(v.clone()).ok().map(|u| u as i64)))
            .map_or(0, |us| (us.max(0) / 1000) as u64);
        let art_url = text("mpris:artUrl");
        if !art_url.is_empty() {
            let art_id = format!("{:x}", sha2::Sha256::digest(format!("{art_url}{}", state.title).as_bytes()))[..16].to_string();
            if known_art != Some(art_id.as_str()) {
                state.art = tokio::task::spawn_blocking(move || load_art(&art_url)).await.ok().flatten();
            }
            state.art_id = Some(art_id);
        }
        state.player = Some(id);
        state
    }

    /// Run a command on a player (or the system volume).
    pub async fn command(&self, player: Option<&str>, command: &str, value: Option<f64>) -> Result<(), String> {
        match command {
            "set_volume" => {
                let percent = value.unwrap_or(50.0).clamp(0.0, 150.0) / 100.0;
                return wpctl(&["set-volume", "@DEFAULT_AUDIO_SINK@", &format!("{percent:.2}")]).await;
            }
            "toggle_mute" => return wpctl(&["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"]).await,
            _ => {}
        }
        let ids = self.player_ids().await;
        let id = self.pick(&ids, player).await.ok_or("nothing is playing on the computer")?;
        let proxy = self.proxy(&id, PLAYER_IFACE).await.ok_or("player unavailable")?;
        let result = match command {
            "play_pause" => proxy.call_method("PlayPause", &()).await,
            "next" => proxy.call_method("Next", &()).await,
            "previous" => proxy.call_method("Previous", &()).await,
            "seek" => {
                let metadata: HashMap<String, OwnedValue> = proxy.get_property("Metadata").await.unwrap_or_default();
                let track = metadata
                    .get("mpris:trackid")
                    .and_then(|v| zbus::zvariant::OwnedObjectPath::try_from(v.clone()).ok())
                    .ok_or("player cannot seek")?;
                let position = (value.unwrap_or(0.0).max(0.0) * 1000.0) as i64;
                proxy.call_method("SetPosition", &(track, position)).await
            }
            other => return Err(format!("unknown media command {other}")),
        };
        result.map(|_| ()).map_err(|e| e.to_string())
    }
}

async fn wpctl(args: &[&str]) -> Result<(), String> {
    let status = tokio::process::Command::new("wpctl").args(args).status().await.map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err("could not change the volume".into()) }
}

/// `wpctl get-volume` → (percent, muted).
async fn system_volume() -> (u32, bool) {
    let Ok(output) = tokio::process::Command::new("wpctl").args(["get-volume", "@DEFAULT_AUDIO_SINK@"]).output().await else {
        return (0, false);
    };
    parse_volume(&String::from_utf8_lossy(&output.stdout))
}

fn parse_volume(text: &str) -> (u32, bool) {
    let volume = text
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse::<f64>().ok())
        .map_or(0, |v| (v * 100.0).round().clamp(0.0, 150.0) as u32);
    (volume, text.contains("[MUTED]"))
}

/// Local album art (`file://`), scaled to at most `ART_SIZE`, as base64
/// JPEG (sharp on phone screens and small enough for one message).
fn load_art(url: &str) -> Option<String> {
    let path = url.strip_prefix("file://")?;
    let path = percent_decode(path);
    let bytes = std::fs::read(&path).ok()?;
    let image = tiny_skia::Pixmap::decode_png(&bytes).ok()?;
    let scale = (ART_SIZE as f32 / image.width().max(image.height()) as f32).min(1.0);
    let (w, h) = (((image.width() as f32 * scale) as u32).max(1), ((image.height() as f32 * scale) as u32).max(1));
    let mut scaled = tiny_skia::Pixmap::new(w, h)?;
    scaled.draw_pixmap(
        0, 0, image.as_ref(), &tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() },
        tiny_skia::Transform::from_scale(scale, scale), None,
    );
    // Composite onto black (art is normally opaque) and drop alpha.
    let rgb: Vec<u8> = scaled.pixels().iter().flat_map(|p| [p.red(), p.green(), p.blue()]).collect();
    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, 82)
        .encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
        .ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(jpeg))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&text[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

use sha2::Digest as _;
