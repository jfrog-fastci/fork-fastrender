use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;
use tempfile::tempdir;
use url::Url;

#[test]
fn display_list_img_empty_bytes_renders_ua_placeholder() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // A 0-byte file should be treated as an invalid image. The image loader should resolve it to
  // the canonical transparent placeholder (so painters can detect and reject it), and then the
  // display-list builder should paint a UA "broken image" placeholder (matching Chrome).
  let tmp = tempdir().expect("tempdir");
  let empty_path = tmp.path().join("empty.bin");
  std::fs::write(&empty_path, []).expect("write empty image");
  let empty_url = Url::from_file_path(&empty_path).expect("file URL");

  let html = format!(
    "<!doctype html>\
    <style>\
      html, body {{ margin: 0; background: rgb(0, 0, 0); }}\
      img {{ display: block; width: 20px; height: 20px; font-size: 20px; line-height: 1; color: transparent; }}\
    </style>\
    <img src=\"{empty_url}\" alt=\"X\">"
  );

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer.render_html(&html, 24, 24).expect("render");

  // The placeholder fill should cover the image content box instead of leaving the black page
  // background visible. Sample a pixel well inside the 1px stroke.
  let inside = pixmap.pixel(2, 2).expect("pixel in bounds");
  assert!(
    inside.red() > 150 && inside.green() > 150 && inside.blue() > 150 && inside.alpha() == 255,
    "expected a light UA placeholder fill for missing image, got {:?}",
    (inside.red(), inside.green(), inside.blue(), inside.alpha())
  );
}
