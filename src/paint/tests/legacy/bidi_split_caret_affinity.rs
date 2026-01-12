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

fn red_bounds(pixmap: &Pixmap) -> Option<(u32, u32, usize)> {
  let mut min_x = u32::MAX;
  let mut max_x = 0u32;
  let mut count = 0usize;

  for y in 0..pixmap.height() {
    for x in 0..pixmap.width() {
      let Some(px) = pixmap.pixel(x, y) else {
        continue;
      };
      if px.alpha() > 200 && px.red() > 200 && px.green() < 100 && px.blue() < 100 {
        count += 1;
        min_x = min_x.min(x);
        max_x = max_x.max(x);
      }
    }
  }

  if count == 0 {
    None
  } else {
    Some((min_x, max_x, count))
  }
}

#[test]
fn legacy_bidi_split_caret_uses_affinity() {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "legacy".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);

  // Use deterministic font fixtures so mixed LTR/RTL shaping has stable advances and the split
  // caret lands at two distinct x positions.
  let html = "<!doctype html>\
    <style>\
      @font-face{font-family:'TestLatin';src:url('tests/fixtures/fonts/NotoSans-subset.ttf') format('truetype');}\
      @font-face{font-family:'TestHebrew';src:url('tests/fixtures/fonts/NotoSansHebrew-subset.ttf') format('truetype');}\
      html,body{margin:0;background:rgb(0,0,0);}\
    </style>\
    <input id=\"target\" value=\"ABC אבג\" style=\"display:block;margin:0;width:260px;height:60px;box-sizing:content-box;border:0;padding:0;background:black;color:rgb(0,255,0);caret-color:rgb(255,0,0);font-family:'TestLatin','TestHebrew';font-size:36px;line-height:1;\">";

  let renderer = FastRender::with_config(config).expect("create renderer");
  let mut doc = BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(320, 120))
    .expect("create BrowserDocument");

  let ids = enumerate_dom_ids(doc.dom());
  let node = find_by_id(doc.dom(), "target").expect("target input");
  let node_id = *ids.get(&(node as *const DomNode)).expect("node id");

  let caret = 4; // boundary after "ABC " (split caret between LTR/RTL runs)

  let render_with_affinity = |doc: &mut BrowserDocument, affinity: CaretAffinity| -> Pixmap {
    let interaction_state = InteractionState {
      focused: Some(node_id),
      text_edit: Some(TextEditPaintState {
        node_id,
        caret,
        caret_affinity: affinity,
        selection: None,
      }),
      ..InteractionState::default()
    };
    doc
      .render_frame_with_scroll_state_and_interaction_state(Some(&interaction_state))
      .expect("render form control")
      .pixmap
  };

  let upstream = render_with_affinity(&mut doc, CaretAffinity::Upstream);
  let downstream = render_with_affinity(&mut doc, CaretAffinity::Downstream);

  let (up_min_x, up_max_x, up_count) =
    red_bounds(&upstream).expect("expected upstream caret pixels");
  let (down_min_x, down_max_x, down_count) =
    red_bounds(&downstream).expect("expected downstream caret pixels");

  // Caret should be a thin vertical line.
  assert!(
    up_max_x.saturating_sub(up_min_x) <= 3,
    "expected upstream caret to be thin (min_x={up_min_x}, max_x={up_max_x}, count={up_count})"
  );
  assert!(
    down_max_x.saturating_sub(down_min_x) <= 3,
    "expected downstream caret to be thin (min_x={down_min_x}, max_x={down_max_x}, count={down_count})"
  );

  let up_center = (up_min_x + up_max_x) / 2;
  let down_center = (down_min_x + down_max_x) / 2;
  let delta = (down_center as i32 - up_center as i32).abs();
  assert!(
    delta >= 5,
    "expected split caret affinities to paint at different x positions (up={up_center}, down={down_center}, delta={delta})"
  );
}
