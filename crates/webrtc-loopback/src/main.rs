#![forbid(unsafe_code)]
//! enzremote WebRTC loopback transport for local development and testing.
//!
//! Negotiates a local WebRTC session using GStreamer's `webrtcbin`,
//! encoding video (H.264 via VA-API, x264, or OpenH264) and optional
//! audio (Opus). Provides latency measurement, adaptive bitrate, and
//! a data channel for control messages.

mod latency;

use enzremote_control::{CONTROL_MESSAGES, ControlMessage, parse_control_message};
use gst::prelude::*;
use latency::{LatencyRing, LatencySummary, compute_latency_summary};
use std::env;
use std::error::Error;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// Control protocol types (ControlMessage, TouchPhase) and parsing
// (parse_control_message, validate_version_and_coordinates) are in control.rs.

/// Thread-safe handle to a persistent EIS input worker process.
struct InputBridge {
    worker: Mutex<Option<InputWorker>>,
}

/// A running `enzremote-input-eis` child process with piped I/O.
struct InputWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl InputBridge {
    fn from_environment() -> Result<Option<Self>, String> {
        let broker_fd = match env::var("ENZREMOTE_EIS_BROKER_FD") {
            Ok(value) => Some(
                value
                    .parse::<i32>()
                    .map_err(|_| "invalid inherited EIS broker capability".to_string())?,
            ),
            Err(env::VarError::NotPresent) => None,
            Err(error) => return Err(format!("invalid EIS broker capability: {error}")),
        };
        let helper = env::var_os("ENZREMOTE_INPUT_EIS_HELPER").map(PathBuf::from);
        if env::var_os("ENZREMOTE_EIS_BROKER_SOCKET").is_some() {
            return Err("network media worker must not inherit the broker socket path".into());
        }
        if broker_fd.is_some()
            && (env::var_os("WAYLAND_DISPLAY").is_some()
                || env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
                || env::var_os("DISPLAY").is_some())
        {
            return Err("network media worker must not inherit display access".into());
        }
        match (broker_fd, helper) {
            (None, None) => Ok(None),
            (Some(broker_fd), Some(helper)) if broker_fd >= 0 && helper.is_file() => {
                let mut child = Command::new(&helper)
                    .arg("--worker-fd")
                    .arg(broker_fd.to_string())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .map_err(|error| format!("could not execute {}: {error}", helper.display()))?;
                let stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| "input worker has no stdin".to_string())?;
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| "input worker has no stdout".to_string())?;
                let stdout = BufReader::new(stdout);
                // Wait for the worker to become ready with a timeout.
                // read_line on a pipe blocks indefinitely, so use a
                // background thread with a deadline.
                let (tx, rx) = std::sync::mpsc::channel();
                let reader_thread = thread::spawn(move || {
                    let mut reader = stdout;
                    let mut line = String::new();
                    let result = reader.read_line(&mut line).map(|_| line);
                    let _ = tx.send(result);
                    reader
                });
                let (ready, stdout) = match rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(Ok(line)) => {
                        let stdout = match reader_thread.join() {
                            Ok(r) => r,
                            Err(_) => {
                                let _ = child.kill();
                                let _ = child.wait();
                                return Err("input worker reader thread panicked".into());
                            }
                        };
                        (line, stdout)
                    }
                    Ok(Err(error)) => {
                        let _ = reader_thread.join();
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!("could not start input worker: {error}"));
                    }
                    Err(_) => {
                        // Timeout; kill the child so the thread unblocks.
                        let _ = child.kill();
                        let _ = reader_thread.join();
                        let _ = child.wait();
                        return Err("input worker timed out waiting for READY".into());
                    }
                };
                if ready.trim() != "READY" {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "input worker failed to become ready: {}",
                        ready.trim()
                    ));
                }
                Ok(Some(Self {
                    worker: Mutex::new(Some(InputWorker {
                        child,
                        stdin,
                        stdout,
                    })),
                }))
            }
            (Some(broker_fd), _) if broker_fd < 0 => {
                Err("invalid inherited EIS broker capability".into())
            }
            (Some(_), Some(helper)) => Err(format!(
                "EIS input helper is unavailable: {}",
                helper.display()
            )),
            _ => Err("EIS broker capability and input helper must be configured together".into()),
        }
    }

    fn send(&self, message: &ControlMessage) -> Result<(), String> {
        // Use the canonical to_worker_command() from the control crate to
        // ensure the format sent to the C worker matches exactly.
        let command = match message.to_worker_command() {
            Some(cmd) => cmd,
            None => {
                // Pipeline, display, and clipboard control messages are not
                // forwarded to the input worker.
                return Ok(());
            }
        };
        let mut worker = self.worker.lock().unwrap_or_else(|e| e.into_inner());
        let worker = worker
            .as_mut()
            .ok_or_else(|| "input capability has been revoked".to_string())?;
        writeln!(worker.stdin, "{command}")
            .and_then(|_| worker.stdin.flush())
            .map_err(|error| format!("could not send input command: {error}"))?;
        let mut response = String::new();
        let bytes_read = worker
            .stdout
            .read_line(&mut response)
            .map_err(|error| format!("could not read input response: {error}"))?;
        if bytes_read == 0 {
            Err("input worker process died (EOF on stdout)".into())
        } else if response.trim() == "OK" {
            Ok(())
        } else {
            Err(format!("input worker rejected command: {command}"))
        }
    }

    fn disconnect(&self) {
        if let Some(mut worker) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = worker.child.kill();
            let _ = worker.child.wait();
        }
    }

    fn is_connected(&self) -> bool {
        self.worker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

impl Drop for InputBridge {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Self-contained WebRTC loopback test: encodes, sends, receives, and
/// decodes media within a single GStreamer pipeline to validate the full
/// stack without a remote peer.
struct Loopback {
    pipeline: gst::Pipeline,
    sender: gst::Element,
    receiver: gst::Element,
    local_channel: Mutex<Option<gst_webrtc::WebRTCDataChannel>>,
    negotiation_started: AtomicBool,
    video_packets: AtomicUsize,
    audio_packets: AtomicUsize,
    decoded_video_frames: AtomicUsize,
    decoded_audio_buffers: AtomicUsize,
    video_latency_us: Mutex<LatencyRing>,
    control_received: AtomicUsize,
    input_bridge: Option<InputBridge>,
    error: Mutex<Option<String>>,
}

impl Loopback {
    fn new(video_source: &str, h264_encoder: &str) -> Result<Arc<Self>, Box<dyn Error>> {
        let pipeline_description = format!(
            "webrtcbin name=send bundle-policy=max-bundle \
             webrtcbin name=recv bundle-policy=max-bundle \
             {video_source} \
             queue ! {h264_encoder} \
             h264parse ! rtph264pay pt=96 config-interval=-1 aggregate-mode=zero-latency ! \
             application/x-rtp,media=video,encoding-name=H264,payload=96,clock-rate=90000 ! \
             queue ! send. \
             audiotestsrc is-live=true wave=sine ! audioconvert ! audioresample ! \
             audio/x-raw,rate=48000,channels=1 ! opusenc bitrate=32000 audio-type=voice ! \
             rtpopuspay pt=97 ! \
             application/x-rtp,media=audio,encoding-name=OPUS,payload=97,clock-rate=48000 ! \
             queue ! send."
        );
        let pipeline = gst::parse::launch(&pipeline_description)?
            .downcast::<gst::Pipeline>()
            .map_err(|_| "parsed media graph is not a pipeline")?;
        let sender = pipeline
            .by_name("send")
            .ok_or("pipeline has no sender webrtcbin")?;
        let receiver = pipeline
            .by_name("recv")
            .ok_or("pipeline has no receiver webrtcbin")?;

        let state = Arc::new(Self {
            pipeline,
            sender,
            receiver,
            local_channel: Mutex::new(None),
            negotiation_started: AtomicBool::new(false),
            video_packets: AtomicUsize::new(0),
            audio_packets: AtomicUsize::new(0),
            decoded_video_frames: AtomicUsize::new(0),
            decoded_audio_buffers: AtomicUsize::new(0),
            video_latency_us: Mutex::new(LatencyRing::new()),
            control_received: AtomicUsize::new(0),
            input_bridge: InputBridge::from_environment()?,
            error: Mutex::new(None),
        });
        state.connect_signals();
        Ok(state)
    }

    fn connect_signals(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        self.sender
            .connect("on-negotiation-needed", false, move |_| {
                if let Some(state) = weak.upgrade() {
                    state.start_negotiation();
                }
                None
            });

        forward_ice(&self.sender, &self.receiver);
        forward_ice(&self.receiver, &self.sender);

        let weak = Arc::downgrade(self);
        self.receiver.connect_pad_added(move |_, pad| {
            if let Some(state) = weak.upgrade()
                && let Err(error) = state.handle_incoming_pad(pad)
            {
                state.fail(format!("could not attach incoming RTP stream: {error}"));
            }
        });

        let weak = Arc::downgrade(self);
        self.receiver
            .connect("on-data-channel", false, move |values| {
                let result = values[1].get::<gst_webrtc::WebRTCDataChannel>();
                match (weak.upgrade(), result) {
                    (Some(state), Ok(channel)) => state.attach_remote_channel(&channel),
                    (Some(state), Err(error)) => {
                        state.fail(format!("invalid remote data channel: {error}"));
                    }
                    _ => {}
                }
                None
            });
    }

    fn create_data_channel(self: &Arc<Self>) -> Result<(), Box<dyn Error>> {
        let channel = self.sender.emit_by_name::<gst_webrtc::WebRTCDataChannel>(
            "create-data-channel",
            &[&"control", &None::<gst::Structure>],
        );
        let weak = Arc::downgrade(self);
        channel.connect_on_open(move |channel| {
            for message in CONTROL_MESSAGES {
                if let Err(error) = channel.send_string_full(Some(message)) {
                    if let Some(state) = weak.upgrade() {
                        state.fail(format!("could not send control message: {error}"));
                    }
                    break;
                }
            }
        });
        let weak = Arc::downgrade(self);
        channel.connect_on_error(move |_, error| {
            if let Some(state) = weak.upgrade() {
                state.fail(format!("local data channel failed: {error}"));
            }
        });
        *self.local_channel.lock().unwrap_or_else(|e| e.into_inner()) = Some(channel);
        Ok(())
    }

    fn start_negotiation(self: &Arc<Self>) {
        if !self.negotiation_started.swap(true, Ordering::SeqCst) {
            self.create_offer();
        }
    }

    fn attach_remote_channel(self: &Arc<Self>, channel: &gst_webrtc::WebRTCDataChannel) {
        let weak = Arc::downgrade(self);
        channel.connect_on_message_string(move |_, message| {
            if let Some(state) = weak.upgrade() {
                let result = message
                    .ok_or_else(|| "control message was empty".to_string())
                    .and_then(parse_control_message)
                    .and_then(|message| {
                        if let Some(bridge) = &state.input_bridge {
                            bridge.send(&message)?;
                        }
                        Ok(())
                    });
                match result {
                    Ok(()) => {
                        state.control_received.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(error) => state.fail(error),
                }
            }
        });
        let weak = Arc::downgrade(self);
        channel.connect_on_error(move |_, error| {
            if let Some(state) = weak.upgrade() {
                if let Some(bridge) = &state.input_bridge {
                    bridge.disconnect();
                }
                state.fail(format!("remote data channel failed: {error}"));
            }
        });
        let weak = Arc::downgrade(self);
        channel.connect_on_close(move |_| {
            if let Some(state) = weak.upgrade()
                && let Some(bridge) = &state.input_bridge
            {
                bridge.disconnect();
            }
        });
    }

    fn create_offer(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let promise = gst::Promise::with_change_func(move |reply| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let result = reply
                .map_err(|error| format!("offer promise failed: {error:?}"))
                .and_then(|reply| reply.ok_or_else(|| "offer promise had no reply".to_string()))
                .and_then(|reply| {
                    reply
                        .get::<gst_webrtc::WebRTCSessionDescription>("offer")
                        .map_err(|error| format!("offer reply had no description: {error}"))
                });
            match result {
                Ok(offer) => state.apply_offer(offer),
                Err(error) => state.fail(error),
            }
        });
        self.sender
            .emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
    }

    fn apply_offer(self: &Arc<Self>, offer: gst_webrtc::WebRTCSessionDescription) {
        self.sender
            .emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);

        let weak = Arc::downgrade(self);
        let promise = gst::Promise::with_change_func(move |reply| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if let Err(error) = promise_succeeded(reply, "remote offer") {
                state.fail(error);
                return;
            }
            state.create_answer();
        });
        self.receiver
            .emit_by_name::<()>("set-remote-description", &[&offer, &promise]);
    }

    fn create_answer(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let promise = gst::Promise::with_change_func(move |reply| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let result = reply
                .map_err(|error| format!("answer promise failed: {error:?}"))
                .and_then(|reply| reply.ok_or_else(|| "answer promise had no reply".to_string()))
                .and_then(|reply| {
                    reply
                        .get::<gst_webrtc::WebRTCSessionDescription>("answer")
                        .map_err(|error| format!("answer reply had no description: {error}"))
                });
            match result {
                Ok(answer) => state.apply_answer(answer),
                Err(error) => state.fail(error),
            }
        });
        self.receiver
            .emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
    }

    fn apply_answer(self: &Arc<Self>, answer: gst_webrtc::WebRTCSessionDescription) {
        self.receiver
            .emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);
        self.sender
            .emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
    }

    fn handle_incoming_pad(self: &Arc<Self>, pad: &gst::Pad) -> Result<(), Box<dyn Error>> {
        if pad.direction() != gst::PadDirection::Src {
            return Ok(());
        }
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let structure = caps.structure(0).ok_or("incoming pad has no caps")?;
        let media = structure.get::<&str>("media")?;
        let is_video = match media {
            "video" => true,
            "audio" => false,
            other => return Err(format!("unsupported incoming media kind: {other}").into()),
        };
        let weak = Arc::downgrade(self);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            if let Some(state) = weak.upgrade() {
                let counter = if is_video {
                    &state.video_packets
                } else {
                    &state.audio_packets
                };
                counter.fetch_add(1, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });

        let description = if is_video {
            "queue ! rtph264depay ! h264parse ! openh264dec ! videoconvert ! \
             fakesink name=decoded sync=false async=false"
        } else {
            "queue ! rtpopusdepay ! opusdec ! audioconvert ! \
             fakesink name=decoded sync=false async=false"
        };
        let sink = gst::parse::bin_from_description(description, true)?;
        let decoded_sink = sink.by_name("decoded").ok_or("decode bin has no sink")?;
        let decoded_pad = decoded_sink
            .static_pad("sink")
            .ok_or("decoded sink has no sink pad")?;
        let weak = Arc::downgrade(self);
        decoded_pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(state) = weak.upgrade() {
                let counter = if is_video {
                    &state.decoded_video_frames
                } else {
                    &state.decoded_audio_buffers
                };
                counter.fetch_add(1, Ordering::Relaxed);
                if is_video
                    && let Some(pts) = info.buffer().and_then(|buffer| buffer.pts())
                    && let Some(running_time) = state.pipeline.current_running_time()
                    && running_time >= pts
                {
                    let mut ring = state
                        .video_latency_us
                        .lock()
                        .unwrap_or_else(|e: std::sync::PoisonError<_>| e.into_inner());
                    ring.push(running_time.nseconds().saturating_sub(pts.nseconds()) / 1_000);
                }
            }
            gst::PadProbeReturn::Ok
        });
        self.pipeline.add(&sink)?;
        if let Err(e) = sink.sync_state_with_parent() {
            let _ = self.pipeline.remove(&sink);
            return Err(e.into());
        }
        let sink_pad = match sink.static_pad("sink") {
            Some(p) => p,
            None => {
                let _ = sink.set_state(gst::State::Null);
                let _ = self.pipeline.remove(&sink);
                return Err("sink bin has no ghost pad".into());
            }
        };
        if let Err(e) = pad.link(&sink_pad) {
            let _ = sink.set_state(gst::State::Null);
            let _ = self.pipeline.remove(&sink);
            return Err(e.into());
        }
        Ok(())
    }

    fn fail(&self, message: String) {
        let mut error = self.error.lock().unwrap_or_else(|e| e.into_inner());
        if error.is_none() {
            *error = Some(message);
        }
    }

    fn succeeded(&self) -> bool {
        self.video_packets.load(Ordering::Relaxed) > 0
            && self.audio_packets.load(Ordering::Relaxed) > 0
            && self.decoded_video_frames.load(Ordering::Relaxed) > 0
            && self.decoded_audio_buffers.load(Ordering::Relaxed) > 0
            && self.control_received.load(Ordering::SeqCst) == CONTROL_MESSAGES.len()
    }

    fn close_control_channel(&self) {
        if let Some(channel) = self
            .local_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            channel.close();
        }
    }

    fn input_connected(&self) -> bool {
        self.input_bridge
            .as_ref()
            .is_some_and(InputBridge::is_connected)
    }

    fn video_latency_summary(&self) -> LatencySummary {
        // Clone samples and release the mutex before computing statistics,
        // so GStreamer's streaming thread isn't blocked during the sort.
        let (samples, total) = {
            let ring = self
                .video_latency_us
                .lock()
                .unwrap_or_else(|e: std::sync::PoisonError<_>| e.into_inner());
            (ring.samples(), ring.total_count())
        };
        compute_latency_summary(samples, total)
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        if let Some(channel) = self
            .local_channel
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            channel.close();
        }
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// Forward ICE candidates from one webrtcbin element to another.
fn forward_ice(source: &gst::Element, destination: &gst::Element) {
    let destination = destination.downgrade();
    source.connect("on-ice-candidate", false, move |values| {
        let index = values[1].get::<u32>();
        let candidate = values[2].get::<String>();
        if let (Some(destination), Ok(index), Ok(candidate)) =
            (destination.upgrade(), index, candidate)
        {
            destination.emit_by_name::<()>("add-ice-candidate", &[&index, &candidate]);
        }
        None
    });
}

