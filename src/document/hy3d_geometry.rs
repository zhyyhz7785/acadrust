//! HY3D planar editing shapes and their native entity boundary.
//!
//! The planar data types are the actual payloads used by HY3D's Geometry
//! variants, not a separately stored cache. They deliberately do not model
//! EntityCommon, Z or extrusion: the complete native record retains those
//! until HY3D's live entity authority is migrated. The projection preserves
//! the existing local XY convention, not a new OCS-to-WCS conversion.

use crate::entities::{Arc, Circle, Line};
use crate::types::Vector3;

/// Planar LINE geometry; point arithmetic remains in the caller's math kernel.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanarLine<P> {
    pub a: P,
    pub b: P,
}

/// Planar ARC geometry; counterclockwise angles in radians.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanarArc<P> {
    pub center: P,
    pub radius: f64,
    pub start: f64,
    pub end: f64,
}

/// Planar CIRCLE geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanarCircle<P> {
    pub center: P,
    pub radius: f64,
}

/// Convert the caller's XY point without changing the existing native Z.
fn with_xy<P>(point: &P, z: f64, xy: impl Fn(&P) -> (f64, f64)) -> Vector3 {
    let (x, y) = xy(point);
    Vector3::new(x, y, z)
}

fn same_xy<P>(native: Vector3, point: &P, xy: impl Fn(&P) -> (f64, f64)) -> bool {
    let (x, y) = xy(point);
    native.x == x && native.y == y
}

/// Invalid thickness is shown as zero by the current HY3D import path.
/// Compare against that projection rather than needlessly rewriting raw data.
pub fn editable_scalar(value: f64) -> f64 {
    if value.is_finite() { value } else { 0.0 }
}

impl<P> PlanarLine<P> {
    pub fn from_native(native: &Line, point: impl Fn(f64, f64) -> P) -> Self {
        Self {
            a: point(native.start.x, native.start.y),
            b: point(native.end.x, native.end.y),
        }
    }

    pub fn matches_native(&self, native: &Line, thickness: f64, xy: impl Fn(&P) -> (f64, f64)) -> bool {
        same_xy(native.start, &self.a, &xy)
            && same_xy(native.end, &self.b, &xy)
            && editable_scalar(native.thickness) == thickness
    }

    /// The caller must obtain a mutable entity through CadDocument to mark
    /// its raw record stale. This changes only the modeled fields.
    pub fn apply_to_native(&self, native: &mut Line, thickness: f64, xy: impl Fn(&P) -> (f64, f64)) {
        native.start = with_xy(&self.a, native.start.z, &xy);
        native.end = with_xy(&self.b, native.end.z, &xy);
        native.thickness = thickness;
    }

    pub fn to_native(&self, thickness: f64, xy: impl Fn(&P) -> (f64, f64)) -> Line {
        let mut native = Line::new();
        self.apply_to_native(&mut native, thickness, xy);
        native
    }
}

impl<P> PlanarCircle<P> {
    pub fn from_native(native: &Circle, point: impl Fn(f64, f64) -> P) -> Self {
        Self {
            center: point(native.center.x, native.center.y),
            radius: native.radius,
        }
    }

    pub fn matches_native(&self, native: &Circle, thickness: f64, xy: impl Fn(&P) -> (f64, f64)) -> bool {
        same_xy(native.center, &self.center, xy)
            && native.radius == self.radius
            && editable_scalar(native.thickness) == thickness
    }

    pub fn apply_to_native(&self, native: &mut Circle, thickness: f64, xy: impl Fn(&P) -> (f64, f64)) {
        native.center = with_xy(&self.center, native.center.z, xy);
        native.radius = self.radius;
        native.thickness = thickness;
    }

    pub fn to_native(&self, thickness: f64, xy: impl Fn(&P) -> (f64, f64)) -> Circle {
        let mut native = Circle::new();
        self.apply_to_native(&mut native, thickness, xy);
        native
    }
}

impl<P> PlanarArc<P> {
    pub fn from_native(native: &Arc, point: impl Fn(f64, f64) -> P) -> Self {
        Self {
            center: point(native.center.x, native.center.y),
            radius: native.radius,
            start: native.start_angle,
            end: native.end_angle,
        }
    }

    pub fn matches_native(&self, native: &Arc, thickness: f64, xy: impl Fn(&P) -> (f64, f64)) -> bool {
        same_xy(native.center, &self.center, xy)
            && native.radius == self.radius
            && native.start_angle == self.start
            && native.end_angle == self.end
            && editable_scalar(native.thickness) == thickness
    }

    pub fn apply_to_native(&self, native: &mut Arc, thickness: f64, xy: impl Fn(&P) -> (f64, f64)) {
        native.center = with_xy(&self.center, native.center.z, xy);
        native.radius = self.radius;
        native.start_angle = self.start;
        native.end_angle = self.end;
        native.thickness = thickness;
    }

    pub fn to_native(&self, thickness: f64, xy: impl Fn(&P) -> (f64, f64)) -> Arc {
        let mut native = Arc::new();
        self.apply_to_native(&mut native, thickness, xy);
        native
    }
}
