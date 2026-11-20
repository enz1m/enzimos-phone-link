use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteApp {
    pub id: String,
    pub name: String,
    pub description: String,
    pub available: bool,
}

const APPS: &[(&str, &str, &str)] = &[
    ("files-app", "Files", "Browse and organize your computer"),
    ("text-app", "Text", "A focused space to write"),
    (
        "terminal-emulator",
        "Terminal",
        "Your computer, at your command",
    ),
    (
        "task-manager-app",
        "System Monitor",
        "Processes and resource usage",
    ),
    ("calendar-app", "Calendar", "Make room for what matters"),
    ("mail-app", "Mail", "Stay on top of your inbox"),
    ("music-app", "Music", "Your library, wherever you are"),
    ("reader-app", "Reader", "Pick up where you left off"),
    ("store-app", "Store", "Discover more applications"),
    ("remote-app", "Remote", "Manage remote connections"),
];

fn apps_root() -> PathBuf {
    std::env::var_os("ENZREMOTE_DESKTOP_APP_DIR")
        .or_else(|| std::env::var_os("ENZREMOTE_ICED_APPS_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/lib/enzui/apps"))
}

fn executable(root: &Path, id: &str) -> Option<PathBuf> {
    if !APPS.iter().any(|app| app.0 == id) {
        return None;
    }
    [
        root.join(id),
        root.join("_shared/target/release").join(id),
        root.join("_shared/target/debug").join(id),
        root.join(id).join("target/release").join(id),
        root.join(id).join("target/debug").join(id),
    ]
    .into_iter()
    .find(|path| {
        fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

pub fn app_command(id: &str) -> Result<Vec<String>, String> {
    executable(&apps_root(), id)
        .map(|path| vec![path.to_string_lossy().into_owned()])
        .ok_or_else(|| "This application is not installed on the computer".into())
}

pub fn catalog() -> Vec<RemoteApp> {
    let root = apps_root();
    APPS.iter()
        .map(|(id, name, description)| RemoteApp {
            id: (*id).into(),
            name: (*name).into(),
            description: (*description).into(),
            available: executable(&root, id).is_some(),
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ComputerInfo {
    pub hostname: String,
    pub os: String,
    pub cpu_model: String,
    pub cpu_threads: u32,
    pub cpu_usage_percent: Option<f64>,
    pub memory_total_bytes: u64,
    pub memory_available_bytes: u64,
    pub disk_total_bytes: u64,
    pub disk_available_bytes: u64,
    pub uptime_secs: u64,
    pub apps: Vec<RemoteApp>,
    #[serde(default)]
    pub shared_desktop: bool,
    /// Revision of the full application catalog (`app_list`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_revision: Option<String>,
    /// The phone can be used as an extra monitor (`second_display_start`).
    #[serde(default)]
    pub second_display: bool,
    /// Phone screens and cameras can be shown here (`uplink_start`).
    #[serde(default)]
    pub uplink: bool,
    /// Codecs this computer can stream; phones offer `h265` in `hello` only
    /// to hosts that list it (older hosts reject unknown codecs).
    #[serde(default)]
    pub video_codecs: Vec<String>,
    /// This computer's monitors can be shown on the phone
    /// (`second_display_start` with `view`); primary first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitors: Vec<crate::second_display::Monitor>,
    /// Network throughput of real interfaces, bytes per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_rx_bytes_per_sec: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_tx_bytes_per_sec: Option<u64>,
}

static CPU_SAMPLE: Mutex<Option<(u64, u64)>> = Mutex::new(None);
static NET_SAMPLE: Mutex<Option<(std::time::Instant, u64, u64)>> = Mutex::new(None);

/// Received and sent bytes of physical interfaces (`/proc/net/dev`);
/// loopback, bridges, containers and VPN tunnels are skipped.
fn network_counters(text: &str) -> (u64, u64) {
    let virtual_prefixes = ["lo", "docker", "br-", "veth", "virbr", "vnet", "tun", "tap", "wg", "zt", "tailscale"];
    text.lines()
        .skip(2)
        .filter_map(|line| {
            let (name, values) = line.split_once(':')?;
            let name = name.trim();
            if virtual_prefixes.iter().any(|p| name.starts_with(p)) {
                return None;
            }
            let fields: Vec<u64> = values.split_whitespace().filter_map(|v| v.parse().ok()).collect();
            Some((*fields.first()?, *fields.get(8)?))
        })
        .fold((0, 0), |(rx, tx), (r, t)| (rx + r, tx + t))
}

/// Bytes per second since the previous poll.
fn network_rates() -> Option<(u64, u64)> {
    let (rx, tx) = network_counters(&fs::read_to_string("/proc/net/dev").ok()?);
    let now = std::time::Instant::now();
    let mut previous = NET_SAMPLE.lock().unwrap_or_else(|e| e.into_inner());
    let rates = (*previous).and_then(|(at, prx, ptx)| {
        let secs = now.duration_since(at).as_secs_f64();
        (secs >= 0.5).then(|| ((rx.saturating_sub(prx) as f64 / secs) as u64, (tx.saturating_sub(ptx) as f64 / secs) as u64))
    });
    if previous.is_none_or(|(at, ..)| now.duration_since(at).as_secs_f64() >= 0.5) {
        *previous = Some((now, rx, tx));
    }
    rates
}

fn cpu_counters(text: &str) -> Option<(u64, u64)> {
    let values: Vec<u64> = text
        .lines()
        .next()?
        .strip_prefix("cpu ")?
        .split_whitespace()
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    if values.len() < 5 {
        return None;
    }
    Some((values.iter().sum(), values[3] + values[4]))
}

fn memory_bytes(text: &str, key: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix(key))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
        .saturating_mul(1024)
}

/// Facts that do not change while the host runs: (OS, CPU model, threads).
fn static_info() -> &'static (String, String, u32) {
    static INFO: std::sync::OnceLock<(String, String, u32)> = std::sync::OnceLock::new();
    INFO.get_or_init(|| {
        let cpu = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let os = fs::read_to_string("/etc/os-release")
            .unwrap_or_default()
            .lines()
            .find_map(|line| line.strip_prefix("PRETTY_NAME="))
            .unwrap_or("Linux")
            .trim_matches('"')
            .to_owned();
        let model = cpu
            .lines()
            .find_map(|line| line.strip_prefix("model name").or_else(|| line.strip_prefix("Hardware")))
            .and_then(|line| line.split_once(':'))
            .map(|(_, name)| name.trim().to_owned())
            .unwrap_or_else(|| "Processor".into());
        let threads = cpu.lines().filter(|line| line.starts_with("processor\t")).count() as u32;
        (os, model, threads)
    })
}

/// H.264 always; H.265 when a VA-API HEVC encoder is installed.
fn encodable_codecs() -> &'static [String] {
    static CODECS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    CODECS.get_or_init(|| {
        let has = |element: &str| {
            std::process::Command::new("gst-inspect-1.0")
                .args(["--exists", element])
                .status()
                .is_ok_and(|s| s.success())
        };
        let mut codecs = vec!["h264".to_string()];
        if has("vah265lpenc") || has("vah265enc") {
            codecs.push("h265".into());
        }
        codecs
    })
}

pub fn computer_info() -> ComputerInfo {
    let (os, cpu_model, cpu_threads) = static_info().clone();
    let memory = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let counters = fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|text| cpu_counters(&text));
    let mut previous = CPU_SAMPLE.lock().unwrap_or_else(|e| e.into_inner());
    let cpu_usage_percent =
        counters
            .zip(*previous)
            .and_then(|((total, idle), (old_total, old_idle))| {
                let elapsed = total.checked_sub(old_total)?;
                if elapsed == 0 {
                    return None;
                }
                Some(
                    (100.0 * (1.0 - idle.saturating_sub(old_idle) as f64 / elapsed as f64))
                        .clamp(0.0, 100.0),
                )
            });
    *previous = counters;
    drop(previous);
    let disk = nix::sys::statvfs::statvfs(Path::new("/")).ok();
    let net_rates = network_rates();
    ComputerInfo {
        hostname: hostname::get()
            .ok()
            .and_then(|s| s.into_string().ok())
            .unwrap_or_else(|| "Computer".into()),
        os,
        cpu_model,
        cpu_threads,
        cpu_usage_percent,
        memory_total_bytes: memory_bytes(&memory, "MemTotal:"),
        memory_available_bytes: memory_bytes(&memory, "MemAvailable:"),
        disk_total_bytes: disk
            .as_ref()
            .map_or(0, |d| d.blocks().saturating_mul(d.fragment_size())),
        disk_available_bytes: disk.as_ref().map_or(0, |d| {
            d.blocks_available().saturating_mul(d.fragment_size())
        }),
        uptime_secs: fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .unwrap_or(0.0) as u64,
        apps: catalog(),
        shared_desktop: std::env::var_os("ENZREMOTE_DESKTOP_RUNROOT").is_some(),
        app_revision: Some(crate::launcher::catalog().revision),
        second_display: crate::second_display::VirtualDisplay::supported(),
        uplink: crate::uplink::supported(),
        video_codecs: encodable_codecs().to_vec(),
        monitors: crate::second_display::monitors_cached(),
        net_rx_bytes_per_sec: net_rates.map(|r| r.0),
        net_tx_bytes_per_sec: net_rates.map(|r| r.1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_apps_never_resolve_as_paths() {
        assert!(executable(Path::new("/"), "../../bin/sh").is_none());
        assert!(executable(Path::new("/"), "/bin/sh").is_none());
    }
}
