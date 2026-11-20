# Phone Link for enzimOS

Pair an Android phone with your enzimOS desktop to share files and clipboard
text, see phone notifications, and control media. Open a desktop app on your
phone in a phone-sized window, or stream the full desktop when you need it.

## Pair your phone

1. In an unlocked enzimOS desktop session, open **Settings → Phone Link** and
   turn on the service. The Phone Link host package must be installed.
2. Open the Phone Link companion on Android. Scan the QR code in Settings or
   select your computer in the app and approve the request on the desktop.
3. In Settings, choose what the paired phone can access. Clipboard sync also
   needs to be enabled on the phone. You can revoke a phone from the same page.

Phone Link starts out disabled. Locking the desktop ends remote streaming and
input. Keep the phone and computer on the same trusted network for discovery
and streaming. If your computer does not appear, see the
[connection and firewall instructions](docs/troubleshooting.md).

## Build the host

Install Rust 1.99.1, a C toolchain, `pkg-config`, and the GStreamer 1.28
development libraries for App, Video, SDP, and WebRTC.

```sh
cargo build --workspace --all-features --locked
```

The native capture and input helpers also need `wayland-scanner` and the
Wayland client, xkbcommon, and libei development libraries. Set
`ENZUI_COMPOSITOR_SETUP_DIR` to the root of the matching enzimOS Shell source
checkout with its compositor sources initialized before running the helper
build scripts.
The Android companion and desktop Settings UI live in their own repositories.

- [Architecture](docs/architecture.md)
- [Desktop and app streaming](docs/shared-desktop.md)
- [Testing](docs/testing.md)

## License

[MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE).
