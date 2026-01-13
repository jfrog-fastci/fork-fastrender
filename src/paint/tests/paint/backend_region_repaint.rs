use super::util::{create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy};
use crate::api::{PreparedPaintOptions, RenderOptions};
use crate::geometry::Rect;
use tiny_skia::Pixmap;

fn crop_pixmap(src: &Pixmap, x: u32, y: u32, w: u32, h: u32) -> Pixmap {
  assert!(x + w <= src.width(), "crop x-range out of bounds");
  assert!(y + h <= src.height(), "crop y-range out of bounds");
  let mut out = Pixmap::new(w, h).expect("pixmap alloc");
  let src_stride = src.width() as usize * 4;
  let dst_stride = out.width() as usize * 4;
  let src_data = src.data();
  let dst_data = out.data_mut();
  for row in 0..h as usize {
    let src_idx = (y as usize + row) * src_stride + x as usize * 4;
    let dst_idx = row * dst_stride;
    dst_data[dst_idx..dst_idx + dst_stride].copy_from_slice(&src_data[src_idx..src_idx + dst_stride]);
  }
  out
}

fn blit_overlapping_region(dst: &mut Pixmap, src: &Pixmap, src_x: i32, src_y: i32) {
  let dst_w = dst.width() as i32;
  let dst_h = dst.height() as i32;
  let src_w = src.width() as i32;
  let src_h = src.height() as i32;
  assert!(dst_w > 0 && dst_h > 0 && src_w > 0 && src_h > 0);

  // Determine intersection of dst rect (0..dst_w, 0..dst_h) mapped into src at (src_x, src_y).
  let x0 = src_x.max(0);
  let y0 = src_y.max(0);
  let x1 = (src_x + dst_w).min(src_w);
  let y1 = (src_y + dst_h).min(src_h);
  if x0 >= x1 || y0 >= y1 {
    return;
  }

  let copy_w = (x1 - x0) as usize;
  let copy_h = (y1 - y0) as usize;
  let dst_off_x = (x0 - src_x) as usize;
  let dst_off_y = (y0 - src_y) as usize;

  let src_stride = src.width() as usize * 4;
  let dst_stride = dst.width() as usize * 4;
  let row_bytes = copy_w * 4;
  let src_data = src.data();
  let dst_data = dst.data_mut();

  for row in 0..copy_h {
    let src_idx = (y0 as usize + row) * src_stride + x0 as usize * 4;
    let dst_idx = (dst_off_y + row) * dst_stride + dst_off_x * 4;
    dst_data[dst_idx..dst_idx + row_bytes].copy_from_slice(&src_data[src_idx..src_idx + row_bytes]);
  }
}

