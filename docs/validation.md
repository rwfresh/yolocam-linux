# Release validation

Validated on 2026-10-11 with an x86-64 Ubuntu 24.04 laptop and a USB-connected
YoloCam S3 running firmware 1.0.4, build 1349. This is one physical S3 device;
other camera models are not covered by these results.

## v0.1.0 automated checks

`cargo fmt --all -- --check`, `cargo clippy --all-targets --locked -- -D warnings`
and all **65 tests** passed. Tests cover the native wire format, bounded TCP
frames, stale/fragmented responses, reconnects, write-only acknowledgements,
crop/HSL/profile compatibility, control coalescing, presets and firmware updates.

Firmware simulations include full 2 MiB packets, a wrong packet acknowledgement,
changed camera identity, a modified package, explicit device failures and a lost
connection after the commit packet. No transfer is repeated automatically.

All five desktop pages rendered successfully using the native Wayland backend.
The interface also opened and rendered using X11. Demo screenshots contain
sample data. The installed desktop launcher passed desktop-file validation.

## v0.1.0 physical camera

The optimized Rust build captured **2,169 frames over 75.02 seconds** at 1280×720.
The final measured frame rate was **30.32 fps**. Preview remained active during
control changes and control-socket recovery.

| Check | Result |
|---|---|
| Contrast 0 → 100 | Captured luminance standard deviation changed from approximately 0 to 26.97 |
| Saturation 0 → 100 | Captured mean chroma changed from 0 to 11.82 |
| Native centred 2× zoom | Read back 2× and the full expected rectangle, 0.25…0.75 |
| Closed control socket | A following contrast write reconnected and read back the requested value |
| Test cleanup | Original contrast, saturation and crop restored |
| Native desktop app | Opened with live capture and one active control connection |
| Session guard | A second CLI control session was rejected while the GUI was open |

Contrast and saturation checks measure actual video pixels. The zoom check
verifies the camera's rectangle readback; it does not assert image geometry.
Other control mappings have simulated-device coverage and real capability
readbacks, rather than exhaustive physical image-effect tests.

## Firmware package

The official metadata service reported S3 **1.0.4/build 1349** as current.
The Rust CLI accepted the corresponding official package, including its archive
layout, manifest, gzip integrity and all three partition-image MD5 checksums.

Package: `YunxiCamera-ch131b-20260831-1.0.4.tar.gz`

Size: 121,887,162 bytes

SHA-256: `2fbc2e20e7236f70b80f6670aeac868ccf74569f98465940719d8bb82aca0b74`

The physical camera was already current, so this release's Rust updater was
**not used to reflash it**. Physical flashing remains unvalidated for this
initial Rust release; the native transfer path has simulated-device tests.
The original Python transfer implementation on which the port is based had
previously updated this S3 successfully.

Private serial numbers, settings backups, captured images and local hardware
result files are not part of the published repository or release packages.

## v0.1.1 brightness correction

Validated on 2026-10-12 with the same S3 on firmware 1.0.4/build 1349.
Formatting, Clippy with warnings denied, all **76 tests**, and the release build
passed. Added tests cover native compensation writes, fresh mode/lock guards,
signed GUI presentation, no writes when rendering, legacy preset migration,
locked preset order/timing and restoration on both early and later failures.

The legacy brightness command (7) acknowledges changes but does not process
the image. Multiple requests including 0, 39 and 100 returned the same
meaningless readback. The app now excludes it, preserves other values from
older presets, and exposes working exposure compensation prominently.

Actual captured video was measured across two repetitions of compensation
8 → 0 → 16, with Auto exposure temporarily unlocked:

| Native compensation | GUI steps | Mean captured luminance, first / second run |
|---|---|---|
| 8 | 0 (neutral) | 139.61 / 138.77 |
| 0 | −8 | 58.68 / 59.58 |
| 16 | +8 | 167.96 / 165.82 |

The average dark-to-bright change was **107.76** on the 0…255 luminance scale.
Preview remained live at approximately **30 fps** throughout. Original
compensation and exposure lock were restored, and no images were saved.
These measurements demonstrate a change to the camera output, not an app-only
preview adjustment. No firmware was flashed for this correction.

With a three-second interval before relocking, native compensation 0 and 16
also produced different captured luminance while locked: **64.66** and **169.29**.
The preset path keeps exposure unlocked through its other changes, allows that
bounded settling interval, then restores the saved or original lock. This was
validated on one camera and scene; it is not a guarantee of AE convergence in
every lighting condition. The tests restored the original compensation/lock.

The optimized final app worker also exercised legacy presets on the physical
camera: unsupported brightness was omitted, native EV was applied while
unlocked, and the lock was restored after settling. Captured luminance changed
from **70.83** to **169.16**, with approximately **30 fps** capture. A direct EV
write while locked was rejected with an unlock instruction, and a direct
legacy brightness write was rejected without the misleading readback error.
The original compensation and lock were restored.
