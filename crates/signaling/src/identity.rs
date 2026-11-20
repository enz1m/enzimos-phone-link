//! Persistent host identity: data directory, stable host ID and display name.
//!
//! Phones remember a host by its ID rather than by an address, so DHCP changes,
//! VPN addresses or USB forwarding never create a second "computer". The ID is
//! random, created once and stored with mode 0600 in the data directory.

use std::path::{Path, PathBuf};

/// Resolve the data directory (`ENZREMOTE_DATA_DIR`, else XDG data home).
pub fn data_dir() -> Option<PathBuf> {
    // Unit tests construct many servers; never let them write the user's
    // real paired-device store.
    if cfg!(test) {
        return None;
    }
    if let Ok(dir) = std::env::var("ENZREMOTE_DATA_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    if let Ok(dir) = std::env::var("XDG_DATA_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir).join("enzremote"));
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".local/share/enzremote"))
}

/// Runtime directory for the local control socket.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ENZREMOTE_RUNTIME_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned());
    PathBuf::from(base).join("enzremote")
}

/// Path of the local control socket used by CLI pairing/device commands.
pub fn control_socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("ENZREMOTE_CONTROL_SOCKET")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    runtime_dir().join("control.sock")
}

/// Stable identity advertised to clients.
#[derive(Debug, Clone)]
pub struct HostIdentity {
    /// 32 lowercase hex characters, stable across restarts.
    pub id: String,
    /// Human-readable computer name.
    pub name: String,
}

impl HostIdentity {
    /// Load (or create) the identity stored in `dir`. Without a directory the
    /// ID is ephemeral, which still works but phones must re-pair after restart.
    pub fn load(dir: Option<&Path>) -> Self {
        let name = display_name();
        let Some(dir) = dir else {
            return Self {
                id: random_hex(16),
                name,
            };
        };
        let path = dir.join("host_id");
        if let Ok(existing) = std::fs::read_to_string(&path) {
            let existing = existing.trim();
            if existing.len() == 32 && existing.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Self {
                    id: existing.to_ascii_lowercase(),
                    name,
                };
            }
        }
        let id = random_hex(16);
        let _ = std::fs::create_dir_all(dir);
        if let Err(error) = write_private(&path, format!("{id}\n").as_bytes()) {
            eprintln!("identity: could not persist host id: {error}");
        }
        Self { id, name }
    }
}

/// Pretty computer name: `ENZREMOTE_HOST_NAME`, the `host_name` setting, `/etc/machine-info`
/// PRETTY_HOSTNAME, then the kernel hostname.
pub fn display_name() -> String {
    let configured = std::env::var("ENZREMOTE_HOST_NAME").unwrap_or_else(|_| crate::host_settings::current().host_name);
    let name = crate::pairing::sanitize_device_name(&configured);
    if !name.is_empty() {
        return name;
    }
    if let Ok(info) = std::fs::read_to_string("/etc/machine-info") {
        for line in info.lines() {
            if let Some(value) = line.strip_prefix("PRETTY_HOSTNAME=") {
                let value = crate::pairing::sanitize_device_name(value.trim_matches('"'));
                if !value.is_empty() {
                    return value;
                }
            }
        }
    }
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "Computer".into())
}

/// Lowercase hex SHA-256.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// Random lowercase hex string of `bytes` random bytes.
pub fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => hex(&buf),
        Err(error) => panic!("/dev/urandom unavailable: {error}"),
    }
}

/// Six-digit pairing verification code.
///
/// Both sides derive it from the TLS certificate fingerprint each observes and
/// both nonces. A man-in-the-middle presenting a different certificate makes
/// the phone and computer show different codes. Over plaintext loopback the
/// fingerprint is empty and the code only confirms which request is approved.
pub fn verification_code(cert_fingerprint: &str, client_nonce: &str, host_nonce: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"enzremote-pair-v1\n");
    hasher.update(cert_fingerprint.to_ascii_lowercase().as_bytes());
    hasher.update(b"\n");
    hasher.update(client_nonce.as_bytes());
    hasher.update(b"\n");
    hasher.update(host_nonce.as_bytes());
    let digest = hasher.finalize();
    let value = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{value:06}")
}

/// Write `data` atomically-enough with mode 0600.
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(data)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_code_known_vector() {
        // Shared with the Android unit test (PairingCodeTest).
        assert_eq!(
            verification_code("00ff", "client", "host"),
            verification_code("00FF", "client", "host")
        );
        assert_eq!(verification_code("00ff", "client", "host"), KNOWN_VECTOR);
    }

    const KNOWN_VECTOR: &str = "767855";
}
