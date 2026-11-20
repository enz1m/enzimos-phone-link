//! Pairing token management for QR code-based device pairing.
//!
//! ## Flow
//!
//! 1. Host generates a one-time pairing token via `enzremote-signaling pair`
//! 2. Token is encoded in a QR code and scanned by the Android client
//! 3. Client sends `Hello` with the one-time token
//! 4. Server validates and consumes the token (single use)
//! 5. Server generates a permanent device token and returns it in `HelloAck`
//! 6. Client stores the device token for future reconnections
//!
//! ## Security
//!
//! - One-time tokens are removed after first use (even if the connection fails)
//! - Tokens have a configurable TTL (default 5 minutes)
//! - Permanent device tokens are long-lived and can be revoked by the server
//! - Token lookup uses standard HashMap; timing side-channels are not a
//!   practical threat for a LAN signaling server

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default TTL for one-time pairing tokens.
const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(300); // 5 minutes

/// Maximum number of active pairing tokens.
const MAX_PAIRING_TOKENS: usize = 10;

/// Maximum number of registered device tokens.
const MAX_DEVICE_TOKENS: usize = 100;

/// A one-time pairing token with expiration.
#[derive(Debug, Clone)]
struct PairingToken {
    /// When this token was created.
    created_at: Instant,
    /// Time-to-live for this token.
    ttl: Duration,
}

impl PairingToken {
    fn is_expired(&self) -> bool {
        self.created_at.elapsed() > self.ttl
    }
}

/// A paired device, bound to exactly one permanent credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    /// Stable client-chosen device identifier (`[A-Za-z0-9._-]`).
    pub device_id: String,
    /// Human-readable device name shown in host UI (may be empty).
    pub name: String,
    /// Unix seconds when the device was paired (0 if unknown/legacy).
    pub paired_at: u64,
}

/// Manages one-time pairing tokens and permanent device tokens.
///
/// Permanent tokens are stored only as SHA-256 digests, so a leaked
/// persistence file does not reveal usable credentials. Each device ID owns at
/// most one token: re-pairing a device replaces its previous credential.
///
/// Thread safety: all methods take `&mut self`. The caller (ServerState)
/// must wrap this in a `Mutex`.
pub struct PairingStore {
    /// One-time pairing tokens (token_string -> metadata).
    pairing_tokens: HashMap<String, PairingToken>,
    /// Permanent device tokens (token digest -> device record).
    device_tokens: HashMap<String, DeviceRecord>,
    /// TTL for new pairing tokens.
    token_ttl: Duration,
    /// Optional file path for persisting device tokens.
    persist_path: Option<std::path::PathBuf>,
}

/// Digest key used to index a permanent token.
fn token_key(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(token.as_bytes());
    let mut key = String::with_capacity(7 + 64);
    key.push_str("sha256:");
    for b in digest {
        use std::fmt::Write;
        let _ = write!(key, "{b:02x}");
    }
    key
}

