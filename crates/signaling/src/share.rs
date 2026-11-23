//! File transfers between paired phones and this computer (KDE Connect style).
//!
//! Phone → computer: `file_offer`, sequential `file_chunk`s acknowledged one
//! by one (`file_progress`), then `file_complete` with the SHA-256. Bytes go to
//! a hidden `.part` file in the Downloads folder and are renamed to a free name
//! once verified. Computer → phone: the computer pushes `file_offer`; the phone
//! pulls `file_data` by offset and confirms with `file_received`.
//!
//! Transfers belong to the device that started them and are dropped (partial
//! files deleted) when it disconnects.

use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::protocol::FILE_CHUNK_SIZE;

/// Largest single file either direction (well above phone videos).
pub const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024 * 1024;
/// Concurrent transfers per device and direction.
const MAX_PER_DEVICE: usize = 8;
/// Disk space kept free after a file is received.
const FREE_SPACE_MARGIN: u64 = 256 * 1024 * 1024;

struct Incoming {
    device_id: String,
    name: String,
    dir: PathBuf,
    part: PathBuf,
    file: File,
    size: u64,
    received: u64,
    hasher: Sha256,
}

/// A file this computer offered to a phone.
#[derive(Clone, Debug)]
pub struct Outgoing {
    pub device_id: String,
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Default)]
pub struct Transfers {
    incoming: Mutex<HashMap<String, Incoming>>,
    outgoing: Mutex<HashMap<String, Outgoing>>,
}

impl Transfers {
    /// Accept a phone's offer: reserve a `.part` file in `dir`.
    pub fn offer(&self, device_id: &str, transfer_id: &str, name: &str, size: u64, dir: &Path) -> Result<(), String> {
        if size > MAX_FILE_SIZE {
            return Err("file is too large".into());
        }
        let mut incoming = self.incoming.lock().unwrap_or_else(|e| e.into_inner());
        if incoming.contains_key(transfer_id) {
            return Err("transfer already exists".into());
        }
        if incoming.values().filter(|t| t.device_id == device_id).count() >= MAX_PER_DEVICE {
            return Err("too many transfers in progress".into());
        }
        fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
        if let Some(free) = free_space(dir)
            && free < size.saturating_add(FREE_SPACE_MARGIN)
        {
            return Err("not enough free space on the computer".into());
        }
        let name = safe_file_name(name);
        let part = dir.join(format!(".{name}.{transfer_id}.part"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .map_err(|e| format!("could not create {}: {e}", part.display()))?;
        incoming.insert(
            transfer_id.to_owned(),
            Incoming { device_id: device_id.to_owned(), name, dir: dir.to_owned(), part, file, size, received: 0, hasher: Sha256::new() },
        );
        Ok(())
    }

    /// Append the next piece; returns bytes received so far.
    pub fn chunk(&self, device_id: &str, transfer_id: &str, offset: u64, data: &str) -> Result<u64, String> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| "chunk is not valid base64".to_string())?;
        if bytes.len() > FILE_CHUNK_SIZE {
            return Err("chunk too large".into());
        }
        let mut incoming = self.incoming.lock().unwrap_or_else(|e| e.into_inner());
        let transfer = incoming
            .get_mut(transfer_id)
            .filter(|t| t.device_id == device_id)
            .ok_or_else(|| "unknown transfer".to_string())?;
        if offset != transfer.received {
            return Err(format!("expected offset {}", transfer.received));
        }
        if transfer.received + bytes.len() as u64 > transfer.size {
            return Err("more data than offered".into());
        }
        transfer.file.write_all(&bytes).map_err(|e| format!("could not write: {e}"))?;
        transfer.hasher.update(&bytes);
        transfer.received += bytes.len() as u64;
        Ok(transfer.received)
    }