/// Check that a GStreamer promise completed successfully.
fn promise_succeeded(
    reply: Result<Option<&gst::StructureRef>, gst::PromiseError>,
    operation: &str,
) -> Result<(), String> {
    match reply {
        Ok(_) => Ok(()),
        Err(error) => Err(format!("{operation} promise failed: {error:?}")),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    gst::init()?;
    let minimum_run =
        parse_run_seconds(env::var("ENZREMOTE_TEST_DURATION_SECONDS").ok().as_deref())?;
    let (video_source, source_name, pipewire_source) = video_source_from_args()?;
    let (h264_encoder, encoder_name) = h264_encoder(pipewire_source)?;
    let state = Loopback::new(&video_source, h264_encoder)?;
    state.pipeline.set_state(gst::State::Ready)?;
    state.create_data_channel()?;
    state.pipeline.set_state(gst::State::Playing)?;
    state.start_negotiation();

    let bus = state.pipeline.bus().ok_or("pipeline has no bus")?;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(15).max(minimum_run + Duration::from_secs(5));
    while Instant::now() < deadline && (!state.succeeded() || started.elapsed() < minimum_run) {
        if let Some(error) = state
            .error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Err(error.into());
        }
        if let Some(message) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
            use gst::MessageView;
            match message.view() {
                MessageView::Error(error) => {
                    return Err(format!(
                        "GStreamer error from {}: {} ({})",
                        error
                            .src()
                            .map(|source| source.path_string())
                            .unwrap_or_else(|| "unknown".into()),
                        error.error(),
                        error.debug().unwrap_or_else(|| "no debug details".into())
                    )
                    .into());
                }
                MessageView::Eos(_) => return Err("pipeline reached EOS before success".into()),
                _ => {}
            }
        }
    }

    if !state.succeeded() {
        return Err(format!(
            "WebRTC loopback timed out: video_packets={} audio_packets={} decoded_video_frames={} decoded_audio_buffers={} control_received={}/{}",
            state.video_packets.load(Ordering::Relaxed),
            state.audio_packets.load(Ordering::Relaxed),
            state.decoded_video_frames.load(Ordering::Relaxed),
            state.decoded_audio_buffers.load(Ordering::Relaxed),
            state.control_received.load(Ordering::SeqCst),
            CONTROL_MESSAGES.len()
        )
        .into());
    }

    state.close_control_channel();
    let revoke_deadline = Instant::now() + Duration::from_secs(1);
    while state.input_connected() && Instant::now() < revoke_deadline {
        let _ = bus.timed_pop(gst::ClockTime::from_mseconds(10));
    }
    if state.input_connected() {
        return Err("input worker survived control channel closure".into());
    }
    let elapsed_ms = started.elapsed().as_millis();
    let lat = state.video_latency_summary();

    println!(
        "WebRTC loopback passed: source={source_name} encoder={encoder_name} elapsed_ms={elapsed_ms} video_packets={} audio_packets={} decoded_video_frames={} decoded_audio_buffers={} latency_samples={} latency_total_samples={} latency_p50_us={} latency_p95_us={} latency_p99_us={} latency_max_us={} latency_mean_us={} latency_stddev_us={} latency_jitter_us={} control_messages={}",
        state.video_packets.load(Ordering::Relaxed),
        state.audio_packets.load(Ordering::Relaxed),
        state.decoded_video_frames.load(Ordering::Relaxed),
        state.decoded_audio_buffers.load(Ordering::Relaxed),
        lat.samples,
        lat.total_samples,
        lat.p50,
        lat.p95,
        lat.p99,
        lat.max,
        lat.mean,
        lat.stddev,
        lat.jitter,
        state.control_received.load(Ordering::SeqCst)
    );
    Ok(())
}

