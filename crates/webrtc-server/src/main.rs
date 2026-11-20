#![forbid(unsafe_code)]
//! enzremote-webrtc-server: per-session WebRTC pipeline process.
//!
//! Launched by the signaling server for each active streaming session.
//! Communicates via a Unix socket using line-delimited JSON IPC.
//!
//! # Usage
//!
//!     enzremote-webrtc-server <ipc-socket-path> [options]
//!
//! # Environment Variables
//!
//! - `ENZREMOTE_ENCODER`: `auto`, `vaapi`, `x264`, or `openh264` (default: `auto`)
//! - `ENZREMOTE_VIDEO_WIDTH`: test source width (default: 390)
//! - `ENZREMOTE_VIDEO_HEIGHT`: test source height (default: 844)
//! - `ENZREMOTE_VIDEO_FPS`: test source / grim capture framerate (default: 30)
//! - `ENZREMOTE_PIPEWIRE_TARGET`: PipeWire target object for capture
//!   (e.g., portal node ID). When set with a VA-API encoder, enables
//!   DMA-BUF zero-copy capture through `vapostproc`.
//! - `ENZREMOTE_PIPEWIRE_FD`: PipeWire remote fd from portal's
//!   `OpenPipeWireRemote` (optional, for sandboxed access).
//! - `ENZREMOTE_GRIM_DISPLAY`: Wayland display name for grim screen capture
//!   (e.g., `wayland-2`). When set, captures from the specified Wayland
//!   compositor using `grim` and wlr-screencopy protocol.
//! - `ENZREMOTE_EIS_BROKER_FD`: inherited EIS broker file descriptor
//! - `ENZREMOTE_INPUT_EIS_HELPER`: path to the EIS input helper binary

use enzremote_webrtc_server::ipc::{FromWebRtc, ToWebRtc, parse_to_webrtc, serialize_message};
use enzremote_webrtc_server::pipeline::{H264Encoder, InputHandler, VideoSource, WebRtcPipeline};
use std::env;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::thread;