    /// Verify and move the finished file to a free name; returns its path.
    pub fn complete(&self, device_id: &str, transfer_id: &str, sha256: &str) -> Result<PathBuf, String> {
        let transfer = {
            let mut incoming = self.incoming.lock().unwrap_or_else(|e| e.into_inner());
            match incoming.get(transfer_id) {
                Some(t) if t.device_id == device_id => incoming.remove(transfer_id).unwrap(),
                _ => return Err("unknown transfer".into()),
            }
        };
        let Incoming { name, dir, part, mut file, size, received, hasher, .. } = transfer;
        let fail = |reason: String| {
            let _ = fs::remove_file(&part);
            Err(reason)
        };
        if received != size {
            return fail(format!("received {received} of {size} bytes"));
        }
        if hex(&hasher.finalize()) != sha256.to_ascii_lowercase() {
            return fail("checksum mismatch".into());
        }
        if let Err(e) = file.flush().and_then(|()| file.sync_all()) {
            return fail(format!("could not save: {e}"));
        }
        drop(file);
        let target = free_path(&dir, &name);
        // Never replace an existing file: hard-link to the free name, then drop the part.
        match fs::hard_link(&part, &target) {
            Ok(()) => {
                let _ = fs::remove_file(&part);
                Ok(target)
            }
            Err(_) => match fs::rename(&part, &target) {
                Ok(()) => Ok(target),
                Err(e) => fail(format!("could not save: {e}")),
            },
        }
    }

