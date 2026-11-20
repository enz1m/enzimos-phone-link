use gst::prelude::*;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Pipeline(gst::Pipeline);
impl Drop for Pipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}
fn pipewire(command: &mut Command, runtime: &Path) {
    command
        .env("PIPEWIRE_RUNTIME_DIR", runtime)
        .env("XDG_RUNTIME_DIR", runtime);
}
fn inventory(runtime: &Path) -> Vec<Value> {
    let mut command = Command::new("pw-dump");
    pipewire(&mut command, runtime);
    let output = command.output().expect("private PipeWire inventory");
    assert!(output.status.success(), "private PipeWire must respond");
    serde_json::from_slice(&output.stdout).expect("PipeWire inventory JSON")
}
fn send(input: &mut impl Write, value: &Value) {
    serde_json::to_writer(&mut *input, value).unwrap();
    input.write_all(b"\n").unwrap();
    input.flush().unwrap();
}

#[test]
#[ignore = "requires local PipeWire tools and GStreamer VP8; run for native media qualification"]
fn native_camera_delivers_generated_video_to_a_private_pipewire_consumer() {
    gst::init().unwrap();
    let context = gst::glib::MainContext::default();
    let _owner = context.acquire().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path();
    let mut command = Command::new("pipewire");
    pipewire(&mut command, runtime);
    let mut daemon = Process(
        command
            .args(["-c", "/usr/share/pipewire/pipewire.conf"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !runtime.join("pipewire-0").exists() {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "private PipeWire exited"
        );
        assert!(
            Instant::now() < deadline,
            "private PipeWire did not become ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_enzremote-uplink"));
    pipewire(&mut command, runtime);
    let mut receiver = Process(
        command
            .args(["--mode", "camera", "--title", "QA \"phone\""])
            .env("ENZREMOTE_ICE_ALL_INTERFACES", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut input = receiver.0.stdin.take().unwrap();
    let output = receiver.0.stdout.take().unwrap();
    let (received, messages) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if received.send(value).is_err() {
                break;
            }
        }
    });
    let sender = Pipeline(gst::parse::launch("videotestsrc is-live=true pattern=smpte ! video/x-raw,width=640,height=480,framerate=15/1 ! vp8enc deadline=1 target-bitrate=1000000 ! rtpvp8pay pt=96 ! application/x-rtp,media=video,encoding-name=VP8,payload=96,clock-rate=90000 ! webrtcbin name=sender bundle-policy=max-bundle")
        .unwrap().downcast::<gst::Pipeline>().unwrap());
    let peer = sender.0.by_name("sender").unwrap();
    let (transmit, outgoing) = mpsc::channel();
    let offers = transmit.clone();
    peer.connect("on-negotiation-needed", false, move |values| {
        let peer = values[0].get::<gst::Element>().unwrap();
        let prepared = peer.clone();
        let offers = offers.clone();
        peer.emit_by_name::<()>(
            "create-offer",
            &[
                &None::<gst::Structure>,
                &gst::Promise::with_change_func(move |reply| {
                    let offer = reply
                        .unwrap()
                        .unwrap()
                        .get::<gst_webrtc::WebRTCSessionDescription>("offer")
                        .unwrap();
                    let _ =
                        offers.send(json!({"type":"offer", "sdp":offer.sdp().as_text().unwrap()}));
                    prepared.emit_by_name::<()>(
                        "set-local-description",
                        &[&offer, &None::<gst::Promise>],
                    );
                }),
            ],
        );
        None
    });
    peer.connect("on-ice-candidate", false, move |values| {
        let _ = transmit.send(json!({"type":"ice", "mline":values[1].get::<u32>().unwrap(), "candidate":values[2].get::<String>().unwrap()}));
        None
    });
    sender.0.set_state(gst::State::Playing).unwrap();
    let mut consumer: Option<Process> = None;
    let frame_path = runtime.join("camera.yuy2");
    let mut pending = Vec::new();
    let mut ready = None;
    let mut remote_ready = false;
    let mut linked = std::collections::BTreeSet::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut next_inventory = Instant::now();
    let sample = loop {
        while context.pending() {
            context.iteration(false);
        }
        assert!(
            receiver.0.try_wait().unwrap().is_none(),
            "native camera receiver exited"
        );
        for value in outgoing.try_iter() {
            send(&mut input, &value);
        }
        for value in messages.try_iter() {
            match value["type"].as_str() {
                Some("answer") => {
                    let sdp = gst_sdp::SDPMessage::parse_buffer(
                        value["sdp"].as_str().unwrap().as_bytes(),
                    )
                    .unwrap();
                    let answer = gst_webrtc::WebRTCSessionDescription::new(
                        gst_webrtc::WebRTCSDPType::Answer,
                        sdp,
                    );
                    let (sent, applied) = mpsc::channel();
                    peer.emit_by_name::<()>(
                        "set-remote-description",
                        &[
                            &answer,
                            &gst::Promise::with_change_func(move |reply| {
                                let _ = sent.send(reply.is_ok());
                            }),
                        ],
                    );
                    ready = Some(applied);
                }
                Some("ice") => pending.push((
                    value["mline"].as_u64().unwrap() as u32,
                    value["candidate"].as_str().unwrap().to_owned(),
                )),
                _ => {}
            }
        }
        if let Some(applied) = &ready
            && let Ok(ok) = applied.try_recv()
        {
            assert!(ok, "generated sender refused the native answer");
            remote_ready = true;
            ready = None;
        }
        if remote_ready {
            for (line, candidate) in pending.drain(..) {
                peer.emit_by_name::<()>("add-ice-candidate", &[&line, &candidate]);
            }
        }
        if let Some(process) = consumer.as_mut()
            && let Some(status) = process.0.try_wait().unwrap()
        {
            assert!(status.success(), "private camera consumer failed");
            break std::fs::read(&frame_path).unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "private camera consumer received no generated frame"
        );
        if Instant::now() >= next_inventory {
            next_inventory = Instant::now() + Duration::from_millis(100);
            let objects = inventory(runtime);
            let nodes: Vec<_> = objects
                .iter()
                .filter(|o| o["type"] == "PipeWire:Interface:Node")
                .collect();
            let camera = nodes
                .iter()
                .find(|o| o["info"]["props"]["node.name"] == "enzremote-camera");
            if let Some(camera) = camera {
                assert_eq!(camera["info"]["props"]["media.class"], "Video/Source");
                assert_eq!(
                    camera["info"]["props"]["node.description"],
                    "QA \"phone\" (enzimOS Remote)"
                );
                if consumer.is_none() {
                    let mut command = Command::new("gst-launch-1.0");
                    pipewire(&mut command, runtime);
                    consumer = Some(Process(
                        command
                            .args([
                                "-q",
                                "pipewiresrc",
                                "target-object=enzremote-camera",
                                "client-name=PhoneLinkQaConsumer",
                                "stream-properties=props,node.name=phone-link-qa-consumer",
                                "num-buffers=1",
                                "!",
                                "video/x-raw,format=YUY2,width=1280,height=720",
                                "!",
                                "filesink",
                            ])
                            .arg(format!("location={}", frame_path.display()))
                            .stdout(Stdio::null())
                            .stderr(Stdio::piped())
                            .spawn()
                            .unwrap(),
                    ));
                }
                if let Some(target) = nodes
                    .iter()
                    .find(|o| o["info"]["props"]["node.name"] == "phone-link-qa-consumer")
                {
                    let ports: Vec<_> = objects
                        .iter()
                        .filter(|o| o["type"] == "PipeWire:Interface:Port")
                        .collect();
                    for output in ports.iter().filter(|o| {
                        o["info"]["props"]["node.id"] == camera["id"]
                            && o["info"]["props"]["port.direction"] == "out"
                    }) {
                        for inlet in ports.iter().filter(|o| {
                            o["info"]["props"]["node.id"] == target["id"]
                                && o["info"]["props"]["port.direction"] == "in"
                        }) {
                            let pair = (
                                output["id"].as_u64().unwrap(),
                                inlet["id"].as_u64().unwrap(),
                            );
                            if linked.insert(pair) {
                                let mut command = Command::new("pw-link");
                                pipewire(&mut command, runtime);
                                assert!(
                                    command
                                        .args([pair.0.to_string(), pair.1.to_string()])
                                        .output()
                                        .unwrap()
                                        .status
                                        .success(),
                                    "private camera link failed"
                                );
                            }
                        }
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(sample.len(), 1280 * 720 * 2);
    assert!(
        sample
            .iter()
            .step_by(1024)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            > 2,
        "generated colour bars must reach the consumer"
    );
    send(&mut input, &json!({"type":"stop"}));
    let deadline = Instant::now() + Duration::from_secs(5);
    while receiver.0.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "native camera receiver did not stop"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    reader.join().unwrap();
}
