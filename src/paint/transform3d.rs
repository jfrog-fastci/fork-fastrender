//! 3D transform helpers shared across paint pipelines.
//!
//! This module provides utilities for working with [`Transform3D`] matrices,
//! including plane backface detection and perspective projection helpers.

use crate::paint::display_list::Transform3D;

/// Project a 3D point using the same validity rules as [`Transform3D::project_point`].
///
/// Returns `None` when the perspective divide would be unstable (w≈0), non-finite, or behind the
/// viewer.
pub fn project_point(transform: &Transform3D, x: f32, y: f32, z: f32) -> Option<[f32; 3]> {
  transform.project_point(x, y, z)
}

/// Returns true if a unit plane on the XY axis faces away from the viewer after
/// applying `transform`.
///
/// The check mirrors CSS `backface-visibility: hidden` semantics by projecting
/// three points on the plane (origin and unit X/Y axes), computing the resulting
/// normal, and culling when the normal points away from the camera (negative Z).
pub fn backface_is_hidden(transform: &Transform3D) -> bool {
  // Only cull when we can project the plane stably. If projection fails (e.g. w<=0), keep the
  // content visible rather than incorrectly dropping it.
  let Some(p0) = transform.project_point(0.0, 0.0, 0.0) else {
    return false;
  };
  let Some(p1) = transform.project_point(1.0, 0.0, 0.0) else {
    return false;
  };
  let Some(p2) = transform.project_point(0.0, 1.0, 0.0) else {
    return false;
  };

  let ux = [p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]];
  let uy = [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]];

  let normal = [
    ux[1] * uy[2] - ux[2] * uy[1],
    ux[2] * uy[0] - ux[0] * uy[2],
    ux[0] * uy[1] - ux[1] * uy[0],
  ];

  normal[2] < 0.0
}

/// Returns the projected depth (camera-space Z) of the plane origin after
/// applying `transform`.
pub fn projected_z(transform: &Transform3D) -> f32 {
  transform
    .project_point(0.0, 0.0, 0.0)
    .map(|position| position[2])
    .filter(|z| z.is_finite())
    .unwrap_or(0.0)
}