/// Parse an optional seconds string into a Duration (max 300 seconds).
fn parse_run_seconds(value: Option<&str>) -> Result<Duration, String> {
    let Some(value) = value else {
        return Ok(Duration::ZERO);
    };
    let seconds = value
        .parse::<u64>()
        .map_err(|_| "ENZREMOTE_TEST_DURATION_SECONDS must be an integer".to_string())?;
    if seconds > 300 {
        return Err("ENZREMOTE_TEST_DURATION_SECONDS must not exceed 300".into());
    }
    Ok(Duration::from_secs(seconds))
}

/// Determine the video source pipeline element from command-line arguments.
/// Returns (pipeline_fragment, source_description, is_pipewire).
fn video_source_from_args() -> Result<(String, String, bool), Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    match arguments.next().as_deref() {
        None => Ok((
            "videotestsrc is-live=true pattern=ball ! \
             video/x-raw,format=I420,width=390,height=844,framerate=30/1 !"
                .into(),
            "generated".into(),
            false,
        )),
        Some("--pipewire-target") => {
            let target = arguments
                .next()
                .ok_or("--pipewire-target requires an object serial")?;
            if arguments.next().is_some()
                || target.is_empty()
                || !target.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err("pipewire target must be one decimal object serial".into());
            }
            Ok((
                format!(
                    "pipewiresrc target-object=\"{target}\" provide-clock=false do-timestamp=true keepalive-time=1000 !"
                ),
                format!("pipewire:{target}"),
                true,
            ))
        }
        Some("--pipewire-fd") => {
            let fd = decimal_argument(arguments.next(), "--pipewire-fd")?;
            if arguments.next().as_deref() != Some("--pipewire-node") {
                return Err("--pipewire-fd must be followed by --pipewire-node NODE_ID".into());
            }
            let node = decimal_argument(arguments.next(), "--pipewire-node")?;
            if arguments.next().is_some() {
                return Err("unexpected arguments after --pipewire-node".into());
            }
            Ok((
                format!(
                    "pipewiresrc fd={fd} path=\"{node}\" provide-clock=false do-timestamp=true keepalive-time=1000 !"
                ),
                format!("portal-pipewire:{node}"),
                true,
            ))
        }
        Some(argument) => Err(format!(
            "unknown argument: {argument}\nusage: enzremote-webrtc-loopback \
             [--pipewire-target SERIAL | --pipewire-fd FD --pipewire-node NODE_ID]"
        )
        .into()),
    }
}

