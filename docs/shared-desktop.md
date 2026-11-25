# Shared desktop and Android

An explicitly configured shared-desktop fork supports two Android sources:

- **An app:** borrow its existing Wayland window and unsaved state. A protected
  snapshot stays in its desktop slot while the real window uses a mobile-sized
  headless output and an independent remote seat. Home returns the window; it
  does not close the application.
- **Open desktop:** stream/control a real desktop output, including the shell and
  cursor. Android rotation, zoom and keyboard presentation do not resize that
  output or its windows. Home stops media without closing desktop applications.

Without shared-desktop configuration, the existing private-per-app compositor
mode remains available. A configured but unavailable desktop fails explicitly;
there is no silent replacement-app fallback.

## Preparing a desktop

Run the enzimOS Shell session with its matching compositor, portal backend,
placeholder and snapshot helpers. Enable Phone Link from Settings. The
installed service uses the desktop's runtime metadata to select the session
and verifies the compositor identity and control socket before each loan.
Opening an app sends an `eval` request on the compositor socket from
`enzremote-signaling`, so the Shell session must list that executable in
`ENZUI_CONTROL_EXES`.

Named-output capture requires a compatible portal backend. An unavailable
output or portal fails the request, including when automatic capture source
selection is enabled. Pairing and per-device access still apply.

## Source and lifecycle contracts

`computer_info.shared_desktop` exposes the configured source capability. On that
host, `app_launch` finds an existing matching desktop app first. If absent, the
allowlisted desktop launcher opens it in the selected desktop. It does not start
a replacement nested compositor. The app session owns the loan, not the process.

`desktop_attach` takes a session ID and optional output name, **not phone
geometry**. The server selects the named or focused eligible output and reports
its native dimensions, scale and refresh. Internal `ENZREMOTE-*` loan outputs
are not eligible. Transformed outputs are currently rejected rather than mapped
incorrectly. Android currently uses the default output; a monitor picker is not
implemented. Local output geometry changes or removal end the old desktop stream
through the source watcher; an explicit later Resume reads the new geometry.
The host never silently switches outputs or keeps using a stale input canvas.

`stream_pause` stops media while retaining a loan for an in-session viewport or
media-recovery change. Android waits for `stream_stopped` before closing its peer;
closing it first can race an SCTP failure against intentional pause. A missing
pause acknowledgement has a bounded client timeout. User Home/Apps uses
`stream_stop`, which returns the borrowed window. `session_stop` forgets a shared
session but never terminates its desktop app or compositor. Where advertised,
Android first drains accepted input on the ordered data channel, with a bounded
warning-and-release fallback; this is not an app-render or document-save
acknowledgement.

Recoverable ICE/peer failures have a bounded 60-second loan grace period. A later
resume/activity invalidates an older failure's release timer, including when a
subsequent negotiation also fails. Real signaling-controller loss, fatal failure,
local Resume here and placeholder loss restore the app. A per-loan watcher
reports external return even when capture continues with black frames. Android
stops retrying and displays a return-to-computer message. Timeout restoration is
not claimed visually seamless when an application stops submitting buffers.

## Input and presentation

Borrowed apps use a capability-bound independent seat. Whole-desktop control is
intentionally global within the selected desktop; its virtual pointer is tied to
the chosen output. Missing or removed outputs fail closed. The media data channel
cannot use legacy global list/focus/launch/resize/clipboard operations to bypass
an app loan's input path.

The mobile app keeps its hardware SurfaceView. Pinch/pan gestures are measured in
untransformed container coordinates to avoid feeding a changing View transform
back into the scale detector. Whole-desktop keyboard presentation is a local
crop around the last tap (or an upper-desktop fallback for shell-menu taps), not
native caret tracking. It restores the preceding presentation when the keyboard
closes.

App keyboard resizing keeps a fixed encoded canvas where supported. Modest
IME-induced side-navigation changes may scale presentation rather than rebuild
the stream. Very short landscape work areas retain at least 160 logical pixels
and are fitted locally so app title/footer controls cannot consume the entire
editor. A compact, still-labelled dock retains 48dp minimum targets in landscape
IME mode. These presentation compromises are distinct from changing the desktop
window's saved geometry.
