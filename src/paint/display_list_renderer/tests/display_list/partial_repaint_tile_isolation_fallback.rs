use crate::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, StackingContextItem,
};
use crate::paint::display_list_renderer::DisplayListRenderer;
use crate::style::types::{BackfaceVisibility, TransformStyle};
use crate::text::font_loader::FontContext;
use crate::{Rect, Rgba};
use tiny_skia::{Color, Pixmap};

#[test]
fn partial_repaint_falls_back_for_preserve_3d() {
  let mut list = DisplayList::new();
  let rect = Rect::from_xywh(0.0, 0.0, 512.0, 512.0);
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: false,
    has_backdrop_sensitive_descendants: false,
    bounds: rect,
    plane_rect: rect,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Preserve3d,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    mask_border: None,
    has_clip_path: false,
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect,
    color: Rgba::RED,
  }));
  list.push(DisplayItem::PopStackingContext);

  // Full repaint reference.
  let full_report = DisplayListRenderer::new(512, 512, Rgba::WHITE, FontContext::new())
    .expect("renderer")
    .render_with_report(&list)
    .expect("full repaint");

  // Start with an obviously wrong previous frame so a "no-op" incremental path can't accidentally
  // pass.
  let mut prev = Pixmap::new(512, 512).expect("pixmap");
  prev.fill(Color::from_rgba8(0, 255, 0, 255));

  let report = DisplayListRenderer::new_from_existing_pixmap(prev, Rgba::WHITE, FontContext::new())
    .expect("renderer")
    .render_damage_with_report(&list, Rect::from_xywh(0.0, 0.0, 10.0, 10.0))
    .expect("damage repaint");

  assert!(
    !report.used_partial,
    "expected preserve-3d to disable partial repaint"
  );
  assert!(
    report
      .fallback_reason
      .as_deref()
      .unwrap_or_default()
      .contains("preserve-3d"),
    "expected preserve-3d fallback reason, got {:?}",
    report.fallback_reason
  );
  assert_eq!(
    report.pixmap.data(),
    full_report.pixmap.data(),
    "expected fallback output to match full repaint"
  );
}
