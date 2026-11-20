# Troubleshooting

## Phone Link is unavailable in Settings

Check that the Phone Link host package is installed and that you are in an
active enzUI Wayland session. The user service belongs to
`enzui-session.target` and starts when enabled in Settings. If an older host
is running, save any remote work and stop it before turning on Phone Link.

```sh
systemctl --user status enzimos-phone-link.service
journalctl --user -u enzimos-phone-link.service -b
```

The service loads optional host settings from
`$XDG_CONFIG_HOME/enzimos-phone-link/host.env`, or
`~/.config/enzimos-phone-link/host.env` with the default XDG paths.

## The phone cannot find the computer

Verify that the host and phone are on the same trusted network. If discovery
is unavailable, use the pairing QR code in Settings. The installed service
requires TLS and pairing approval. Use the `enzimos-phone-link` firewalld
service definition to allow discovery and media on the intended trusted
zone. Discovery uses mDNS, signaling uses TCP 9470, and WebRTC media uses UDP
9471–9599 in both directions. Installing the service definition leaves
firewall zones unchanged. If you change the signaling port, update the
trusted zone's rule to match.

## Pairing works but the stream does not start

Check that the session is unlocked, the app has permission to stream, and the
desktop portal is available. A named-output stream requires the compositor
and portal backend to agree on the selected output. See the
[shared desktop guide](shared-desktop.md).

Run `scripts/media-smoke` on the host to check the GStreamer plugins with a
test video source. When sharing logs, remove pairing information and peer
addresses.
