use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;
use tiny_skia::Pixmap;

fn count_accent_blue(pixmap: &Pixmap, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
  let mut total = 0usize;
  for y in y0..y1 {
    for x in x0..x1 {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      // The UA arrow affordance uses the default `accent-color: auto` (blue-ish). We use a loose
      // threshold so we can distinguish it from black text and the white background.
      if px.alpha() > 200 && px.blue() > 200 && px.red() < 150 && px.green() < 220 {
        total += 1;
      }
    }
  }
  total
}

#[test]
fn legacy_dropdown_select_arrow_is_painted_in_padding_box() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // Make the right padding large enough to contain the arrow affordance. The legacy backend used
  // to render the arrow inside the content box and reserve extra width, leaving the right padding
  // unused.
  let html = r#"
    <!doctype html>
    <style>
      html, body { margin: 0; background: rgb(0, 0, 0); }
      select {
        display: block;
        width: 100px;
        height: 30px;
        box-sizing: border-box;
        border: 0;
        padding: 0;
        padding-right: 20px;
        background: rgb(255, 255, 255);
        color: rgb(0, 0, 0);
        font-size: 20px;
        line-height: 1;
      }
    </style>
    <select>
      <option selected>One</option>
    </select>
  "#;

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer
    .render_html(html, 120, 60)
    .expect("render form control");

  // Content box x=[0..80), padding-right x=[80..100). The arrow should be painted into the right
  // padding region rather than inside the content box.
  let padding_blue = count_accent_blue(&pixmap, 80, 0, 100, 30);
  assert!(
    padding_blue > 0,
    "expected dropdown select arrow to paint inside padding box (blue pixels in padding={padding_blue})"
  );

  // The arrow should not be painted inside the content area when there is enough padding space.
  let content_blue = count_accent_blue(&pixmap, 60, 0, 80, 30);
  assert_eq!(
    content_blue, 0,
    "expected dropdown select arrow to not paint inside the content box (blue pixels in content={content_blue})"
  );
}

