# Hardware support

The supported target is YoloLiv YoloCam S3, USB ID **46f8:0855**. The protocol
is shared with other devices, but this app does not claim those models have
been tested, and its firmware updater accepts only the S3 `ch131b` model.

| Feature | Interface | Firmware notes |
|---|---|---|
| Webcam video | Standard Linux UVC/V4L2 | Native MJPEG preview; OBS/video calls capture directly |
| USB control link | Standard CDC NCM | USB DHCP address discovered automatically |
| Image/exposure/WB/focus | Vendor TCP protobuf | Readable settings verified; write-only face region uses explicit acknowledgement |
| Exposure compensation | Command 15 | Auto exposure, unlocked; native 0…16, neutral 8; GUI displays −8…+8 |
| HDR | Command 98 | Added in S3 1.0.4 |
| Native crop | Command 117 | Replaces command 13 from S3 0.0.8 onward |
| Crop presets A–D | Command 127 | Four positions stored in camera |
| Eight-colour HSL | Commands 118/128 | Three axes, enable switch, native string wire values |
| Colour profiles | Commands 50/53 | IDs/styles, independent SDR/HDR profile banks |
| Manual focus readback | Command 111 | Manual mode is required for lens positioning |
| LUT grading | Commands 94–96 | 5,508 packed 30-bit entries, no byte-RGB substitution |
| Firmware updates | Commands 2/3/28 | Official package validation; upload/commit/reboot tested with simulated cameras, physical Rust flashing unvalidated |

S3 firmware 1.0.4 acknowledges the legacy brightness command (7) without changing
the image, and its getter does not supply a meaningful current value. The app
does not send or expose that command as a working slider. Use exposure
compensation to brighten or darken the actual camera output in unlocked Auto
exposure, or ISO/shutter in Manual exposure. The compensation scale's steps are
not photographic stops. The vendor desktop app's colour-grading brightness is
a separate LUT transformation; a LUT brightness editor is not implemented here.

The firmware also acknowledges dehaze/shadow/highlight commands without
performing image processing. They are excluded. Rejected controls, including
legacy global hue/gamma, are disabled. Unsupported values in older local
presets are omitted with a visible notice while usable settings are retained.

Sharpness uses the native negative-value encoding (`-1..-10` becomes `61..70`).
S3 firmware 0.0.8 onward supports -10 through 10 rather than the older range
ending at 30. Crop coordinates must remain inside the normalized image.

Colour profile selection preserves the current HDR state and selects the
corresponding bank; it does not download colour profile packages. Existing
installed profiles can be selected. Audio/HDMI settings depend on capabilities
reported by the connected camera and optional hardware.

Face exposure region (214) and microphone type (79) are write-only. The app
cannot discover their initial selection. It sends these choices once on an
explicit Apply action and displays an acknowledgement, without replaying a
lost response or including them in readback presets.

The app's single control worker and per-user session lock prevent its own
clients from fighting over a camera connection. Other vendor control tools
can still replace the connection; avoid running those concurrently.

Native controls travel over USB networking. They are not UVC extension controls
and are not registered as additional V4L2 controls by this application. No
out-of-tree module is necessary for video or USB networking.

Vendor release history: https://www.yololiv.com/blog/yolocam-s3-version-history/
Community protocol reference: https://gitlab.com/yolocam-linux/yolocam-linux
