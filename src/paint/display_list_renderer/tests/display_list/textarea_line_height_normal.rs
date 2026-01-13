use crate::debug::runtime::RuntimeToggles;
use crate::dom::{enumerate_dom_ids, DomNode};
use crate::interaction::InteractionState;
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

fn red_bounds(pixmap: &Pixmap) -> Option<(u32, u32, u32, u32)> {
  let mut min_x = u32::MAX;
  let mut min_y = u32::MAX;
  let mut max_x = 0u32;
  let mut max_y = 0u32;
  let mut any = false;

  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      if px.alpha() > 200 && px.red() > 200 && px.green() < 100 && px.blue() < 100 {
        any = true;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x);
        max_y = max_y.max(y);
      }
    }
  }

  any.then_some((min_x, min_y, max_x, max_y))
}

#[test]
fn display_list_textarea_line_height_normal_positions_caret_using_font_metrics() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // The `mvar-metrics-test.ttf` fixture has weight-dependent MVAR deltas that make `line-height:
  // normal` much larger at the heavy instance, so the second line should be clipped out of this
  // textarea and the caret should remain on the first line.
  let html = "<!doctype html>\
    <style>\
      @font-face{font-family:\"VarMVAR\";src:url(\"tests/fixtures/fonts/mvar-metrics-test.ttf\") format(\"truetype\");font-weight:100 900;}\
      html,body{margin:0;background:black;}\
    </style>\
    <textarea id=\"target\" style=\"display:block;margin:0;width:220px;height:65px;min-height:0;box-sizing:content-box;border:0;padding:0;background:black;color:rgb(0,255,0);caret-color:rgb(255,0,0);font-family:'VarMVAR';font-size:50px;font-weight:900;font-variation-settings:'wght' 900;line-height:normal;\">A\n</textarea>";

  let renderer = FastRender::with_config(config).expect("create renderer");
  let mut doc = BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(240, 120))
    .expect("create BrowserDocument");
  let ids = enumerate_dom_ids(doc.dom());
  let node = find_by_id(doc.dom(), "target").expect("target textarea");
  let node_id = *ids.get(&(node as *const DomNode)).expect("node id");
  let mut interaction_state = InteractionState::default();
  interaction_state.focused = Some(node_id);
  interaction_state.set_focus_chain(vec![node_id]);
  let pixmap = doc
    .render_frame_with_scroll_state_and_interaction_state(Some(&interaction_state))
    .expect("render textarea")
    .pixmap;

  let Some((_min_x, min_y, _max_x, _max_y)) = red_bounds(&pixmap) else {
    panic!("expected caret to paint in red pixels");
  };

  let total_red = count_red(&pixmap, 0, 0, 240, 120);

  // With incorrect `line-height: normal` handling, the caret is pushed to the second (clipped)
  // line, leaving all red pixels below the first line.
  let top_red = count_red(&pixmap, 0, 0, 240, 40);
  assert!(
    top_red > 0,
    "expected caret to appear on the first line when line-height is computed from font metrics (min_y={min_y}, top_red={top_red}, total_red={total_red})"
  );
}
