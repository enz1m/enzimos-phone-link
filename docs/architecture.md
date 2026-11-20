# Architecture

Phone Link has an authenticated signaling service, a session manager and
separate media workers. The installed service runs within the active enzUI
Wayland session. Shell Settings exposes the host's [local settings API](settings-api.md)
for pairing, permissions and session management.

## Pairing and control

The host requires TLS and an approved device. The Android client pins the host
certificate when it pairs. A QR secret expires and is single use. Each paired
device gets its own revocable credentials and sharing permissions. Discovery
advertises the host without sending pairing credentials.

The [signaling protocol](signaling-protocol.md) handles discovery, app catalog
requests, session setup and lifecycle. Once a stream is established, media and
[input messages](control-protocol.md) travel over WebRTC. The host suspends
streaming and remote input when the local desktop is locked.

## Capture and input

A [shared desktop](shared-desktop.md) app stream moves the selected window onto
a phone-sized output with its own remote seat. The host borrows the window for
the duration of the stream. Returning home on the phone returns the window to
the desktop. A whole-desktop stream uses the selected output and its native
geometry until the stream stops.

The media worker gets a scoped PipeWire connection from the desktop portal.
For named outputs, the compositor and portal must agree on the selected
output. Capture fails if the source cannot be scoped. It does not fall back to
an unrestricted screen capture. Whole-desktop input applies to the selected
desktop, while a borrowed app uses its own seat.

A private portal session limits activation to the services needed for capture.
The host and worker release the portal session, input authority and media
resources when the stream ends. Clipboard sharing uses the Shell clipboard
service and requires permission from both ends.

## Protocol references

- [Signaling](signaling-protocol.md)
- [Input and media control](control-protocol.md)
- [Shell Settings integration](settings-api.md)
- [ScreenCast portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html)
