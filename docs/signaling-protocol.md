# Signaling Protocol Reference

The signaling protocol carries session control and WebRTC negotiation messages
over a WebSocket connection. All messages are versioned JSON with
`deny_unknown_fields` semantics. Unknown types, fields, and versions are
rejected rather than ignored.

## Transport

- WebSocket (text frames only; binary frames are rejected)
- Default port: 9470 (configurable via `ENZREMOTE_SIGNALING_ADDR`)
- TLS by default: a persistent self-signed host identity is generated under
  `$ENZREMOTE_DATA_DIR/tls/` (or supplied via `ENZREMOTE_TLS_CERT`/`_KEY`).
  Phones pin its SHA-256 certificate fingerprint at pairing. The same port
  accepts plaintext only from loopback peers (CLI tools, `adb reverse`);
  `ENZREMOTE_TLS=off` disables TLS for development.
- LAN discovery via mDNS (`_enzremote._tcp.local.`). TXT: `v`, `id` (stable
  host ID), `name`, `fp` (certificate SHA-256), `tls=1`, `protocol` (`wss`/`ws`).
  Container, VM and VPN interfaces are not advertised.

## Identity, pairing and persistent connections

Each host has a random, persistent 32-hex `host_id` (`$ENZREMOTE_DATA_DIR/host_id`)
returned in `hello_ack` and mDNS. Phones key paired computers by this ID, not
by address, and keep one long-lived connection per paired computer.

By default (`ENZREMOTE_REQUIRE_PAIRING=1`) every `hello` must present a device
credential issued by this host, and the credential is bound to the `device_id`
it was issued to. Credentials are stored as SHA-256 digests in
`$ENZREMOTE_DATA_DIR/device_tokens`. `ENZREMOTE_ALLOW_ANONYMOUS_LOOPBACK=1`
admits unauthenticated loopback tools for development only.

An unpaired phone sends `pair_request` instead of `hello`:

```json
{"type":"pair_request","v":1,"device_id":"android-1a2b","device_name":"Xiaomi Mi 10T","client_nonce":"<32 hex>"}
```

The host replies `pair_pending` (`host_id`, `host_name`, `host_nonce`,
`timeout_secs`) and asks a person on the computer to approve it — a desktop
notification with Pair/Decline actions, or `enzremote-signaling approve <code>`.
Both sides display a six-digit code:

```
code = u32_be(SHA-256("enzremote-pair-v1\n" + fp + "\n" + client_nonce + "\n" + host_nonce)[0..4]) mod 1_000_000
```

`fp` is the lowercase certificate fingerprint each side observes (empty over
plaintext loopback), so a man-in-the-middle certificate yields different codes.
The host answers `pair_result` (`accepted`, `device_token` or `reason`). After
acceptance the same connection continues with `hello` using that token. A
decline starts a 30-second per-address cooldown; at most three requests wait
at once and each times out after 120 seconds or when the phone disconnects.

A host-displayed QR code (`enzremote-signaling pair`) carries a one-time
`token`. Sending it as `pair_request.pairing_token` approves without a prompt.
QR payload (`v:2`): `id`, `name`, `host`, `addrs`, `port`, `token`, `tls`, `fp`.

`hello_reject` carries `code:"not_paired"` when the credential is unknown,
revoked or belongs to another device; clients should offer to pair again rather
than retrying. Authenticated clients may send `{"type":"unpair","v":1}`; the
host revokes the credential and replies `unpaired` before closing. Host-side
`enzremote-signaling unpair <device-id>` sends `unpaired` to a connected phone.

Host CLI (talks to the running server over `$XDG_RUNTIME_DIR/enzremote/control.sock`,
same-UID only): `pair [--json]`, `pending`, `approve [CODE]`, `decline [CODE]`,
`devices`, `unpair ID`.
- Maximum message size: 128 KiB (WebSocket-level) / 64 KiB (application-level)

## Connection Lifecycle

1. **Connect**: client opens WebSocket to host
2. **Hello**: client sends `hello` with device_id and token within 5 seconds
3. **HelloAck/HelloReject**: host validates token and responds
4. **Authenticated session**: client sends commands, host responds
5. **Disconnect**: session ownership released, connection cleaned up

