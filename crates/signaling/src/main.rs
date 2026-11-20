#![forbid(unsafe_code)]
//! enzremote-signaling: WebSocket-based session control and WebRTC signaling server.
//!
//! Subcommands: `serve` (WebSocket server), `discover` (mDNS browse),
//! `status` (connectivity check), and `settings` (native Phone Link preferences).

use enzremote_signaling::discovery;
use enzremote_signaling::server::{ServerConfig, ServerState, run};
use std::net::SocketAddr;
use std::process::ExitCode;
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("serve") | None => serve().await,
        Some("settings") => {
            use std::os::unix::process::CommandExt;
            let directory = std::env::var_os("ENZUI_SHELL_BIN_DIR")
                .map(std::path::PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| "/usr/lib/enzui/bin".into());
            let error = std::process::Command::new(directory.join("enzui-settings"))
                .env("ENZUI_SETTINGS_PAGE", "phone")
                .env_remove("ENZUI_SETTINGS_PLUGIN")
                .exec();
            eprintln!("Phone Link settings could not open: {error}");
            ExitCode::FAILURE
        }
        Some("discover") => {
            let timeout = match args.next() {
                Some(s) => match s.parse::<u64>() {
                    Ok(v) => v,
                    Err(_) => {
                        eprintln!("error: discover timeout must be a positive integer, got {s:?}");
                        return ExitCode::FAILURE;
                    }
                },
                None => 3,
            };
            discover(Duration::from_secs(timeout))
        }
        Some("status") => {
            let addr = args.next().unwrap_or_else(|| "127.0.0.1:9470".into());
            status(&addr).await
        }
        Some("version" | "-V" | "--version") => {
            println!(
                "enzremote-signaling {} (protocol v{})",
                env!("CARGO_PKG_VERSION"),
                enzremote_signaling::protocol::PROTOCOL_VERSION
            );
            ExitCode::SUCCESS
        }
        Some("apps") => {
            let catalog = enzremote_signaling::launcher::catalog();
            for app in &catalog.apps {
                println!(
                    "{:<40} {:<7} {} {}",
                    app.id,
                    app.kind,
                    if app.icon.is_some() { "icon" } else { "    " },
                    app.name
                );
            }
            eprintln!("{} apps, revision {}", catalog.apps.len(), catalog.revision);
            ExitCode::SUCCESS
        }
        Some("pair") => pair().await,
        Some("approve") => resolve_pending(args.next().unwrap_or_default(), true).await,
        Some("decline") => resolve_pending(args.next().unwrap_or_default(), false).await,
        Some("pending") => pending().await,
        Some("devices") => devices().await,
        Some("send") => send_files(args.collect()).await,
        Some("ring") => ring(args.collect()).await,
        Some("unpair") => match args.next() {
            Some(device_id) => unpair(device_id).await,
            None => {
                eprintln!("usage: enzremote-signaling unpair <device-id>   (see `devices`)");
                ExitCode::FAILURE
            }
        },
        Some("internal-eis-broker") => {
            let socket = match args.next() {
                Some(path) => std::path::PathBuf::from(path),
                None => {
                    eprintln!("error: internal-eis-broker requires a socket path argument");
                    return ExitCode::FAILURE;
                }
            };
            match enzremote_session::eis_broker::run_eis_broker(&socket) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("eis-broker error: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Some("help" | "-h" | "--help") => {
            let mut help = String::from(
                "Usage: enzremote-signaling [COMMAND]\n\n\
                 Commands:\n  \
                   serve      Start the signaling server (default)\n  \
                   pair [--json]  Show a one-time pairing QR code (running server)\n  \
                   pending    List phones waiting for pairing approval\n  \
                   approve [CODE] Approve a waiting phone (code shown on the phone)\n  \
                   decline [CODE] Decline a waiting phone\n  \
                   devices    List paired phones\n  \
                   apps       List the applications phones can open\n  \
                   unpair ID  Revoke a paired phone and disconnect it\n  \
                   discover [SECONDS]  Browse for enzremote services on the LAN\n  \
                    status [HOST:PORT]  Check if the signaling server is running\n  \
                   settings   Open native Phone Link preferences\n  \
                   version    Show version information\n  \
                   help       Show this help\n\n\
                 Environment:\n  \
                   ENZREMOTE_SIGNALING_ADDR  Bind address (default: 0.0.0.0:9470)\n  \
                   ENZREMOTE_NO_MDNS         Disable mDNS advertisement\n",
            );
            #[cfg(feature = "tls")]
            help.push_str(
                "  ENZREMOTE_TLS_CERT        PEM certificate path (enables TLS)\n  \
                   ENZREMOTE_TLS_KEY         PEM private key path (enables TLS)\n",
            );
            help.push_str(
                "  ENZREMOTE_SESSION_TIMEOUT Session idle timeout in seconds (default: 3600)\n  \
                   ENZREMOTE_CONNECTION_TIMEOUT  Connection idle timeout in seconds (default: 300)\n  \
                   ENZREMOTE_MAX_SESSIONS       Maximum active sessions (default: 8, 0 = unlimited)\n  \
                   ENZREMOTE_AUTH_TOKEN         Required auth token (empty = no auth)\n  \
                   ENZREMOTE_PAIRING_TOKENS     Comma-separated one-time pairing tokens\n  \
                   ENZREMOTE_MAX_CONNECTIONS    Maximum concurrent connections (default: 16)\n  \
                   ENZREMOTE_RATE_LIMIT         Messages per second per connection (default: 60)\n  \
                   ENZREMOTE_PING_INTERVAL      WebSocket ping interval in seconds (default: 30, 0 = disabled)\n  \
                   ENZREMOTE_GC_INTERVAL        Session GC interval in seconds (default: 300)\n  \
                   ENZREMOTE_DATA_DIR           Data directory for identity/devices (default: ~/.local/share/enzremote)\n  \
                   ENZREMOTE_TLS                auto (default: generated pinned identity) or off\n  \
                   ENZREMOTE_REQUIRE_PAIRING    1 (default): only paired phones may connect\n  \
                   ENZREMOTE_ALLOW_ANONYMOUS_LOOPBACK  1: admit unauthenticated loopback tools\n  \
                   ENZREMOTE_PAIRING_PROMPT     notify (default) or none (approve via CLI)\n  \
                   ENZREMOTE_HOST_NAME          Computer name shown on phones\n  \
                   ENZREMOTE_DEMO_MODE         Enable demo mode (session_request skips compositor launch)",
            );
            print!("{help}");
            ExitCode::SUCCESS
        }
        Some(command) => {
            eprintln!("unknown command: {command}");
            ExitCode::FAILURE
        }
    }
}

