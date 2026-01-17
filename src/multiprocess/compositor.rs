//! Browser compositor for multiprocess rendering.
//!
//! In a multiprocess browser architecture, cross-origin iframes (and other
//! nested browsing contexts) may be rendered out-of-process into their own
//! pixmaps. The browser process then composites those child pixmaps into the
//! root (tab) surface based on iframe embedding geometry.
//!
//! MVP behaviour:
//! - Child frame pixmaps are assumed to already be rendered with a transparent
//!   background (matching `render_iframe_*` behaviour in the renderer).
//! - Each child is drawn into the root pixmap at the iframe *content box*
//!   rectangle (`rect_css`) mapped to device pixels via the *embedding (parent)*
//!   frame's device pixel ratio (DPR).
//! - The child draw is clipped to the content box with optional border-radius
//!   (`clip_radii`), matching the renderer-side `emit_iframe_image` clipping
//!   behaviour.
//! - Stacking order is deterministic: children are composited in the order they
//!   are provided. **Limitation:** this is currently expected to be DOM order as
//!   reported by the renderer; z-index stacking contexts are not yet modeled at
//!   the compositor boundary.

use crate::geometry::Rect;
use crate::paint::display_list::BorderRadii;
use tiny_skia::{FillRule, Mask, Pixmap, PixmapPaint, Transform};

/// A child frame surface that should be composited into the tab surface.
#[derive(Debug)]
pub struct EmbeddedFrame<'a> {
  /// Stable frame identifier (for diagnostics / IPC correlation).
  pub frame_id: u64,
  /// The rendered child frame surface (device pixels).
  pub pixmap: &'a Pixmap,
  /// The child frame's content box rectangle, in CSS pixels, in the *root
  /// frame's coordinate space*.
  pub rect_css: Rect,
  /// Device pixel ratio of the embedding (parent) frame.
  ///
  /// The compositor converts `rect_css` to device pixels using this value.
  /// (The child surface is expected to be rendered at `rect_css.size * parent_dpr`.)
  pub parent_dpr: f32,
  /// Content-box clip radii, in CSS pixels.
  pub clip_radii: BorderRadii,
}

fn sanitize_dpr(dpr: f32) -> f32 {
  if dpr.is_finite() && dpr > 0.0 {
    dpr
  } else {
    1.0
  }
}

fn ds_radii(radii: BorderRadii, scale: f32) -> BorderRadii {
  BorderRadii {
    top_left: radii.top_left * scale,
    top_right: radii.top_right * scale,
    bottom_right: radii.bottom_right * scale,
    bottom_left: radii.bottom_left * scale,
  }
}

fn css_rect_to_device_rect(rect_css: Rect, parent_dpr: f32) -> Option<Rect> {
  if !rect_css.x().is_finite()
    || !rect_css.y().is_finite()
    || !rect_css.width().is_finite()
    || !rect_css.height().is_finite()
  {
    return None;
  }
  if rect_css.width() <= 0.0 || rect_css.height() <= 0.0 {
    return None;
  }

  let dpr = sanitize_dpr(parent_dpr);

  // Snap iframe surfaces to device pixels to avoid fractional translation (blur).
  // Use edge snapping (left/right, top/bottom) rather than rounding width/height
  // independently to reduce off-by-one errors when the rect origin is fractional.
  let left = (rect_css.x() * dpr).round();
  let top = (rect_css.y() * dpr).round();
  let right = ((rect_css.x() + rect_css.width()) * dpr).round();
  let bottom = ((rect_css.y() + rect_css.height()) * dpr).round();

  if !left.is_finite() || !top.is_finite() || !right.is_finite() || !bottom.is_finite() {
    return None;
  }

  let width = (right - left).max(1.0);
  let height = (bottom - top).max(1.0);

  Some(Rect::from_xywh(left, top, width, height))
}

fn build_rounded_rect_clip_mask(
  target_width: u32,
  target_height: u32,
  rect_device: Rect,
  radii_device: BorderRadii,
) -> Option<Mask> {
  if target_width == 0 || target_height == 0 {
    return None;
  }
  if rect_device.width() <= 0.0
    || rect_device.height() <= 0.0
    || !rect_device.x().is_finite()
    || !rect_device.y().is_finite()
    || !rect_device.width().is_finite()
    || !rect_device.height().is_finite()
  {
    return None;
  }

  let mut mask = Mask::new(target_width, target_height)?;
  mask.data_mut().fill(0);

  let path = crate::paint::rasterize::build_rounded_rect_path(
    rect_device.x(),
    rect_device.y(),
    rect_device.width(),
    rect_device.height(),
    &radii_device,
  )?;

  // Use anti-aliasing when radii are present to match renderer clip behaviour.
  let anti_alias = !radii_device.is_zero();
  mask.fill_path(&path, FillRule::Winding, anti_alias, Transform::identity());
  Some(mask)
}

