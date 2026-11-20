//! enzremote-uplink: receive a phone's video for enzimOS Remote.
//!
//!   enzremote-uplink --mode view --title "Pixel 8"    window mirroring the phone
//!   enzremote-uplink --mode camera --title "Pixel 8"  PipeWire camera for other apps
//!
//! JSON lines on stdin: `{"type":"offer","sdp"}`, `{"type":"ice","candidate","mline"}`,
//! `{"type":"stop"}`. On stdout: `{"type":"answer","sdp"}`, `{"type":"ice",…}`,
//! `{"type":"input","event":{…}}` (view: pointer and keys for the phone),
//! `{"type":"closed"}`.

use std::io::{BufRead, Write};
use std::sync::Mutex;

use gst::prelude::*;
mod viewer;
use serde_json::{Value, json};

static OUT: Mutex<()> = Mutex::new(());

fn emit(message: Value) {
    let _guard = OUT.lock().unwrap_or_else(|e| e.into_inner());
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{message}");
    let _ = stdout.flush();
}

fn input(event: Value) {
    emit(json!({"type": "input", "event": event}));
}

struct Options {
    view: bool,
    title: String,
}

fn options() -> Options {
    let mut args = std::env::args().skip(1);
    let mut options = Options {
        view: true,
        title: "Phone".into(),
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" => options.view = args.next().as_deref() != Some("camera"),
            "--title" => options.title = args.next().unwrap_or_default(),
            _ => {}
        }
    }
    options
}