## Protocol Version

Current version: **1** (`"v": 1`)

All messages include a `v` field with the protocol version.

## Computer-home flow

After `hello_ack`, request `computer_info` and `session_list`. Neither starts
media. Select an app with `app_launch`, wait for `session_created`, then send
`stream_start`. Home uses `stream_stop`; Close uses `session_stop`. A returning
connection must `session_resume` before stopping or streaming an existing app.

### computer_info

```json
{"type":"computer_info","v":1}
```

Responds with `computer_info_result`, containing `v:1` and `computer`:
`hostname`, `os`, `cpu_model`, `cpu_threads`, nullable `cpu_usage_percent`,
`memory_total_bytes`, `memory_available_bytes`, `disk_total_bytes`,
`disk_available_bytes`, `uptime_secs`, and `apps`. Each app has `id`, `name`,
`description`, `available`. Memory/storage are bytes; disk refers to `/`.
CPU usage is a sample delta, absent until a baseline exists. Recommended polling
interval: 3 seconds while the home screen is visible. `shared_desktop` is a
boolean (missing means false) identifying an explicitly configured desktop host.
On such a host, app launch borrows an existing window or opens the allowlisted
app in that desktop, rather than creating a private compositor.

### app_list

```json
{"type":"app_list","v":1}
```

Responds with `app_list_result`: `revision` and `apps`, sorted by name. Each
app has `id` (catalog ID, `[a-z0-9-]`, stable across restarts), `name`,
`description`, `kind` (`iced` or `desktop`), optional `icon` (icon revision),
`keywords` and `categories` (freedesktop main categories). The catalog lists
the installed enzUI ICED apps and every launchable freedesktop `.desktop`
entry in the XDG data directories (user, system, Flatpak exports), filtered
like a desktop launcher: `Type=Application`, not `NoDisplay`/`Hidden`,
`OnlyShowIn`/`NotShowIn` against `XDG_CURRENT_DESKTOP`, `TryExec` and the
`Exec` program present, localized `Name`/`Comment` for the host locale.
`Terminal=true` entries are not offered yet. `computer_info.app_revision`
changes whenever the catalog does, so clients refetch only then.

### app_icons

```json
{"type":"app_icons","v":1,"app_ids":["org-gnome-nautilus-7555bb"]}
```

1–32 catalog IDs per request (one request stays within the rate limit).
Responds with `app_icons_result`: `icons`, each `{app_id, icon, png}` where
`png` is a base64 128×128 PNG rendered from the user's icon theme (then
hicolor, Adwaita, pixmaps; SVG preferred). IDs without an icon are omitted.
Clients cache by `icon` revision.

### app_launch

```json
{"type":"app_launch","v":1,"session_id":"files-app-phone1","app_id":"files-app","width":1078,"height":2046,"scale":2.75,"refresh_hz":60}
```

IDs follow `[a-z0-9_-]`, 1–64 bytes. Dimensions/scale/refresh have the same limits
as `session_request`. The host resolves the allowlisted app ID to an installed
ICED binary. No client executable path or shell expression is accepted here.
Returns `session_created` or `session_error`; never starts video implicitly.
Any catalog ID may be launched. The host builds the command from the entry's
`Exec` line (field codes expanded for a launch without files) and adds
Wayland hints for Electron, Firefox, SDL and Java. A single-instance app
whose D-Bus name (its reverse-DNS desktop ID) is already owned on the desktop
is refused with a `session_error` asking the user to close it on the computer:
a second instance would only open a window there, or would share and could
overwrite the running instance's per-user state (for example unsaved drafts).
`session_list_result` sessions carry `app_id` when the host launched them.
Demo-mode hosts reject `app_launch` rather than pretending an app was launched.
Unknown/uninstalled apps return a user-visible error. This does not restrict the
legacy `session_request` arbitrary-command API; that remains a release blocker.

### desktop_attach

```json
{"type":"desktop_attach","v":1,"session_id":"desktop-phone1","output":"WAYLAND-1"}
```

Requires a configured real desktop and authentication. `output` is optional;
otherwise the focused eligible output is selected. No phone width, height or
scale is accepted. Returns `session_created` with native desktop geometry.
Internal loan outputs and unsupported transforms are rejected. Media still
requires `stream_start`. See [desktop ownership and presentation](shared-desktop.md).