fn valid_field(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Percent-encode a display name so it fits one whitespace-free field.
fn encode_name(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    if out.is_empty() { "-".into() } else { out }
}

fn decode_name(field: &str) -> String {
    if field == "-" {
        return String::new();
    }
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    sanitize_device_name(&String::from_utf8_lossy(&out))
}

/// Normalize a client-supplied device name: printable, single line, bounded.
pub fn sanitize_device_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    cleaned.chars().take(64).collect()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl PairingStore {
    /// Create a new empty store with default TTL.
    pub fn new() -> Self {
        Self::with_ttl(DEFAULT_TOKEN_TTL)
    }

    /// Create a store with a custom TTL.
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            pairing_tokens: HashMap::new(),
            device_tokens: HashMap::new(),
            token_ttl: ttl,
            persist_path: None,
        }
    }

    /// Set the file path for persisting device tokens.
    ///
    /// If the file exists, device tokens are loaded from it. Legacy plaintext
    /// entries are converted to digests and rewritten immediately.
    /// New tokens are saved automatically on register/revoke.
    ///
    /// Security: the file must be owned by the current user and have
    /// mode 0600 (no group/world access). Files with wrong permissions
    /// are rejected to prevent token injection from other users.
    pub fn with_persistence(mut self, path: std::path::PathBuf) -> Self {
        if path.exists()
            && let Err(reason) = check_persistence_permissions(&path)
        {
            eprintln!("pairing: refusing to load {}: {reason}", path.display());
            // Still set persist_path so new tokens get saved
            // (write_private_file creates with 0600).
            self.persist_path = Some(path);
            return self;
        }
        let mut migrated = false;
        if path.exists()
            && let Ok(contents) = std::fs::read_to_string(&path)
        {
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let mut fields = line.split(' ');
                let (Some(token), Some(device_id)) = (fields.next(), fields.next()) else {
                    continue;
                };
                if !valid_field(token) || !valid_field(device_id) {
                    eprintln!("pairing: skipping invalid entry in {}", path.display());
                    continue;
                }
                let hashed = token.starts_with("sha256:");
                // Legacy plaintext lines have exactly two fields.
                if !hashed && fields.clone().next().is_some() {
                    eprintln!("pairing: skipping invalid entry in {}", path.display());
                    continue;
                }
                let name = fields.next().map(decode_name).unwrap_or_default();
                let paired_at = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let key = if token.starts_with("sha256:") {
                    token.to_string()
                } else {
                    migrated = true;
                    token_key(token)
                };
                self.device_tokens.insert(
                    key,
                    DeviceRecord {
                        device_id: device_id.to_string(),
                        name,
                        paired_at,
                    },
                );
            }
        }
        self.persist_path = Some(path);
        if migrated {
            let _ = self.persist_device_tokens();
        }
        self
    }

    /// Register a one-time pairing token.
    ///
    /// Returns `true` if the token was added, `false` if it already exists
    /// or the maximum number of active pairing tokens has been reached.
    pub fn add_pairing_token(&mut self, token: String) -> bool {
        if self.pairing_tokens.contains_key(&token) {
            return false;
        }
        // Purge expired tokens before checking the cap — don't count dead tokens.
        self.purge_expired();
        if self.pairing_tokens.len() >= MAX_PAIRING_TOKENS {
            eprintln!(
                "pairing: rejected new pairing token — limit of {} active tokens reached",
                MAX_PAIRING_TOKENS,
            );
            return false;
        }
        self.pairing_tokens.insert(
            token,
            PairingToken {
                created_at: Instant::now(),
                ttl: self.token_ttl,
            },
        );
        true
    }

    /// Mint and register a fresh one-time pairing token.
    pub fn mint_pairing_token(&mut self) -> Result<String, String> {
        let token = generate_device_token()?;
        if self.add_pairing_token(token.clone()) {
            Ok(token)
        } else {
            Err("too many active pairing codes; wait for one to expire".into())
        }
    }

    /// TTL applied to new one-time pairing tokens.
    pub fn token_ttl(&self) -> Duration {
        self.token_ttl
    }

    /// Attempt to consume a one-time pairing token.
    ///
    /// If the token is valid and not expired, it is removed and a new
    /// permanent device token is generated and returned.
    ///
    /// If the token is expired, it is removed and `None` is returned.
    /// Returns `Err` if device token generation fails (e.g. `/dev/urandom`
    /// unavailable).
    pub fn consume_pairing_token(&mut self, token: &str) -> Result<Option<String>, String> {
        // Remove expired tokens first.
        self.purge_expired();

        // Try to remove the token (single use).
        let entry = match self.pairing_tokens.remove(token) {
            Some(e) => e,
            None => return Ok(None),
        };
        if entry.is_expired() {
            return Ok(None);
        }

        // The caller registers the returned permanent token with the
        // device_id from the same Hello message.
        generate_device_token().map(Some)
    }

    /// Generate a new permanent device token without consuming anything.
    /// Used after an interactive approval.
    pub fn issue_device_token(&mut self) -> Result<String, String> {
        generate_device_token()
    }

    /// Check if a permanent device token is valid.
    pub fn validate_device_token(&self, token: &str) -> bool {
        self.device_tokens.contains_key(&token_key(token))
    }

    /// Return the device bound to a permanent token.
    pub fn device_for_token(&self, token: &str) -> Option<&DeviceRecord> {
        self.device_tokens.get(&token_key(token))
    }

    /// Register a permanent device token with the device_id that was assigned.
    ///
    /// If a persistence path is set, the device tokens are saved to disk.
    /// Returns `Err` if the device_id contains invalid characters or persistence fails.
    pub fn register_device_token(
        &mut self,
        device_token: String,
        device_id: String,
    ) -> Result<(), String> {
        self.register_device(device_token, device_id, String::new())
    }

    /// Register a permanent token for a named device, replacing any older
    /// credential held by the same device ID.
    pub fn register_device(
        &mut self,
        device_token: String,
        device_id: String,
        name: String,
    ) -> Result<(), String> {
        // Reject device_id with whitespace or control characters to prevent
        // persistence file injection.
        if device_id.is_empty() {
            return Err("device_id must not be empty".into());
        }
        if !valid_field(&device_id) {
            return Err("device_id contains invalid characters".into());
        }
        if device_token.is_empty() {
            return Err("device_token must not be empty".into());
        }
        if !valid_field(&device_token) {
            return Err("device_token contains invalid characters".into());
        }
        let key = token_key(&device_token);
        let replacing = self.device_tokens.contains_key(&key)
            || self
                .device_tokens
                .values()
                .any(|d| d.device_id == device_id);
        // Enforce cap on device tokens (but allow replacing existing tokens).
        if !replacing && self.device_tokens.len() >= MAX_DEVICE_TOKENS {
            return Err(format!(
                "maximum number of device tokens ({MAX_DEVICE_TOKENS}) reached; \
                 revoke an existing token first"
            ));
        }
        let mut next = self.device_tokens.clone();
        next.retain(|_, d| d.device_id != device_id);
        next.insert(
            key,
            DeviceRecord {
                device_id,
                name: sanitize_device_name(&name),
                paired_at: unix_now(),
            },
        );
        self.persist_records(&next)?;
        self.device_tokens = next;
        Ok(())
    }

    /// Revoke a credential durably before publishing the new paired-device set.
    pub fn revoke_device_token(&mut self, token: &str) -> Result<bool, String> {
        let mut next = self.device_tokens.clone();
        let removed = next.remove(&token_key(token)).is_some();
        if removed { self.persist_records(&next)?; self.device_tokens = next; }
        Ok(removed)
    }

    /// Revoke every credential held by a device ID.
    pub fn revoke_device(&mut self, device_id: &str) -> Result<bool, String> {
        let mut next = self.device_tokens.clone();
        next.retain(|_, record| record.device_id != device_id);
        let removed = next.len() != self.device_tokens.len();
        if removed { self.persist_records(&next)?; self.device_tokens = next; }
        Ok(removed)
    }

    /// Paired devices, oldest first.
    pub fn list_devices(&self) -> Vec<DeviceRecord> {
        let mut devices: Vec<_> = self.device_tokens.values().cloned().collect();
        devices.sort_by(|a, b| (a.paired_at, &a.device_id).cmp(&(b.paired_at, &b.device_id)));
        devices
    }

    /// Number of active (non-expired) pairing tokens.
    pub fn pairing_token_count(&self) -> usize {
        self.pairing_tokens
            .values()
            .filter(|t| !t.is_expired())
            .count()
    }

    /// Number of registered device tokens.
    pub fn device_token_count(&self) -> usize {
        self.device_tokens.len()
    }

    /// Has any pairing tokens registered (for checking if pairing mode is active).
    pub fn has_pairing_tokens(&self) -> bool {
        self.pairing_tokens.values().any(|t| !t.is_expired())
    }

    /// Remove expired pairing tokens.
    fn purge_expired(&mut self) {
        self.pairing_tokens.retain(|_, t| !t.is_expired());
    }

    /// Persist device tokens to the configured file path.
    ///
    /// Returns `Err` if the file write fails. If no persistence path is
    /// configured, returns `Ok`.
    fn persist_device_tokens(&self) -> Result<(), String> { self.persist_records(&self.device_tokens) }

    fn persist_records(&self, records: &HashMap<String, DeviceRecord>) -> Result<(), String> {
        let Some(ref path) = self.persist_path else {
            return Ok(());
        };
        let mut content = String::from(
            "# enzremote paired devices\n# format: sha256:<token digest> <device_id> <name> <paired_at>\n",
        );
        for (key, device) in records {
            content.push_str(&format!(
                "{key} {} {} {}\n",
                device.device_id,
                encode_name(&device.name),
                device.paired_at
            ));
        }
        write_private_file(path, content.as_bytes()).map_err(|error| format!("failed to persist device tokens: {error}"))
    }

    /// Load pairing tokens from an environment variable (comma-separated).
    ///
    /// Returns the number of tokens loaded.
    pub fn load_from_env(&mut self, var_name: &str) -> usize {
        match std::env::var(var_name) {
            Ok(val) if !val.is_empty() => {
                let mut count = 0;
                for token in val.split(',') {
                    let token = token.trim();
                    if !token.is_empty() && self.add_pairing_token(token.to_string()) {
                        count += 1;
                    }
                }
                count
            }
            _ => 0,
        }
    }
}

