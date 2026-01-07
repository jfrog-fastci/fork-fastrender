use fastrender::debug::runtime::RuntimeToggles;
use fastrender::{FastRender, FastRenderConfig};
use std::collections::HashMap;
use tiny_skia::Pixmap;

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

fn count_reddish(pixmap: &Pixmap, x0: u32, y0: u32, x1: u32, y1: u32) -> usize {
  let mut total = 0usize;
  for y in y0..y1 {
    for x in x0..x1 {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      // Be tolerant of antialiasing differences across backends by counting "reddish" pixels.
      if px.alpha() > 32
        && px.red() > px.green().saturating_add(30)
        && px.red() > px.blue().saturating_add(30)
      {
        total += 1;
      }
    }
  }
  total
}

#[test]
fn display_list_form_control_text_is_clipped_to_padding_box_when_overflow_clips() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let value = "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM";
  let html = format!(
    "<!doctype html>\
     <style>html,body{{margin:0;background:rgb(0,0,0);}}</style>\
     <input value=\"{value}\" style=\"display:block;margin:0;width:10px;height:40px;box-sizing:content-box;border:10px solid rgb(0,0,255);padding:10px;background:rgb(0,150,0);color:rgb(255,0,0);font-size:40px;line-height:1;overflow:hidden;\">",
    value = value
  );

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer
    .render_html(&html, 100, 100)
    .expect("render form control");

  // Content box: (x,y)=(20,20), size=(10,40). Ensure the value text renders somewhere inside.
  let content_red = count_red(&pixmap, 21, 21, 29, 59);
  assert!(
    content_red > 0,
    "expected form control text to paint inside content box"
  );

  // Right padding region: x=[30..40). When `overflow` clips, we follow CSS overflow behavior and
  // clip to the padding box, so long values may paint into padding but must not reach the border.
  let padding_red = count_reddish(&pixmap, 31, 21, 39, 59);
  assert!(
    padding_red > 0,
    "expected form control text to paint into padding when clipped to the padding box (red pixels in padding={padding_red})"
  );

  // Right border: x=[40..50). Text must not leak into the border box.
  let border_red = count_red(&pixmap, 41, 21, 49, 59);
  assert_eq!(
    border_red, 0,
    "expected form control text overflow to be clipped to the padding box (red pixels in border={border_red})"
  );
}

#[test]
fn display_list_form_control_text_is_clipped_by_default() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  let value = "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM";
  let html = format!(
    "<!doctype html>\
     <style>html,body{{margin:0;background:rgb(0,0,0);}}</style>\
     <input value=\"{value}\" style=\"display:block;margin:0;width:10px;height:40px;box-sizing:content-box;border:10px solid rgb(0,0,255);padding:10px;background:rgb(0,150,0);color:rgb(255,0,0);font-size:40px;line-height:1;\">",
    value = value
  );

  let mut renderer = FastRender::with_config(config).expect("create renderer");
  let pixmap = renderer
    .render_html(&html, 100, 100)
    .expect("render form control");

  // Content box: (x,y)=(20,20), size=(10,40). Ensure the value text renders somewhere inside.
  let content_red = count_red(&pixmap, 21, 21, 29, 59);
  assert!(
    content_red > 0,
    "expected form control text to paint inside content box"
  );

  // Right padding box: x=[30..40). Text should be clipped to the content box, leaving padding green.
  let padding_red = count_red(&pixmap, 31, 21, 39, 59);
  assert_eq!(
    padding_red, 0,
    "expected form control text overflow to be clipped by default (red pixels in padding={padding_red})"
  );
}