/// Send a `FromWebRtc` message over the IPC connection.
fn send_ipc(stream: &Mutex<Option<UnixStream>>, msg: FromWebRtc) {
    let mut guard = stream.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ref mut conn) = *guard {
        let json = match serialize_message(&msg) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("webrtc-server: serialize error: {e}");
                return;
            }
        };
        if json.len() > enzremote_webrtc_ipc::MAX_IPC_MESSAGE_SIZE {
            eprintln!(
                "webrtc-server: IPC message too large ({} bytes, max {}), dropping",
                json.len(),
                enzremote_webrtc_ipc::MAX_IPC_MESSAGE_SIZE
            );
            return;
        }
        if let Err(e) = writeln!(conn, "{json}") {
            eprintln!("webrtc-server: IPC write error: {e}");
            *guard = None;
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: enzremote-webrtc-server <ipc-socket-path>");
        std::process::exit(1);
    }
    let socket_path = &args[1];

    // Clean up stale socket file.
    let _ = std::fs::remove_file(socket_path);

    // Bind the Unix socket before signaling readiness.
    let listener = UnixListener::bind(socket_path)
        .map_err(|e| format!("failed to bind IPC socket at {socket_path}: {e}"))?;

    // Restrict socket permissions so only the owning user can connect.
    // Without this, any local user could inject IPC messages.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600));
    }

    let capture_mode = env::var("ENZREMOTE_CAPTURE_MODE").unwrap_or_else(|_| "auto".into());
    if !["auto", "pipewire", "screencopy"].contains(&capture_mode.as_str()) {
        return Err("ENZREMOTE_CAPTURE_MODE must be auto, pipewire, or screencopy".into());
    }
    let named_output = env::var_os("ENZREMOTE_CAPTURE_OUTPUT").is_some();
    if named_output && capture_mode == "screencopy" {
        return Err(
            "Named-output capture requires scoped PipeWire; unscoped CPU fallback is disabled"
                .into(),
        );
    }
    if named_output && env::var_os("ENZREMOTE_PIPEWIRE_TARGET").is_some() {
        return Err("Named-output capture cannot use an explicit PipeWire target override".into());
    }
    if capture_mode == "pipewire"
        && env::var_os("ENZREMOTE_CAPTURE_OUTPUT").is_none()
        && env::var_os("ENZREMOTE_PIPEWIRE_TARGET").is_none()
    {
        return Err("PipeWire capture requires a session output or explicit target".into());
    }
    // Retain the portal's restricted descriptor and helper until the media pipeline stops.
    let portal = if capture_mode != "screencopy"
        && env::var_os("ENZREMOTE_CAPTURE_OUTPUT").is_some()
        && env::var_os("ENZREMOTE_PIPEWIRE_TARGET").is_none()
    {
        Some(Arc::new(
            enzremote_webrtc_server::portal::PortalCapture::start()?,
        ))
    } else {
        None
    };
    let source = if let Some(capture) = &portal {
        VideoSource::PipeWire {
            target_object: Some(capture.target.clone()),
            pipewire_fd: capture.remote_fd(),
        }
    } else if let Ok(target) = env::var("ENZREMOTE_PIPEWIRE_TARGET") {
        // Optional: PipeWire remote fd from portal's OpenPipeWireRemote.
        // Allows connecting to a specific PipeWire instance rather than
        // the default daemon.
        let pipewire_fd: Option<i32> = env::var("ENZREMOTE_PIPEWIRE_FD")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|fd| *fd >= 0);
        if let Some(fd) = pipewire_fd {
            eprintln!("webrtc-server: using PipeWire fd={fd}");
        }
        let target_object = if target.is_empty() {
            None
        } else {
            Some(target)
        };
        if let Some(ref t) = target_object {
            eprintln!("webrtc-server: using PipeWire target-object={t}");
        }
        VideoSource::PipeWire {
            target_object,
            pipewire_fd,
        }
    } else if let Ok(display) = env::var("ENZREMOTE_GRIM_DISPLAY") {
        if display.is_empty() {
            return Err("ENZREMOTE_GRIM_DISPLAY is set but empty".into());
        }
        let fps: u32 = env::var("ENZREMOTE_VIDEO_FPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        eprintln!("webrtc-server: using grim capture on {display} at {fps} fps");
        VideoSource::GrimCapture {
            wayland_display: display,
            output: env::var_os("ENZREMOTE_CAPTURE_OUTPUT")
                .map(|value| value.to_string_lossy().into_owned()),
            framerate: fps,
        }
    } else {
        // No capture source specified — use test source.
        let width: u32 = env::var("ENZREMOTE_VIDEO_WIDTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(544);
        let height: u32 = env::var("ENZREMOTE_VIDEO_HEIGHT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1216);
        let fps: u32 = env::var("ENZREMOTE_VIDEO_FPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        VideoSource::Test {
            width,
            height,
            framerate: fps,
        }
    };

    // Initialize GStreamer early — auto_detect() probes element factories.
    gst::init().unwrap_or_else(|e| {
        eprintln!("webrtc-server: GStreamer init failed: {e}");
        std::process::exit(1);
    });

    let encoder = match env::var("ENZREMOTE_ENCODER")
        .unwrap_or_else(|_| "auto".into())
        .as_str()
    {
        "vaapi" => H264Encoder::VaApi,
        "x264" => H264Encoder::X264,
        "openh264" => H264Encoder::OpenH264,
        "hevc" | "h265" => H264Encoder::HevcVaApi,
        _ => H264Encoder::auto_detect(),
    };

    // Shared IPC connection handle for the pipeline's IPC sender callback.
    let ipc_conn: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));

    // Create the pipeline with an IPC sender that writes to the connection.
    let conn_for_sender = ipc_conn.clone();
    let ipc_sender = Box::new(move |msg: FromWebRtc| {
        send_ipc(&conn_for_sender, msg);
    });

    // A borrowed desktop seat must never fall through to global input injection.
    let input_handler = if std::env::var("ENZREMOTE_NO_INPUT").as_deref() == Ok("1") {
        // Input for this stream travels another way (second display).
        None
    } else if std::env::var_os("ENZREMOTE_DESKTOP_INPUT_SOCKET").is_some() {
        if std::env::var_os("ENZREMOTE_INPUT_WL_HELPER").is_none()
            || std::env::var("ENZREMOTE_INPUT_WORKER_UNICODE").as_deref() != Ok("1")
        {
            return Err("Scoped desktop input configuration is incomplete".into());
        }
        let bridge = enzremote_webrtc_server::wayland_input::WaylandInputBridge::from_environment()
            .ok_or("Scoped desktop input helper is unavailable")?;
        Some(InputHandler::Wayland(bridge))
    } else if std::env::var("ENZREMOTE_FULL_DESKTOP").as_deref() == Ok("1") {
        if std::env::var("ENZREMOTE_INPUT_OUTPUT")
            .ok()
            .filter(|name| !name.is_empty())
            .is_none()
        {
            return Err("Desktop input output is missing".into());
        }
        let bridge = enzremote_webrtc_server::wayland_input::WaylandInputBridge::from_environment()
            .ok_or("Desktop input helper is unavailable")?;
        Some(InputHandler::Wayland(bridge))
    } else if let Some(eis_bridge) =
        enzremote_webrtc_server::input_bridge::InputBridge::from_environment()
    {
        eprintln!("webrtc-server: EIS input bridge available");
        Some(InputHandler::Eis(eis_bridge))
    } else if let Some(wl_bridge) =
        enzremote_webrtc_server::wayland_input::WaylandInputBridge::from_environment()
    {
        eprintln!("webrtc-server: Wayland worker input bridge available");
        Some(InputHandler::Wayland(wl_bridge))
    } else {
        eprintln!(
            "webrtc-server: no input bridge available (no EIS broker fd, no WAYLAND_DISPLAY)"
        );
        None
    };

    let pipeline = WebRtcPipeline::new(source, encoder, ipc_sender, input_handler)?;

    // Print readiness on stdout (parent process reads this).
    // Explicit flush is required because stdout is block-buffered when
    // connected to a pipe (which it will be when launched by the
    // signaling server).
    println!("READY {socket_path}");
    let _ = std::io::stdout().flush();

    // Accept one signaling connection (blocking).
    // The signaling server connects after seeing READY.
    listener
        .set_nonblocking(false)
        .map_err(|e| format!("failed to set blocking mode: {e}"))?;

    let (stream, _addr) = listener.accept().map_err(|e| {
        let _ = std::fs::remove_file(socket_path);
        format!("failed to accept IPC connection: {e}")
    })?;

    // Drop the listener — we only accept one connection. Prevents spurious
    // connection attempts and frees the bound socket path for reuse on crash.
    drop(listener);

    // Store the connection for the IPC sender.
    let reader_stream = stream
        .try_clone()
        .map_err(|e| format!("failed to clone stream: {e}"))?;
    {
        let mut guard = ipc_conn.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(stream);
    }

    // Start the pipeline (triggers offer generation).
    pipeline.start()?;

    // Read IPC messages from the signaling server in a background thread.
    let pipeline_for_reader = pipeline.clone();
    let reader_handle = thread::spawn(move || {
        let mut reader = BufReader::new(reader_stream);
        let mut line_buf = String::new();
        loop {
            line_buf.clear();
            // Read one line. The size check below is post-hoc (read_line
            // has already allocated the full line). This is acceptable because
            // the IPC peer is the signaling server on the same machine, which
            // already enforces MAX_IPC_MESSAGE_SIZE. A true bounded reader
            // (e.g., BufRead::take) could prevent the allocation but adds
            // complexity for this non-network-facing boundary.
            match reader.read_line(&mut line_buf) {
                Ok(0) => {
                    eprintln!("webrtc-server: IPC read: EOF");
                    break;
                }
                Ok(n) if n > enzremote_webrtc_ipc::MAX_IPC_MESSAGE_SIZE + 128 => {
                    eprintln!("webrtc-server: IPC message too large ({n} bytes), dropping");
                    continue;
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("webrtc-server: IPC read error: {e}");
                    break;
                }
            }
            let line = line_buf.trim_end();
            if line.is_empty() {
                continue;
            }
            match parse_to_webrtc(line) {
                Ok(ToWebRtc::Answer { sdp }) => {
                    if let Err(e) = pipeline_for_reader.apply_answer(&sdp) {
                        eprintln!("webrtc-server: failed to apply answer: {e}");
                    }
                }
                Ok(ToWebRtc::Ice {
                    candidate,
                    sdp_m_line_index,
                }) => {
                    pipeline_for_reader.add_ice_candidate(&candidate, sdp_m_line_index);
                }
                Ok(ToWebRtc::Shutdown) => {
                    eprintln!("webrtc-server: received shutdown");
                    break;
                }
                Err(e) => {
                    eprintln!("webrtc-server: invalid IPC message: {e}");
                }
            }
        }
    });

    // Run the GLib main loop on the main thread.
    //
    // webrtcbin requires the GLib main context to be iterated for ICE/DTLS
    // processing. Without it, ICE candidates are exchanged but connectivity
    // checks never execute, and the connection never establishes.
    //
    // The bus watch (installed in pipeline.start()) handles error/EOS on
    // this same main context. The IPC reader thread handles signaling.
    let main_loop = gst::glib::MainLoop::new(None, false);

    // Periodically check for pipeline errors and IPC reader death.
    let pipeline_for_check = pipeline.clone();
    let main_loop_quit = main_loop.clone();
    let portal_for_check = portal.clone();
    let conn_for_check = ipc_conn.clone();
    gst::glib::timeout_add(std::time::Duration::from_millis(200), move || {
        if !reader_handle.is_finished()
            && pipeline_for_check.error().is_none()
            && let Some(Err(reason)) = portal_for_check
                .as_ref()
                .map(|capture| capture.check_health())
        {
            eprintln!("webrtc-server: capture error: {reason}");
            send_ipc(&conn_for_check, FromWebRtc::Error { reason });
            main_loop_quit.quit();
            return gst::glib::ControlFlow::Break;
        }
        if pipeline_for_check.error().is_some() || reader_handle.is_finished() {
            if let Some(error) = pipeline_for_check.error() {
                eprintln!("webrtc-server: pipeline error: {error}");
            }
            if reader_handle.is_finished() {
                eprintln!("webrtc-server: IPC reader finished, shutting down");
            }
            main_loop_quit.quit();
            return gst::glib::ControlFlow::Break;
        }
        gst::glib::ControlFlow::Continue
    });

    main_loop.run();

    pipeline.stop();

    // Clean up socket.
    let _ = std::fs::remove_file(socket_path);

    Ok(())
}