## Client messages

### hello
Authenticate and declare capabilities.
```json
{"type":"hello","v":1,"device_id":"phone-1","token":"abc123","capabilities":{"input":["touch","pointer"],"video_codecs":["h264"],"resize":true,"clipboard":false,"audio":false}}
```
- `device_id`: 1-128 chars, `[a-zA-Z0-9._-]` only
- `token`: 1-512 chars, no ASCII control characters
- `capabilities`: optional, defaults to empty
- `capabilities.input`: known types: `pointer`, `pointer_relative`, `touch`, `keyboard`, `scroll`
- `capabilities.video_codecs`: known codecs: `h264`, `av1`, `vp8`

### session_request
Create a new session.
```json
{"type":"session_request","v":1,"session_id":"notes","command":["/usr/bin/app"],"width":390,"height":844,"scale":2.0,"refresh_hz":120}
```
- `session_id`: 1-64 chars, `[a-z0-9_-]` only
- `command`: 1-64 elements, each 1-4096 bytes, no NUL
- `width`/`height`: 240-8192
- `scale`: 0.5-4.0 (default: 1.0)
- `refresh_hz`: 15-240 (default: 60)

### session_resume
Resume an interrupted session (ownership transfers to sender).
```json
{"type":"session_resume","v":1,"session_id":"notes"}
```

### session_resize
Resize a session viewport (requires ownership).
```json
{"type":"session_resize","v":1,"session_id":"notes","width":844,"height":390,"scale":2.0,"refresh_hz":120}
```

### session_stop
Stop a session (requires ownership).
```json
{"type":"session_stop","v":1,"session_id":"notes"}
```

### session_inspect
Query detailed session state.
```json
{"type":"session_inspect","v":1,"session_id":"notes"}
```

### session_list
List all active sessions.
```json
{"type":"session_list","v":1}
```

### sdp
Exchange WebRTC SDP (requires session ownership).
```json
{"type":"sdp","v":1,"session_id":"notes","sdp_type":"offer","sdp":"v=0..."}
```
- `sdp_type`: `"offer"` or `"answer"`
- `sdp`: max 32768 bytes

### ice
Send an ICE candidate (requires session ownership).
```json
{"type":"ice","v":1,"session_id":"notes","candidate":"candidate:...","sdp_m_line_index":0}
```
- `candidate`: max 4096 bytes

### stream_start
Start media streaming for a session (requires ownership).
```json
{"type":"stream_start","v":1,"session_id":"notes"}
```

### stream_stop
Stop media; a borrowed window returns to its desktop slot. Applications and the
compositor remain running. Requires ownership.
```json
{"type":"stream_stop","v":1,"session_id":"notes"}
```

### stream_pause

```json
{"type":"stream_pause","v":1,"session_id":"notes"}
```

Requires ownership. Stops media while retaining a shared app loan for imminent
resize/recovery. Responds with `stream_stopped`; clients should wait for that
acknowledgement before closing their peer. It is not the user Home action.
Private sessions use their normal non-destructive media-stop behavior.

### probe
Query host capabilities and server status.
```json
{"type":"probe","v":1}
```

### ping
Application-level ping for RTT measurement.
```json
{"type":"ping","v":1,"timestamp_ms":1721000000000}
```

## Host Messages (17 types)

### hello_ack
Authentication accepted.
```json
{"type":"hello_ack","v":1,"capabilities":{"input":["pointer","keyboard","touch","scroll"],"video_codecs":["h264"],"resize":true,"clipboard":false,"audio":false},"server_version":"0.1.0","host_id":"cb7c…","host_name":"Studio PC"}
```

### hello_reject
Authentication rejected.
```json
{"type":"hello_reject","v":1,"reason":"invalid authentication token","code":"not_paired"}
```

### session_created
Session successfully created.
```json
{"type":"session_created","v":1,"session_id":"notes","width":390,"height":844,"scale":2.0,"refresh_hz":120}
```

