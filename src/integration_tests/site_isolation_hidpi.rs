use crate::api::{FastRender, FastRenderConfig};
use crate::layout::engine::LayoutParallelism;
use crate::multiprocess::compositor::{composite_tab_surface, EmbeddedFrame};
use crate::paint::display_list::BorderRadii;
use crate::paint::display_list_renderer::PaintParallelism;
use crate::resource::ResourcePolicy;
use crate::text::font_db::FontConfig;
use crate::Rect;
use tiny_skia::Pixmap;

fn pix_rgba(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel in bounds");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn remote_iframe_composites_with_parent_device_pixel_ratio() {
  // Simulate a site-isolated iframe:
  // - The parent document renders with `max_iframe_depth=0` so iframe contents are skipped.
  // - The child document is rendered separately (as a remote surface).
  // - A browser-side compositor merges the child surface back into the parent using parent DPR.

  let dpr = 2.0;

  let config = FastRenderConfig::new()
    .with_font_sources(FontConfig::bundled_only())
    .with_resource_policy(ResourcePolicy::default().allow_http(false).allow_https(false))
    .with_device_pixel_ratio(dpr)
    .with_max_iframe_depth(0)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  let mut parent_renderer = FastRender::with_config(config).expect("parent renderer");

  let parent_html = r#"
    <style>
      html, body { margin: 0; padding: 0; background: rgb(255, 0, 0); }
      iframe { display: block; width: 10px; height: 10px; border: 0; padding: 0; background: transparent; }
    </style>
    <iframe srcdoc="<style>html,body{margin:0;padding:0;background:rgb(0,255,0);}</style>"></iframe>
  "#;

  let mut parent_pixmap = parent_renderer
    .render_html(parent_html, 10, 10)
    .expect("render parent");
  assert_eq!(
    (parent_pixmap.width(), parent_pixmap.height()),
    (20, 20),
    "expected parent output to be scaled by DPR"
  );

  // Before compositing the remote surface, the iframe area should show the parent background.
  assert_eq!(pix_rgba(&parent_pixmap, 15, 15), (255, 0, 0, 255));

  let child_config = FastRenderConfig::new()
    .with_font_sources(FontConfig::bundled_only())
    .with_resource_policy(ResourcePolicy::default().allow_http(false).allow_https(false))
    .with_device_pixel_ratio(dpr)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  let mut child_renderer = FastRender::with_config(child_config).expect("child renderer");

  let child_html = r#"
    <style>
      html, body { margin: 0; padding: 0; background: rgb(0, 255, 0); }
    </style>
  "#;
  let child_pixmap = child_renderer
    .render_html(child_html, 10, 10)
    .expect("render child");
  assert_eq!(
    (child_pixmap.width(), child_pixmap.height()),
    (20, 20),
    "expected child output to be scaled by DPR"
  );

  // Composite the remote iframe surface back into the parent output using parent DPR.
  let frames = [EmbeddedFrame {
    frame_id: 1,
    pixmap: &child_pixmap,
    rect_css: Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
    parent_dpr: dpr,
    clip_radii: BorderRadii::ZERO,
  }];
  let parent_pixmap = composite_tab_surface(parent_pixmap, &frames);

  // Pixel coordinate is specified in *device pixels*; it should land inside the 20×20 region.
  assert_eq!(
    pix_rgba(&parent_pixmap, 15, 15),
    (0, 255, 0, 255),
    "expected remote iframe surface to be positioned/scaled with parent DPR"
  );
}
