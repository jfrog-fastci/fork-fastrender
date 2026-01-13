//! Media primitives used by the renderer.
//!
//! The primary consumer is the paint pipeline: embedders can provide per-tab media state (e.g. the
//! current decoded frame for a `<video>` element) so paints can update without rerunning layout.

use std::sync::Arc;

/// Supplies decoded media frames to the paint pipeline.
///
/// This is intentionally trait-object-friendly (`Send` + `Sync`) so UI code can share a single
/// provider across paint calls, while still keeping media state scoped to a browser tab.
///
/// When no frame is available, implementations should return `None` and the renderer will fall back
/// to posters/placeholders.
pub trait MediaFrameProvider: Send + Sync {
  /// Returns the current video frame for a `<video>` element.
  ///
  /// The default implementation returns `None`.
  #[allow(unused_variables)]
  fn video_frame(
    &self,
    box_id: Option<usize>,
    src: &str,
  ) -> Option<Arc<crate::paint::display_list::ImageData>> {
    None
  }
}

