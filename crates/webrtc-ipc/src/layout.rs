//! Installed worker and compositor locations, with explicit development overrides.

use std::path::{Path, PathBuf};

/// First UDP media port accepted by the installed Phone Link firewall service.
pub const MEDIA_PORT_FIRST: u32 = 9471;
/// Last UDP media port accepted by the installed Phone Link firewall service.
pub const MEDIA_PORT_LAST: u32 = 9599;

/// Resolve a worker from an explicit override, beside the daemon, or its installed directory.
pub fn helper(variable: &str, name: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(variable) {
        return PathBuf::from(path);
    }
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        let sibling = parent.join(name);
        if sibling.is_file() {
            return sibling;
        }
    }
    Path::new("/usr/libexec/enzimos-phone-link").join(name)
}

/// Locate the controller in an exported OS runtime or an explicitly selected development stack.
pub fn hyprctl(root: &Path) -> PathBuf {
    [
        "hyprland-current-build/hyprctl/enz-hyprctl",
        "hyprland-current-build/hyprctl/hyprctl",
        "enz-hyprctl",
        "hyprctl",
    ]
    .into_iter()
    .map(|name| root.join(name))
    .find(|path| path.is_file())
    .unwrap_or_else(|| root.join("hyprland-current-build/hyprctl/enz-hyprctl"))
}
