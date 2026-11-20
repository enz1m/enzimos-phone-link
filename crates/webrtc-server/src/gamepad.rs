//! Virtual game controllers for streamed apps and games.
//!
//! Each phone-side gamepad (a controller attached to the phone or the
//! on-screen controller) becomes a uinput device laid out like the Linux
//! `xpad` driver's Xbox 360 pad, so SDL, Steam and Proton map it without
//! configuration. Every device is owned by one thread that applies state
//! snapshots and watches for rumble, which is reported back to the phone.

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventSummary, EventType, FFEffectCode, FFEffectKind,
    InputEvent, InputId, KeyCode, UInputCode, UinputAbsSetup,
};
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

/// Phone-side button bits (see the Android `GamepadButtons`).
const BUTTONS: [(u32, KeyCode); 11] = [
    (1 << 0, KeyCode::BTN_SOUTH),
    (1 << 1, KeyCode::BTN_EAST),
    (1 << 2, KeyCode::BTN_NORTH),
    (1 << 3, KeyCode::BTN_WEST),
    (1 << 4, KeyCode::BTN_TL),
    (1 << 5, KeyCode::BTN_TR),
    (1 << 6, KeyCode::BTN_SELECT),
    (1 << 7, KeyCode::BTN_START),
    (1 << 8, KeyCode::BTN_MODE),
    (1 << 9, KeyCode::BTN_THUMBL),
    (1 << 10, KeyCode::BTN_THUMBR),
];
const UP: u32 = 1 << 11;
const DOWN: u32 = 1 << 12;
const LEFT: u32 = 1 << 13;
const RIGHT: u32 = 1 << 14;

pub const MAX_PADS: u8 = 4;

/// One snapshot from the phone (sticks -1..1 with +y down, triggers 0..1).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PadState {
    pub buttons: u32,
    pub lx: f64,
    pub ly: f64,
    pub rx: f64,
    pub ry: f64,
    pub lt: f64,
    pub rt: f64,
}

/// Rumble requested by a game: magnitudes 0..1, 0 duration stops it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rumble {
    pub id: u8,
    pub strong: f64,
    pub weak: f64,
    pub duration_ms: u32,
}

/// The gamepads of one stream; dropped with it, which unplugs them.
pub struct Gamepads {
    pads: HashMap<u8, mpsc::Sender<PadState>>,
    rumble: mpsc::Sender<Rumble>,
}

impl Gamepads {
    /// `rumble` receives force-feedback requests for the phone.
    pub fn new(rumble: mpsc::Sender<Rumble>) -> Self {
        Self { pads: HashMap::new(), rumble }
    }

    /// Apply a snapshot, plugging the pad in on first use.
    pub fn update(&mut self, id: u8, state: PadState) -> Result<(), String> {
        if id >= MAX_PADS {
            return Err(format!("gamepad id must be below {MAX_PADS}"));
        }
        if let Some(pad) = self.pads.get(&id)
            && pad.send(state).is_ok()
        {
            return Ok(());
        }
        let device = create().map_err(|e| format!("cannot create a virtual gamepad (is /dev/uinput writable?): {e}"))?;
        let (tx, rx) = mpsc::channel();
        let rumble = self.rumble.clone();
        std::thread::Builder::new()
            .name(format!("gamepad-{id}"))
            .spawn(move || run(id, device, &rx, &rumble))
            .map_err(|e| e.to_string())?;
        let _ = tx.send(state);
        self.pads.insert(id, tx);
        eprintln!("webrtc-server: gamepad {id} connected");
        Ok(())
    }

    /// Unplug one pad (its thread ends and the device disappears).
    pub fn remove(&mut self, id: u8) {
        if self.pads.remove(&id).is_some() {
            eprintln!("webrtc-server: gamepad {id} disconnected");
        }
    }
}

fn create() -> std::io::Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for (_, key) in BUTTONS {
        keys.insert(key);
    }
    let mut ff = AttributeSet::<FFEffectCode>::new();
    ff.insert(FFEffectCode::FF_RUMBLE);
    let stick = AbsInfo::new(0, -32768, 32767, 16, 128, 0);
    let trigger = AbsInfo::new(0, 0, 255, 0, 0, 0);
    let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);
    let mut builder = VirtualDevice::builder()?
        // Same identity and layout as xpad's Xbox 360 pad: SDL and Steam
        // ship mappings for it.
        .name("Microsoft X-Box 360 pad")
        .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0110))
        .with_keys(&keys)?
        .with_ff(&ff)?
        .with_ff_effects_max(16);
    for (axis, info) in [
        (AbsoluteAxisCode::ABS_X, stick),
        (AbsoluteAxisCode::ABS_Y, stick),
        (AbsoluteAxisCode::ABS_RX, stick),
        (AbsoluteAxisCode::ABS_RY, stick),
        (AbsoluteAxisCode::ABS_Z, trigger),
        (AbsoluteAxisCode::ABS_RZ, trigger),
        (AbsoluteAxisCode::ABS_HAT0X, hat),
        (AbsoluteAxisCode::ABS_HAT0Y, hat),
    ] {
        builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, info))?;
    }
    let device = builder.build()?;
    // Non-blocking, so the owner thread can poll for rumble between updates.
    let fd = std::os::fd::AsFd::as_fd(&device);
    let flags = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL).map_err(std::io::Error::from)?;
    let flags = nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFL(flags)).map_err(std::io::Error::from)?;
    Ok(device)
}

