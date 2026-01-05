use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;
use tiny_skia::Pixmap;

fn rgba(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).unwrap();
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn legacy_iframe_content_is_clipped_to_content_box_radius() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let inner = "<!doctype html><style>html,body{margin:0;width:100vw;height:100vh;background:rgb(255,0,0);}</style>";
  let outer = format!(
    "<!doctype html>\
     <style>html,body{{margin:0;background:rgb(0,0,0);}}</style>\
     <iframe srcdoc='{inner}' style='display:block;margin:0;width:100px;height:100px;box-sizing:content-box;border:20px solid rgb(255,200,0);padding:20px;border-radius:80px;background:rgb(0,150,0);overflow:clip;'></iframe>"
  );

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer
    .render_html(&outer, 200, 200)
    .expect("render legacy iframe");

  assert_eq!(
    rgba(&pixmap, 90, 10),
    (255, 200, 0, 255),
    "expected border color at (90,10)"
  );
  assert_eq!(
    rgba(&pixmap, 90, 30),
    (0, 150, 0, 255),
    "expected iframe background in padding at (90,30)"
  );
  assert_eq!(
    rgba(&pixmap, 90, 90),
    (255, 0, 0, 255),
    "expected iframe content in content box at (90,90)"
  );
  assert_eq!(
    rgba(&pixmap, 45, 45),
    (0, 150, 0, 255),
    "expected iframe content to be clipped at rounded corner (45,45)"
  );
}
