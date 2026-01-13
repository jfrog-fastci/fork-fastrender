use crate::debug::runtime::RuntimeToggles;
use crate::dom::{enumerate_dom_ids, DomNode};
use crate::interaction::InteractionState;
use crate::{
  BrowserDocument, FastRender, FastRenderConfig, RenderArtifactRequest, RenderOptions,
};
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

fn render_focused(html: &str) -> Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let renderer = FastRender::with_config(config).expect("create renderer");
  let mut doc = BrowserDocument::new(renderer, html, RenderOptions::new().with_viewport(100, 100))
    .expect("create BrowserDocument");
  let ids = enumerate_dom_ids(doc.dom());
  let node = find_by_id(doc.dom(), "target").expect("target element");
  let node_id = *ids.get(&(node as *const DomNode)).expect("node id");
  let mut interaction_state = InteractionState::default();
  interaction_state.focused = Some(node_id);
  interaction_state.set_focus_chain(vec![node_id]);
  doc
    .render_frame_with_scroll_state_and_interaction_state(Some(&interaction_state))
    .expect("render focused form control")
    .pixmap
}

fn render_autofocus_with_display_list_artifact(html: &str) -> Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new().with_runtime_toggles(toggles);
  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer
    .render_html_with_stylesheets_report(
      html,
      "https://example.com/",
      RenderOptions::new().with_viewport(100, 100),
      RenderArtifactRequest::full(),
    )
    .expect("render with display-list artifact")
    .pixmap
}

#[test]
fn display_list_form_control_appearance_none_input_paints_caret() {
  let html = "<!doctype html>\
    <style>html,body{margin:0;background:rgb(0,0,0);}</style>\
    <input id=\"target\" value=\"\" style=\"display:block;margin:0;width:40px;height:40px;box-sizing:content-box;border:0;padding:0;background:black;color:rgb(0,255,0);caret-color:rgb(255,0,0);font-size:30px;line-height:1;appearance:none;-webkit-appearance:none;position:relative;z-index:1;\">";
  let pixmap = render_focused(html);

  // Caret should be near the left edge of the focused input.
  let caret_red = count_red(&pixmap, 0, 0, 12, 50);
  assert!(caret_red > 0, "expected caret to paint in red pixels");

  // Ensure no other red pixels exist outside of the caret region.
  let total_red = count_red(&pixmap, 0, 0, 100, 100);
  assert_eq!(
    total_red, caret_red,
    "expected caret to be the only red pixels (total_red={total_red}, caret_red={caret_red})"
  );
}

#[test]
fn display_list_form_control_appearance_none_textarea_paints_caret() {
  let html = "<!doctype html>\
    <style>html,body{margin:0;background:rgb(0,0,0);}</style>\
    <textarea id=\"target\" style=\"display:block;margin:0;width:40px;height:40px;box-sizing:content-box;border:0;padding:0;background:black;color:rgb(0,255,0);caret-color:rgb(255,0,0);font-size:30px;line-height:1;appearance:none;-webkit-appearance:none;position:relative;z-index:1;\"></textarea>";
  let pixmap = render_focused(html);

  // Caret should be near the left edge of the focused textarea.
  let caret_red = count_red(&pixmap, 0, 0, 12, 50);
  assert!(caret_red > 0, "expected caret to paint in red pixels");

  // Ensure no other red pixels exist outside of the caret region.
  let total_red = count_red(&pixmap, 0, 0, 100, 100);
  assert_eq!(
    total_red, caret_red,
    "expected caret to be the only red pixels (total_red={total_red}, caret_red={caret_red})"
  );
}

#[test]
fn display_list_form_control_appearance_none_autofocus_with_display_list_artifact_paints_caret() {
  let html = "<!doctype html>\
    <style>html,body{margin:0;background:rgb(0,0,0);}</style>\
    <input id=\"target\" autofocus value=\"\" style=\"display:block;margin:0;width:40px;height:40px;box-sizing:content-box;border:0;padding:0;background:black;color:rgb(0,255,0);caret-color:rgb(255,0,0);outline:none;font-size:30px;line-height:1;appearance:none;-webkit-appearance:none;position:relative;z-index:1;\">";
  let pixmap = render_autofocus_with_display_list_artifact(html);

  // Caret should be near the left edge of the autofocus input.
  let caret_red = count_red(&pixmap, 0, 0, 12, 50);
  assert!(caret_red > 0, "expected caret to paint in red pixels");

  // Ensure no other red pixels exist outside of the caret region.
  let total_red = count_red(&pixmap, 0, 0, 100, 100);
  assert_eq!(
    total_red, caret_red,
    "expected caret to be the only red pixels (total_red={total_red}, caret_red={caret_red})"
  );
}