#[test]
fn region_paint_and_scroll_blit_match_full_repaint_under_both_backends() {
  // This HTML intentionally avoids viewport units and `position: fixed` so that:
  // - `paint_region` (viewport=tile, scroll=tile origin) should match cropping a larger full paint,
  // - scroll blitting (copy + repaint newly-exposed strip) should match a full repaint at the new scroll.
  //
  // Include `overflow:hidden` + `border-radius` to cover clip/region mask behavior.
  let html = r#"
    <!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; background: rgb(250, 250, 250); }
          .stripe { height: 20px; }
          /* A clipped, rounded region that overlaps scroll boundaries. */
          #clipper {
            position: absolute;
            left: 8px;
            top: 28px;
            width: 48px;
            height: 48px;
            overflow: hidden;
            border-radius: 12px;
            background: rgb(255, 255, 255);
          }
          #inner {
            position: absolute;
            left: -12px;
            top: -12px;
            width: 72px;
            height: 72px;
            background: rgb(0, 128, 255);
            box-shadow: 0 0 6px rgba(0, 0, 0, 0.4);
          }
        </style>
      </head>
      <body>
        <div class="stripe" style="background: rgb(255, 0, 0);"></div>
        <div class="stripe" style="background: rgb(0, 255, 0);"></div>
        <div class="stripe" style="background: rgb(0, 0, 255);"></div>
        <div class="stripe" style="background: rgb(255, 255, 0);"></div>
        <div class="stripe" style="background: rgb(255, 0, 255);"></div>
        <div class="stripe" style="background: rgb(0, 255, 255);"></div>
        <div class="stripe" style="background: rgb(128, 128, 128);"></div>
        <div class="stripe" style="background: rgb(0, 0, 0);"></div>
        <div style="height: 80px; background: rgb(200, 200, 200);"></div>
        <div id="clipper"><div id="inner"></div></div>
      </body>
    </html>
  "#;

  let view_w = 64u32;
  let view_h = 64u32;
  let region = Rect::from_xywh(13.0, 17.0, 31.0, 29.0);
  let scroll_dy = 16u32;

  for (backend_name, mut renderer) in [
    ("display_list", create_stacking_context_bounds_renderer()),
    ("legacy", create_stacking_context_bounds_renderer_legacy()),
  ] {
    let prepared = renderer
      .prepare_html(html, RenderOptions::new().with_viewport(view_w, view_h))
      .unwrap_or_else(|err| panic!("prepare_html failed for backend={backend_name}: {err}"));

    // ---------------------------------------------------------------------------
    // Region paint matches cropping a larger full paint.
    // ---------------------------------------------------------------------------
    let full = prepared
      .paint_default()
      .unwrap_or_else(|err| panic!("paint_default failed for backend={backend_name}: {err}"));
    assert_eq!(full.width(), view_w);
    assert_eq!(full.height(), view_h);

    let tile = prepared
      .paint_region(region)
      .unwrap_or_else(|err| panic!("paint_region failed for backend={backend_name}: {err}"));

    let crop = crop_pixmap(
      &full,
      region.x().round() as u32,
      region.y().round() as u32,
      region.width().round() as u32,
      region.height().round() as u32,
    );
    assert_eq!(
      tile.data(),
      crop.data(),
      "region paint should match full-pixmap crop for backend={backend_name}"
    );

    // ---------------------------------------------------------------------------
    // Scroll blit (copy + repaint newly exposed strip) matches full repaint.
    // ---------------------------------------------------------------------------
    let base = prepared
      .paint_with_options(
        PreparedPaintOptions::default()
          .with_scroll(0.0, 0.0)
          .with_viewport(view_w, view_h),
      )
      .unwrap_or_else(|err| panic!("paint scroll=0 failed for backend={backend_name}: {err}"));
    let full_scrolled = prepared
      .paint_with_options(
        PreparedPaintOptions::default()
          .with_scroll(0.0, scroll_dy as f32)
          .with_viewport(view_w, view_h),
      )
      .unwrap_or_else(|err| panic!("paint scroll={scroll_dy} failed for backend={backend_name}: {err}"));

    // Shift the old frame by the scroll delta and repaint only the newly exposed bottom strip.
    let mut incremental = Pixmap::new(view_w, view_h).expect("pixmap alloc");
    // Overlap region: copy old y=[scroll_dy..view_h) to new y=[0..view_h-scroll_dy).
    blit_overlapping_region(&mut incremental, &base, 0, scroll_dy as i32);

    // Newly exposed region: page y=[view_h..view_h+scroll_dy) -> viewport y=[view_h-scroll_dy..view_h).
    let strip = prepared
      .paint_region(Rect::from_xywh(
        0.0,
        view_h as f32,
        view_w as f32,
        scroll_dy as f32,
      ))
      .unwrap_or_else(|err| panic!("paint_region strip failed for backend={backend_name}: {err}"));
    assert_eq!(strip.width(), view_w);
    assert_eq!(strip.height(), scroll_dy);

    {
      let dst_stride = view_w as usize * 4;
      let src_stride = view_w as usize * 4;
      let dst_data = incremental.data_mut();
      let src_data = strip.data();
      let dst_y0 = (view_h - scroll_dy) as usize;
      for row in 0..scroll_dy as usize {
        let dst_idx = (dst_y0 + row) * dst_stride;
        let src_idx = row * src_stride;
        dst_data[dst_idx..dst_idx + dst_stride].copy_from_slice(&src_data[src_idx..src_idx + src_stride]);
      }
    }

    assert_eq!(
      incremental.data(),
      full_scrolled.data(),
      "scroll blit + partial repaint should match full repaint for backend={backend_name}"
    );
  }
}

