#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{PointerButton, PointerModifiers, RepaintReason, TabId, UiToWorker};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

use super::support;

fn selected_option_id_attrs(dom: &fastrender::dom::DomNode) -> Vec<Option<String>> {
  let mut ids = Vec::new();
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
      && node.get_attribute_ref("selected").is_some()
    {
      ids.push(node.get_attribute_ref("id").map(ToString::to_string));
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  ids
}

fn node_id_by_id_attr(dom: &fastrender::dom::DomNode, id_attr: &str) -> usize {
  let ids = fastrender::dom::enumerate_dom_ids(dom);
  let mut stack: Vec<&fastrender::dom::DomNode> = vec![dom];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id").is_some_and(|id| id == id_attr) {
      return *ids
        .get(&(node as *const fastrender::dom::DomNode))
        .unwrap_or_else(|| panic!("node id missing for element with id={id_attr:?}"));
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("expected element with id={id_attr:?}");
}

#[test]
fn browser_tab_controller_select_typeahead_skips_hidden_options() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (200, 80);
  let url = "https://example.com/index.html";
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #sel { position: absolute; left: 0; top: 0; width: 120px; height: 24px; }
        </style>
      </head>
      <body>
        <select id="sel">
          <option style="display:none">Cherry</option>
          <option id="vis">Cherry</option>
        </select>
      </body>
    </html>
  "#;

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    html,
    url,
    viewport_css,
    1.0,
  )?;

  // Ensure the document is laid out before hit-testing pointer events.
  let _ = controller.handle_message(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })?;

  // Click to focus the select.
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })?;
  let _ = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (10.0, 10.0),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  let select_node_id = node_id_by_id_attr(controller.document().dom(), "sel");
  assert_eq!(
    controller.interaction_state().focused,
    Some(select_node_id),
    "expected click to focus the <select>"
  );

  // Typeahead should match "Cherry" while skipping the hidden option.
  let _ = controller.handle_message(UiToWorker::TextInput {
    tab_id,
    text: "c".to_string(),
  })?;

  assert_eq!(
    selected_option_id_attrs(controller.document().dom()),
    vec![Some("vis".to_string())],
    "expected typeahead to select the visible option and skip display:none options",
  );

  Ok(())
}

