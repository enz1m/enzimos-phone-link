use gst::prelude::*;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
struct Profile {
    start: Duration,
    duration: Duration,
    percent: u16,
}

impl Profile {
    fn parse(value: &str) -> Result<Self, String> {
        let values = value
            .split(':')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "RTP loss test must be START:DURATION:PERCENT integers")?;
        if values.len() != 3 || values[0] > 300 || !(1..=60).contains(&values[1]) || values[2] > 100
        {
            return Err(
                "RTP loss test bounds: start 0..300s, duration 1..60s, loss 0..100%".into(),
            );
        }
        Ok(Self {
            start: Duration::from_secs(values[0]),
            duration: Duration::from_secs(values[1]),
            percent: values[2] as u16,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Waiting,
    Active,
    Complete,
}

struct DropSchedule {
    profile: Profile,
    phase: Phase,
    accumulator: u16,
    packets: u64,
    dropped: u64,
}

impl DropSchedule {
    fn new(profile: Profile) -> Self {
        Self {
            profile,
            phase: Phase::Waiting,
            accumulator: 0,
            packets: 0,
            dropped: 0,
        }
    }
    fn packet(&mut self, elapsed: Duration) -> bool {
        if elapsed < self.profile.start {
            return false;
        }
        if elapsed >= self.profile.start + self.profile.duration {
            self.phase = Phase::Complete;
            return false;
        }
        self.phase = Phase::Active;
        self.packets += 1;
        self.accumulator += self.profile.percent;
        if self.accumulator >= 100 {
            self.accumulator -= 100;
            self.dropped += 1;
            true
        } else {
            false
        }
    }
}

fn is_rtp(buffer: &gst::BufferRef) -> bool {
    buffer
        .map_readable()
        .is_ok_and(|map| map.len() >= 12 && map[0] >> 6 == 2 && !(192..=223).contains(&map[1]))
}

pub fn install(webrtc: &gst::Element, value: Option<&str>) -> Result<(), String> {
    let Some(value) = value else { return Ok(()) };
    let profile = Profile::parse(value)?;
    if gst::glib::subclass::SignalId::lookup("request-post-rtp-aux-sender", webrtc.type_())
        .is_none()
    {
        return Err("RTP loss diagnostics require GStreamer 1.26 or newer".into());
    }
    if gst::ElementFactory::find("identity").is_none() {
        return Err("RTP loss diagnostics require the identity element".into());
    }
    eprintln!(
        "RTP_TEST configured start_s={} duration_s={} loss_percent={}; this worker's outgoing RTP only",
        profile.start.as_secs(),
        profile.duration.as_secs(),
        profile.percent
    );
    webrtc.connect("request-post-rtp-aux-sender", false, move |_| {
        let identity = gst::ElementFactory::make("identity").build().ok()?;
        let pad = identity.static_pad("src")?;
        let state = Mutex::new((None::<Instant>, DropSchedule::new(profile)));
        pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            move |_, info| {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                let elapsed = now.duration_since(*state.0.get_or_insert(now));
                let old_phase = state.1.phase;
                let mut result = gst::PadProbeReturn::Ok;
                match info.data.as_mut() {
                    Some(gst::PadProbeData::Buffer(buffer)) if is_rtp(buffer) => {
                        if state.1.packet(elapsed) {
                            result = gst::PadProbeReturn::Drop;
                        }
                    }
                    Some(gst::PadProbeData::BufferList(list)) => {
                        let list = list.make_mut();
                        let mut drops = Vec::new();
                        for i in 0..list.len() {
                            if list.get(i).is_some_and(is_rtp) && state.1.packet(elapsed) {
                                drops.push(i);
                            }
                        }
                        for i in drops.into_iter().rev() {
                            list.remove(i..i + 1);
                        }
                        if list.is_empty() {
                            result = gst::PadProbeReturn::Drop;
                        }
                    }
                    _ => {}
                }
                if old_phase != state.1.phase {
                    eprintln!(
                        "RTP_TEST phase={:?} elapsed_ms={} packets={} dropped={}",
                        state.1.phase,
                        elapsed.as_millis(),
                        state.1.packets,
                        state.1.dropped
                    );
                }
                if state.1.phase == Phase::Complete {
                    return gst::PadProbeReturn::Remove;
                }
                result
            },
        );
        Some(identity.to_value())
    });
    Ok(())
}
