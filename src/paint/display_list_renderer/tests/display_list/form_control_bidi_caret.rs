use crate::debug::runtime::RuntimeToggles;
use crate::dom::{enumerate_dom_ids, DomNode};
use crate::interaction::state::TextEditPaintState;
use crate::interaction::InteractionState;
use crate::text::caret::CaretAffinity;
use crate::{BrowserDocument, FastRender, FastRenderConfig, RenderOptions};
use std::collections::HashMap;
use tiny_skia::Pixmap;

fn find_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  if node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  node.children.iter().find_map(|child| find_by_id(child, id))
}

fn is_caret_red(px: tiny_skia::PremultipliedColorU8) -> bool {
  // Form-control caret rectangles are emitted as opaque-ish solid fills. Use a generous threshold
  // so minor antialiasing or blending changes don't make this test flaky.
  px.alpha() > 200 && px.red() > 200 && px.green() < 80 && px.blue() < 80
}

fn caret_center_x(pixmap: &Pixmap) -> u32 {
  let mut any = false;
  let mut min_x = u32::MAX;
  let mut max_x = 0u32;
  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      if is_caret_red(px) {
        any = true;
        min_x = min_x.min(x);
        max_x = max_x.max(x);
      }
    }
  }
  assert!(any, "expected caret to paint in red pixels");
  (min_x + max_x) / 2
}

#[test]
fn display_list_form_control_rtl_caret_maps_logical_start_to_visual_right() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // Use a deterministic Hebrew subset font fixture so shaping + advances are stable across
  // platforms.
  let html = "<!doctype html>\
    <style>\
      @font-face{font-family:'TestHebrew';src:url('tests/fixtures/fonts/NotoSansHebrew-subset.ttf') format('truetype');}\
      html,body{margin:0;background:black;}\
    </style>\
    <input id='target' value='אבגדהוזח' style='display:block;margin:0;width:240px;height:60px;box-sizing:content-box;border:0;padding:0;background:black;color:rgb(0,255,0);caret-color:rgb(255,0,0);font-family:\"TestHebrew\";font-size:40px;line-height:1;direction:rtl;text-align:start;'>";

  let renderer = FastRender::with_config(config).expect("create renderer");
  let mut doc = BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(280, 100))
    .expect("create BrowserDocument");
  let ids = enumerate_dom_ids(doc.dom());
  let node = find_by_id(doc.dom(), "target").expect("target input");
  let node_id = *ids.get(&(node as *const DomNode)).expect("node id");
  let value_len = "אבגדהוזח".chars().count();

  let interaction_start = InteractionState {
    focused: Some(node_id),
    text_edit: Some(TextEditPaintState {
      node_id,
      caret: 0,
      caret_affinity: CaretAffinity::Downstream,
      selection: None,
    }),
    ..InteractionState::default()
  };
  let pixmap_start = doc
    .render_frame_with_scroll_state_and_interaction_state(Some(&interaction_start))
    .expect("render rtl caret at start")
    .pixmap;
  let caret_x_start = caret_center_x(&pixmap_start);

  let interaction_end = InteractionState {
    focused: Some(node_id),
    text_edit: Some(TextEditPaintState {
      node_id,
      caret: value_len,
      caret_affinity: CaretAffinity::Downstream,
      selection: None,
    }),
    ..InteractionState::default()
  };
  let pixmap_end = doc
    .render_frame_with_scroll_state_and_interaction_state(Some(&interaction_end))
    .expect("render rtl caret at end")
    .pixmap;
  let caret_x_end = caret_center_x(&pixmap_end);

  assert!(
    caret_x_start > caret_x_end + 20,
    "expected RTL caret at char_idx=0 to paint to the right of caret at end (start_x={caret_x_start}, end_x={caret_x_end})"
  );
}