/// Composites a root frame pixmap with descendant frame pixmaps to produce the final tab surface.
///
/// Children are drawn in the order they appear in `frames` (deterministic order).
pub fn composite_tab_surface(mut root: Pixmap, frames: &[EmbeddedFrame<'_>]) -> Pixmap {
  for frame in frames {
    let Some(dest_rect_device) = css_rect_to_device_rect(frame.rect_css, frame.parent_dpr) else {
      continue;
    };

    let src_w = frame.pixmap.width() as f32;
    let src_h = frame.pixmap.height() as f32;
    if src_w <= 0.0 || src_h <= 0.0 {
      continue;
    }

    let scale_x = dest_rect_device.width() / src_w;
    let scale_y = dest_rect_device.height() / src_h;
    if !scale_x.is_finite() || !scale_y.is_finite() || scale_x <= 0.0 || scale_y <= 0.0 {
      continue;
    }

    let dpr = sanitize_dpr(frame.parent_dpr);
    let radii_device = ds_radii(frame.clip_radii, dpr);
    let clip_mask = if radii_device.is_zero() {
      None
    } else {
      build_rounded_rect_clip_mask(root.width(), root.height(), dest_rect_device, radii_device)
    };

    let paint = PixmapPaint::default();
    // Use SourceOver blending (default), matching iframe image compositing.
    let transform = Transform::from_row(
      scale_x,
      0.0,
      0.0,
      scale_y,
      dest_rect_device.x(),
      dest_rect_device.y(),
    );
    root.draw_pixmap(
      0,
      0,
      frame.pixmap.as_ref(),
      &paint,
      transform,
      clip_mask.as_ref(),
    );
  }

  root
}

#[cfg(test)]
mod tests {
  use super::*;
  use tiny_skia::Color;

  fn rgba(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let px = pixmap.pixel(x, y).unwrap();
    (px.red(), px.green(), px.blue(), px.alpha())
  }

  #[test]
  fn composites_child_iframe_pixmap_at_expected_coordinates() {
    let mut root = Pixmap::new(32, 32).expect("root pixmap");
    root.fill(Color::from_rgba8(0, 255, 0, 255));

    let mut child = Pixmap::new(8, 8).expect("child pixmap");
    child.fill(Color::from_rgba8(255, 0, 0, 255));

    let frames = [EmbeddedFrame {
      frame_id: 1,
      pixmap: &child,
      rect_css: Rect::from_xywh(4.0, 6.0, 8.0, 8.0),
      parent_dpr: 1.0,
      clip_radii: BorderRadii::ZERO,
    }];

    let out = composite_tab_surface(root, &frames);
    assert_eq!(rgba(&out, 0, 0), (0, 255, 0, 255), "background stays green");
    assert_eq!(rgba(&out, 4, 6), (255, 0, 0, 255), "child paints at (4,6)");
    assert_eq!(
      rgba(&out, 11, 13),
      (255, 0, 0, 255),
      "child paints within its rect"
    );
    assert_eq!(
      rgba(&out, 12, 14),
      (0, 255, 0, 255),
      "outside child rect stays green"
    );
  }

  #[test]
  fn border_radius_clip_preserves_parent_background_in_corners() {
    let mut root = Pixmap::new(32, 32).expect("root pixmap");
    root.fill(Color::from_rgba8(0, 255, 0, 255));

    let mut child = Pixmap::new(16, 16).expect("child pixmap");
    child.fill(Color::from_rgba8(255, 0, 0, 255));

    let frames = [EmbeddedFrame {
      frame_id: 2,
      pixmap: &child,
      rect_css: Rect::from_xywh(8.0, 8.0, 16.0, 16.0),
      parent_dpr: 1.0,
      clip_radii: BorderRadii::uniform(6.0),
    }];

    let out = composite_tab_surface(root, &frames);
    assert_eq!(
      rgba(&out, 8, 8),
      (0, 255, 0, 255),
      "top-left corner pixel should remain parent background due to rounded clip"
    );
    assert_eq!(
      rgba(&out, 16, 16),
      (255, 0, 0, 255),
      "center of iframe should be painted"
    );
  }

  #[test]
  fn composites_with_non_unit_dpr_mapping_css_to_device_pixels() {
    let mut root = Pixmap::new(32, 32).expect("root pixmap");
    root.fill(Color::from_rgba8(0, 255, 0, 255));

    // 10×10 CSS px at DPR=2 => 20×20 device px.
    let mut child = Pixmap::new(20, 20).expect("child pixmap");
    child.fill(Color::from_rgba8(255, 0, 0, 255));

    let frames = [EmbeddedFrame {
      frame_id: 3,
      pixmap: &child,
      rect_css: Rect::from_xywh(1.0, 2.0, 10.0, 10.0),
      parent_dpr: 2.0,
      clip_radii: BorderRadii::ZERO,
    }];

    let out = composite_tab_surface(root, &frames);
    assert_eq!(
      rgba(&out, 2, 4),
      (255, 0, 0, 255),
      "expected child to start at (rect_css*dpr) = (2,4)"
    );
    assert_eq!(
      rgba(&out, 21, 23),
      (255, 0, 0, 255),
      "expected child to cover 20×20 device px region"
    );
    assert_eq!(
      rgba(&out, 22, 24),
      (0, 255, 0, 255),
      "pixels outside the mapped child rect should remain background"
    );
  }

  #[test]
  fn border_radius_clip_scales_with_dpr() {
    let mut root = Pixmap::new(32, 32).expect("root pixmap");
    root.fill(Color::from_rgba8(0, 255, 0, 255));

    // 8×8 CSS px at DPR=2 => 16×16 device px.
    let mut child = Pixmap::new(16, 16).expect("child pixmap");
    child.fill(Color::from_rgba8(255, 0, 0, 255));

    // Radius of 2 CSS px at DPR=2 => 4 device px.
    let frames = [EmbeddedFrame {
      frame_id: 4,
      pixmap: &child,
      rect_css: Rect::from_xywh(0.0, 0.0, 8.0, 8.0),
      parent_dpr: 2.0,
      clip_radii: BorderRadii::uniform(2.0),
    }];

    let out = composite_tab_surface(root, &frames);
    assert_eq!(
      rgba(&out, 0, 0),
      (0, 255, 0, 255),
      "corner pixel should be clipped at scaled border radius"
    );
    assert_eq!(
      rgba(&out, 2, 1),
      (255, 0, 0, 255),
      "pixel inside the scaled quarter-circle should remain visible"
    );
  }
}