### session_resumed
Session successfully resumed with current viewport configuration.
```json
{"type":"session_resumed","v":1,"session_id":"notes","width":390,"height":844,"scale":2.0,"refresh_hz":120}
```

### session_resized
Session viewport resized.
```json
{"type":"session_resized","v":1,"session_id":"notes","width":844,"height":390,"scale":2.0,"refresh_hz":120}
```

### session_stopped
Session stopped.
```json
{"type":"session_stopped","v":1,"session_id":"notes"}
```

### session_inspection
Detailed session state with negotiation status.
```json
{"type":"session_inspection","v":1,"session_id":"notes","width":390,"height":844,"scale":2.0,"refresh_hz":120,"compositor_alive":true,"app_alive":true,"stream_alive":false,"command":["/usr/bin/app"],"negotiation_state":"Idle"}
```
Negotiation states: `Idle`, `WaitingForOffer`, `OfferSent`, `Negotiating`, `Connected`, `Failed("reason")`

### session_list_result
List of active sessions.
```json
{"type":"session_list_result","v":1,"sessions":[{"session_id":"notes","width":390,"height":844,"active":true,"uptime_secs":120,"scale":2.0,"refresh_hz":120,"owner_device_id":"phone-1","negotiation_state":"Connected"}]}
```

### session_error
Session operation failed.
```json
{"type":"session_error","v":1,"session_id":"notes","reason":"not the owner of this session"}
```

### sdp_relay
Relayed SDP from another peer.
```json
{"type":"sdp_relay","v":1,"session_id":"notes","sdp_type":"answer","sdp":"v=0..."}
```

### ice_relay
Relayed ICE candidate.
```json
{"type":"ice_relay","v":1,"session_id":"notes","candidate":"candidate:...","sdp_m_line_index":0}
```

### stream_started
Media stream started.
```json
{"type":"stream_started","v":1,"session_id":"notes"}
```

### stream_stopped
Media stream stopped.
```json
{"type":"stream_stopped","v":1,"session_id":"notes"}
```

### probe_result
Host capabilities and server status.
```json
{"type":"probe_result","v":1,"pipewire_running":true,"render_nodes":["/dev/dri/renderD128"],"h264_encoders":["vah264enc"],"av1_encoders":[],"active_sessions":2,"active_connections":1,"server_uptime_secs":3600,"max_sessions":8,"hostname":"desktop","server_version":"0.1.0"}
```
Optional fields: `hostname`, `server_version` (both omitted if not available).

### pong
Response to ping.
```json
{"type":"pong","v":1,"timestamp_ms":1721000000000}
```

### error
General protocol error.
```json
{"type":"error","v":1,"reason":"binary messages are not supported"}
```

### session_event
Asynchronous session event notification (server-push).
```json
{"type":"session_event","v":1,"session_id":"notes","event":"app_exited"}
```
Event kinds: `app_exited`, `compositor_crashed`, `stream_ended`, `stopped_externally`,
`garbage_collected`, `ownership_transferred`, `resized`.

The host watches every private app session. When the app has neither a
running launch process nor a mapped window for 3 s (launchers that hand off to
a child are allowed that grace), the host stops the stream and session and sends
`app_exited` (or `compositor_crashed`). An exit within about 20 s of launch is a
failed start and carries an optional `detail`: the app's last error output,
without compositor/toolkit noise, at most 300 characters:
```json
{"type":"session_event","v":1,"session_id":"org-example-app-1a2b3c-x1","event":"app_exited","detail":"bwrap: Can't find source path …"}
```
Clients show it as "<App> couldn't start on the computer." followed by the
detail, and offer Home instead of retrying.

Flatpak entries are launched with `--no-documents-portal` when the document
portal's FUSE mount (`$XDG_RUNTIME_DIR/doc`) is missing, since `flatpak run`
otherwise aborts in bwrap before the app starts.

For shared apps, `stopped_externally` also reports local takeover or loss of the
compositor loan. It ends remote control, not the desktop app process. Clients
must cancel automatic media recovery/resize and require an explicit user action
before borrowing again; PipeWire EOF is not a reliable ownership notification.

## Session Ownership

