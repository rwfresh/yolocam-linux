# Contributing

Changes and bug reports are welcome through GitHub issues and pull requests.
This code is GPL-3.0-or-later; contributions use the same license.

Run `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, and
`cargo test --all-targets` before submitting a change. Automated tests use
simulated camera sockets and archives. Do not make tests flash a real device.

For hardware bugs, include the app version, Linux distribution, camera model,
firmware version/build, and the operation that failed. Redact your serial number
and personal settings. Captured images and device backups are not required.

Protocol changes need a wire fixture or a simulated-camera test. A camera
acknowledgement alone does not prove a feature changes the image: check the
video output on hardware and document whether that check was performed.

Keep firmware transfer writes single-shot. A disconnect after the final
packet is uncertain and must be resolved by reading the running version after
reboot, without automatically replaying the transfer.
