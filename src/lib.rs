// SPDX-License-Identifier: GPL-3.0-or-later
//! YoloCam S3 Linux protocol, native controls and verified firmware updates.

// Keep the generated protocol model aligned with the upstream wire schema.
#[allow(clippy::large_enum_variant)]
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/tv.yunxi.protobuf.rs"));
}
pub mod camera;
pub mod compatibility;
pub mod discovery;
pub mod firmware;
pub mod focus;
pub mod gui;
pub mod preview;
pub mod protocol;
pub mod settings;
pub mod worker;
