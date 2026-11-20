#![forbid(unsafe_code)]
//! enzremote-session CLI: session lifecycle management for the enzremote system.
//!
//! Subcommands: `stream` (start streaming session), `create`/`stop`/`inspect`/
//! `resize`/`resume` (session management), `probe` (capability detection),
//! `status` (session status), `eis-broker` (EIS input bridge).

use enzremote_session::eis_broker::run_eis_broker;
use enzremote_session::{
    create_session, default_capture_helper, default_helper, default_input_pointer_helper,
    inspect_session, load_manifest, probe_capabilities, resize_session_full, resume_session,
    send_pointer, start_stream, stop_session, stop_stream,
};
use serde::Serialize;
use std::env;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut arguments = env::args().skip(1);
    match arguments.next().as_deref() {
        Some("validate") => {
            let path = one_argument(arguments, "validate MANIFEST.json")?;
            let manifest = load_manifest(Path::new(&path))?;
            print_json(&manifest)
        }
        Some("probe") => {
            no_arguments(arguments, "probe")?;
            print_json(&probe_capabilities())
        }
        Some("create") => {
            let path = one_argument(arguments, "create MANIFEST.json")?;
            let manifest = load_manifest(Path::new(&path))?;
            let state = create_session(manifest, &default_helper()?)?;
            print_json(&state)
        }
        Some("inspect") => {
            let id = one_argument(arguments, "inspect SESSION_ID")?;
            print_json(&inspect_session(&id)?)
        }
        Some("resume") => {
            let id = one_argument(arguments, "resume SESSION_ID")?;
            print_json(&resume_session(&id)?)
        }
        Some("resize") => {
            let values: Vec<String> = arguments.collect();
            if values.len() < 2 || values.len() > 4 {
                return Err(
                    "usage: enzremote-session resize SESSION_ID WIDTHxHEIGHT [SCALE] [REFRESH_HZ]"
                        .into(),
                );
            }
            let (width, height) = parse_size(&values[1])?;
            let scale: Option<f32> = values
                .get(2)
                .map(|s| s.parse().map_err(|_| "scale must be a float".to_string()))
                .transpose()?;
            let refresh_hz: Option<u32> = values
                .get(3)
                .map(|s| {
                    s.parse()
                        .map_err(|_| "refresh_hz must be an integer".to_string())
                })
                .transpose()?;
            print_json(&resize_session_full(
                &values[0], width, height, scale, refresh_hz,
            )?)
        }
        Some("stream") => {
            let values: Vec<String> = arguments.collect();
            if values.len() != 2 {
                return Err("usage: enzremote-session stream SESSION_ID CONSUMER".into());
            }
            print_json(&start_stream(
                &values[0],
                Path::new(&values[1]),
                &default_capture_helper(),
            )?)
        }
        Some("pointer") => {
            let values: Vec<String> = arguments.collect();
            if !(values.len() == 3 || (values.len() == 4 && values[3] == "--click")) {
                return Err("usage: enzremote-session pointer SESSION_ID X Y [--click]".into());
            }
            let x = values[1]
                .parse()
                .map_err(|_| "pointer X must be an integer".to_string())?;
            let y = values[2]
                .parse()
                .map_err(|_| "pointer Y must be an integer".to_string())?;
            print_json(&send_pointer(
                &values[0],
                x,
                y,
                values.len() == 4,
                &default_input_pointer_helper(),
            )?)
        }
        Some("stop-stream") => {
            let id = one_argument(arguments, "stop-stream SESSION_ID")?;
            print_json(&stop_stream(&id)?)
        }
        Some("stop") => {
            let id = one_argument(arguments, "stop SESSION_ID")?;
            print_json(&stop_session(&id)?)
        }
        Some("internal-eis-broker") => {
            let socket = one_argument(arguments, "internal-eis-broker SOCKET")?;
            run_eis_broker(Path::new(&socket))
        }
        Some("version" | "-V" | "--version") => {
            println!(
                "enzremote-session {} (schema v{})",
                env!("CARGO_PKG_VERSION"),
                enzremote_session::SCHEMA_VERSION
            );
            Ok(())
        }
        Some("help" | "-h" | "--help") | None => {
            usage();
            Ok(())
        }
        Some(command) => Err(format!("unknown command: {command}\n\n{}", usage_text())),
    }
}

fn one_argument(
    mut arguments: impl Iterator<Item = String>,
    usage: &str,
) -> Result<String, String> {
    let value = arguments
        .next()
        .ok_or_else(|| format!("usage: enzremote-session {usage}"))?;
    if arguments.next().is_some() {
        return Err(format!("usage: enzremote-session {usage}"));
    }
    Ok(value)
}

fn no_arguments(mut arguments: impl Iterator<Item = String>, usage: &str) -> Result<(), String> {
    if arguments.next().is_some() {
        Err(format!("usage: enzremote-session {usage}"))
    } else {
        Ok(())
    }
}

fn parse_size(value: &str) -> Result<(u32, u32), String> {
    let (width, height) = value
        .split_once('x')
        .ok_or_else(|| "size must use WIDTHxHEIGHT syntax".to_string())?;
    let width = width
        .parse()
        .map_err(|_| "width must be an integer".to_string())?;
    let height = height
        .parse()
        .map_err(|_| "height must be an integer".to_string())?;
    Ok((width, height))
}

fn print_json(value: &impl Serialize) -> Result<(), String> {
    let json = serde_json::to_string_pretty(value)
        .map_err(|error| format!("could not serialize output: {error}"))?;
    println!("{json}");
    Ok(())
}

fn usage() {
    println!("{}", usage_text());
}

fn usage_text() -> &'static str {
    "Usage: enzremote-session COMMAND\n\n\
     Commands:\n  \
       validate MANIFEST.json   Validate and normalize a session manifest\n  \
       probe                    Probe local media and graphics capabilities\n  \
       create MANIFEST.json     Start a launched-app session\n  \
       resume SESSION_ID        Continue an interrupted session start\n  \
       resize ID WxH [SCALE] [HZ]  Reconfigure an active app viewport\n  \
       stream ID CONSUMER       Start managed portal capture and media consumer\n  \
       stop-stream ID           Stop the stream process for reconnect\n  \
       pointer ID X Y [--click] Send a bounded pointer event\n  \
       inspect SESSION_ID       Show state and observed compositor liveness\n  \
       stop SESSION_ID          Stop a session and its child processes\n  \
       help                     Show this help"
}
