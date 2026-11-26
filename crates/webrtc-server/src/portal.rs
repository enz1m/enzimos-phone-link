use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid};
use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, ReturnFlags, recvmsg};
use serde::Deserialize;
use std::io::IoSliceMut;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::{net::UnixStream, process::CommandExt};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Deserialize)]
struct Reply {
    node_id: Option<u32>,
    #[serde(default)]
    serial: u64,
    error: Option<String>,
}

pub struct PortalCapture {
    helper: Child,
    control: UnixStream,
    remote: Option<OwnedFd>,
    pub target: String,
    _runtime: tempfile::TempDir,
}

impl PortalCapture {
    pub fn start() -> Result<Self, String> {
        let helper = enzremote_webrtc_ipc::layout::helper("ENZREMOTE_PORTAL_HELPER", "portal-session-helper");
        Self::start_with_helper(&helper, Duration::from_secs(8))
    }

    fn start_with_helper(helper: &std::path::Path, timeout: Duration) -> Result<Self, String> {
        let runtime_root = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let runtime = tempfile::Builder::new()
            .prefix("enzremote-capture-")
            .tempdir_in(runtime_root)
            .map_err(|e| format!("Could not create private portal runtime: {e}"))?;
        let (control, child_control) = UnixStream::pair().map_err(|e| e.to_string())?;
        control
            .set_read_timeout(Some(timeout))
            .map_err(|e| e.to_string())?;
        let child_control = OwnedFd::from(child_control);
        let mut attempts = 0;
        let child = loop {
            let stdin = child_control
                .try_clone()
                .map_err(|e| format!("Could not launch private portal helper: {e}"))?;
            match Command::new(helper)
                .env("ENZREMOTE_PORTAL_RUNTIME_DIR", runtime.path())
                .stdin(Stdio::from(stdin))
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .process_group(0)
                .spawn()
            {
                // A child forked elsewhere may briefly hold a just-written helper open for
                // writing until it execs; exec then fails with "text file busy".
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 20 => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => {
                    break result.map_err(|e| format!("Could not launch private portal helper: {e}"))?;
                }
            }
        };
        drop(child_control);
        let mut capture = Self {
            helper: child,
            control,
            remote: None,
            target: String::new(),
            _runtime: runtime,
        };
        let (reply, mut fds) = receive_reply(&capture.control)?;
        if let Some(error) = reply.error {
            return Err(error);
        }
        if fds.len() != 1 {
            return Err("Portal did not return exactly one restricted PipeWire descriptor".into());
        }
        capture.remote = fds.pop();
        let node = reply.node_id.ok_or("Portal returned no video node")?;
        capture.target = if reply.serial != 0 {
            reply.serial.to_string()
        } else {
            node.to_string()
        };
        eprintln!(
            "capture: private PipeWire portal ready (node={node}, serial={})",
            reply.serial
        );
        Ok(capture)
    }

    /// Borrowed capability; `pipewiresrc` duplicates it rather than taking ownership.
    pub fn remote_fd(&self) -> Option<RawFd> {
        self.remote.as_ref().map(AsRawFd::as_raw_fd)
    }

    fn exited_without_reaping(&self) -> nix::Result<bool> {
        let pid = i32::try_from(self.helper.id()).map_err(|_| nix::errno::Errno::EINVAL)?;
        waitid(
            Id::Pid(nix::unistd::Pid::from_raw(pid)),
            WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
        )
        .map(|status| !matches!(status, WaitStatus::StillAlive))
    }

    pub fn check_health(&self) -> Result<(), String> {
        match self.exited_without_reaping() {
            Ok(false) => Ok(()),
            Ok(true) => Err("Private portal helper exited".into()),
            Err(error) => Err(format!("Could not inspect private portal helper: {error}")),
        }
    }
}

fn receive_reply(control: &UnixStream) -> Result<(Reply, Vec<OwnedFd>), String> {
    let mut bytes = [0u8; 4096];
    let mut storage = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
    let mut ancillary = RecvAncillaryBuffer::new(&mut storage);
    let mut io = [IoSliceMut::new(&mut bytes)];
    let message = recvmsg(control, &mut io, &mut ancillary, RecvFlags::CMSG_CLOEXEC)
        .map_err(|e| format!("Portal descriptor handshake failed: {e}"))?;
    if message
        .flags
        .intersects(ReturnFlags::CTRUNC | ReturnFlags::TRUNC)
    {
        return Err("Portal descriptor handshake was truncated".into());
    }
    let reply = serde_json::from_slice::<Reply>(&bytes[..message.bytes])
        .map_err(|e| format!("Invalid portal handshake: {e}"))?;
    let mut fds = Vec::new();
    for item in ancillary.drain() {
        if let RecvAncillaryMessage::ScmRights(received) = item {
            fds.extend(received);
        }
    }
    Ok((reply, fds))
}

