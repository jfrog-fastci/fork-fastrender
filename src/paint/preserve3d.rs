//! Preserve-3D scene helpers.
//!
//! This module intentionally contains *no* preserve-3d-specific algorithms.
//! Instead it wraps the runtime implementations used by the renderer (namely
//! [`crate::paint::transform3d::backface_is_hidden`] and
//! [`crate::paint::depth_sort::depth_sort`]) so tests can validate behavior
//! without depending on the full display-list renderer.

use crate::geometry::Rect;
use crate::paint::depth_sort;
use crate::paint::display_list::Transform3D;
use crate::paint::transform3d::backface_is_hidden;
use crate::style::types::BackfaceVisibility;

/// A plane participating in a 3D scene.
#[derive(Debug, Clone)]
pub struct SceneItem<T> {
  /// Payload carried alongside the plane.
  pub item: T,
  /// Transform accumulated through preserve-3d ancestors (already includes any
  /// flattening of inlined descendants).
  pub accumulated_transform: Transform3D,
  /// Local plane rect (in the plane's own coordinate space) before transformation.
  ///
  /// The renderer's preserve-3d depth sorting uses this geometry to determine
  /// overlap and sample depths; keeping it here ensures tests exercise the same
  /// path.
  pub plane_rect: Rect,
  /// Whether the plane should be culled when facing away from the viewer.
  pub backface_visibility: BackfaceVisibility,
}

impl<T> SceneItem<T> {
  /// Returns true if this plane should be culled per `backface-visibility`.
  pub fn is_backface_hidden(&self) -> bool {
    matches!(self.backface_visibility, BackfaceVisibility::Hidden)
      && backface_is_hidden(&self.accumulated_transform)
  }
}

/// Filters out planes whose backfaces are hidden.
pub fn cull_backfaces<T>(items: Vec<SceneItem<T>>) -> Vec<SceneItem<T>> {
  items
    .into_iter()
    .filter(|item| !item.is_backface_hidden())
    .collect()
}

/// Culls hidden backfaces and returns planes sorted back-to-front by depth.
pub fn depth_sort_scene<T>(items: Vec<SceneItem<T>>) -> Vec<SceneItem<T>> {
  let kept = cull_backfaces(items);
  let sort_items: Vec<depth_sort::SceneItem> = kept
    .iter()
    .enumerate()
    .map(|(paint_order, item)| depth_sort::SceneItem {
      transform: item.accumulated_transform,
      plane_rect: item.plane_rect,
      paint_order,
    })
    .collect();
  let order = depth_sort::depth_sort(&sort_items);

  // Reorder without requiring `T: Clone`.
  let mut slots: Vec<Option<SceneItem<T>>> = kept.into_iter().map(Some).collect();
  let mut sorted = Vec::with_capacity(order.len());
  for idx in order {
    let Some(item) = slots.get_mut(idx).and_then(Option::take) else {
      debug_assert!(false, "depth_sort returned index in-bounds");
      continue;
    };
    sorted.push(item);
  }
  sorted
}
