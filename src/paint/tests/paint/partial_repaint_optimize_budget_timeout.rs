use crate::debug::runtime::{with_runtime_toggles, RuntimeToggles};
use crate::debug::trace::TraceHandle;
use crate::geometry::Rect;
use crate::paint::display_list::{DisplayItem, DisplayList, FillRectItem};
use crate::paint::display_list_renderer::PaintParallelism;
use crate::paint::painter::{
  paint_display_list_with_resources_scaled_with_trace_report,
  repaint_display_list_region_with_resources_scaled_with_trace, PartialRepaintDisplayList,
};
use crate::render_control::{with_deadline, RenderDeadline};
use crate::style::color::Rgba;
use crate::text::font_loader::FontContext;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn partial_repaint_matches_full_paint_when_optimize_budget_times_out() {
  let mut raw = HashMap::new();
  raw.insert(
    "FASTR_DISPLAY_LIST_OPTIMIZE_BUDGET_CAP_MS".to_string(),
    "0".to_string(),
  );
  let toggles = Arc::new(RuntimeToggles::from_map(raw));

  with_runtime_toggles(toggles, || {
    let deadline = RenderDeadline::new(Some(Duration::from_secs(1)), None);
    with_deadline(Some(&deadline), || {
      let width = 64;
      let height = 64;
      let scale = 1.0;
      let background = Rgba::WHITE;
      let font_ctx = FontContext::new();
      let trace = TraceHandle::disabled();

      let mut list = DisplayList::new();
      // Non-uniform content makes it easy to spot incorrect repaints.
      list.push(DisplayItem::FillRect(FillRectItem {
        rect: Rect::from_xywh(0.0, 0.0, width as f32, height as f32),
        color: Rgba::RED,
      }));
      list.push(DisplayItem::FillRect(FillRectItem {
        rect: Rect::from_xywh(16.0, 16.0, 32.0, 32.0),
        color: Rgba::BLUE,
      }));

      let full_report = paint_display_list_with_resources_scaled_with_trace_report(
        &list,
        width,
        height,
        background,
        font_ctx.clone(),
        scale,
        PaintParallelism::disabled(),
        &trace,
        /* optimize_display_list */ true,
      )
      .expect("full paint should succeed");

      assert!(
        !full_report.used_optimized_list,
        "expected optimizer to time out under zero optimize budget cap"
      );

      // Corrupt a dirty region so we can confirm partial repaint restores it byte-for-byte.
      let dirty_rect_css = Rect::from_xywh(8.0, 8.0, 40.0, 40.0);
      let mut repainted = full_report.pixmap.clone();

      let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
      let device_w = repainted.width() as i64;
      let device_h = repainted.height() as i64;
      let x0 = (dirty_rect_css.min_x() * scale).floor() as i64;
      let y0 = (dirty_rect_css.min_y() * scale).floor() as i64;
      let x1 = (dirty_rect_css.max_x() * scale).ceil() as i64;
      let y1 = (dirty_rect_css.max_y() * scale).ceil() as i64;
      let clamp_i64 = |v: i64, min: i64, max: i64| v.max(min).min(max);
      let x0 = clamp_i64(x0, 0, device_w) as usize;
      let y0 = clamp_i64(y0, 0, device_h) as usize;
      let x1 = clamp_i64(x1, 0, device_w) as usize;
      let y1 = clamp_i64(y1, 0, device_h) as usize;
      let stride = repainted.width() as usize * 4;
      for y in y0..y1 {
        let row_off = y * stride;
        for x in x0..x1 {
          let idx = row_off + x * 4;
          repainted.data_mut()[idx..idx + 4].copy_from_slice(&[0, 255, 0, 255]);
        }
      }

      let partial_report = repaint_display_list_region_with_resources_scaled_with_trace(
        PartialRepaintDisplayList::Single {
          display_list: &list,
          already_optimized: false,
        },
        &mut repainted,
        dirty_rect_css,
        width,
        height,
        background,
        font_ctx.clone(),
        scale,
        &trace,
      )
      .expect("partial repaint should succeed");

      assert_eq!(
        partial_report.used_optimized_list,
        full_report.used_optimized_list,
        "partial repaint must rasterize the same list variant (optimized vs original) as full paint"
      );

      assert_eq!(repainted.data(), full_report.pixmap.data());
    });
  });
}