impl Drop for PortalCapture {
    fn drop(&mut self) {
        self.remote.take();
        let _ = self.control.shutdown(Shutdown::Both);
        let deadline = Instant::now() + Duration::from_millis(700);
        let exited = loop {
            match self.exited_without_reaping() {
                Ok(true) => break Some(true),
                Err(nix::errno::Errno::EINTR) => {}
                Err(_) => break None,
                Ok(false) => {}
            }
            if Instant::now() >= deadline {
                break Some(false);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // process_group(0) isolates the helper. WNOWAIT reserves its leader ID
        // until group cleanup, including when a crash leaves descendants alive.
        if let Some(exited) = exited
            && let Ok(pid) = i32::try_from(self.helper.id())
        {
            let group = nix::unistd::Pid::from_raw(pid);
            if !exited {
                let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGTERM);
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
        }
        let _ = self.helper.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
    use std::io::{IoSlice, Read, Write};

    fn helper_script(tag: &str, source: &str) -> std::path::PathBuf {
        use std::os::unix::fs::OpenOptionsExt;
        let path =
            std::env::temp_dir().join(format!("enzremote-portal-{tag}-{}.py", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&path)
            .unwrap();
        writeln!(file, "#!/usr/bin/env python3\n{source}").unwrap();
        path
    }

    #[test]
    fn accepts_restricted_descriptor_and_serial() {
        let (a, b) = UnixStream::pair().unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        let payload = br#"{"node_id":42,"serial":1234}"#;
        sendmsg::<()>(
            b.as_raw_fd(),
            &[IoSlice::new(payload)],
            &[ControlMessage::ScmRights(&[file.as_raw_fd()])],
            MsgFlags::empty(),
            None,
        )
        .unwrap();
        let (reply, fds) = receive_reply(&a).unwrap();
        assert_eq!(reply.node_id, Some(42));
        assert_eq!(reply.serial, 1234);
        assert_eq!(fds.len(), 1);
        assert!(
            rustix::io::fcntl_getfd(&fds[0])
                .unwrap()
                .contains(rustix::io::FdFlags::CLOEXEC)
        );
        drop(fds);
    }
    #[test]
    fn drop_closes_capability_and_reaps_helper() {
        let path = helper_script(
            "close",
            "import socket,os,array\ns=socket.socket(fileno=0)\nf=os.open('/dev/null',os.O_RDONLY)\ns.sendmsg([b'{\"node_id\":42,\"serial\":99}'],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,array.array('i',[f]))])\nos.close(f)\ns.recv(1)",
        );
        let capture = PortalCapture::start_with_helper(&path, Duration::from_secs(2)).unwrap();
        let pid = capture.helper.id();
        assert_eq!(capture.target, "99");
        assert!(capture.remote_fd().is_some());
        drop(capture);
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn malformed_and_truncated_handshakes_close_received_descriptors() {
        for (payload, count) in [(b"invalid JSON".as_slice(), 1), (b"{}".as_slice(), 8)] {
            let (a, b) = UnixStream::pair().unwrap();
            let (mut probe, sent) = UnixStream::pair().unwrap();
            let rights = vec![sent.as_raw_fd(); count];
            sendmsg::<()>(
                b.as_raw_fd(),
                &[IoSlice::new(payload)],
                &[ControlMessage::ScmRights(&rights)],
                MsgFlags::empty(),
                None,
            )
            .unwrap();
            drop(sent);
            assert!(receive_reply(&a).is_err());
            probe
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            assert_eq!(probe.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn startup_timeout_reaps_unresponsive_helper() {
        let path = helper_script("timeout", "import time; time.sleep(60)");
        let started = Instant::now();
        assert!(PortalCapture::start_with_helper(&path, Duration::from_millis(100)).is_err());
        assert!(started.elapsed() < Duration::from_secs(3));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn crashed_leader_does_not_leave_its_process_group_alive() {
        let path = helper_script(
            "orphan",
            r#"import array,json,os,socket,subprocess,sys
p=subprocess.Popen([sys.executable,'-c','import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); time.sleep(60)'])
s=socket.socket(fileno=0)
f=os.open('/dev/null',os.O_RDONLY)
s.sendmsg([json.dumps({'node_id':p.pid}).encode()],[(socket.SOL_SOCKET,socket.SCM_RIGHTS,array.array('i',[f]))])
os.close(f)
s.recv(1)
"#,
        );
        let capture = PortalCapture::start_with_helper(&path, Duration::from_secs(2)).unwrap();
        let helper = capture.helper.id();
        let runtime = capture._runtime.path().to_path_buf();
        let descendant = capture.target.parse::<u32>().unwrap();
        assert!(capture.check_health().is_ok());
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(helper as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while capture.check_health().is_ok() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(capture.check_health().is_err());
        drop(capture);
        let running = || {
            std::fs::read_to_string(format!("/proc/{descendant}/stat")).is_ok_and(|stat| {
                !stat
                    .rsplit_once(") ")
                    .is_some_and(|(_, fields)| fields.starts_with('Z'))
            })
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!running(), "portal descendant survived its crashed leader");
        assert!(!std::path::Path::new(&format!("/proc/{helper}")).exists());
        assert!(!runtime.exists());
        std::fs::remove_file(path).unwrap();
    }
}