impl Default for PairingStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a random device token (256-bit hex string).
///
/// Uses `/dev/urandom` for cryptographically secure randomness.
/// Returns an error if `/dev/urandom` is not available — refusing to
/// generate a token is safer than generating a predictable one.
/// The token is 64 hex chars (256 bits).
fn generate_device_token() -> Result<String, String> {
    let bytes = read_urandom(32)
        .map_err(|e| format!("failed to read /dev/urandom for device token: {e}"))?;
    let mut hex = String::with_capacity(64);
    for b in &bytes {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    Ok(hex)
}

/// Check that a persistence file is safe to load.
///
/// Returns `Ok(())` if the file is not a symlink and has mode 0600
/// (no group/world access). Returns `Err` with a human-readable reason
/// otherwise.
fn check_persistence_permissions(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("cannot stat file: {e}"))?;
    // Reject symlinks — prevents symlink injection attacks.
    if meta.file_type().is_symlink() {
        return Err("file is a symlink".into());
    }
    // Check permissions — must have no group/world bits set.
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "file has mode {:04o} — group/world bits must be 0 (expected 0600)",
            mode,
        ));
    }
    Ok(())
}

/// Write `data` to `path` with mode 0600 (owner read/write only).
///
/// Device tokens are long-lived secrets; they must not be world-readable.
pub(crate) fn write_private_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Private file needs a parent"))?;
    enzui_security::secure_dir::ensure_private_dir(parent).map_err(std::io::Error::other)?;
    let mut stage = enzui_security::tempstage::TempStage::create(path)?;
    stage.write_all(data)?;
    stage.commit()
}