/// Parse an environment variable with a default value, warning on invalid values.
fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Ok(value) => match value.parse() {
            Ok(parsed) => parsed,
            Err(_) => {
                eprintln!("warning: {name}={value:?} is not valid; using default");
                default
            }
        },
        Err(_) => default,
    }
}

/// Clean up stale FIFO files from previous sessions.
fn cleanup_stale_fifos() {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    let fifo_dir = std::path::Path::new(&runtime_dir).join("enzremote");
    if let Ok(entries) = std::fs::read_dir(&fifo_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("grim-capture-") && name.ends_with(".fifo") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

async fn serve() -> ExitCode {
    // Remove stale FIFO files from previous server instances.
    cleanup_stale_fifos();

    let bind_addr: SocketAddr = env_or("ENZREMOTE_SIGNALING_ADDR", ([0, 0, 0, 0], 9470).into());

    let hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "enzremote".into());

    let config = ServerConfig {
        bind_addr,
        #[cfg(feature = "tls")]
        tls: match (
            std::env::var("ENZREMOTE_TLS_CERT"),
            std::env::var("ENZREMOTE_TLS_KEY"),
        ) {
            (Ok(cert), Ok(key)) => {
                eprintln!("TLS enabled: cert={cert}, key={key}");
                Some(enzremote_signaling::tls::TlsConfig {
                    cert_path: cert,
                    key_path: key,
                })
            }
            (Ok(_), Err(_)) | (Err(_), Ok(_)) => {
                eprintln!("error: both ENZREMOTE_TLS_CERT and ENZREMOTE_TLS_KEY must be set");
                return ExitCode::FAILURE;
            }
            _ if std::env::var("ENZREMOTE_TLS").as_deref() == Ok("off") => None,
            _ => {
                let Some(dir) = enzremote_signaling::identity::data_dir() else {
                    eprintln!(
                        "error: no data directory for the TLS identity; set ENZREMOTE_DATA_DIR or ENZREMOTE_TLS=off"
                    );
                    return ExitCode::FAILURE;
                };
                match enzremote_signaling::tls::ensure_self_signed(&dir.join("tls"), &hostname) {
                    Ok(tls) => Some(tls),
                    Err(error) => {
                        eprintln!("error: {error}");
                        return ExitCode::FAILURE;
                    }
                }
            }
        },
        require_pairing: env_flag("ENZREMOTE_REQUIRE_PAIRING", true),
        allow_anonymous_loopback: env_flag("ENZREMOTE_ALLOW_ANONYMOUS_LOOPBACK", false),
        pairing_requests: env_flag("ENZREMOTE_PAIRING_REQUESTS", true),
        allow_lan_plaintext: env_flag("ENZREMOTE_ALLOW_LAN_PLAINTEXT", false),
        session_timeout_secs: env_or("ENZREMOTE_SESSION_TIMEOUT", 3600),
        connection_timeout_secs: env_or("ENZREMOTE_CONNECTION_TIMEOUT", 300),
        max_sessions: env_or("ENZREMOTE_MAX_SESSIONS", 8),
        gc_interval_secs: env_or("ENZREMOTE_GC_INTERVAL", 300),
        max_connections: env_or("ENZREMOTE_MAX_CONNECTIONS", 16),
        rate_limit_per_second: env_or("ENZREMOTE_RATE_LIMIT", 60),
        ping_interval_secs: env_or("ENZREMOTE_PING_INTERVAL", 30),
        auth_token: std::env::var("ENZREMOTE_AUTH_TOKEN").unwrap_or_default(),
        demo_mode: std::env::var("ENZREMOTE_DEMO_MODE").is_ok(),
        require_active_session: env_flag("ENZREMOTE_REQUIRE_ACTIVE_SESSION", true),
        allow_legacy_commands: std::env::var("ENZREMOTE_ALLOW_LEGACY_COMMANDS").as_deref() == Ok("1"),
    };
    if let Err(error) = config.validate() {
        eprintln!("configuration error: {error}");
        return ExitCode::FAILURE;
    }
    if !config.auth_token.is_empty() {
        eprintln!("auth token required (set via ENZREMOTE_AUTH_TOKEN)");
    }
    if !config.require_pairing && config.auth_token.is_empty() && !bind_addr.ip().is_loopback() {
        eprintln!(
            "error: refusing an unauthenticated LAN listener; keep ENZREMOTE_REQUIRE_PAIRING=1"
        );
        return ExitCode::FAILURE;
    }
    let state = ServerState::new(config);
    eprintln!(
        "host: \"{}\" id={} tls={}",
        state.identity().name,
        state.identity().id,
        state
            .cert_fingerprint()
            .map_or("off".to_string(), |fp| format!("sha256:{}…", &fp[..16]))
    );
    enzremote_signaling::control::spawn(
        state.clone(),
        &enzremote_signaling::identity::control_socket_path(),
    );
    state.start_clipboard();
    state.start_phone_features();
    // Sessions do not survive a host restart; drop their audio sinks.
    tokio::task::spawn_blocking(enzremote_session::cleanup_audio_sinks);

    // Advertise via mDNS for LAN discovery.
    let _mdns = if std::env::var("ENZREMOTE_NO_MDNS").is_err() && enzremote_signaling::host_settings::current().discoverable {
        let advertised = discovery::AdvertisedIdentity {
            host_id: state.identity().id.clone(),
            name: state.identity().name.clone(),
            cert_fingerprint: state.cert_fingerprint().map(str::to_string),
        };
        match discovery::advertise(bind_addr, &hostname, &advertised) {
            Ok(daemon) => Some(daemon),
            Err(error) => {
                eprintln!("warning: mDNS advertisement failed: {error}");
                None
            }
        }
    } else {
        None
    };

    // Handle shutdown signals (Ctrl+C and SIGTERM for systemd).
    let shutdown_state = state.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            match signal(SignalKind::terminate()) {
                Ok(mut sigterm) => {
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {
                            eprintln!("received SIGINT, shutting down...");
                        }
                        _ = sigterm.recv() => {
                            eprintln!("received SIGTERM, shutting down...");
                        }
                    }
                }
                Err(e) => {
                    eprintln!("warning: could not register SIGTERM handler: {e}");
                    // Fall back to ctrl_c only.
                    let _ = tokio::signal::ctrl_c().await;
                    eprintln!("received SIGINT, shutting down...");
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        shutdown_state.shutdown();
    });

    match run(state).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn status(addr: &str) -> ExitCode {
    use enzremote_signaling::protocol::{self, HostMessage};
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let url = format!("ws://{addr}");
    eprintln!("Connecting to {url}...");

    let connect = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(&url),
    )
    .await;

    let (mut ws, _) = match connect {
        Ok(Ok((ws, response))) => {
            eprintln!("Connected (HTTP {})", response.status());
            (ws, response)
        }
        Ok(Err(error)) => {
            eprintln!("Connection failed: {error}");
            return ExitCode::FAILURE;
        }
        Err(_) => {
            eprintln!("Connection timed out");
            return ExitCode::FAILURE;
        }
    };

    // Send hello with the configured auth token (if any).
    let token = std::env::var("ENZREMOTE_AUTH_TOKEN").unwrap_or_else(|_| "probe".into());
    let hello = format!(
        r#"{{"type":"hello","v":{},"device_id":"status-check","token":"{token}"}}"#,
        protocol::PROTOCOL_VERSION
    );
    if let Err(error) = ws.send(Message::Text(hello.into())).await {
        eprintln!("Could not send hello: {error}");
        return ExitCode::FAILURE;
    }

    // Read response.
    match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
        Ok(Some(Ok(Message::Text(text)))) => {
            match serde_json::from_str::<HostMessage>(&text) {
                Ok(HostMessage::HelloAck {
                    v,
                    capabilities,
                    server_version,
                    ..
                }) => {
                    let ver = server_version.as_deref().unwrap_or("unknown");
                    println!("Server is running (protocol v{v}, version {ver})");
                    println!("Input: {:?}", capabilities.input);
                    println!("Video codecs: {:?}", capabilities.video_codecs);
                    println!(
                        "Features: resize={} clipboard={} audio={}",
                        capabilities.resize, capabilities.clipboard, capabilities.audio
                    );

                    // Also send a probe to get server status.
                    let probe = format!(r#"{{"type":"probe","v":{}}}"#, protocol::PROTOCOL_VERSION);
                    if let Ok(()) = ws.send(Message::Text(probe.into())).await
                        && let Ok(Some(Ok(Message::Text(probe_text)))) =
                            tokio::time::timeout(Duration::from_secs(5), ws.next()).await
                        && let Ok(HostMessage::ProbeResult {
                            active_sessions,
                            active_connections,
                            server_uptime_secs,
                            max_sessions,
                            hostname,
                            h264_encoders,
                            av1_encoders,
                            total_connections,
                            total_messages,
                            peak_connections,
                            total_auth_failures,
                            total_rate_limits,
                            total_sessions_created,
                            ..
                        }) = serde_json::from_str::<HostMessage>(&probe_text)
                    {
                        let host = hostname.as_deref().unwrap_or("unknown");
                        println!("Hostname: {host}");
                        println!("Uptime: {}s", server_uptime_secs);
                        let limit = if max_sessions == 0 {
                            "unlimited".to_string()
                        } else {
                            format!("max {max_sessions}")
                        };
                        println!(
                            "Sessions: {active_sessions} active ({limit}), {active_connections} connections"
                        );
                        if !h264_encoders.is_empty() {
                            println!("H.264 encoders: {}", h264_encoders.join(", "));
                        }
                        if !av1_encoders.is_empty() {
                            println!("AV1 encoders: {}", av1_encoders.join(", "));
                        }
                        if total_connections > 0 || total_messages > 0 {
                            println!(
                                "Metrics: {total_connections} total conns, {total_messages} msgs, peak {peak_connections} conns"
                            );
                            println!(
                                "  sessions_created={total_sessions_created} auth_failures={total_auth_failures} rate_limits={total_rate_limits}"
                            );
                        }

                        // Also list sessions if there are any.
                        if active_sessions > 0 {
                            let list_msg = format!(
                                r#"{{"type":"session_list","v":{}}}"#,
                                protocol::PROTOCOL_VERSION
                            );
                            if let Ok(()) = ws.send(Message::Text(list_msg.into())).await
                                && let Ok(Some(Ok(Message::Text(list_text)))) =
                                    tokio::time::timeout(Duration::from_secs(5), ws.next()).await
                                && let Ok(HostMessage::SessionListResult { sessions, .. }) =
                                    serde_json::from_str::<HostMessage>(&list_text)
                            {
                                println!("\nActive sessions:");
                                for s in &sessions {
                                    let owner = s.owner_device_id.as_deref().unwrap_or("-");
                                    let neg = s.negotiation_state.as_deref().unwrap_or("?");
                                    let status = if s.active { "active" } else { "inactive" };
                                    println!(
                                        "  {} ({}x{} @{}x {}Hz) {status} owner={owner} neg={neg} uptime={}s",
                                        s.session_id,
                                        s.width,
                                        s.height,
                                        s.scale,
                                        s.refresh_hz,
                                        s.uptime_secs
                                    );
                                }
                            }
                        }
                    }
                }
                Ok(other) => {
                    println!("Unexpected response: {other:?}");
                }
                Err(error) => {
                    eprintln!("Invalid response: {error}");
                    return ExitCode::FAILURE;
                }
            }
        }
        Ok(Some(Ok(_))) => {
            eprintln!("Unexpected non-text response");
            return ExitCode::FAILURE;
        }
        Ok(Some(Err(error))) => {
            eprintln!("WebSocket error: {error}");
            return ExitCode::FAILURE;
        }
        Ok(None) => {
            eprintln!("Server closed connection");
            return ExitCode::FAILURE;
        }
        Err(_) => {
            eprintln!("Response timed out");
            return ExitCode::FAILURE;
        }
    }

    let _ = ws.close(None).await;
    ExitCode::SUCCESS
}

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name).as_deref() {
        Ok("1" | "true" | "yes" | "on") => true,
        Ok("0" | "false" | "no" | "off") => false,
        Ok(other) => {
            eprintln!("warning: {name}={other:?} is not a boolean; using default");
            default
        }
        Err(_) => default,
    }
}

