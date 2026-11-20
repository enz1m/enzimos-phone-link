# Control Protocol

The WebRTC `control` data channel carries ordered, versioned JSON messages after
the DTLS fingerprint has been bound to an authenticated session. Unknown fields,
versions, and message types are rejected rather than ignored.

The version-1 pointer message is:

```json
{"v":1,"type":"pointer","x":123,"y":456,"click":true}
```

`x` and `y` are logical coordinates in the EIS region sharing the capture
stream's mapping ID. Protocol parsing limits each coordinate to less than 8192;
libei and the compositor enforce the current region. `click` sends a complete
left-button press/release after the absolute motion.

Keyboard messages carry Linux evdev keycodes and explicit state:

```json
{"v":1,"type":"key","code":30,"pressed":true}
```

Keycodes above `KEY_MAX` are rejected. Clients must balance presses and
releases; disconnect and EIS device replacement restore neutral state.

Native multitouch messages preserve up to 16 concurrent slots:

```json
{"v":1,"type":"touch","slot":0,"phase":"down","x":100,"y":200}
```

`phase` is `down`, `move`, `up`, or `cancel`. A slot must begin with `down`, may
receive `move`, and ends with `up` or `cancel`. Coordinates use the same mapped
logical region as pointer events.

The loopback receiver invokes the scoped libei bridge only when sessiond has
provided both a preconnected one-request broker capability and the input helper.
A generated-media transport test without an input grant validates the message
but performs no host input. The portal launcher strips `WAYLAND_DISPLAY`,
`HYPRLAND_INSTANCE_SIGNATURE`, and `DISPLAY` before starting the network media
worker; a broker-enabled worker refuses to run if display access leaks through.
The portal handoff also removes compositor runtime paths and the broker socket
path before starting the consumer.

## Button Messages

Explicit button press and release messages for right-click, middle-click,
and other mouse buttons:

```json
{"v":1,"type":"button","code":272,"pressed":true}
{"v":1,"type":"button","code":272,"pressed":false}
```

`code` is a Linux evdev button code (BTN_LEFT=272, BTN_RIGHT=273,
BTN_MIDDLE=274). Codes above `KEY_MAX` (0x2ff) are rejected. Clients must
balance presses and releases; disconnect restores neutral state.

## Scroll Messages

Scroll messages carry axis deltas for vertical and horizontal scrolling:

```json
{"v":1,"type":"scroll","dx":0.0,"dy":-3.0}
```

`dx` and `dy` are floating-point scroll deltas. Positive `dy` scrolls down,
negative scrolls up. Each delta is capped at an absolute value of 1000.
Non-finite values (NaN, Infinity) are rejected. This is the legacy wheel path;
use the following pixel gesture for finger scrolling.

### Continuous pixel scrolling

```json
{"v":1,"type":"scroll_pixels","x":500.0,"y":800.0,"dx":0.0,"dy":12.5,"stop":false}
{"v":1,"type":"scroll_pixels","x":500.0,"y":800.0,"dx":0.0,"dy":0.0,"stop":true}
```

The anchor `x/y` and deltas `dx/dy` are in **stream pixels**. The anchor stays at
the gesture's starting point so the same scrollable region receives the whole
gesture. Positive deltas scroll right/down; a finger moving up produces positive
`dy`. Coordinates have the same bounds as `pointer`; finite deltas are bounded
by 8192 pixels per message. All fields, including `stop`, are required.

The host maps capture pixels into compositor logical units using the session
output scale. The Wayland worker emits finger-source axes, preserves fractions,
and sends explicit axis stops. It sets the source *after each axis request*,
before the frame, because the pinned compositor replaces per-axis state on
`axis()`. It does not round finger deltas into wheel detents.

The client may generate inertia but must end it on new contact, cancellation,
resize, pause or connection replacement. No mouse button is pressed during a
scroll. Both peers must support `scroll_pixels`; older servers reject the new
variant rather than interpreting it as a drag.

`ENZREMOTE_ICED_SCROLL_COMPAT=1` compensates for Iced clients that interpret
physical pixel-wheel deltas as logical widget offsets. The host enables this
for affected installed apps. Set it to `0` for apps with corrected scroll
handling. This setting leaves the wire units unchanged.

## Relative Pointer Messages

Relative pointer messages carry sub-pixel motion deltas:

```json
{"v":1,"type":"pointer_relative","dx":10.5,"dy":-5.0}
```

`dx` and `dy` are floating-point pixel offsets. Each delta is capped at an
absolute value of 8192. Non-finite values are rejected. These complement the
absolute pointer message for desktop-class mouse input where the cursor is
managed by the compositor.

## Text Input Messages

Direct text insertion bypasses evdev keycodes for mobile virtual keyboard
input:

```json
{"v":1,"type":"text","text":"Hello, world!"}
```

`text` is a UTF-8 string, 1-4096 bytes. NUL bytes are rejected. This uses
libei's text input capability when available. For key-by-key input, use the
keyboard message instead.

## Adaptation Messages

### Input Drain Before Leaving

`stream_config` can advertise `"input_drain":true`. On the same reliable ordered
control channel used for input, the client may then send:

```json
{"v":1,"type":"input_drain","serial":1}
```

`serial` must be in 1..2147483647. The marker shares the dedicated input queue;
its reply cannot bypass earlier queued input on the pipeline callback thread:

```json
{"v":1,"type":"input_drain_ack","serial":1,"applied":true}
```

This acknowledges worker input forwarding, **not** application acceptance,
rendering, clipboard updates or document saving. A queueing/forwarding error
observed since the preceding drain produces `applied:false`; an unavailable/full
queue rejects the marker immediately. Normal shutdown also drains already
received input, but cannot order input still in transit against signaling.

Android Home cancels gestures, commits its local IME draft, stops accepting new
input and waits for the matching drain reply before `stream_stop`. A rejection
or 1.5-second timeout still releases control and warns that recent input may not
have reached the computer. Unsupported peers and an already paused/unavailable
input channel retain the compatibility stop path. Stale replies/timeouts cannot
complete a different drain or a replacement data channel.

### Work-area Resize (Keyboard)

The client first asks whether the current worker supports a fixed capture canvas:

```json
{"v":1,"type":"stream_config"}
```

The response includes `{"v":1,"type":"stream_config","content_resize":true}` or false;
`input_drain` is an independent optional capability described above.
Only an explicitly supported connection uses this path. It requires configured
private compositor endpoints and a matching, unrotated single-output canvas.
Set `ENZREMOTE_CONTENT_RESIZE=0` when starting the host to disable it.

```json
{"v":1,"type":"content_size","width":1078,"height":1144,"serial":1}
```

Width/height are capture pixels in 240..8192. Width must equal the fixed canvas;
height cannot exceed it, and the reserved bottom band must be integral in
compositor logical pixels. `serial` is an unsigned request identifier, echoed in:

```json
{"v":1,"type":"content_size_ack","width":1078,"height":1144,"serial":1,"applied":true}
```

This resizes the app work area, **not** the physical output, encoded video or
input coordinate system. The compositor uses internal maximization while keeping
the client's fullscreen styling. Bottom reserved space reflows the app; Android
keeps the video width/scale and top origin, clipping only the band covered by the
IME. Restoring the canvas height removes that reserved space. No SDP negotiation,
encoder restart, capture restart or app restart is needed.

Android coalesces changes behind one in-flight request and ignores stale replies
from replaced data channels. `applied` acknowledges compositor control acceptance,
not a displayed frame. A rejection or one-second acknowledgement timeout disables
the fast path for that activity and falls back to stop/resize/restart. Changes in
width, scale or height beyond the existing canvas also use that fallback.

### Legacy In-band Viewport Resize

The legacy physical-output resize message remains for diagnostic clients:

```json
{"v":1,"type":"resize","width":844,"height":390}
```

Dimensions must be between 240 and 8192 pixels each. This handler attempts an
output resize at scale 1 and requests a keyframe; it does not reliably reconfigure
fixed-size capture or input mappings. The Android app does not use it. Use the
signaling `session_resize` path for physical size/scale changes, stopping media
first; use `content_size` above for keyboard work-area changes.

Note: The signaling protocol's `session_request` and `session_resize`
messages carry additional `scale` (0.5-4.0, default 1.0) and `refresh_hz`
(15-240, default 60) parameters for HiDPI and variable refresh rate
support. The in-band resize message does not carry these because encoder
parameters are renegotiated via signaling.

### Keyframe Request

Force the encoder to produce an IDR frame for recovery after packet loss:

```json
{"v":1,"type":"keyframe"}
```

The encoder inserts a keyframe at the next opportunity. Repeated requests
within one GOP are collapsed.

### Bitrate Adaptation

Bitrate adaptation messages allow the host or client to request encoder
parameter changes in response to network conditions:

```json
{"v":1,"type":"bitrate","target_kbps":3000}
```

`target_kbps` must be between 128 and 100000. The host encoder adjusts its
target bitrate on the next keyframe or within one GOP. The actual encoding
bitrate may differ from the requested target depending on encoder capabilities
and rate-control mode.

**Note:** Sending a `bitrate` message updates the ABR ceiling — the maximum
bitrate the adaptive bitrate controller will target. ABR continues to manage
the bitrate within the new range (decrease on congestion, increase up to the
ceiling when stable). ABR is disabled only when `ENZREMOTE_VIDEO_BITRATE` is set.