/// Events that move the device from `old` to `new`.
fn events(old: &PadState, new: &PadState) -> Vec<InputEvent> {
    let mut out = Vec::new();
    let changed = old.buttons ^ new.buttons;
    for (bit, key) in BUTTONS {
        if changed & bit != 0 {
            out.push(InputEvent::new(EventType::KEY.0, key.code(), i32::from(new.buttons & bit != 0)));
        }
    }
    let stick = |v: f64| (v.clamp(-1.0, 1.0) * 32767.0).round() as i32;
    let trigger = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as i32;
    let hat = |buttons: u32, minus: u32, plus: u32| i32::from(buttons & plus != 0) - i32::from(buttons & minus != 0);
    for (axis, before, after) in [
        (AbsoluteAxisCode::ABS_X, stick(old.lx), stick(new.lx)),
        (AbsoluteAxisCode::ABS_Y, stick(old.ly), stick(new.ly)),
        (AbsoluteAxisCode::ABS_RX, stick(old.rx), stick(new.rx)),
        (AbsoluteAxisCode::ABS_RY, stick(old.ry), stick(new.ry)),
        (AbsoluteAxisCode::ABS_Z, trigger(old.lt), trigger(new.lt)),
        (AbsoluteAxisCode::ABS_RZ, trigger(old.rt), trigger(new.rt)),
        (AbsoluteAxisCode::ABS_HAT0X, hat(old.buttons, LEFT, RIGHT), hat(new.buttons, LEFT, RIGHT)),
        (AbsoluteAxisCode::ABS_HAT0Y, hat(old.buttons, UP, DOWN), hat(new.buttons, UP, DOWN)),
    ] {
        if before != after {
            out.push(InputEvent::new(EventType::ABSOLUTE.0, axis.0, after));
        }
    }
    out
}

fn run(id: u8, mut device: VirtualDevice, updates: &mpsc::Receiver<PadState>, rumble: &mpsc::Sender<Rumble>) {
    let mut state = PadState::default();
    // Uploaded rumble effects: id → (strong, weak, length ms).
    let mut effects: HashMap<i16, (f64, f64, u32)> = HashMap::new();
    loop {
        match updates.recv_timeout(Duration::from_millis(10)) {
            Ok(next) => {
                let batch = events(&state, &next);
                if !batch.is_empty() && device.emit(&batch).is_err() {
                    return;
                }
                state = next;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        // Force feedback from games: uploads, erases and play requests.
        let pending: Vec<InputEvent> = match device.fetch_events() {
            Ok(events) => events.collect(),
            Err(_) => Vec::new(),
        };
        for event in pending {
            match event.destructure() {
                EventSummary::UInput(event, UInputCode::UI_FF_UPLOAD, _) => {
                    if let Ok(mut upload) = device.process_ff_upload(event) {
                        let effect = upload.effect();
                        if let FFEffectKind::Rumble { strong_magnitude, weak_magnitude } = effect.kind {
                            effects.insert(
                                upload.effect_id(),
                                (
                                    f64::from(strong_magnitude) / 65535.0,
                                    f64::from(weak_magnitude) / 65535.0,
                                    u32::from(effect.replay.length),
                                ),
                            );
                        }
                        upload.set_retval(0);
                    }
                }
                EventSummary::UInput(event, UInputCode::UI_FF_ERASE, _) => {
                    if let Ok(erase) = device.process_ff_erase(event) {
                        effects.remove(&(erase.effect_id() as i16));
                    }
                }
                EventSummary::ForceFeedback(_, code, value) => {
                    let (strong, weak, length) = effects.get(&(code.0 as i16)).copied().unwrap_or_default();
                    let playing = value > 0;
                    let _ = rumble.send(Rumble {
                        id,
                        strong: if playing { strong } else { 0.0 },
                        weak: if playing { weak } else { 0.0 },
                        duration_ms: if playing { length.max(1) } else { 0 },
                    });
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_become_xpad_events() {
        let pressed = PadState { buttons: 1 | UP | RIGHT, lx: -1.0, rt: 0.5, ..PadState::default() };
        let batch = events(&PadState::default(), &pressed);
        let has = |kind: EventType, code: u16, value: i32| {
            batch.iter().any(|e| e.event_type() == kind && e.code() == code && e.value() == value)
        };
        assert!(has(EventType::KEY, KeyCode::BTN_SOUTH.code(), 1));
        assert!(has(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_X.0, -32767));
        assert!(has(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_RZ.0, 128));
        assert!(has(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_HAT0X.0, 1));
        assert!(has(EventType::ABSOLUTE, AbsoluteAxisCode::ABS_HAT0Y.0, -1));
        assert_eq!(batch.len(), 5);
        // Releasing restores neutral values; unchanged axes send nothing.
        let released = events(&pressed, &PadState::default());
        assert!(released.iter().all(|e| e.value() == 0));
        assert_eq!(released.len(), 5);
        assert!(events(&pressed, &pressed).is_empty());
    }
}