async fn control(
    request: enzremote_signaling::control::ControlRequest,
) -> Result<serde_json::Value, ExitCode> {
    let path = enzremote_signaling::identity::control_socket_path();
    match enzremote_signaling::control::request(&path, &request).await {
        Ok(value) if value["ok"].as_bool() == Some(true) => Ok(value),
        Ok(value) => {
            eprintln!(
                "error: {}",
                value["error"].as_str().unwrap_or("request failed")
            );
            Err(ExitCode::FAILURE)
        }
        Err(error) => {
            eprintln!("error: {error}");
            Err(ExitCode::FAILURE)
        }
    }
}

/// Render a QR code with Unicode half blocks (two modules per character row).
fn render_qr(data: &str) -> Option<String> {
    let code =
        qrcode::QrCode::with_error_correction_level(data.as_bytes(), qrcode::EcLevel::L).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    let dark = |x: isize, y: isize| -> bool {
        x >= 0
            && y >= 0
            && (x as usize) < width
            && (y as usize) < width
            && colors[y as usize * width + x as usize] == qrcode::Color::Dark
    };
    let quiet = 2isize;
    let mut out = String::new();
    let mut y = -quiet;
    while y < width as isize + quiet {
        for x in -quiet..width as isize + quiet {
            // Light-on-dark terminals: print light modules as blocks.
            out.push(match (!dark(x, y), !dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push('\n');
        y += 2;
    }
    Some(out)
}

async fn pair() -> ExitCode {
    let json_only = std::env::args().any(|a| a == "--json");
    let value = match control(enzremote_signaling::control::ControlRequest::PairCode).await {
        Ok(value) => value,
        Err(code) => return code,
    };
    let payload = serde_json::to_string(&value["payload"]).unwrap_or_default();
    if json_only {
        println!("{payload}");
        return ExitCode::SUCCESS;
    }
    let name = value["payload"]["name"].as_str().unwrap_or("this computer");
    let minutes = value["expires_in"].as_u64().unwrap_or(300).div_ceil(60);
    println!();
    println!("  Pair a phone with {name}");
    println!("  In enzimOS Remote: Add computer → Scan QR code\n");
    match render_qr(&payload) {
        Some(qr) => print!("{qr}"),
        None => println!("{payload}"),
    }
    println!();
    println!("  The code works once and expires in {minutes} minutes.");
    println!("  Nearby phones can also pair without a code: tap this computer in the app");
    println!("  and approve the request here or in the desktop notification.");
    ExitCode::SUCCESS
}

async fn pending() -> ExitCode {
    let value = match control(enzremote_signaling::control::ControlRequest::Pending).await {
        Ok(value) => value,
        Err(code) => return code,
    };
    let pending = value["pending"].as_array().cloned().unwrap_or_default();
    if pending.is_empty() {
        println!("No phone is waiting for approval.");
    }
    for p in pending {
        println!(
            "{}  code {}  ({}, {}s ago)",
            p["name"].as_str().unwrap_or("?"),
            p["code"].as_str().unwrap_or("?"),
            p["device_id"].as_str().unwrap_or("?"),
            p["age_secs"].as_u64().unwrap_or(0)
        );
    }
    ExitCode::SUCCESS
}

async fn resolve_pending(target: String, accept: bool) -> ExitCode {
    use enzremote_signaling::control::ControlRequest;
    let request = if accept {
        ControlRequest::Approve { target }
    } else {
        ControlRequest::Decline { target }
    };
    match control(request).await {
        Ok(value) => {
            println!(
                "{} {}",
                if accept { "Paired" } else { "Declined" },
                value["name"].as_str().unwrap_or("the phone")
            );
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}

/// `ring [--device ID] [--stop] [--notify]`: find my phone.
async fn ring(args: Vec<String>) -> ExitCode {
    let mut device_id = None;
    let mut stop = false;
    let mut notify = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => device_id = args.next(),
            "--stop" => stop = true,
            "--notify" => notify = true,
            other => {
                eprintln!("usage: enzremote-signaling ring [--device ID] [--stop]   (unknown {other})");
                return ExitCode::FAILURE;
            }
        }
    }
    let request = enzremote_signaling::control::ControlRequest::Ring { device_id, stop };
    let path = enzremote_signaling::identity::control_socket_path();
    let (ok, message) = match enzremote_signaling::control::request(&path, &request).await {
        Ok(value) if value["ok"].as_bool() == Some(true) => {
            let names: Vec<&str> = value["devices"].as_array().map(|d| d.iter().filter_map(|n| n.as_str()).collect()).unwrap_or_default();
            (true, format!("{} {}", if stop { "Stopped ringing" } else { "Ringing" }, names.join(", ")))
        }
        Ok(value) => (false, value["error"].as_str().unwrap_or("request failed").to_string()),
        Err(error) => (false, format!("enzimOS Remote is not running ({error})")),
    };
    if ok { println!("{message}") } else { eprintln!("error: {message}") }
    if notify {
        let _ = std::process::Command::new("notify-send")
            .args(["--app-name=enzimOS Remote", "--icon=phone"])
            .arg(if ok { "Find My Phone" } else { "Could not ring the phone" })
            .arg(&message)
            .status();
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// `send [--device ID] [--notify] FILE...`: offer files to a connected phone.
/// `--notify` reports the outcome as a desktop notification (file managers).
async fn send_files(args: Vec<String>) -> ExitCode {
    let mut device_id = None;
    let mut notify = false;
    let mut paths = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device" => device_id = args.next(),
            "--notify" => notify = true,
            path => match std::fs::canonicalize(path) {
                Ok(path) => paths.push(path.to_string_lossy().into_owned()),
                Err(error) => {
                    eprintln!("error: {path}: {error}");
                    return ExitCode::FAILURE;
                }
            },
        }
    }
    if paths.is_empty() {
        eprintln!("usage: enzremote-signaling send [--device ID] FILE...   (see `devices`)");
        return ExitCode::FAILURE;
    }
    let request = enzremote_signaling::control::ControlRequest::SendFiles { device_id, paths };
    let path = enzremote_signaling::identity::control_socket_path();
    let (ok, message) = match enzremote_signaling::control::request(&path, &request).await {
        Ok(value) if value["ok"].as_bool() == Some(true) => {
            let files = value["files"].as_array().map_or(0, Vec::len);
            let device = value["device"].as_str().unwrap_or("the phone").to_string();
            let what = if files == 1 { value["files"][0].as_str().unwrap_or("1 file").to_string() } else { format!("{files} files") };
            (true, format!("Sending {what} to {device}"))
        }
        Ok(value) => (false, value["error"].as_str().unwrap_or("request failed").to_string()),
        Err(error) => (false, format!("enzimOS Remote is not running ({error})")),
    };
    if ok { println!("{message}") } else { eprintln!("error: {message}") }
    if notify {
        let _ = std::process::Command::new("notify-send")
            .args(["--app-name=enzimOS Remote", "--icon=phone"])
            .arg(if ok { "Send to Phone" } else { "Could not send to phone" })
            .arg(&message)
            .status();
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

async fn devices() -> ExitCode {
    let value = match control(enzremote_signaling::control::ControlRequest::Devices).await {
        Ok(value) => value,
        Err(code) => return code,
    };
    let devices = value["devices"].as_array().cloned().unwrap_or_default();
    if devices.is_empty() {
        println!("No paired phones. Run `enzremote-signaling pair` to add one.");
    }
    for d in devices {
        let name = d["name"]
            .as_str()
            .filter(|n| !n.is_empty())
            .unwrap_or("(unnamed)");
        let status = if d["connected"].as_bool() == Some(true) {
            let battery = d["battery"]["level"].as_u64().map(|level| {
                format!(", battery {level}%{}", if d["battery"]["charging"].as_bool() == Some(true) { " (charging)" } else { "" })
            });
            format!("connected from {}{}", d["address"].as_str().unwrap_or("?"), battery.unwrap_or_default())
        } else {
            "offline".into()
        };
        println!(
            "{name}  [{}]  {status}",
            d["device_id"].as_str().unwrap_or("?")
        );
    }
    ExitCode::SUCCESS
}

async fn unpair(device_id: String) -> ExitCode {
    match control(enzremote_signaling::control::ControlRequest::Unpair {
        device_id: device_id.clone(),
    })
    .await
    {
        Ok(_) => {
            println!("Unpaired {device_id}; it was disconnected and must pair again.");
            ExitCode::SUCCESS
        }
        Err(code) => code,
    }
}

fn discover(timeout: Duration) -> ExitCode {
    eprintln!("Browsing for enzremote services ({timeout:?})...");
    match discovery::browse(timeout) {
        Ok(services) => {
            if services.is_empty() {
                println!("No enzremote services found.");
            } else {
                for service in &services {
                    println!(
                        "{} at {} port {} (v{}) addresses: {}",
                        service.name,
                        service.hostname,
                        service.port,
                        service.protocol_version,
                        service.addresses.join(", ")
                    );
                }
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
