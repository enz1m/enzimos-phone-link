//! Installed-application catalog, like a Linux desktop launcher.
//!
//! Applications come from freedesktop `.desktop` entries in the XDG data
//! directories (system, user and Flatpak exports), filtered the way desktop
//! launchers do (`NoDisplay`, `Hidden`, `OnlyShowIn`/`NotShowIn`, `TryExec`,
//! missing executables). The installed enzUI ICED apps are listed too.
//!
//! Phones only ever send a catalog ID; the host resolves it to a command
//! from the entry's `Exec` line, so no client-supplied command is executed.
//! Icons are resolved through the user's icon theme (then hicolor and
//! pixmaps) and rendered to small PNGs on demand.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// One launchable application as sent to phones.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppEntry {
    /// Stable catalog ID (`[a-z0-9-]`, ≤ 40 bytes).
    pub id: String,
    pub name: String,
    /// Comment / GenericName (may be empty).
    #[serde(default)]
    pub description: String,
    /// `iced` (enzUI app) or `desktop` (freedesktop entry).
    pub kind: String,
    /// Icon revision hash; request pixels with `app_icons`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Lowercase search keywords.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// freedesktop main categories (Office, Graphics, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
}

/// Internal launch information for an entry.
#[derive(Debug, Clone)]
struct Launchable {
    entry: AppEntry,
    command: Vec<String>,
    working_dir: Option<PathBuf>,
    icon_path: Option<PathBuf>,
    /// Well-known D-Bus name single-instance apps claim (reverse-DNS desktop ID).
    dbus_name: Option<String>,
}

/// How to start a catalog app.
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchPlan {
    pub name: String,
    pub command: Vec<String>,
    pub working_dir: Option<PathBuf>,
    pub dbus_name: Option<String>,
    pub iced: bool,
}

/// A built catalog with a revision hash clients compare.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub revision: String,
    pub apps: Vec<AppEntry>,
    launchables: HashMap<String, Launchable>,
}

/// Rebuild when an applications directory changes (install, removal), and
/// at least this often to catch entries edited in place.
const CATALOG_TTL: Duration = Duration::from_secs(300);
const ICON_SIZE: u32 = 128;
const MAX_DESCRIPTION: usize = 120;

/// Built at, applications-directory mtimes, catalog.
type CachedCatalog = (Instant, Vec<Option<std::time::SystemTime>>, Catalog);
static CATALOG: Mutex<Option<CachedCatalog>> = Mutex::new(None);
static ICONS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Current catalog (cached). Blocking: call from `spawn_blocking`.
pub fn catalog() -> Catalog {
    let env = Environment::detect();
    // Directory mtimes change when desktop files are added or removed.
    let fingerprint: Vec<_> = env
        .data_dirs
        .iter()
        .map(|dir| std::fs::metadata(dir.join("applications")).and_then(|m| m.modified()).ok())
        .collect();
    let mut guard = CATALOG.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((built, seen, catalog)) = guard.as_ref()
        && built.elapsed() < CATALOG_TTL
        && *seen == fingerprint
    {
        return catalog.clone();
    }
    let catalog = build_catalog(&env);
    *guard = Some((Instant::now(), fingerprint, catalog.clone()));
    catalog
}

/// How to launch a catalog ID, or a user-facing error.
pub fn launch_plan(id: &str) -> Result<LaunchPlan, String> {
    let catalog = catalog();
    let app = catalog
        .launchables
        .get(id)
        .ok_or_else(|| "This application is not installed on the computer".to_string())?;
    Ok(LaunchPlan {
        name: app.entry.name.clone(),
        command: adapt_flatpak(app.command.clone(), documents_portal_mounted()),
        working_dir: app.working_dir.clone(),
        dbus_name: app.dbus_name.clone(),
        iced: app.entry.kind == "iced",
    })
}