/// Select the H.264 encoder based on environment and source type.
fn h264_encoder(pipewire_source: bool) -> Result<(&'static str, &'static str), Box<dyn Error>> {
    let requested = env::var("ENZREMOTE_H264_ENCODER").unwrap_or_else(|_| "auto".into());
    let va_available = gst::ElementFactory::find("vah264enc").is_some();
    select_h264_encoder(&requested, va_available, pipewire_source)
}

/// Select the H.264 encoder pipeline fragment given user preference,
/// VA-API availability, and whether the source is PipeWire (DMA-BUF).
fn select_h264_encoder(
    requested: &str,
    va_available: bool,
    pipewire_source: bool,
) -> Result<(&'static str, &'static str), Box<dyn Error>> {
    let va_encoder = if pipewire_source {
        "videorate ! video/x-raw(memory:DMABuf),format=DMA_DRM,framerate=30/1 ! \
         vapostproc ! video/x-raw(memory:VAMemory),format=NV12 ! \
         vah264enc rate-control=vcm bitrate=2000 key-int-max=30 target-usage=7 ! \
         video/x-h264,profile=constrained-baseline !"
    } else {
        "videorate ! video/x-raw,framerate=30/1 ! \
         vapostproc ! video/x-raw(memory:VAMemory),format=NV12 ! \
         vah264enc rate-control=vcm bitrate=2000 key-int-max=30 target-usage=7 ! \
         video/x-h264,profile=constrained-baseline !"
    };
    match requested {
        "auto" if va_available => Ok((va_encoder, "vah264enc")),
        "auto" | "openh264" => Ok((
            "videoconvert ! videorate ! video/x-raw,format=I420,framerate=30/1 ! \
             openh264enc usage-type=screen complexity=low \
             rate-control=bitrate bitrate=2000000 gop-size=30 !",
            "openh264enc",
        )),
        "va" if va_available => Ok((va_encoder, "vah264enc")),
        "va" => Err("ENZREMOTE_H264_ENCODER=va requested but vah264enc is unavailable".into()),
        value => Err(format!(
            "unsupported ENZREMOTE_H264_ENCODER={value}; expected auto, va, or openh264"
        )
        .into()),
    }
}

/// Validate that an optional argument is a non-negative decimal integer.
fn decimal_argument(value: Option<String>, option: &str) -> Result<String, Box<dyn Error>> {
    let value = value.ok_or_else(|| format!("{option} requires a decimal value"))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{option} requires a decimal value").into());
    }
    Ok(value)
}