Sessions are bound to the device that created or resumed them:
- `session_request` -> creator becomes owner
- `session_resume` -> resuming device becomes new owner
- Ownership is checked for: `sdp`, `ice`, `session_resize`, `session_stop`,
  `stream_start`, `stream_stop`, `stream_pause`
- Ownership is released on disconnect or session GC

## File transfer

Paired phones and the computer exchange files over the same pinned link
(KDE Connect style). Transfer IDs are 8-64 `[A-Za-z0-9_-]` characters chosen
by the sender. Pieces carry at most 40 KiB (`FILE_CHUNK_SIZE`) as base64, so
every message stays under the 64 KiB parse limit; files may be up to 16 GiB.

Phone to computer (saved to the XDG Downloads folder):
```json
{"type":"file_offer","v":1,"transfer_id":"9f2c…","name":"IMG_0042.jpg","size":3145728,"mime":"image/jpeg"}
→ {"type":"file_accepted","v":1,"transfer_id":"9f2c…","chunk_size":40960}
{"type":"file_chunk","v":1,"transfer_id":"9f2c…","offset":0,"data":"<base64>"}
→ {"type":"file_progress","v":1,"transfer_id":"9f2c…","received":40960}
{"type":"file_complete","v":1,"transfer_id":"9f2c…","sha256":"<hex>"}
→ {"type":"file_saved","v":1,"transfer_id":"9f2c…","name":"IMG_0042 (1).jpg"}
```
Chunks must be sequential; the phone keeps up to 8 unacknowledged. Bytes go to
a hidden `.part` file that is only renamed (never over an existing file) after
the size and SHA-256 match; names are reduced to a plain file name. Any error
returns `file_error` and deletes the part file, as does a disconnect. The
computer shows a notification with Open / Show in Folder.

Computer to phone (`enzremote-signaling send [--device ID] FILE…`, the "Send to
Phone" file-manager entry): the host pushes
`{"type":"file_offer",…,"sha256":"<hex>"}`; the phone pulls pieces with
`{"type":"file_pull","transfer_id":…,"offset":N}` → `file_data`
(`offset`, `data`, `eof`), up to 8 in flight, verifies the hash, and confirms
with `file_received` → `file_finished`. Either side may send `file_cancel`
(→ `file_cancelled`). Phones save to Downloads (MediaStore, pending until
verified).

## Clipboard

`{"type":"clipboard_sync","v":1,"enabled":true}` (→ `clipboard_ack`) opts a
device in to the computer's clipboard: when it changes, the host pushes
`{"type":"clipboard","v":1,"text":"…"}`. A phone sets the computer's clipboard
with the same `clipboard` message (→ `clipboard_ack`). Text is limited to
48 KiB. The installed host uses the Shell clipboard service. Pending text
from a phone stays in memory and expires after five seconds. Delivery
requires an active, unlocked desktop session and clipboard permission for
the connected phone.

Standalone hosts can use the X11 `CLIPBOARD` bridge, which skips entries
marked `x-kde-passwordManagerHint`. Set `ENZREMOTE_CLIPBOARD=0` to disable
clipboard sharing. `hello_ack.capabilities.clipboard` reports availability.
Android reads changed clipboard text when the app gains focus and skips
clips marked sensitive.

## Phone notifications, battery, find my phone

Phones with notification access mirror their notifications:
```json
{"type":"phone_notification","v":1,"key":"0|com.whatsapp|1|…","app":"com.whatsapp","app_name":"WhatsApp",
 "title":"Anna","text":"Lunch?","icon":"<base64 PNG ≤32 KiB>","actions":[{"id":"0","title":"Reply","reply":true}]}
{"type":"phone_notification_removed","v":1,"key":"…"}
{"type":"phone_status","v":1,"battery":64,"charging":false}
```
Each is answered with `{"type":"ack","v":1}`. The host shows them through
`org.freedesktop.Notifications` (app icon as `image-data`, replaced on
update, closed when removed on the phone). Choosing a button pushes
`{"type":"notification_action","v":1,"key":…,"action_id":"0"}`; reply actions
add `"text"` from the server's inline reply (KDE) or a reply dialog (GNOME).
Dismissing on the computer pushes `notification_dismiss`. A phone's
notifications close when it disconnects. Battery appears in
`enzremote-signaling devices`; at 15 % and not charging the computer warns once.

Find my phone: `enzremote-signaling ring [--device ID] [--stop]` (and the
"Find My Phone" launcher entry) pushes `{"type":"ring","v":1,"ringing":true}`.
Android rings at alarm volume with a full-screen "Found it" screen; iOS plays an
alarm while the app is open. The Android notification listener keeps computer
links up in the background, so mirroring and ringing work with the app closed.

## Media remote and desktop input

`{"type":"media_state_request","v":1,"player":null,"known_art":"<art_id>"}` →
`{"type":"media_state","v":1,"state":{"players":[{"id":"spotify","name":"Spotify"}],"player":"spotify",
"status":"Playing","title":…,"artist":…,"album":…,"length_ms":…,"position_ms":…,"can_seek":true,
"art_id":"…","art":"<base64 JPEG (PNG from older hosts), only when art_id differs from known_art>","volume":40,"muted":false}}`.
Players are the desktop's MPRIS players (the chosen one, else the first
playing one); volume is the default PipeWire sink (`wpctl`, 0-150).
`{"type":"media_command","v":1,"player":…,"command":"play_pause|next|previous|seek|set_volume|toggle_mute","value":…}`
(seek in ms, volume in %) runs it and answers with `media_state`.

