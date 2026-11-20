//! Desktop clipboard bridge for clipboard sharing with paired phones.
//!
//! Uses X11 selections through Xwayland: GNOME (Mutter) offers no Wayland
//! data-control protocol, but mirrors the Wayland clipboard to the X11
//! `CLIPBOARD` selection in both directions, and so do KWin and wlroots
//! compositors with Xwayland. XFixes reports owner changes; text set from a
//! phone is served by owning the selection. Entries marked by password
//! managers (`x-kde-passwordManagerHint`) are never forwarded.

use std::sync::mpsc;
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, SELECTION_NOTIFY_EVENT,
    SelectionNotifyEvent, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use crate::protocol::MAX_CLIPBOARD_TEXT;

/// Handle to the bridge thread; dropping it stops the bridge.
pub struct Clipboard {
    set: mpsc::Sender<String>,
}

impl Clipboard {
    /// Put text on the desktop clipboard (it does not echo back as a change).
    pub fn set(&self, text: String) {
        let _ = self.set.send(text);
    }
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    utf8: Atom,
    text: Atom,
    text_plain: Atom,
    incr: Atom,
    property: Atom,
    password_hint: Atom,
}

/// Start watching the desktop clipboard. `on_change` receives new text copied
/// on the computer. Fails when no X display is reachable.
pub fn start(on_change: impl Fn(String) + Send + 'static) -> Result<Clipboard, String> {
    let (conn, screen) = x11rb::connect(None).map_err(|e| format!("no X display for the clipboard: {e}"))?;
    let (set_tx, set_rx) = mpsc::channel::<String>();
    let root = conn.setup().roots[screen].root;
    std::thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || {
            if let Err(error) = run(&conn, root, &set_rx, &on_change) {
                eprintln!("clipboard: bridge stopped: {error}");
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Clipboard { set: set_tx })
}

fn run(
    conn: &impl Connection,
    root: Window,
    set_rx: &mpsc::Receiver<String>,
    on_change: &impl Fn(String),
) -> Result<(), Box<dyn std::error::Error>> {
    let intern = |name: &[u8]| -> Result<Atom, Box<dyn std::error::Error>> {
        Ok(conn.intern_atom(false, name)?.reply()?.atom)
    };
    let atoms = Atoms {
        clipboard: intern(b"CLIPBOARD")?,
        targets: intern(b"TARGETS")?,
        utf8: intern(b"UTF8_STRING")?,
        text: intern(b"TEXT")?,
        text_plain: intern(b"text/plain;charset=utf-8")?,
        incr: intern(b"INCR")?,
        property: intern(b"ENZREMOTE_CLIPBOARD")?,
        password_hint: intern(b"x-kde-passwordManagerHint")?,
    };
    let window = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        0, 0, 1, 1, 0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    conn.xfixes_query_version(5, 0)?.reply()?;
    conn.xfixes_select_selection_input(window, atoms.clipboard, SelectionEventMask::SET_SELECTION_OWNER)?;
    conn.flush()?;

    // Text we own (set from a phone), served to paste requests.
    let mut owned: Option<String> = None;
    loop {
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::XfixesSelectionNotify(e) if e.selection == atoms.clipboard && e.owner != window && e.owner != 0 => {
                    owned = None;
                    // Ask for the formats first to honour password-manager hints.
                    conn.convert_selection(window, atoms.clipboard, atoms.targets, atoms.property, e.selection_timestamp)?;
                    conn.flush()?;
                }
                Event::SelectionNotify(e) if e.requestor == window && e.property != 0 => {
                    let reply = conn
                        .get_property(true, window, atoms.property, AtomEnum::ANY, 0, (MAX_CLIPBOARD_TEXT as u32).div_ceil(4))?
                        .reply()?;
                    if e.target == atoms.targets {
                        let offered: Vec<Atom> = reply.value32().map(|v| v.collect()).unwrap_or_default();
                        if offered.contains(&atoms.password_hint) {
                            continue;
                        }
                        let target = if offered.contains(&atoms.utf8) || offered.is_empty() { atoms.utf8 } else { u32::from(AtomEnum::STRING) };
                        conn.convert_selection(window, atoms.clipboard, target, atoms.property, x11rb::CURRENT_TIME)?;
                        conn.flush()?;
                    } else if reply.type_ != atoms.incr && reply.bytes_after == 0 {
                        // Larger (incremental or truncated) contents are not shared.
                        if let Ok(text) = String::from_utf8(reply.value)
                            && !text.is_empty()
                        {
                            on_change(text);
                        }
                    }
                }
                Event::SelectionRequest(e) => {
                    let property = if e.property == 0 { e.target } else { e.property };
                    let served = match &owned {
                        Some(text) if e.selection == atoms.clipboard => {
                            if e.target == atoms.targets {
                                let formats = [atoms.targets, atoms.utf8, atoms.text, atoms.text_plain, u32::from(AtomEnum::STRING)];
                                conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &formats)?;
                                true
                            } else if [atoms.utf8, atoms.text, atoms.text_plain, u32::from(AtomEnum::STRING)].contains(&e.target) {
                                conn.change_property8(PropMode::REPLACE, e.requestor, property, e.target, text.as_bytes())?;
                                true
                            } else {
                                false
                            }
                        }
                        _ => false,
                    };
                    let notify = SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: e.time,
                        requestor: e.requestor,
                        selection: e.selection,
                        target: e.target,
                        property: if served { property } else { 0 },
                    };
                    conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
                    conn.flush()?;
                }
                Event::SelectionClear(e) if e.selection == atoms.clipboard => owned = None,
                _ => {}
            }
        }
        match set_rx.try_recv() {
            Ok(text) => {
                conn.set_selection_owner(window, atoms.clipboard, x11rb::CURRENT_TIME)?;
                conn.flush()?;
                owned = Some(text);
            }
            Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}