    /// Register a file to offer; hashes it (blocking).
    pub fn register_outgoing(&self, device_id: &str, transfer_id: &str, path: &Path) -> Result<Outgoing, String> {
        let meta = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if !meta.is_file() {
            return Err(format!("{} is not a file", path.display()));
        }
        if meta.len() > MAX_FILE_SIZE {
            return Err(format!("{} is too large", path.display()));
        }
        let mut file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1024 * 1024];
        loop {
            let n = file.read(&mut buffer).map_err(|e| format!("{}: {e}", path.display()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        let outgoing = Outgoing {
            device_id: device_id.to_owned(),
            path: path.to_owned(),
            name: safe_file_name(&name),
            size: meta.len(),
            sha256: hex(&hasher.finalize()),
        };
        let mut map = self.outgoing.lock().unwrap_or_else(|e| e.into_inner());
        if map.values().filter(|t| t.device_id == device_id).count() >= MAX_PER_DEVICE * 4 {
            return Err("too many files waiting for this phone".into());
        }
        map.insert(transfer_id.to_owned(), outgoing.clone());
        Ok(outgoing)
    }

    /// Read the piece at `offset`; returns (base64, eof).
    pub fn pull(&self, device_id: &str, transfer_id: &str, offset: u64) -> Result<(String, bool), String> {
        let transfer = self
            .outgoing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(transfer_id)
            .filter(|t| t.device_id == device_id)
            .cloned()
            .ok_or_else(|| "unknown transfer".to_string())?;
        if offset > transfer.size {
            return Err("offset beyond end of file".into());
        }
        let mut file = File::open(&transfer.path).map_err(|e| format!("could not read file: {e}"))?;
        file.seek(SeekFrom::Start(offset)).map_err(|e| format!("could not read file: {e}"))?;
        let length = (transfer.size - offset).min(FILE_CHUNK_SIZE as u64) as usize;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes).map_err(|e| format!("could not read file: {e}"))?;
        Ok((base64::engine::general_purpose::STANDARD.encode(&bytes), offset + length as u64 >= transfer.size))
    }

    /// The phone stored an offered file (or the offer ended).
    pub fn finish_outgoing(&self, device_id: &str, transfer_id: &str) -> Option<Outgoing> {
        let mut map = self.outgoing.lock().unwrap_or_else(|e| e.into_inner());
        match map.get(transfer_id) {
            Some(t) if t.device_id == device_id => map.remove(transfer_id),
            _ => None,
        }
    }

    /// Cancel either direction; true if something was cancelled.
    pub fn cancel(&self, device_id: &str, transfer_id: &str) -> bool {
        let removed = {
            let mut incoming = self.incoming.lock().unwrap_or_else(|e| e.into_inner());
            match incoming.get(transfer_id) {
                Some(t) if t.device_id == device_id => incoming.remove(transfer_id),
                _ => None,
            }
        };
        if let Some(transfer) = removed {
            let _ = fs::remove_file(&transfer.part);
            return true;
        }
        self.finish_outgoing(device_id, transfer_id).is_some()
    }

    /// Device went away: delete its partial files and forget its offers.
    pub fn drop_device(&self, device_id: &str) {
        let parts: Vec<PathBuf> = {
            let mut incoming = self.incoming.lock().unwrap_or_else(|e| e.into_inner());
            let ids: Vec<String> = incoming.iter().filter(|(_, t)| t.device_id == device_id).map(|(id, _)| id.clone()).collect();
            ids.into_iter().filter_map(|id| incoming.remove(&id)).map(|t| t.part).collect()
        };
        for part in parts {
            let _ = fs::remove_file(part);
        }
        self.outgoing.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, t| t.device_id != device_id);
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Just a file name: no directories, control characters or hidden/dot names.
pub fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base.chars().map(|c| if c.is_control() { '_' } else { c }).collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim().to_string();
    let mut out = if cleaned.is_empty() { "file".to_string() } else { cleaned };
    while out.len() > 200 {
        out.pop();
    }
    out
}

/// `name`, or `name (1).ext`, `name (2).ext`... whichever does not exist.
pub fn free_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

/// The user's Downloads folder (XDG user dirs), created if missing.
pub fn downloads_dir() -> PathBuf {
    let configured = crate::host_settings::current().downloads_dir;
    if configured.starts_with('/') {
        return PathBuf::from(configured);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
    if let Ok(dirs) = fs::read_to_string(config.join("user-dirs.dirs")) {
        for line in dirs.lines() {
            if let Some(value) = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=") {
                let value = value.trim_matches('"').replace("$HOME", &home.to_string_lossy());
                if value.starts_with('/') {
                    return PathBuf::from(value);
                }
            }
        }
    }
    home.join("Downloads")
}

fn free_space(dir: &Path) -> Option<u64> {
    let stat = nix::sys::statvfs::statvfs(dir).ok()?;
    Some(stat.blocks_available() as u64 * stat.fragment_size() as u64)
}

fn received_description(path: &Path, device_name: &str) -> String {
    fn label(value: &str, limit: usize) -> String {
        // Neutralize markup for servers that interpret the description as HTML.
        crate::phone_notify::display_text(value, limit, false)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .map(|c| match c {
                '<' => '‹',
                '>' => '›',
                '&' => '＆',
                c => c,
            })
            .collect()
    }

    let name = path
        .file_name()
        .map_or_else(String::new, |name| label(&name.to_string_lossy(), 112));
    let device = label(device_name, 64);
    let name = if name.is_empty() { "Received file" } else { &name };
    let device = if device.is_empty() { "a paired phone" } else { &device };
    format!("{name}\nFrom {device}")
}

/// Shows a received file with Open and Show in Folder actions. Clicking the card reveals the file.
pub fn notify_received(path: PathBuf, device_name: String) {
    tokio::spawn(async move {
        let Ok(output) = tokio::process::Command::new("notify-send")
            .arg("--app-name=Phone Link")
            .arg("--icon=document-save")
            .arg("--action=default=Show in Folder")
            .arg("--action=open=Open")
            .arg("--action=folder=Show in Folder")
            .arg("--wait")
            .arg("File received")
            .arg(received_description(&path, &device_name))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
        else {
            return;
        };
        match String::from_utf8_lossy(&output.stdout).trim() {
            "open" => {
                let _ = tokio::process::Command::new("xdg-open").arg(&path).spawn();
            }
            "default" | "folder" => show_in_folder(&path).await,
            _ => {}
        }
    });
}

async fn show_in_folder(path: &Path) {
    let uri = format!("file://{}", uri_path(path));
    let shown = tokio::process::Command::new("gdbus")
        .args([
            "call", "--session", "--dest", "org.freedesktop.FileManager1",
            "--object-path", "/org/freedesktop/FileManager1",
            "--method", "org.freedesktop.FileManager1.ShowItems",
        ])
        .arg(format!("['{uri}']"))
        .arg("")
        .status()
        .await
        .is_ok_and(|s| s.success());
    if !shown && let Some(dir) = path.parent() {
        let _ = tokio::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

fn uri_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str()
        .as_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn names_cannot_escape_or_hide() {
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("..\\x.txt"), "x.txt");
        assert_eq!(safe_file_name(".bashrc"), "bashrc");
        assert_eq!(safe_file_name("  "), "file");
        assert_eq!(safe_file_name("a\nb.jpg"), "a_b.jpg");
        assert!(safe_file_name(&"x".repeat(400)).len() <= 200);
    }

    #[test]
    fn received_file_displays_a_bounded_literal_label() {
        let path = PathBuf::from(format!("/custom location/{}<a>.jpg", "é".repeat(110)));
        let description = received_description(&path, "Photo\n<Phone> & Tablet\u{202e}");
        assert_eq!(
            description,
            format!("{}‹…\nFrom Photo ‹Phone› ＆ Tablet", "é".repeat(110))
        );
    }

    #[test]
    fn receives_verifies_and_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("photo.jpg"), b"existing").unwrap();
        let t = Transfers::default();
        let data = b"hello phone file";
        t.offer("phone", "transfer-1", "photo.jpg", data.len() as u64, dir.path()).unwrap();
        assert!(t.chunk("other", "transfer-1", 0, &b64(data)).is_err(), "other devices cannot write");
        assert!(t.chunk("phone", "transfer-1", 3, &b64(data)).is_err(), "offsets are sequential");
        assert_eq!(t.chunk("phone", "transfer-1", 0, &b64(&data[..5])).unwrap(), 5);
        assert_eq!(t.chunk("phone", "transfer-1", 5, &b64(&data[5..])).unwrap(), data.len() as u64);
        let sha = hex(&Sha256::digest(data));
        let saved = t.complete("phone", "transfer-1", &sha).unwrap();
        assert_eq!(saved, dir.path().join("photo (1).jpg"));
        assert_eq!(fs::read(&saved).unwrap(), data);
        assert_eq!(fs::read(dir.path().join("photo.jpg")).unwrap(), b"existing");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2, "no part files left");
    }

    #[test]
    fn bad_checksum_and_disconnect_remove_partial_files() {
        let dir = tempfile::tempdir().unwrap();
        let t = Transfers::default();
        t.offer("phone", "transfer-2", "a.txt", 3, dir.path()).unwrap();
        t.chunk("phone", "transfer-2", 0, &b64(b"abc")).unwrap();
        assert!(t.complete("phone", "transfer-2", &"0".repeat(64)).is_err());
        t.offer("phone", "transfer-3", "b.txt", 10, dir.path()).unwrap();
        assert!(t.chunk("phone", "transfer-3", 0, &b64(&[0; 11])).is_err(), "no more than offered");
        t.drop_device("phone");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn outgoing_files_are_pulled_in_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let data: Vec<u8> = (0..FILE_CHUNK_SIZE * 2 + 10).map(|i| (i % 251) as u8).collect();
        fs::write(&path, &data).unwrap();
        let t = Transfers::default();
        let offer = t.register_outgoing("phone", "transfer-4", &path).unwrap();
        assert_eq!(offer.sha256, hex(&Sha256::digest(&data)));
        assert!(t.pull("other", "transfer-4", 0).is_err());
        let mut received = Vec::new();
        loop {
            let (chunk, eof) = t.pull("phone", "transfer-4", received.len() as u64).unwrap();
            received.extend(base64::engine::general_purpose::STANDARD.decode(chunk).unwrap());
            if eof {
                break;
            }
        }
        assert_eq!(received, data);
        assert!(t.finish_outgoing("phone", "transfer-4").is_some());
        assert!(t.pull("phone", "transfer-4", 0).is_err());
    }
}
