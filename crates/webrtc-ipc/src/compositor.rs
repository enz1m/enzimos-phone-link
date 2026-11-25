//! Requests on a Hyprland instance's control socket.
//!
//! The compositor admits control requests by the socket peer's executable, so
//! they must come from the admitted process itself, not from a `hyprctl` child.

use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_REPLY: u64 = 16 << 20;

/// Why a compositor request did not produce a reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The instance name or socket is invalid, or the socket could not be used.
    Unavailable(String),
    /// No complete reply arrived before the deadline.
    TimedOut,
    /// The compositor answered with an `error:` reply; holds its first line.
    Rejected(String),
    /// The reply was not UTF-8 or exceeded 16 MiB.
    InvalidReply,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(formatter, "compositor is unavailable: {reason}"),
            Self::TimedOut => formatter.write_str("compositor did not respond in time"),
            Self::Rejected(reason) if reason.is_empty() => {
                formatter.write_str("compositor rejected the request")
            }
            Self::Rejected(reason) => {
                write!(formatter, "compositor rejected the request: {reason}")
            }
            Self::InvalidReply => formatter.write_str("invalid compositor reply"),
        }
    }
}

impl std::error::Error for Error {}

/// One Hyprland instance's request socket, owned by the current user.
#[derive(Debug, Clone)]
pub struct Compositor {
    socket: PathBuf,
}

impl Compositor {
    /// Selects `instance` under `$XDG_RUNTIME_DIR/hypr`, or `/run/user/<uid>/hypr`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unavailable`] for an invalid instance name.
    pub fn for_instance(instance: &str) -> Result<Self, Error> {
        let runtime = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(runtime) if !runtime.is_empty() => PathBuf::from(runtime),
            _ => PathBuf::from(format!("/run/user/{}", own_uid()?)),
        };
        Self::in_runtime(&runtime, instance)
    }

    /// Selects `instance` under `runtime/hypr`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unavailable`] for a relative runtime directory or an
    /// invalid instance name.
    pub fn in_runtime(runtime: &Path, instance: &str) -> Result<Self, Error> {
        if !runtime.is_absolute() {
            return Err(Error::Unavailable(
                "runtime directory is not absolute".into(),
            ));
        }
        if instance.is_empty()
            || instance.len() > 128
            || !instance
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err(Error::Unavailable("invalid instance name".into()));
        }
        Ok(Self {
            socket: runtime.join("hypr").join(instance).join(".socket.sock"),
        })
    }

    /// Sends `command` (for example `eval <code>` or `output create headless NAME`)
    /// and returns the reply text.
    ///
    /// Writing the request and reading the reply share `timeout`.
    ///
    /// # Errors
    ///
    /// See [`Error`].
    pub fn command(&self, command: &str, timeout: Duration) -> Result<String, Error> {
        self.send(&format!("/{command}"), timeout)
    }

    /// Sends a read-only query such as `monitors` and returns its JSON reply.
    ///
    /// Writing the request and reading the reply share `timeout`.
    ///
    /// # Errors
    ///
    /// See [`Error`].
    pub fn query_json(&self, query: &str, timeout: Duration) -> Result<String, Error> {
        self.send(&format!("j/{query}"), timeout)
    }

    fn send(&self, line: &str, timeout: Duration) -> Result<String, Error> {
        let deadline = Instant::now() + timeout;
        let metadata = std::fs::symlink_metadata(&self.socket)
            .map_err(|error| Error::Unavailable(error.to_string()))?;
        if !metadata.file_type().is_socket() || metadata.uid() != own_uid()? {
            return Err(Error::Unavailable(
                "control socket belongs to another user".into(),
            ));
        }
        let mut stream = UnixStream::connect(&self.socket)
            .map_err(|error| Error::Unavailable(error.to_string()))?;
        stream
            .set_write_timeout(Some(remaining(deadline)?))
            .map_err(|error| Error::Unavailable(error.to_string()))?;
        stream.write_all(line.as_bytes()).map_err(io_error)?;
        let mut reply = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            stream
                .set_read_timeout(Some(remaining(deadline)?))
                .map_err(|error| Error::Unavailable(error.to_string()))?;
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    reply.extend_from_slice(&chunk[..read]);
                    if reply.len() as u64 > MAX_REPLY {
                        return Err(Error::InvalidReply);
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        let reply = String::from_utf8(reply).map_err(|_| Error::InvalidReply)?;
        if let Some(reason) = reply.strip_prefix("error:") {
            return Err(Error::Rejected(
                reason
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or_default()
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(240)
                    .collect(),
            ));
        }
        Ok(reply)
    }
}

