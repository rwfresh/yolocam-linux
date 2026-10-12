# Architecture

`protocol` handles the camera's dual-envelope protobuf framing. Replies must
match both sequence and command. A receive deadline covers the whole frame and
notification drain. Broken or partial streams are discarded. Reads can be
reconnected and repeated once; writes are never replayed by the transport.

`camera` translates UI units to actual firmware commands and verifies readback.
Capabilities come from version/read responses. Firmware-aware crop replaces
legacy zoom, HSL uses the native 24-field structure, and profiles use ID/style/
HDR-bank fields. This layer is shared by the CLI and GUI.

`compatibility` checks HDR/frame rates, mode dependencies and preset
capabilities without I/O. The camera layer refreshes controlling readbacks
before writes; the worker checks the complete preset before following its
compatible HDR/frame-rate order. V4L2 capture intervals and the native camera
frame-rate command are checked separately, without inventing missing values.
UVC negotiation can update the camera's native frame rate; after capture starts,
the GUI queues a serialized refresh rather than assigning a guessed readback.

`focus` moves command-24 rectangles on verified firmware. It reads the current
autofocus mode, area level and actual rectangle before preserving its size,
sends one movement, then verifies the rectangle with sensor-rounding tolerance.
The worker does not replay this action after a lost acknowledgement. The GUI
maps clicks/drag releases inside the letterboxed image and uses producer frame
timestamps to disable interaction on stale video.

Known nonfunctional brightness/hue/gamma and unsafe face ROI commands are
rejected before I/O;
their legacy specifications remain only for preset migration. Exposure
compensation uses native 0…16 (neutral 8); the GUI displays the equivalent
−8…+8 scale. Every EV write checks fresh camera mode and exposure-lock readbacks.
The GUI offers explicit mode/unlock actions and never changes those on startup.

`worker` owns the GUI's one camera connection. Pending slider values are
coalesced by key; individual HSL axes/colours remain independent. Heartbeats and
control changes are serialized. Presets and firmware installation are barriers
to control changes, and firmware never runs on another competing socket.

`preview` enumerates MJPEG resolutions and exact frame intervals, opens Linux
V4L2 directly through mmap and decodes the selected mode into the latest frame
mailbox. The default is 720p/30. The UI copies and uploads only a changed frame.
Each capture session has an independent stop flag, so delayed old threads
cannot become active when a new preview starts. Capture stops are bounded;
a thread still releasing its device prevents a competing new capture session.
The negotiated mode and measured delivery rate are reported separately.
No image files are written.

`firmware` separates metadata, download, archive verification, transfer, commit
and reboot verification. The package is checked again from the exact in-memory
bytes to close the gap between verification and upload. Packet responses must
acknowledge the expected total/current indices. Failure before commit aborts;
transport or acknowledgement failure after the final packet becomes
`UpdateUncertain` and leads to version verification, without another upload.
An explicit camera failure remains `CameraUpdateFailed`.

`settings` stores preferences, presets and update backups outside the source
tree. `gui` renders the native egui desktop app; it does no socket I/O. The
binary's advisory session lock prevents multiple app/CLI processes from taking
over the same camera control connection. Diagnosis/listing/demo need no lock.
Presets normalize legacy unsupported controls into omission notices. Auto
exposure compensation is restored while unlocked, followed by the other preset
settings and a bounded settling interval before locking. A partial preset
preserves the original lock when it does not specify one. Failures restore that
lock without replaying the compensation write.

Linux `uvcvideo` and `cdc_ncm` continue to handle the device. The app uses their
existing interfaces; it does not change network configuration or install a
kernel driver.
