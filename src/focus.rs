// SPDX-License-Identifier: GPL-3.0-or-later
//! S3 focus-area movement in the normalized output image.
//!
//! CID24 is a focus target, not the unsafe face-detection ROI command CID214.
//! The camera applies its own ePTZ transform. Clients must preserve the actual
//! GET24 rectangle's size rather than infer it from the focus-area level.

use crate::{
    camera::{Camera, CameraState, CameraVersion},
    pb,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const FOCUS_TARGET_CID: u32 = 24;
/// The ISP rounds target edges to sensor pixels. This covers one 1080p pixel.
pub const READBACK_TOLERANCE: f32 = 0.001;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FocusTarget {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl FocusTarget {
    pub fn new(left: f32, top: f32, right: f32, bottom: f32) -> Result<Self> {
        let target = Self {
            left,
            top,
            right,
            bottom,
        };
        target.validate()?;
        Ok(target)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            [self.left, self.top, self.right, self.bottom]
                .iter()
                .all(|edge| edge.is_finite() && (0.0..=1.0).contains(edge)),
            "Focus target coordinates must be finite and between 0 and 1"
        );
        ensure!(
            self.left < self.right && self.top < self.bottom,
            "Focus target must have positive width and height"
        );
        Ok(())
    }

    pub fn width(&self) -> f32 {
        self.right - self.left
    }
    pub fn height(&self) -> f32 {
        self.bottom - self.top
    }
    pub fn center(&self) -> (f32, f32) {
        (
            (self.left + self.right) / 2.0,
            (self.top + self.bottom) / 2.0,
        )
    }

    /// Move within the displayed image, keeping the whole existing area inside
    /// its bounds. Near an edge, the requested center is clamped inward.
    pub fn moved_to(&self, x: f32, y: f32) -> Result<Self> {
        self.validate()?;
        validate_point(x, y)?;
        let width = self.width();
        let height = self.height();
        let left = (x - width / 2.0).clamp(0.0, 1.0 - width);
        let top = (y - height / 2.0).clamp(0.0, 1.0 - height);
        Self::new(left, top, (left + width).min(1.0), (top + height).min(1.0))
    }

    pub fn approximately_equal(&self, other: &Self) -> bool {
        self.validate().is_ok()
            && other.validate().is_ok()
            && [
                self.left - other.left,
                self.top - other.top,
                self.right - other.right,
                self.bottom - other.bottom,
            ]
            .iter()
            .all(|delta| delta.abs() <= READBACK_TOLERANCE)
    }

    fn from_pb(value: pb::RectF) -> Result<Self> {
        Self::new(value.left, value.top, value.right, value.bottom)
    }

    fn to_pb(self) -> pb::RectF {
        pb::RectF {
            left: self.left,
            top: self.top,
            right: self.right,
            bottom: self.bottom,
        }
    }
}

fn validate_point(x: f32, y: f32) -> Result<()> {
    ensure!(
        x.is_finite() && y.is_finite() && (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y),
        "Focus point must be finite and inside the output image"
    );
    Ok(())
}

/// Explicit hardware-verified compatibility allowlist. Other models, firmware
/// versions and builds require verification before this action is enabled.
pub fn firmware_supported(version: &CameraVersion) -> bool {
    version.firmware == "YunxiCamera-ch131b-1.0.4" && version.build == 1349
}

pub fn control_reason(state: &CameraState) -> Option<String> {
    match state.version.as_ref() {
        Some(version) if firmware_supported(version) => {}
        _ => {
            return Some(
                "Focus-area movement requires verified S3 firmware 1.0.4 build 1349".into(),
            );
        }
    }
    for key in ["focus-mode", "focus-level", "focus-target"] {
        if let Some(reason) = state.unsupported.get(key) {
            return Some(reason.clone());
        }
    }
    match state.values.get("focus-mode") {
        Some(1 | 3) => {}
        Some(_) => return Some("Select AF-S or AF-C to move the focus area".into()),
        None => return Some("Current autofocus mode is unavailable".into()),
    }
    match state.values.get("focus-level") {
        Some(0..=2) => {}
        Some(_) => {
            return Some("S3 focus-area movement supports Large, Medium and Small areas".into());
        }
        None => return Some("Current focus-area level is unavailable".into()),
    }
    match state.focus_target {
        Some(target) if target.validate().is_ok() => None,
        _ => Some("Current focus-area bounds are unavailable; refresh the camera".into()),
    }
}

impl Camera {
    fn require_focus_firmware(&self) -> Result<()> {
        ensure!(
            self.state.version.as_ref().is_some_and(firmware_supported),
            "Focus-area movement requires verified S3 firmware 1.0.4 build 1349"
        );
        Ok(())
    }

    /// Read actual normalized bounds. Reading the current region is safe on the
    /// verified firmware even in Manual/Face mode; moving it requires AF-S/AF-C.
    pub fn read_focus_target(&mut self) -> Result<FocusTarget> {
        self.require_focus_firmware()?;
        self.state.focus_target = None;
        let read = (|| -> Result<FocusTarget> {
            let reply = self.client.get(FOCUS_TARGET_CID)?;
            accepted(&reply)?;
            match reply.body.and_then(|body| body.value) {
                Some(pb::command_body::Value::RectF(rect)) => FocusTarget::from_pb(rect),
                _ => bail!("Camera returned no normalized focus target"),
            }
        })();
        match read {
            Ok(target) => {
                self.state.focus_target = Some(target);
                self.state.unsupported.remove("focus-target");
                Ok(target)
            }
            Err(error) => {
                self.state
                    .unsupported
                    .insert("focus-target".into(), format!("{error:#}"));
                Err(error.context("Cannot read current focus-area bounds"))
            }
        }
    }

    fn fresh_focus_region(&mut self) -> Result<FocusTarget> {
        self.require_focus_firmware()?;
        ensure!(
            matches!(self.get_param("focus-mode")?, 1 | 3),
            "Select AF-S or AF-C to move the focus area"
        );
        ensure!(
            matches!(self.get_param("focus-level")?, 0..=2),
            "S3 focus-area movement supports Large, Medium and Small areas"
        );
        self.read_focus_target()
    }

    /// Move a region, keeping the size freshly reported by the camera. This
    /// action may trigger single autofocus and must never be replayed after an
    /// uncertain acknowledgement. A successful return verifies the bounds,
    /// not that autofocus has finished or that the subject is in focus.
    pub fn set_focus_target(&mut self, requested: FocusTarget) -> Result<FocusTarget> {
        requested.validate()?;
        let current = self.fresh_focus_region()?;
        ensure!(
            (requested.width() - current.width()).abs() <= READBACK_TOLERANCE
                && (requested.height() - current.height()).abs() <= READBACK_TOLERANCE,
            "S3 focus-area movement preserves the current area size; refresh before moving it"
        );
        let (x, y) = requested.center();
        let desired = current.moved_to(x, y)?;
        self.write_focus_target(desired)
    }

    pub fn move_focus_target(&mut self, x: f32, y: f32) -> Result<FocusTarget> {
        validate_point(x, y)?;
        let current = self.fresh_focus_region()?;
        self.write_focus_target(current.moved_to(x, y)?)
    }

    fn write_focus_target(&mut self, desired: FocusTarget) -> Result<FocusTarget> {
        // An applied action with a lost ACK invalidates the old target too.
        self.state.focus_target = None;
        let reply = self.client.set(
            FOCUS_TARGET_CID,
            pb::command_body::Value::RectF(desired.to_pb()),
        )?;
        accepted(&reply)?;
        let actual = self.read_focus_target().context(
            "Camera acknowledged the focus move, but its target readback is unavailable; the action was not replayed"
        )?;
        ensure!(
            actual.approximately_equal(&desired),
            "Camera acknowledged the focus move but reported different bounds; the action was not replayed"
        );
        Ok(actual)
    }
}

fn accepted(message: &pb::Message) -> Result<()> {
    ensure!(
        message.resp_code == 200,
        "Camera rejected focus command {} with code {}",
        message.id,
        message.resp_code
    );
    Ok(())
}
