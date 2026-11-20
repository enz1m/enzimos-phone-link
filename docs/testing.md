# Testing Phone Link

Run the Rust checks with the native development dependencies listed in the
[README](../README.md). The tests use disposable state and do not need a paired
phone.

```sh
cargo test --workspace --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
python3 -m unittest discover -s packaging -p 'test_*.py'
```

Use `scripts/media-smoke` to check the GStreamer plugins with synthetic media.
`scripts/test-portal-helper` checks the portal helper. The native worker checks
in `scripts/test-input-wl` and `scripts/test-desktop-input` exercise the input
transport with disposable compositor or protocol fixtures.

For an end-to-end check, use a disposable session of enzimOS Shell with the
host service installed, then pair a test phone or emulator. Use synthetic
notifications, a test app and dummy files. Check pairing and revocation,
clipboard opt-in, app streaming and return to the desktop. Lock the desktop
and verify that remote streaming and input stop. Disconnect the test phone and
verify that its media and app sessions are released.