`{"type":"remote_input","v":1,"events":[…]}` (1-64 events; exempt from the
message rate limit) drives the computer's own desktop: `{"kind":"move","dx","dy"}`
(relative pixels), `{"kind":"button","button":"left|right|middle","pressed"}`,
`{"kind":"click","button"}`, `{"kind":"scroll","dx","dy","stop"}` (smooth pixels),
`{"kind":"key","code":<evdev>,"pressed"}`, `{"kind":"text","text"}`. GNOME uses
Mutter's RemoteDesktop session (closed after 60 s idle or on disconnect);
wlroots/Hyprland uses the `enzremote-input-wl` virtual pointer/keyboard helper.
Answered with `ack`, or `error` when no backend is available.

## Second display

When `computer_info.second_display` is true (GNOME), a phone or tablet can
extend the desktop: `{"type":"second_display_start","v":1,"session_id":"display-…","width":588,"height":1142}`
creates a virtual monitor (Mutter RemoteDesktop + ScreenCast `RecordVirtual`;
its size follows the PipeWire format webrtc-server requests) and answers
`session_created`. The normal `stream_start` / SDP flow then streams it.
Touch is sent over signaling as
`{"type":"display_input","v":1,"session_id":…,"events":[{"kind":"down|move","slot":0,"x":…,"y":…},{"kind":"up","slot":0}]}`
(also `pointer`, `button`, `scroll`; exempt from the rate limit), injected
as touch on that monitor so the desktop handles gestures. `session_stop` or
a disconnect removes the monitor; second displays are not listed as sessions.
Phones request about 1.5 desktop pixels per dp.

### Desktop view (whole screen)

`computer_info.monitors` lists the monitors that can be shown on a phone
(`connector`, `name`, `width`, `height` in the rotated orientation, `primary`;
primary first, refreshed at most every 5 s). Adding `"view":true` and
optionally `"output":"<connector>"` (default: primary) to
`second_display_start` records that monitor instead (`RecordMonitor`, no
virtual monitor). `width`/`height` then bound the streamed size: the host
keeps the monitor's shape, never upscales, scales on the GPU
(`ENZREMOTE_OUTPUT_SIZE`) and answers `session_created` with the size used.
`display_input` coordinates are in that size and are scaled back to monitor
pixels. Only send `view` to hosts that list `monitors` (older hosts reject
unknown fields). Android: one finger is touch on the desktop (held 70 ms so
a pinch never taps), pinch zooms locally, an unzoomed two-finger drag
scrolls (`pointer` + `scroll`).

`computer_info` also carries `net_rx_bytes_per_sec` / `net_tx_bytes_per_sec`
(physical interfaces) for the home screen.

## Phone screen and camera on the computer (uplink)

