# Host settings API (for the enzUI Settings app)

The running host (`enzremote-signaling serve`, user unit
`enzimos-phone-link.service`)
answers JSON on a Unix socket: `$XDG_RUNTIME_DIR/enzremote/control.sock`
(override `ENZREMOTE_CONTROL_SOCKET`). Mode 0600, same-UID peers only. Each
connection sends one JSON line and reads one JSON line back, except `watch`,
which keeps streaming. Every answer has `"ok"`; failures add `"error"` (a
sentence that can be shown to the user).

Read `settings_get` to render the schema, write changes with `settings_set`,
and refresh on `watch` events. The Phone Link page in enzimOS Shell Settings
uses this API for device names, permissions, approvals, and the pairing QR code.

## Settings

`{"cmd":"settings_get"}` →

```json
{"ok":true,
 "settings":{"host_name":"","discoverable":true,"approval":"notification","clipboard":true,
             "phone_notifications":true,"receive_files":true,"downloads_dir":"",
             "remote_control":true,"phone_video":true,"sms":true,
             "video_max_kbps":0,"video_max_fps":60,"video_codec":"auto"},
 "schema":[{"key":"host_name","title":"Computer name","description":"…","group":"general",
            "kind":"string","restart":true,"locked":false}, …]}
```

Schema fields: `group` (`general`, `sharing`, `phones`, `streaming`), `kind`
(`bool`, `string`, `path`, `int` with `min`/`max`, `choice` with
`choices:[{value,label}]`), `restart` (applies after the host restarts) and
`locked` (an `ENZREMOTE_*` environment variable in `host.env` fixes it; show
it read-only).

`{"cmd":"settings_set","values":{"clipboard":false,"video_max_fps":"30"}}` →
`{"ok":true,"settings":{…},"restart_required":false}`. Values are validated
all-or-nothing. Saved to `$XDG_CONFIG_HOME/enzremote/settings.json`.
Everything except `host_name` and `discoverable` applies at once (stream
settings to streams started afterwards). Restart with
`systemctl --user restart enzimos-phone-link.service` when `restart_required` is true;
this ends running remote app sessions, so ask first.

## Paired phones (connections)

- `{"cmd":"devices"}` → `devices:[{device_id,name,device_name,paired_at,
  connected,address,battery:{level,charging}|null,permissions}]`. `name` is
  the alias set here, else the phone's own `device_name`.
- `{"cmd":"device_update","device_id":"…","name":"Work phone",
  "permissions":{"files":true,"clipboard":true,"notifications":true,
  "remote_control":false,"phone_video":true,"sms":false}}` — both optional;
  permissions merge. An empty name restores the phone's own. Saved in
  `<data dir>/devices.json`; applies at once. A feature works only when the
  host setting and the device permission both allow it.
- `{"cmd":"unpair","device_id":"…"}` — revokes the credential, disconnects
  the phone and forgets its preferences.

## Apps phones opened here

- `{"cmd":"sessions"}` → `sessions:[{session_id,app_id,app_name,device_id,device_name,
  uptime_secs,streaming}]` (extra monitors are not listed).
- `{"cmd":"session_stop","session_id":"…"}` closes one through the same path
  the phone uses (stream, audio sink and ownership are cleaned up); the app
  quits, so warn about unsaved work. Emits `session_stopped`.

## Adding a phone

- `{"cmd":"pair_code"}` → `{"payload":{…},"expires_in":300}`: render
  `payload` as JSON in a QR code; the phone scans it (one-time token).
- Phones that tap this computer in their list ask for approval:
  `{"cmd":"pending"}` lists `{id,device_id,name,code,age_secs}`; show the
  six-digit `code` (the phone shows the same) and answer with
  `{"cmd":"approve","target":"<id or code>"}` or `{"cmd":"decline",…}`.
  With `approval: "notification"` a desktop notification also offers it.

## Events

`{"cmd":"watch"}` answers `{"ok":true,"event":"watching"}` and then one line
per event: `device_connected`, `device_disconnected`, `device_unpaired`,
`device_updated` (with `device_id`), `pairing_pending` (`device_id`, `name`,
`code`), `settings_changed` (`keys`), and `resync` if the reader fell behind
(re-read everything). Close the socket to stop.

## Other commands

`status` (host ID, name, port, TLS fingerprint, addresses), `ring`
(`device_id?`, `stop?`), `send_files` (`device_id?`, `paths`),
`sms_devices`, `sms` (see `signaling-protocol.md`).
