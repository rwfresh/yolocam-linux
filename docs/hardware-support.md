# Hardware support

The supported target is YoloLiv YoloCam S3, USB ID **46f8:0855**. The protocol
is shared with other devices, but this app does not claim those models have
been tested, and its firmware updater accepts only the S3 `ch131b` model.

| Feature | Interface | Firmware notes |
|---|---|---|
| Webcam video | Standard Linux UVC/V4L2 | Native MJPEG preview; OBS/video calls capture directly |
| USB control link | Standard CDC NCM | USB DHCP address discovered automatically |
| Image/exposure/WB/focus | Vendor TCP protobuf | Readable settings verified; write-only face region uses explicit acknowledgement |
| HDR | Command 98 | Added in S3 1.0.4 |
| Native crop | Command 117 | Replaces command 13 from S3 0.0.8 onward |
| Crop presets A–D | Command 127 | Four positions stored in camera |
| Eight-colour HSL | Commands 118/128 | Three axes, enable switch, native string wire values |
| Colour profiles | Commands 50/53 | IDs/styles, independent SDR/HDR profile banks |
| Manual focus readback | Command 111 | Manual mode is required for lens positioning |
| LUT grading | Commands 94–96 | 5,508 packed 30-bit entries, no byte-RGB substitution |
| Firmware updates | Commands 2/3/28 | Verified package upload, final-packet commit, reboot verification |

The firmware can acknowledge dehaze/shadow/highlight commands without performing
image processing. They are not working S3 controls in firmware 1.0.4 and are
excluded. Rejected controls, including legacy global hue/gamma, are disabled.

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