/// `flatpak run` aborts in bwrap when the document portal's FUSE mount is
/// missing (the portal crashed or was unmounted). Launch without it then:
/// the app works, only portal file sharing is unavailable.
fn adapt_flatpak(mut command: Vec<String>, portal_mounted: bool) -> Vec<String> {
    let is_flatpak_run = command.first().is_some_and(|c| c.ends_with("/flatpak") || c == "flatpak")
        && command.get(1).is_some_and(|c| c == "run");
    if is_flatpak_run && !portal_mounted && !command.iter().any(|c| c == "--no-documents-portal") {
        eprintln!("launcher: flatpak document portal is not mounted; launching without it");
        command.insert(2, "--no-documents-portal".into());
    }
    command
}

fn documents_portal_mounted() -> bool {
    let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return true;
    };
    let target = std::path::Path::new(&runtime).join("doc");
    let Ok(mounts) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return true;
    };
    mounts
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .any(|point| std::path::Path::new(point) == target)
}

/// D-Bus name for a reverse-DNS desktop ID (`org.gnome.Calculator.desktop`).
fn dbus_name_for(desktop_id: &str) -> Option<String> {
    let name = desktop_id.strip_suffix(".desktop")?;
    let parts: Vec<&str> = name.split('.').collect();
    let valid = parts.len() >= 2
        && name.len() <= 255
        && parts.iter().all(|p| {
            !p.is_empty()
                && !p.starts_with(|c: char| c.is_ascii_digit())
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
    valid.then(|| name.to_string())
}

/// Whether a single-instance app already runs on the desktop's session bus.
/// Launching it again would only open a window on the computer, and a second
/// instance on a private bus would share (and could overwrite) the running
/// instance's per-user state, such as unsaved drafts.
pub fn already_running(dbus_name: &str) -> bool {
    std::process::Command::new("busctl")
        .args([
            "--user",
            "--no-pager",
            "--timeout=2",
            "call",
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameHasOwner",
            "s",
            dbus_name,
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "b true"
        })
}

/// Base64 PNG for an app icon, rendered at 128 px and cached by revision.
pub fn icon_png(id: &str) -> Option<(String, String)> {
    let catalog = catalog();
    let app = catalog.launchables.get(id)?;
    let hash = app.entry.icon.clone()?;
    {
        let cache = ICONS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(png) = cache.as_ref().and_then(|c| c.get(&hash)) {
            return Some((hash, png.clone()));
        }
    }
    let path = app.icon_path.as_ref()?;
    let png = render_icon(path, ICON_SIZE)?;
    let encoded = base64_encode(&png);
    let mut cache = ICONS.lock().unwrap_or_else(|e| e.into_inner());
    let map = cache.get_or_insert_with(HashMap::new);
    if map.len() > 2000 {
        map.clear();
    }
    map.insert(hash.clone(), encoded.clone());
    Some((hash, encoded))
}

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Environment {
    /// Data dirs in precedence order (user first).
    data_dirs: Vec<PathBuf>,
    /// Icon base dirs in precedence order.
    icon_dirs: Vec<PathBuf>,
    pixmap_dirs: Vec<PathBuf>,
    theme: String,
    desktops: Vec<String>,
    languages: Vec<String>,
    path: Vec<PathBuf>,
    iced: Vec<IcedApp>,
}

#[derive(Debug, Clone)]
struct IcedApp {
    id: String,
    name: String,
    description: String,
    icon_names: Vec<String>,
    command: Vec<String>,
}

impl Environment {
    fn detect() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".local/share"));
        let mut data_dirs = vec![data_home.clone(), data_home.join("flatpak/exports/share")];
        let system = std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        for dir in system.split(':').filter(|d| d.starts_with('/')) {
            data_dirs.push(PathBuf::from(dir));
        }
        for extra in [
            "/var/lib/flatpak/exports/share",
            "/usr/local/share",
            "/usr/share",
        ] {
            data_dirs.push(PathBuf::from(extra));
        }
        dedup_paths(&mut data_dirs);

        let mut icon_dirs = vec![data_home.join("icons"), home.join(".icons")];
        icon_dirs.extend(data_dirs.iter().map(|d| d.join("icons")));
        dedup_paths(&mut icon_dirs);
        let pixmap_dirs = data_dirs.iter().map(|d| d.join("pixmaps")).collect();

        let desktops = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .split(':')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect();
        let path = std::env::var("PATH")
            .unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into())
            .split(':')
            .filter(|p| p.starts_with('/'))
            .map(PathBuf::from)
            .collect();

        Self {
            data_dirs,
            icon_dirs,
            pixmap_dirs,
            theme: icon_theme(),
            desktops,
            languages: languages(),
            path,
            iced: iced_apps(),
        }
    }
}