fn main() {
    let options = options();
    gst::init().expect("GStreamer");
    let frames = std::sync::Arc::new(viewer::Frames::default());
    let main_loop = gst::glib::MainLoop::new(None, false);
    let pipeline = gst::Pipeline::new();
    let webrtc = gst::ElementFactory::make("webrtcbin")
        .property_from_str("bundle-policy", "max-bundle")
        .property("latency", 50u32)
        .build()
        .expect("webrtcbin");
    restrict_ice_interfaces(&webrtc);
    let queue = gst::ElementFactory::make("queue")
        .property("max-size-buffers", 2u32)
        .property_from_str("leaky", "downstream")
        .build()
        .expect("queue");
    let convert = gst::ElementFactory::make("videoconvert")
        .build()
        .expect("videoconvert");
    let sink = if options.view {
        viewer::sink(frames.clone()).upcast::<gst::Element>()
    } else {
        let props = gst::Structure::builder("props")
            .field("media.class", "Video/Source")
            .field("media.type", "Video")
            .field("media.category", "Capture")
            .field("media.role", "Camera")
            .field("node.name", "enzremote-camera")
            .field("node.description", format!("{} (enzimOS Remote)", options.title))
            .build();
        gst::ElementFactory::make("pipewiresink")
            .property_from_str("mode", "provide")
            .property("stream-properties", props)
            .property("sync", false)
            .build()
            .expect("pipewiresink")
    };
    pipeline
        .add_many([&webrtc, &queue, &convert, &sink])
        .expect("add");
    if options.view {
        gst::Element::link_many([&queue, &convert, &sink]).expect("link");
    } else {
        // Camera apps cannot follow size changes mid-stream, while the phone's
        // encoder adapts its resolution: publish one fixed 720p format.
        let scale = gst::ElementFactory::make("videoscale")
            .property("add-borders", true)
            .build()
            .expect("videoscale");
        let caps = gst::ElementFactory::make("capsfilter")
            .property(
                "caps",
                gst::Caps::builder("video/x-raw")
                    .field("format", "YUY2")
                    .field("width", 1280)
                    .field("height", 720)
                    .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
                    .build(),
            )
            .build()
            .expect("capsfilter");
        pipeline.add_many([&scale, &caps]).expect("add caps");
        gst::Element::link_many([&queue, &convert, &scale, &caps, &sink]).expect("link");
        // pipewiresink's pool fails while no app consumes the camera, which
        // would stop the stream; let upstream allocate and pipewiresink copy.
        sink.static_pad("sink").expect("sink pad").add_probe(
            gst::PadProbeType::QUERY_DOWNSTREAM,
            |_, info| match info.query() {
                Some(query) if query.type_() == gst::QueryType::Allocation => {
                    gst::PadProbeReturn::Handled
                }
                _ => gst::PadProbeReturn::Ok,
            },
        );
    }

    // Decode whatever video arrives (H.264 or VP8).
    let pipeline_weak = pipeline.downgrade();
    let queue_weak = queue.downgrade();
    webrtc.connect_pad_added(move |_, pad| {
        if pad.direction() != gst::PadDirection::Src {
            return;
        }
        let codec = pad.current_caps().and_then(|c| {
            c.structure(0)
                .and_then(|s| s.get::<String>("encoding-name").ok())
        });
        eprintln!("uplink: receiving {}", codec.as_deref().unwrap_or("video"));
        let (Some(pipeline), Some(queue)) = (pipeline_weak.upgrade(), queue_weak.upgrade()) else {
            return;
        };
        let decode = gst::ElementFactory::make("decodebin")
            .build()
            .expect("decodebin");
        let queue_weak = queue.downgrade();
        decode.connect_pad_added(move |_, pad| {
            let is_video = pad
                .current_caps()
                .or_else(|| Some(pad.query_caps(None)))
                .and_then(|c| c.structure(0).map(|s| s.name().starts_with("video/")))
                .unwrap_or(false);
            if let (true, Some(queue)) = (is_video, queue_weak.upgrade())
                && let Err(error) = pad.link(&queue.static_pad("sink").expect("queue sink"))
            {
                eprintln!("uplink: decoder link failed: {error:?}");
            }
        });
        let _ = pipeline.add(&decode);
        let _ = decode.sync_state_with_parent();
        let _ = pad.link(&decode.static_pad("sink").expect("decodebin sink"));
    });
    webrtc.connect_notify(Some("connection-state"), |webrtc, _| {
        let state = webrtc.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
        eprintln!("uplink: connection {state:?}");
    });
    let received_frames = std::sync::atomic::AtomicU64::new(0);
    sink.static_pad("sink").expect("sink pad").add_probe(
        gst::PadProbeType::BUFFER,
        move |pad, _| {
            let count = received_frames.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if count == 0 {
                let size = pad.current_caps().and_then(|c| gst_video_size(&c));
                eprintln!("uplink: first frame {}", size.unwrap_or_default());
            }
            gst::PadProbeReturn::Ok
        },
    );
    webrtc.connect("on-ice-candidate", false, |values| {
        let mline = values[1].get::<u32>().unwrap_or(0);
        let candidate = values[2].get::<String>().unwrap_or_default();
        emit(json!({"type": "ice", "candidate": candidate, "mline": mline}));
        None
    });

    // Stop when the stream ends or fails.
    let bus = pipeline.bus().expect("bus");
    let loop_for_bus = main_loop.clone();
    let _watch = bus
        .add_watch(move |_, message| {
            match message.view() {
                gst::MessageView::Error(error) => {
                    eprintln!("uplink: {}", error.error());
                    loop_for_bus.quit();
                }
                gst::MessageView::Eos(_) => loop_for_bus.quit(),
                _ => {}
            }
            gst::glib::ControlFlow::Continue
        })
        .expect("bus watch");

    // Commands from the host, handled on the main loop.
    let webrtc_for_stdin = webrtc.clone();
    let loop_for_stdin = main_loop.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let webrtc = webrtc_for_stdin.clone();
            let main_loop = loop_for_stdin.clone();
            gst::glib::idle_add_once(move || handle(&webrtc, &main_loop, &line));
        }
        let main_loop = loop_for_stdin.clone();
        gst::glib::idle_add_once(move || main_loop.quit());
    });

    pipeline.set_state(gst::State::Playing).expect("play");
    if options.view {
        let worker_loop = main_loop.clone();
        let worker_frames = frames.clone();
        let worker = std::thread::spawn(move || {
            worker_loop.run();
            worker_frames
                .done
                .store(true, std::sync::atomic::Ordering::Release);
        });
        if let Err(error) = viewer::run(options.title, frames) {
            eprintln!("uplink: viewer failed: {error}");
        }
        // Queue shutdown so an early viewer exit cannot precede loop startup.
        let loop_for_close = main_loop.clone();
        gst::glib::idle_add_once(move || loop_for_close.quit());
        let _ = worker.join();
    } else {
        main_loop.run();
    }
    emit(json!({"type": "closed"}));
    let _ = pipeline.set_state(gst::State::Null);
}

