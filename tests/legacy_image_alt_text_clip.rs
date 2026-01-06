use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;
use tempfile::tempdir;
use tiny_skia::Pixmap;
use url::Url;

fn count_red(pixmap: &Pixmap, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
  let mut total = 0usize;
  for y in y0..y1 {
    for x in x0..x1 {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      if px.alpha() > 200 && px.red() > 200 && px.green() < 100 && px.blue() < 100 {
        total += 1;
      }
    }
  }
  total
}

#[test]
fn legacy_img_alt_text_is_clipped_to_content_box() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let temp = tempdir().expect("tempdir");
  let missing_path = temp.path().join("missing.png");
  let missing_url = Url::from_file_path(&missing_path).unwrap();

  let alt = "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM";
  let html = format!(
    "<!doctype html>\
     <style>html,body{{margin:0;background:rgb(0,0,0);}}</style>\
     <img src=\"{src}\" alt=\"{alt}\" style=\"display:block;margin:0;width:20px;height:40px;box-sizing:content-box;border:10px solid rgb(0,0,255);padding:10px;background:rgb(0,150,0);color:rgb(255,0,0);font-size:40px;line-height:1;overflow:clip;\">",
    src = missing_url,
    alt = alt
  );

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer.render_html(&html, 100, 100).expect("render image");

  // Content box: (x,y)=(20,20), size=(20,40). Ensure alt text renders somewhere inside.
  let content_red = count_red(&pixmap, 21, 21, 39, 59);
  assert!(
    content_red > 0,
    "expected alt text to paint inside content box"
  );

  // Right padding box: x=[40..50). Text should be clipped to the content box, leaving padding green.
  let padding_red = count_red(&pixmap, 41, 21, 49, 59);
  assert_eq!(
    padding_red, 0,
    "expected alt text overflow to be clipped (red pixels in padding={padding_red})"
  );
}