fn dedup_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
}

/// The desktop's icon theme: `ENZREMOTE_ICON_THEME`, GNOME settings, KDE
/// settings, then Adwaita.
fn icon_theme() -> String {
    if let Ok(theme) = std::env::var("ENZREMOTE_ICON_THEME")
        && !theme.is_empty()
    {
        return theme;
    }
    if let Ok(output) = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .stderr(std::process::Stdio::null())
        .output()
        && output.status.success()
    {
        let theme = String::from_utf8_lossy(&output.stdout)
            .trim()
            .trim_matches('\'')
            .to_string();
        if !theme.is_empty() {
            return theme;
        }
    }
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(text) = std::fs::read_to_string(PathBuf::from(home).join(".config/kdeglobals"))
    {
        let mut in_icons = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_icons = line == "[Icons]";
            } else if in_icons && let Some(theme) = line.strip_prefix("Theme=") {
                return theme.trim().to_string();
            }
        }
    }
    "Adwaita".into()
}

/// Locale fallbacks for localized keys: `de_AT.UTF-8` → `de_AT`, `de`.
fn languages() -> Vec<String> {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default();
    let base = raw.split(['.', '@']).next().unwrap_or("").to_string();
    let mut out = Vec::new();
    if !base.is_empty() && base != "C" && base != "POSIX" {
        out.push(base.clone());
        if let Some((lang, _)) = base.split_once('_') {
            out.push(lang.to_string());
        }
    }
    out
}