### Network Statistics (Adaptive Bitrate)

The client periodically sends network statistics to enable server-side
adaptive bitrate control:

```json
{"v":1,"type":"network_stats","packet_loss":0.02,"rtt_ms":4.5,"available_bandwidth_kbps":15000,"jitter_ms":1.2}
```

| Field | Type | Range | Description |
|-------|------|-------|-------------|
| `packet_loss` | float | 0.0–1.0 | Fraction of packets lost |
| `rtt_ms` | float | 0–60000 | Round-trip time in milliseconds |
| `available_bandwidth_kbps` | int | 0+ | Estimated bandwidth (0 if unknown, optional) |
| `jitter_ms` | float | 0+ | Jitter in milliseconds (optional, default 0) |

The server uses these to adjust encoder bitrate via AIMD (Additive Increase
Multiplicative Decrease): halve on >2% loss, increase 10% when stable for 2s.

### Cursor Visibility

Toggle the host-side cursor visibility for touch-primary or mouse-primary
modes:

```json
{"v":1,"type":"cursor_visibility","visible":false}
```

This message is reserved; the current input workers do not implement dynamic
compositor cursor toggling. App capture instead requests the portal's **hidden**
cursor mode by default, and CPU screencopy also excludes the cursor. Android
renders its native pointer for a connected mouse. For capture diagnostics,
`ENZREMOTE_CAPTURE_CURSOR=embedded` includes the host cursor on the next portal
session; the default is `hidden`. This does not change the host desktop cursor.

### Window Management

Request the list of windows in the compositor:

```json
{"v":1,"type":"list_windows"}
```

The server responds with a `window_list` message on the data channel:

```json
{"type":"window_list","v":1,"windows":[{"address":"0x56789abc","title":"Calculator","class":"gnome-calculator","mapped":true,"focusHistoryID":0}]}
```

Focus a specific window by its compositor address:

```json
{"v":1,"type":"focus_window","window_address":"0x56789abc"}
```

`window_address` must be a valid hex address (e.g. `0x12345678`). Only hex
digits are allowed (with optional `0x` prefix), 1–256 characters.

### Clipboard Sync

Send clipboard text from the client to the host:

```json
{"v":1,"type":"clipboard","text":"Hello World"}
```

`text` must be 1–65536 characters. The server uses `wl-copy` to set the
compositor clipboard. The server may send clipboard text back to the client
as a `clipboard` response on the data channel.

### Game Controllers

```json
{"type":"gamepad","v":1,"id":0,"buttons":2049,"lx":-0.5,"ly":0.25,"rx":0,"ry":0,"lt":0,"rt":1}
{"type":"gamepad","v":1,"id":0,"connected":false}
```

State snapshots for up to four gamepads (`id` 0-3): sticks -1..1 with +y
down, triggers 0..1, `buttons` bits A, B, X, Y, LB, RB, View, Menu, Guide,
L3, R3, Up, Down, Left, Right (bit 0 upward). The first snapshot plugs in a
uinput device laid out like the kernel `xpad` Xbox 360 pad (045e:028e), so
SDL, Steam and Proton games map it without setup; `connected:false` or the
end of the stream unplugs it. It needs a writable `/dev/uinput` (Steam's
udev rules grant it to the seat user). Android sends a physical controller
attached to the phone, or the per-app "On-screen controller" overlay; button
changes are sent reliably and stick motion is coalesced to ~120 Hz.

When a game plays a force-feedback rumble the host answers
`{"type":"gamepad_rumble","v":1,"id":0,"strong":0.8,"weak":0.3,"duration_ms":200}`
(`duration_ms:0` stops it); Android vibrates that controller, or the phone
for the on-screen one.

### Server Responses

The server sends JSON messages back to the client via the data channel:

| Type | Description |
|------|-------------|
| `window_list` | List of compositor windows (response to `list_windows`) |
| `abr_status` | Adaptive bitrate controller status (on bitrate change) |
| `clipboard` | Clipboard text from the host |

Example ABR status:

```json
{"type":"abr_status","current_kbps":3500,"max_kbps":8000,"enabled":true}
```

## Reconnect State

One long-lived input worker owns the connected EIS descriptor for the WebRTC
channel lifetime, preserving keyboard state and touch slots across messages. It
adopts the compositor's replacement EIS device after a viewport resize and
continues on the new region. Closing or failing the control channel terminates
the worker, causing the compositor to release held keys and buttons and cancel
active touches.

When a stream is stopped (`stop-stream`), the compositor, app, and input broker
remain active. A subsequent `stream` command acquires a fresh EIS capability from
the existing broker and starts a new media consumer. The previous consumer's
input worker is terminated before the new stream begins.