The reverse direction: the phone sends video and the computer receives it.
`{"type":"uplink_start","v":1,"uplink_id":"…","kind":"screen|camera","sdp":<send-only video offer>}`
spawns the `enzremote-uplink` receiver (GStreamer webrtcbin; path from
`ENZREMOTE_UPLINK_HELPER`) and is answered with `uplink_answer` (`sdp`).
Trickle ICE flows both ways as `uplink_ice` (`candidate`, `sdp_m_line_index`).

- `screen`: a window titled after the phone shows the mirror, with Back /
  Home / Recents buttons. Clicks, drags, wheel and keys in the window are
  pushed to the phone as `{"type":"uplink_input","uplink_id",…,"event":{…}}`
  with normalized coordinates: `{"kind":"tap","x","y"}`,
  `{"kind":"swipe","x","y","x2","y2"}`, `{"kind":"scroll","dy"}`,
  `{"kind":"global","action":"back|home|recents"}` (right-click and Escape
  are Back), `{"kind":"text","text"}`, `{"kind":"key","key":"enter|backspace"}`.
  Android applies them with its "control from computer" accessibility
  service when the user enabled it; mirroring works without it.
- `camera`: a PipeWire `Video/Source` node `enzremote-camera` named
  "<phone> (enzimOS Remote)" for OBS, browsers and other PipeWire camera
  users. It always publishes 1280x720 YUY2 (letterboxed) because camera
  consumers cannot follow the phone encoder's resolution changes.

`uplink_stop` (either side) or a disconnect ends it; the host answers or
pushes `uplink_stopped` when the window is closed or the receiver exits.
Phones set a bitrate floor (1.2–1.5 Mbit/s): the receiver sends little
congestion feedback, and without it the sender stays at its start rate.
Android mirrors through a private auto-mirror display, which other apps
(such as SecondScreen) do not mistake for a connected monitor.

## Text messages (SMS)

Opt-in on the phone ("Text messages on computers"; SMS permission, contacts
optional). The phone reports `{"type":"sms_status","v":1,"enabled":true}` on
connect and when the setting changes, and `{"…","changed":true}` (debounced)
when its SMS store changes. The host asks with
`{"type":"sms_request","v":1,"request_id":"sms-…","request":{…}}` and the
phone answers `{"type":"sms_result","v":1,"request_id":…,"result":{"ok":true,…}}`
(acknowledged with `ack`; only the phone that was asked may answer, 20 s
timeout). Requests:

- `{"op":"conversations","limit":50}` → `conversations`: `thread_id`,
  `address`, `name` (contacts permission), `snippet`, `date` (ms), `sent`, `unread`.
- `{"op":"thread","thread_id":…,"limit":80}` → `messages` oldest first:
  `id`, `address`, `body`, `date`, `sent`, `status` (`done|sending|failed|draft`).
  Results stay below ~90 KB to fit the 128 KiB message limit.
- `{"op":"send","address","text"}` → `{"ok":true}`; Android records the
  text in Sent itself. MMS is not included.

Desktop apps use the control socket: `{"cmd":"sms_devices"}` lists sharing
phones with a `revision` that increases on each change report (poll it
cheaply), and `{"cmd":"sms","device_id"?,"request":{…}}` forwards one request.
`enzremote-messages` (GTK/libadwaita, "Phone Messages") is the desktop UI;
`enzremote-messages --to NUMBER` or an `sms:` link opens a conversation.

## Rate Limiting

The server enforces a per-connection message rate limit (default: 60 msg/s).
Exceeding the limit returns an error response. `file_chunk` and `file_pull`
are exempt; they are bounded by the offered size of a known transfer.

## Security

- `device_id` restricted to `[a-zA-Z0-9._-]` (prevents log injection)
- `token` validated: no ASCII control characters (prevents log injection)
- Capabilities validated: only known input types and video codecs accepted
- WebSocket message size limited to 128 KiB at transport level
- Application-level message size limited to 64 KiB
- Binary WebSocket frames explicitly rejected
- Duplicate device IDs rejected (atomic check-and-register)
- Semaphore-based connection limiting (prevents TOCTOU)
- Paired device credentials (digest-stored, bound to device_id); optional
  static `ENZREMOTE_AUTH_TOKEN` for automation
- Host messages validated with same rigor as client messages
- Broadcast channel errors handled (lagged receivers logged, closed channel breaks loop)