/// Installed ICED apps with freedesktop icon names that themes provide.
fn iced_apps() -> Vec<IcedApp> {
    const ICONS: &[(&str, &[&str])] = &[
        (
            "files-app",
            &["system-file-manager", "org.gnome.Nautilus", "folder"],
        ),
        (
            "text-app",
            &[
                "accessories-text-editor",
                "org.gnome.TextEditor",
                "text-editor",
            ],
        ),
        (
            "terminal-emulator",
            &["utilities-terminal", "org.gnome.Terminal", "terminal"],
        ),
        (
            "task-manager-app",
            &["utilities-system-monitor", "org.gnome.SystemMonitor"],
        ),
        ("calendar-app", &["x-office-calendar", "org.gnome.Calendar"]),
        (
            "mail-app",
            &["internet-mail", "mail-client", "org.gnome.Evolution"],
        ),
        ("music-app", &["multimedia-audio-player", "org.gnome.Music"]),
        (
            "reader-app",
            &[
                "accessories-document-viewer",
                "org.gnome.Evince",
                "x-office-document",
            ],
        ),
        (
            "store-app",
            &["system-software-install", "org.gnome.Software"],
        ),
        (
            "remote-app",
            &[
                "preferences-desktop-remote-desktop",
                "krdc",
                "remote-desktop",
            ],
        ),
    ];
    crate::desktop::catalog()
        .into_iter()
        .filter(|app| app.available)
        .filter_map(|app| {
            let command = crate::desktop::app_command(&app.id).ok()?;
            let icon_names = ICONS
                .iter()
                .find(|(id, _)| *id == app.id)
                .map(|(_, names)| names.iter().map(|s| s.to_string()).collect())
                .unwrap_or_default();
            Some(IcedApp {
                id: app.id,
                name: app.name,
                description: app.description,
                icon_names,
                command,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

fn build_catalog(env: &Environment) -> Catalog {
    let icons = IconIndex::build(env);
    let mut launchables: HashMap<String, Launchable> = HashMap::new();

    for app in &env.iced {
        let icon_path = app.icon_names.iter().find_map(|name| icons.lookup(name));
        let entry = AppEntry {
            id: app.id.clone(),
            name: app.name.clone(),
            description: app.description.clone(),
            kind: "iced".into(),
            icon: icon_path.as_deref().map(icon_hash),
            keywords: Vec::new(),
            categories: Vec::new(),
        };
        launchables.insert(
            app.id.clone(),
            Launchable {
                entry,
                command: app.command.clone(),
                working_dir: None,
                icon_path,
                dbus_name: None,
            },
        );
    }

    let mut seen_desktop_ids = HashSet::new();
    for dir in &env.data_dirs {
        let applications = dir.join("applications");
        for (desktop_id, path) in desktop_files(&applications) {
            // Earlier data dirs shadow later ones with the same desktop ID.
            if !seen_desktop_ids.insert(desktop_id.clone()) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(entry) = DesktopEntry::parse(&text, &env.languages) else {
                continue;
            };
            let Some(launchable) = entry.launchable(&desktop_id, &path, env, &icons) else {
                continue;
            };
            launchables.insert(launchable.entry.id.clone(), launchable);
        }
    }

    let mut apps: Vec<AppEntry> = launchables.values().map(|l| l.entry.clone()).collect();
    apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    let mut hasher = Sha256::new();
    for app in &apps {
        hasher.update(app.id.as_bytes());
        hasher.update(app.name.as_bytes());
        hasher.update(app.icon.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\n");
    }
    let revision = hex(&hasher.finalize()[..8]);
    Catalog {
        revision,
        apps,
        launchables,
    }
}

/// `(desktop_id, path)` for every `.desktop` file under an applications dir;
/// subdirectories become `-`-joined prefixes (freedesktop desktop IDs).
fn desktop_files(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                if prefix.matches('-').count() < 4 {
                    stack.push((path, format!("{prefix}{name}-")));
                }
            } else if name.ends_with(".desktop") {
                out.push((format!("{prefix}{name}"), path));
            }
        }
    }
    out
}

/// Stable, session-ID-safe catalog ID for a desktop ID.
pub fn slug_for(desktop_id: &str) -> String {
    let base = desktop_id
        .strip_suffix(".desktop")
        .unwrap_or(desktop_id)
        .to_ascii_lowercase();
    let mut slug = String::new();
    for c in base.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    // Keep the most specific (last) part of long reverse-DNS IDs.
    let start = slug.len().saturating_sub(30);
    let short = slug[start..].trim_start_matches('-');
    let hash = hex(&Sha256::digest(desktop_id.as_bytes())[..3]);
    format!("{short}-{hash}")
}

#[derive(Debug, Default)]
struct DesktopEntry {
    values: HashMap<String, String>,
}

impl DesktopEntry {
    /// Parse the `[Desktop Entry]` group; localized keys are resolved for
    /// `languages` (falling back to the unlocalized value).
    fn parse(text: &str, languages: &[String]) -> Option<Self> {
        let mut in_group = false;
        let mut raw: HashMap<String, String> = HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                in_group = line == "[Desktop Entry]";
                continue;
            }
            if !in_group {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                raw.entry(key.trim().to_string())
                    .or_insert_with(|| value.trim().to_string());
            }
        }
        if raw.is_empty() {
            return None;
        }
        let mut values = HashMap::new();
        for key in ["Name", "GenericName", "Comment", "Keywords", "Icon"] {
            let localized = languages
                .iter()
                .find_map(|lang| raw.get(&format!("{key}[{lang}]")));
            if let Some(value) = localized.or_else(|| raw.get(key)) {
                values.insert(key.to_string(), unescape(value));
            }
        }
        for key in [
            "Type",
            "Exec",
            "TryExec",
            "Path",
            "Terminal",
            "NoDisplay",
            "Hidden",
            "OnlyShowIn",
            "NotShowIn",
            "Categories",
        ] {
            if let Some(value) = raw.get(key) {
                values.insert(key.to_string(), unescape(value));
            }
        }
        Some(Self { values })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    fn flag(&self, key: &str) -> bool {
        self.get(key) == Some("true")
    }

    fn list(&self, key: &str) -> Vec<String> {
        self.get(key)
            .unwrap_or("")
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn launchable(
        &self,
        desktop_id: &str,
        path: &Path,
        env: &Environment,
        icons: &IconIndex,
    ) -> Option<Launchable> {
        if self.get("Type") != Some("Application") || self.flag("NoDisplay") || self.flag("Hidden")
        {
            return None;
        }
        // Our own desktop helpers (Find My Phone) are not apps to stream.
        if desktop_id.starts_with("enzremote-") {
            return None;
        }
        // Terminal apps need a terminal emulator around them; not offered yet.
        if self.flag("Terminal") {
            return None;
        }
        let shown_in = |key: &str| {
            self.list(key)
                .iter()
                .any(|d| env.desktops.contains(&d.to_ascii_lowercase()))
        };
        if !self.list("OnlyShowIn").is_empty() && !shown_in("OnlyShowIn") {
            return None;
        }
        if shown_in("NotShowIn") {
            return None;
        }
        if let Some(try_exec) = self.get("TryExec")
            && find_executable(try_exec, &env.path).is_none()
        {
            return None;
        }
        let name = self.get("Name")?.trim().to_string();
        if name.is_empty() {
            return None;
        }
        let icon_name = self.get("Icon").map(str::trim).filter(|s| !s.is_empty());
        let command = expand_exec(self.get("Exec")?, &name, icon_name, path)?;
        find_executable(&command[0], &env.path)?;
        let icon_path = icon_name.and_then(|icon| icons.lookup(icon));
        let description = self
            .get("Comment")
            .or_else(|| self.get("GenericName"))
            .unwrap_or("")
            .chars()
            .take(MAX_DESCRIPTION)
            .collect();
        let keywords = self
            .list("Keywords")
            .into_iter()
            .chain(self.get("GenericName").map(str::to_string))
            .map(|k| k.to_lowercase())
            .take(12)
            .collect();
        const MAIN: &[&str] = &[
            "AudioVideo",
            "Audio",
            "Video",
            "Development",
            "Education",
            "Game",
            "Graphics",
            "Network",
            "Office",
            "Science",
            "Settings",
            "System",
            "Utility",
        ];
        let categories = self
            .list("Categories")
            .into_iter()
            .filter(|c| MAIN.contains(&c.as_str()))
            .collect();
        let working_dir = self
            .get("Path")
            .filter(|p| p.starts_with('/'))
            .map(PathBuf::from);
        Some(Launchable {
            entry: AppEntry {
                id: slug_for(desktop_id),
                name,
                description,
                kind: "desktop".into(),
                icon: icon_path.as_deref().map(icon_hash),
                keywords,
                categories,
            },
            command,
            working_dir,
            icon_path,
            dbus_name: dbus_name_for(desktop_id),
        })
    }
}

/// Unescape desktop-entry string values (`\s \n \t \r \\`).
fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('s') => out.push(' '),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Split an `Exec` value into argv and expand field codes for a launch
/// without files: `%f %F %u %U` and deprecated codes vanish, `%i` becomes
/// `--icon NAME`, `%c` the name, `%k` the entry path, `%%` a percent sign.
pub fn expand_exec(
    exec: &str,
    name: &str,
    icon: Option<&str>,
    entry_path: &Path,
) -> Option<Vec<String>> {
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut quoted = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                in_arg = true;
            }
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() && !quoted => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            c => {
                current.push(c);
                in_arg = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if in_arg {
        args.push(current);
    }

    let mut out = Vec::new();
    for arg in args {
        match arg.as_str() {
            "%f" | "%F" | "%u" | "%U" | "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => continue,
            "%i" => {
                if let Some(icon) = icon {
                    out.push("--icon".into());
                    out.push(icon.into());
                }
                continue;
            }
            _ => {}
        }
        let mut expanded = String::new();
        let mut chars = arg.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                expanded.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => expanded.push('%'),
                Some('c') => expanded.push_str(name),
                Some('k') => expanded.push_str(&entry_path.to_string_lossy()),
                Some(_) | None => {} // file/URL codes have nothing to expand
            }
        }
        if !expanded.is_empty() {
            out.push(expanded);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn find_executable(program: &str, path: &[PathBuf]) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        let p = PathBuf::from(program);
        return (p.is_absolute() && executable(&p)).then_some(p);
    }
    path.iter()
        .map(|dir| dir.join(program))
        .find(|p| executable(p))
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

/// Name → best file per theme, for the theme inheritance chain.
struct IconIndex {
    themes: Vec<HashMap<String, (u32, PathBuf)>>,
    pixmaps: Vec<PathBuf>,
}

impl IconIndex {
    fn build(env: &Environment) -> Self {
        let mut chain = Vec::new();
        let mut queue = vec![env.theme.clone()];
        while let Some(theme) = queue.pop() {
            if chain.contains(&theme) || chain.len() > 8 {
                continue;
            }
            for base in &env.icon_dirs {
                let index = base.join(&theme).join("index.theme");
                if let Ok(text) = std::fs::read_to_string(&index) {
                    for line in text.lines() {
                        if let Some(parents) = line.trim().strip_prefix("Inherits=") {
                            for parent in parents.split(',').rev() {
                                queue.push(parent.trim().to_string());
                            }
                        }
                    }
                    break;
                }
            }
            chain.push(theme);
        }
        for fallback in ["hicolor", "Adwaita"] {
            if !chain.iter().any(|t| t == fallback) {
                chain.push(fallback.to_string());
            }
        }
        let themes = chain
            .iter()
            .map(|theme| {
                let mut map: HashMap<String, (u32, PathBuf)> = HashMap::new();
                for base in &env.icon_dirs {
                    index_theme_dir(&base.join(theme), &mut map);
                }
                map
            })
            .collect();
        Self {
            themes,
            pixmaps: env.pixmap_dirs.clone(),
        }
    }

    fn lookup(&self, icon: &str) -> Option<PathBuf> {
        let candidate = Path::new(icon);
        if candidate.is_absolute() {
            return candidate.is_file().then(|| candidate.to_path_buf());
        }
        let name = icon
            .strip_suffix(".png")
            .or_else(|| icon.strip_suffix(".svg"))
            .unwrap_or(icon);
        for theme in &self.themes {
            if let Some((_, path)) = theme.get(name) {
                return Some(path.clone());
            }
        }
        for dir in &self.pixmaps {
            for ext in ["svg", "png"] {
                let path = dir.join(format!("{name}.{ext}"));
                if path.is_file() {
                    return Some(path);
                }
            }
        }
        None
    }
}

/// Score an icon file: scalable SVG best, then larger PNGs up to 512.
fn icon_score(path: &Path) -> u32 {
    let svg = path.extension().is_some_and(|e| e == "svg");
    let size = path
        .components()
        .filter_map(|c| {
            let s = c.as_os_str().to_string_lossy();
            let digits: String = s.chars().take_while(|ch| ch.is_ascii_digit()).collect();
            digits
                .parse::<u32>()
                .ok()
                .filter(|n| *n >= 8 && *n <= 1024)
                .map(|n| {
                    if s.contains("@2x") {
                        n * 2
                    } else if s.contains("@3x") {
                        n * 3
                    } else {
                        n
                    }
                })
        })
        .next_back();
    match (svg, size) {
        (true, None) => 1000, // scalable/
        (true, Some(n)) => 500 + n.min(400),
        (false, Some(n)) if n <= 512 => n,
        (false, Some(_)) => 100,
        (false, None) => 50,
    }
}

fn index_theme_dir(dir: &Path, map: &mut HashMap<String, (u32, PathBuf)>) {
    let mut stack = vec![(dir.to_path_buf(), 0)];
    while let Some((path, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() || (kind.is_symlink() && p.is_dir()) {
                if depth < 4 {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            let file = entry.file_name().to_string_lossy().into_owned();
            let Some((stem, ext)) = file.rsplit_once('.') else {
                continue;
            };
            if !(ext == "svg" || ext == "png") || stem.ends_with("-symbolic") {
                continue;
            }
            // Application icons live in apps/ (or legacy applications/).
            let in_apps = p.components().any(|c| {
                let s = c.as_os_str();
                s == "apps" || s == "applications"
            });
            if !in_apps {
                continue;
            }
            let score = icon_score(&p);
            match map.get(stem) {
                Some((best, _)) if *best >= score => {}
                _ => {
                    map.insert(stem.to_string(), (score, p));
                }
            }
        }
    }
}

fn icon_hash(path: &Path) -> String {
    let meta = std::fs::metadata(path).ok();
    let modified = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hasher.update(modified.to_le_bytes());
    hasher.update(meta.map(|m| m.len()).unwrap_or(0).to_le_bytes());
    hex(&hasher.finalize()[..8])
}

/// Render an SVG or PNG icon into a `size`×`size` PNG (aspect kept, centered).
pub fn render_icon(path: &Path, size: u32) -> Option<Vec<u8>> {
    use resvg::tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};
    let data = std::fs::read(path).ok()?;
    let mut target = Pixmap::new(size, size)?;
    if path.extension().is_some_and(|e| e == "svg" || e == "svgz") {
        let options = resvg::usvg::Options::default();
        let tree = resvg::usvg::Tree::from_data(&data, &options).ok()?;
        let tree_size = tree.size();
        let scale = (size as f32 / tree_size.width()).min(size as f32 / tree_size.height());
        let dx = (size as f32 - tree_size.width() * scale) / 2.0;
        let dy = (size as f32 - tree_size.height() * scale) / 2.0;
        resvg::render(
            &tree,
            Transform::from_row(scale, 0.0, 0.0, scale, dx, dy),
            &mut target.as_mut(),
        );
    } else {
        let source = Pixmap::decode_png(&data).ok()?;
        let scale = (size as f32 / source.width() as f32).min(size as f32 / source.height() as f32);
        let dx = (size as f32 - source.width() as f32 * scale) / 2.0;
        let dy = (size as f32 - source.height() as f32 * scale) / 2.0;
        let paint = PixmapPaint {
            quality: FilterQuality::Bicubic,
            ..PixmapPaint::default()
        };
        target.draw_pixmap(
            0,
            0,
            source.as_ref(),
            &paint,
            Transform::from_row(scale, 0.0, 0.0, scale, dx, dy),
            None,
        );
    }
    target.encode_png().ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_field_codes_expand_for_a_plain_launch() {
        let path = Path::new("/usr/share/applications/x.desktop");
        assert_eq!(
            expand_exec("gedit %U", "Text", None, path).unwrap(),
            vec!["gedit"]
        );
        assert_eq!(
            expand_exec(
                "\"/opt/My App/run\" --name=%c %i --x=100%%",
                "My App",
                Some("myapp"),
                path
            )
            .unwrap(),
            vec![
                "/opt/My App/run",
                "--name=My App",
                "--icon",
                "myapp",
                "--x=100%"
            ]
        );
        assert_eq!(
            expand_exec(
                "flatpak run --branch=stable --command=foo org.x.Y @@u %U @@",
                "Y",
                None,
                path
            )
            .unwrap(),
            vec![
                "flatpak",
                "run",
                "--branch=stable",
                "--command=foo",
                "org.x.Y",
                "@@u",
                "@@"
            ]
        );
        assert_eq!(
            expand_exec("sh -c \"echo \\\"hi\\\"\"", "S", None, path).unwrap(),
            vec!["sh", "-c", "echo \"hi\""]
        );
        assert!(expand_exec("broken \"quote", "B", None, path).is_none());
        assert!(expand_exec("%U", "B", None, path).is_none());
    }

    fn env(desktops: &[&str]) -> Environment {
        Environment {
            data_dirs: vec![],
            icon_dirs: vec![],
            pixmap_dirs: vec![],
            theme: "hicolor".into(),
            desktops: desktops.iter().map(|s| s.to_string()).collect(),
            languages: vec!["de_AT".into(), "de".into()],
            path: vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")],
            iced: vec![],
        }
    }

    fn launchable(text: &str, desktops: &[&str]) -> Option<Launchable> {
        let env = env(desktops);
        let icons = IconIndex {
            themes: vec![],
            pixmaps: vec![],
        };
        DesktopEntry::parse(text, &env.languages)?.launchable(
            "x.desktop",
            Path::new("/tmp/x.desktop"),
            &env,
            &icons,
        )
    }

    #[test]
    fn launcher_filters_match_desktop_behaviour() {
        let base = "[Desktop Entry]\nType=Application\nName=Shell\nName[de]=Hülle\nComment=Runs\nExec=sh %U\nCategories=System;Utility;X-Foo;\nKeywords=cli;posix;\n";
        let app = launchable(base, &["gnome"]).expect("plain app");
        assert_eq!(app.entry.name, "Hülle");
        assert_eq!(app.command, vec!["sh"]);
        assert_eq!(app.entry.categories, vec!["System", "Utility"]);
        assert!(app.entry.keywords.contains(&"posix".to_string()));
        assert!(launchable(&format!("{base}NoDisplay=true\n"), &["gnome"]).is_none());
        assert!(launchable(&format!("{base}Hidden=true\n"), &["gnome"]).is_none());
        assert!(launchable(&format!("{base}Terminal=true\n"), &["gnome"]).is_none());
        assert!(launchable(&format!("{base}OnlyShowIn=KDE;\n"), &["gnome"]).is_none());
        assert!(launchable(&format!("{base}OnlyShowIn=GNOME;\n"), &["gnome"]).is_some());
        assert!(launchable(&format!("{base}NotShowIn=GNOME;\n"), &["gnome"]).is_none());
        assert!(
            launchable(
                &format!("{base}TryExec=definitely-not-installed-xyz\n"),
                &["gnome"]
            )
            .is_none()
        );
        assert!(
            launchable(
                &base.replace("Exec=sh", "Exec=definitely-not-installed-xyz"),
                &["gnome"]
            )
            .is_none()
        );
        assert!(launchable(&base.replace("Type=Application", "Type=Link"), &["gnome"]).is_none());
        // Other groups (actions) never override the main entry.
        let with_action = format!("{base}\n[Desktop Action new]\nName=New\nExec=false\n");
        assert_eq!(
            launchable(&with_action, &["gnome"]).unwrap().entry.name,
            "Hülle"
        );
    }

    #[test]
    fn flatpak_skips_missing_documents_portal() {
        let command: Vec<String> = ["/usr/bin/flatpak", "run", "--branch=stable", "org.example.App"]
            .map(String::from)
            .to_vec();
        assert_eq!(adapt_flatpak(command.clone(), true), command);
        let adapted = adapt_flatpak(command.clone(), false);
        assert_eq!(adapted[2], "--no-documents-portal");
        assert_eq!(adapted.len(), command.len() + 1);
        let native: Vec<String> = vec!["gnome-calculator".into()];
        assert_eq!(adapt_flatpak(native.clone(), false), native);
    }
}
