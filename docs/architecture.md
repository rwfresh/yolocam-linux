# Architecture

`protocol` handles the camera's dual-envelope protobuf framing. Replies must
match both sequence and command. A receive deadline covers the whole frame and
notification drain. Broken or partial streams are discarded. Reads can be
reconnected and repeated once; writes are never replayed by the transport.

`camera` translates UI units to actual firmware commands and verifies readback.
Capabilities come from version/read responses. Firmware-aware crop replaces
legacy zoom, HSL uses the native 24-field structure, and profiles use ID/style/
HDR-bank fields. This layer is shared by the CLI and GUI.

Known nonfunctional brightness/hue/gamma commands are rejected before I/O;
their legacy specifications remain only for preset migration. Exposure
compensation uses native 0…16 (neutral 8); the GUI displays the equivalent
−8…+8 scale. Every EV write checks fresh camera mode and exposure-lock readbacks.
The GUI offers explicit mode/unlock actions and never changes those on startup.

`worker` owns the GUI's one camera connection. Pending slider values are
coalesced by key; individual HSL axes/colours remain independent. Heartbeats and
control changes are serialized. Presets and firmware installation are barriers
to control changes, and firmware never runs on another competing socket.

`preview` opens Linux V4L2 directly through mmap, captures 720p MJPEG and decodes
it into the latest frame mailbox. The UI uploads only the most recent image.
Each capture session has an independent stop flag, so delayed old threads
cannot become active when a new preview starts. No image files are written.

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