/// "WxH" of raw video caps.
fn gst_video_size(caps: &gst::Caps) -> Option<String> {
    let s = caps.structure(0)?;
    Some(format!(
        "{}x{}",
        s.get::<i32>("width").ok()?,
        s.get::<i32>("height").ok()?
    ))
}

/// ICE only on real interfaces (no container bridges) and without UPnP,
/// like the stream worker; `ENZREMOTE_ICE_ALL_INTERFACES=1` opts out.
fn restrict_ice_interfaces(webrtc: &gst::Element) {
    let ice = webrtc.property::<gst_webrtc::WebRTCICE>("ice-agent");
    ice.set_property(
        "min-rtp-port",
        enzremote_webrtc_ipc::layout::MEDIA_PORT_FIRST,
    );
    ice.set_property(
        "max-rtp-port",
        enzremote_webrtc_ipc::layout::MEDIA_PORT_LAST,
    );
    if ice.find_property("ice-tcp").is_some() {
        ice.set_property("ice-tcp", false);
    }
    if std::env::var_os("ENZREMOTE_ICE_ALL_INTERFACES").is_some() {
        return;
    }
    let Ok(interfaces) = nix::ifaddrs::getifaddrs() else {
        return;
    };
    let skip = |n: &str| {
        [
            "lo", "docker", "br-", "veth", "virbr", "vmnet", "cni", "flannel", "podman",
        ]
        .iter()
        .any(|p| n.starts_with(p))
    };
    let mut chosen = Vec::new();
    for interface in interfaces.filter(|i| !skip(&i.interface_name)) {
        let Some(address) = interface.address else {
            continue;
        };
        if let Some(v4) = address.as_sockaddr_in() {
            chosen.push(std::net::SocketAddrV4::from(*v4).ip().to_string());
        } else if let Some(v6) = address.as_sockaddr_in6() {
            let ip = *std::net::SocketAddrV6::from(*v6).ip();
            if !ip.is_loopback() && ip.segments()[0] & 0xffc0 != 0xfe80 {
                chosen.push(ip.to_string());
            }
        }
    }
    if chosen.is_empty() {
        return;
    }
    for address in &chosen {
        ice.emit_by_name::<bool>("add-local-ip-address", &[address]);
    }
    if ice.find_property("agent").is_some() {
        let agent = ice.property::<gst::glib::Object>("agent");
        if agent.find_property("upnp").is_some() {
            agent.set_property("upnp", false);
        }
    }
}

fn handle(webrtc: &gst::Element, main_loop: &gst::glib::MainLoop, line: &str) {
    let Ok(message) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match message["type"].as_str() {
        Some("offer") => {
            let Some(text) = message["sdp"].as_str() else {
                return;
            };
            let Ok(sdp) = gst_sdp::SDPMessage::parse_buffer(text.as_bytes()) else {
                eprintln!("uplink: invalid offer");
                return;
            };
            let offer =
                gst_webrtc::WebRTCSessionDescription::new(gst_webrtc::WebRTCSDPType::Offer, sdp);
            let webrtc_weak = webrtc.downgrade();
            let promise = gst::Promise::with_change_func(move |_| {
                let Some(webrtc) = webrtc_weak.upgrade() else {
                    return;
                };
                let webrtc_weak = webrtc.downgrade();
                let answer_promise = gst::Promise::with_change_func(move |reply| {
                    let Some(webrtc) = webrtc_weak.upgrade() else {
                        return;
                    };
                    let Ok(Some(reply)) = reply else { return };
                    let Ok(answer) = reply.get::<gst_webrtc::WebRTCSessionDescription>("answer")
                    else {
                        return;
                    };
                    webrtc.emit_by_name::<()>(
                        "set-local-description",
                        &[&answer, &None::<gst::Promise>],
                    );
                    if let Ok(sdp) = answer.sdp().as_text() {
                        emit(json!({"type": "answer", "sdp": sdp}));
                    }
                });
                webrtc.emit_by_name::<()>(
                    "create-answer",
                    &[&None::<gst::Structure>, &answer_promise],
                );
            });
            webrtc.emit_by_name::<()>("set-remote-description", &[&offer, &promise]);
        }
        Some("ice") => {
            let mline = message["mline"].as_u64().unwrap_or(0) as u32;
            if let Some(candidate) = message["candidate"].as_str() {
                webrtc.emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
            }
        }
        Some("stop") => main_loop.quit(),
        _ => {}
    }
}
