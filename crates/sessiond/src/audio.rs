//! Per-session audio sink: sound from a phone-opened app plays into its own
//! null sink (not the computer's speakers) and the stream worker sends that
//! sink's monitor to the phone. Uses `pactl` (PulseAudio or pipewire-pulse).

use std::path::Path;
use std::process::Command;

/// Sink name for a session ID (`enzremote_<id>`, letters, digits, `_`).
pub fn sink_name(session_id: &str) -> String {
    let id: String = session_id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!("enzremote_{id}")
}

/// Create the session's sink (once); records the module and sink in the
/// runroot. Returns the sink name, or None when no sound server answers.
pub fn ensure_sink(runroot: &Path, session_id: &str, description: &str) -> Option<String> {
    let name = sink_name(session_id);
    if runroot.join("audio-module").is_file() {
        return Some(name);
    }
    let description: String = description.chars().filter(|c| c.is_alphanumeric() || *c == ' ').take(48).collect();
    let output = Command::new("pactl")
        .args([
            "load-module",
            "module-null-sink",
            &format!("sink_name={name}"),
            &format!("sink_properties=device.description=\"{description} (enzimOS Remote)\""),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let module = String::from_utf8_lossy(&output.stdout).trim().to_string();
    std::fs::write(runroot.join("audio-module"), &module).ok()?;
    std::fs::write(runroot.join("audio-sink"), &name).ok()?;
    Some(name)
}

/// Remove the session's sink; its streams move to the default output.
pub fn remove_sink(runroot: &Path) {
    let file = runroot.join("audio-module");
    if let Ok(module) = std::fs::read_to_string(&file)
        && module.trim().chars().all(|c| c.is_ascii_digit())
    {
        let _ = Command::new("pactl").args(["unload-module", module.trim()]).status();
    }
    let _ = std::fs::remove_file(file);
    let _ = std::fs::remove_file(runroot.join("audio-sink"));
}

/// The sink created for a session, if any.
pub fn session_sink(runroot: &Path) -> Option<String> {
    std::fs::read_to_string(runroot.join("audio-sink")).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Unload `enzremote_` sinks that no live session owns (a host restart
/// ends sessions without stopping them). `owned` lists module IDs to keep.
pub fn remove_stale_sinks(owned: &[String]) {
    let Ok(output) = Command::new("pactl").args(["list", "short", "modules"]).output() else { return };
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split('\t');
        let (Some(id), Some(name), Some(args)) = (fields.next(), fields.next(), fields.next()) else { continue };
        if name == "module-null-sink" && args.contains("sink_name=enzremote_") && !owned.iter().any(|o| o == id) {
            let _ = Command::new("pactl").args(["unload-module", id]).status();
        }
    }
}