fn remaining(deadline: Instant) -> Result<Duration, Error> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|left| !left.is_zero())
        .ok_or(Error::TimedOut)
}

fn io_error(error: std::io::Error) -> Error {
    match error.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => Error::TimedOut,
        _ => Error::Unavailable(error.to_string()),
    }
}

fn own_uid() -> Result<u32, Error> {
    std::fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .map_err(|error| Error::Unavailable(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::thread::JoinHandle;

    fn serve(
        reply: &'static [u8],
        delay: Duration,
    ) -> (tempfile::TempDir, Compositor, JoinHandle<String>) {
        let runtime = tempfile::tempdir().unwrap();
        let directory = runtime.path().join("hypr/instance_1");
        std::fs::create_dir_all(&directory).unwrap();
        let listener = UnixListener::bind(directory.join(".socket.sock")).unwrap();
        let compositor = Compositor::in_runtime(runtime.path(), "instance_1").unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let read = stream.read(&mut request).unwrap();
            std::thread::sleep(delay);
            let _ = stream.write_all(reply);
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        (runtime, compositor, server)
    }

    #[test]
    fn commands_and_queries_use_the_socket_request_line() {
        let (_runtime, compositor, server) = serve(b"ok", Duration::ZERO);
        assert_eq!(
            compositor.command("output create headless ENZREMOTE", Duration::from_secs(2)),
            Ok("ok".into())
        );
        assert_eq!(server.join().unwrap(), "/output create headless ENZREMOTE");

        let (_runtime, compositor, server) = serve(b"[]", Duration::ZERO);
        assert_eq!(
            compositor.query_json("monitors", Duration::from_secs(2)),
            Ok("[]".into())
        );
        assert_eq!(server.join().unwrap(), "j/monitors");
    }

    #[test]
    fn error_replies_are_rejections_with_their_first_line() {
        let (_runtime, compositor, server) = serve(
            b"error: compositor control is limited to the desktop shell\nmore",
            Duration::ZERO,
        );
        assert_eq!(
            compositor.command("eval return 1", Duration::from_secs(2)),
            Err(Error::Rejected(
                "compositor control is limited to the desktop shell".into()
            ))
        );
        server.join().unwrap();
    }

    #[test]
    fn a_silent_compositor_times_out_at_the_deadline() {
        let (_runtime, compositor, server) = serve(b"ok", Duration::from_millis(600));
        let started = Instant::now();
        assert_eq!(
            compositor.command("eval return 1", Duration::from_millis(150)),
            Err(Error::TimedOut)
        );
        assert!(started.elapsed() < Duration::from_millis(500));
        server.join().unwrap();
    }

    #[test]
    fn instance_names_cannot_leave_the_runtime_directory() {
        for instance in ["", "../other", "a/b", "a b"] {
            assert!(Compositor::in_runtime(Path::new("/run/user/1000"), instance).is_err());
        }
        assert!(Compositor::in_runtime(Path::new("relative"), "instance").is_err());
    }

    #[test]
    fn a_missing_socket_is_unavailable() {
        let runtime = tempfile::tempdir().unwrap();
        let compositor = Compositor::in_runtime(runtime.path(), "absent").unwrap();
        assert!(matches!(
            compositor.command("eval return 1", Duration::from_secs(1)),
            Err(Error::Unavailable(_))
        ));
    }
}