/// Read `n` bytes from `/dev/urandom`.
fn read_urandom(n: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consume_token_is_single_use() {
        let mut store = PairingStore::new();
        store.add_pairing_token("token1".into());
        assert!(
            store
                .consume_pairing_token("token1")
                .expect("should not fail")
                .is_some()
        );
        assert!(
            store
                .consume_pairing_token("token1")
                .expect("should not fail")
                .is_none()
        );
    }

    #[test]
    fn expired_token_not_consumable() {
        let mut store = PairingStore::with_ttl(Duration::from_millis(1));
        store.add_pairing_token("token1".into());
        // Wait for expiry.
        std::thread::sleep(Duration::from_millis(10));
        assert!(
            store
                .consume_pairing_token("token1")
                .expect("should not fail")
                .is_none()
        );
    }

    #[test]
    fn persistence_saves_and_loads() {
        let dir = std::env::temp_dir().join("enzremote_test_persist");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_tokens");
        let _ = std::fs::remove_file(&path);

        // Create store with persistence and register a device token.
        {
            let mut store = PairingStore::new().with_persistence(path.clone());
            store
                .register_device_token("dt-persist-1".into(), "device-a".into())
                .unwrap();
            store
                .register_device_token("dt-persist-2".into(), "device-b".into())
                .unwrap();
            assert_eq!(store.device_token_count(), 2);
        }

        // Create a new store from the same file — tokens should be loaded.
        {
            let store = PairingStore::new().with_persistence(path.clone());
            assert_eq!(store.device_token_count(), 2);
            assert!(store.validate_device_token("dt-persist-1"));
            assert!(store.validate_device_token("dt-persist-2"));
        }

        // Clean up.
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn persistence_handles_revocation() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("store");
        let path = parent.join("tokens");
        let mut store = PairingStore::new().with_persistence(path.clone());
        store.register_device_token("dt-1".into(), "d1".into()).unwrap();
        store.register_device_token("dt-2".into(), "d2".into()).unwrap();
        let saved = std::fs::read(&path).unwrap();
        // A failed publication must be reported and preserve both memory and
        // disk. Restore the directory, then verify a successful durable revoke.
        let parked = directory.path().join("parked");
        std::fs::rename(&parent, &parked).unwrap();
        std::fs::write(&parent, b"blocked directory").unwrap();
        assert!(store.revoke_device("d1").is_err());
        assert!(store.validate_device_token("dt-1"));
        assert!(store.register_device_token("dt-3".into(), "d3".into()).is_err());
        assert!(!store.validate_device_token("dt-3"));
        assert_eq!(std::fs::read(parked.join("tokens")).unwrap(), saved);
        std::fs::remove_file(&parent).unwrap();
        std::fs::rename(&parked, &parent).unwrap();
        assert!(store.revoke_device_token("dt-1").unwrap());
        let loaded = PairingStore::new().with_persistence(path);
        assert!(!loaded.validate_device_token("dt-1"));
        assert!(loaded.validate_device_token("dt-2"));
    }

    #[test]
    fn persistence_file_has_restricted_permissions() {
        let dir = std::env::temp_dir().join("enzremote_test_perms");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_perms_tokens");
        let _ = std::fs::remove_file(&path);

        let mut store = PairingStore::new().with_persistence(path.clone());
        store
            .register_device_token("dt-perm".into(), "device-p".into())
            .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "token file should be owner-only: {mode:o}");
        }

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn pairing_token_cap_enforced() {
        let mut store = PairingStore::new();
        // Add MAX_PAIRING_TOKENS tokens.
        for i in 0..MAX_PAIRING_TOKENS {
            assert!(store.add_pairing_token(format!("tok-{i}")));
        }
        // One more should be rejected.
        assert!(!store.add_pairing_token("tok-overflow".into()));
        assert_eq!(store.pairing_token_count(), MAX_PAIRING_TOKENS);
    }

    #[test]
    fn device_token_cap_enforced() {
        let mut store = PairingStore::new();
        for i in 0..MAX_DEVICE_TOKENS {
            store
                .register_device_token(format!("dt-{i}"), format!("dev-{i}"))
                .unwrap();
        }
        // One more should fail.
        let err = store.register_device_token("dt-overflow".into(), "dev-overflow".into());
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("maximum"));
    }

    #[test]
    fn persistence_rejects_world_readable_file() {
        let dir = std::env::temp_dir().join("enzremote_test_bad_perms");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_world_readable");
        // Write with default 0644 perms (world-readable).
        std::fs::write(&path, "dt-secret secret-device\n").unwrap();

        // with_persistence should reject the file and load 0 tokens.
        let store = PairingStore::new().with_persistence(path.clone());
        assert_eq!(store.device_token_count(), 0);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn persistence_rejects_symlink() {
        let dir = std::env::temp_dir().join("enzremote_test_symlink");
        let _ = std::fs::create_dir_all(&dir);
        let real_path = dir.join("real_tokens");
        let link_path = dir.join("link_tokens");
        let _ = std::fs::remove_file(&link_path);
        let _ = std::fs::remove_file(&real_path);

        // Create a real file with 0600 perms and a symlink to it.
        write_private_file(&real_path, b"dt-linked linked-device\n").unwrap();
        std::os::unix::fs::symlink(&real_path, &link_path).unwrap();

        // Loading via symlink should be rejected.
        let store = PairingStore::new().with_persistence(link_path.clone());
        assert_eq!(store.device_token_count(), 0);

        let _ = std::fs::remove_file(&link_path);
        let _ = std::fs::remove_file(&real_path);
        let _ = std::fs::remove_dir(&dir);
    }
}
