use nix::cmsg_space;
use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, send, sendmsg};
use nix::unistd::close;
use std::fs;
use std::io::{IoSlice, IoSliceMut, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::time::Duration;

/// File descriptor inherited from sessiond for the control channel.
/// This is fd 0 (the parent's end of the socketpair) with CLOEXEC cleared.
const CONTROL_FD: RawFd = 0;

/// Run the EIS (Emulated Input Subsystem) broker.
///
/// Accepts connections on a Unix socket and forwards file descriptors
/// from the compositor's control channel (fd 0) to each EIS client
/// via SCM_RIGHTS.
pub fn run_eis_broker(socket: &Path) -> Result<(), String> {
    let control_fd = CONTROL_FD;

    // Validate that fd 0 is a Unix socket (not stdin/terminal).
    // Use send() with empty data as a socket probe — it will fail with
    // ENOTSOCK if fd 0 is not a socket.
    match send(control_fd, &[], MsgFlags::MSG_DONTWAIT) {
        Err(nix::errno::Errno::ENOTSOCK) => {
            return Err(
                "fd 0 is not a Unix socket. The internal-eis-broker command must be launched by sessiond, not run directly.".into(),
            );
        }
        // EAGAIN/EWOULDBLOCK is fine — it means it IS a socket.
        Err(nix::errno::Errno::EAGAIN) | Ok(_) => {}
        Err(e) => {
            return Err(format!(
                "fd 0 probe failed: {e}. This command must be launched by sessiond, not run directly."
            ));
        }
    }

    let probe = request_eis_fd(control_fd)?;
    close(probe).map_err(|error| format!("could not close probe descriptor: {error}"))?;

    if let Err(error) = fs::remove_file(socket)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(format!(
            "could not remove stale broker socket {}: {error}",
            socket.display()
        ));
    }
    let listener = UnixListener::bind(socket)
        .map_err(|error| format!("could not bind {}: {error}", socket.display()))?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("could not restrict {}: {error}", socket.display()))?;

    for connection in listener.incoming() {
        let mut connection = match connection {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("accept failed: {error}");
                continue;
            }
        };
        if let Err(error) = connection.set_read_timeout(Some(Duration::from_secs(2))) {
            eprintln!("could not set broker timeout: {error}");
            continue;
        }
        let mut request = [0_u8; 1];
        if let Err(error) = connection.read_exact(&mut request) {
            eprintln!("rejected incomplete EIS broker request: {error}");
            continue;
        }
        if request[0] != b'E' {
            continue;
        }
        let client_fd = match request_eis_fd(control_fd) {
            Ok(fd) => fd,
            Err(error) => {
                // The compositor control channel is broken; the broker
                // cannot serve further requests.
                return Err(error);
            }
        };
        let bytes = *b"E";
        let iov = [IoSlice::new(&bytes)];
        let raw_fd = client_fd;
        let sent = sendmsg::<()>(
            connection.as_raw_fd(),
            &iov,
            &[ControlMessage::ScmRights(&[raw_fd])],
            MsgFlags::empty(),
            None,
        );
        let _ = close(client_fd);
        if let Err(error) = sent {
            eprintln!("could not send EIS descriptor: {error}");
        }
    }
    Ok(())
}

fn request_eis_fd(control_fd: RawFd) -> Result<RawFd, String> {
    send(control_fd, b"E", MsgFlags::empty())
        .map_err(|error| format!("could not request EIS descriptor: {error}"))?;

    let mut byte = [0_u8; 1];
    let mut iov = [IoSliceMut::new(&mut byte)];
    let mut control_messages = cmsg_space!([RawFd; 1]);
    let message = recvmsg::<()>(
        control_fd,
        &mut iov,
        Some(&mut control_messages),
        MsgFlags::MSG_CMSG_CLOEXEC,
    )
    .map_err(|error| format!("could not receive EIS descriptor: {error}"))?;
    let response_bytes = message.bytes;
    let mut received_fd = None;
    for message in message
        .cmsgs()
        .map_err(|error| format!("invalid EIS control message: {error}"))?
    {
        if let ControlMessageOwned::ScmRights(fds) = message {
            for fd in fds {
                if received_fd.is_none() {
                    received_fd = Some(fd);
                } else {
                    // Close unexpected extra descriptors.
                    let _ = close(fd);
                }
            }
        }
    }
    if response_bytes != 1 || byte[0] != b'E' {
        if let Some(fd) = received_fd {
            let _ = close(fd);
        }
        return Err("compositor returned an invalid EIS broker response".into());
    }
    received_fd.ok_or_else(|| "compositor response did not include an EIS descriptor".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::unix::net::UnixStream;
    use std::thread;

    #[test]
    fn receives_descriptor_from_compositor_channel() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server_thread = thread::spawn(move || {
            let mut request = [0_u8; 1];
            server.read_exact(&mut request).unwrap();
            assert_eq!(request, [b'E']);

            let file = File::open("/dev/null").unwrap();
            let payload = *b"E";
            let iov = [IoSlice::new(&payload)];
            sendmsg::<()>(
                server.as_raw_fd(),
                &iov,
                &[ControlMessage::ScmRights(&[file.as_raw_fd()])],
                MsgFlags::empty(),
                None,
            )
            .unwrap();
        });

        let received = request_eis_fd(client.as_raw_fd()).unwrap();
        assert!(received >= 0);
        close(received).unwrap();
        server_thread.join().unwrap();
    }
}
