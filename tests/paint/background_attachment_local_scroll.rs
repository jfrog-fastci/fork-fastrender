use super::util::{create_stacking_context_bounds_renderer, create_stacking_context_bounds_renderer_legacy};
use fastrender::debug::inspect::InspectQuery;
use fastrender::RenderOptions;
use tiny_skia::Pixmap;

const VIEWPORT_W: u32 = 60;
const VIEWPORT_H: u32 = 60;

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let pixel = pixmap.pixel(x, y).expect("pixel in bounds");
  (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha())
}

fn assert_is_red(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r > 200 && g < 60 && b < 60 && a > 200,
    "{msg}: expected red, got rgba=({r},{g},{b},{a})"
  );
}

fn assert_is_blue(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    b > 200 && r < 60 && g < 60 && a > 200,
    "{msg}: expected blue, got rgba=({r},{g},{b},{a})"
  );
}

fn inspect_scroller_box_id(renderer: &mut fastrender::FastRender, html: &str) -> usize {
  let dom = renderer.parse_html(html).expect("parse html");
  let snapshots = renderer
    .inspect(
      &dom,
      VIEWPORT_W,
      VIEWPORT_H,
      InspectQuery::Id("scroller".to_string()),
    )
    .expect("inspect");
  let snapshot = snapshots.first().expect("scroller snapshot");
  snapshot.boxes.first().expect("scroller box").box_id
}

fn render_with_scroll(
  renderer: &mut fastrender::FastRender,
  html: &str,
  scroller_box_id: usize,
  scroll_y: f32,
) -> Pixmap {
  let options = RenderOptions::default()
    .with_viewport(VIEWPORT_W, VIEWPORT_H)
    .with_element_scroll(scroller_box_id, 0.0, scroll_y);
  renderer
    .render_html_with_options(html, options)
    .expect("render")
}

#[test]
fn background_attachment_local_scrolls_with_element_contents() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; background: white; }
      #scroller {
        width: 50px;
        height: 50px;
        overflow: auto;
        background-image: repeating-linear-gradient(
          to bottom,
          rgb(255,0,0) 0px,
          rgb(255,0,0) 10px,
          rgb(0,0,255) 10px,
          rgb(0,0,255) 20px
        );
        background-attachment: local;
      }
      #spacer { height: 200px; }
    </style>
    <div id="scroller"><div id="spacer"></div></div>
  "#;

  for (backend, mut renderer) in [
    ("display_list", create_stacking_context_bounds_renderer()),
    ("legacy", create_stacking_context_bounds_renderer_legacy()),
  ] {
    let scroller_box_id = inspect_scroller_box_id(&mut renderer, html);

    let pixmap_scroll_0 = render_with_scroll(&mut renderer, html, scroller_box_id, 0.0);
    assert_is_red(
      rgba_at(&pixmap_scroll_0, 10, 5),
      &format!("{backend}: expected red at y=5 when scroll=0"),
    );

    let pixmap_scroll_10 = render_with_scroll(&mut renderer, html, scroller_box_id, 10.0);
    assert_is_blue(
      rgba_at(&pixmap_scroll_10, 10, 5),
      &format!("{backend}: expected blue at y=5 when scroll=10"),
    );
  }
}

