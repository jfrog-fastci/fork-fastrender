use super::support;
use fastrender::ui::messages::{
  PointerButton, PointerModifiers, RepaintReason, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::BrowserTabController;
use fastrender::{dom::DomNode, Result};

fn find_element_by_id<'a>(dom: &'a DomNode, element_id: &str) -> &'a DomNode {
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(element_id) {
      return node;
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("expected element with id={element_id:?}");
}

#[test]
fn datalist_choose_sets_input_value_to_option_value_not_label() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (320, 160);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #q { position: absolute; left: 0; top: 0; width: 240px; height: 32px; font-size: 16px; }
        </style>
      </head>
      <body>
        <input id="q" list="dl">
        <datalist id="dl">
          <option value="nyc">New York City</option>
          <option value="sfo">San Francisco</option>
        </datalist>
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

  // Ensure layout exists before hit-testing.
  let _ = controller.handle_message(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })?;

  // Focus the input.
  let click = (10.0, 10.0);
  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: click,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })?;
  let _ = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: click,
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  // Open datalist suggestions by typing a prefix that matches the *label* ("New York City") but
  // should commit the `<option>`'s `value` attribute when chosen.
  let open_msgs = controller.handle_message(UiToWorker::TextInput {
    tab_id,
    text: "n".to_string(),
  })?;
  let (input_node_id, options) = open_msgs
    .iter()
    .find_map(|msg| match msg {
      WorkerToUi::DatalistOpened {
        tab_id: msg_tab,
        input_node_id,
        options,
        ..
      } if *msg_tab == tab_id => Some((*input_node_id, options)),
      _ => None,
    })
    .expect("expected WorkerToUi::DatalistOpened after typing into <input list=...>");

  let option_node_id = options
    .iter()
    .find(|opt| opt.value == "nyc")
    .map(|opt| opt.option_node_id)
    .expect("expected datalist option with value=\"nyc\"");

  let choose_msgs = controller.handle_message(UiToWorker::datalist_choose(
    tab_id,
    input_node_id,
    option_node_id,
  ))?;

  assert!(
    choose_msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::DatalistClosed { tab_id: msg_tab } if *msg_tab == tab_id)),
    "expected DatalistClosed after choosing a suggestion"
  );

  let dom = controller.document().dom();
  let input = find_element_by_id(dom, "q");
  assert_eq!(input.get_attribute_ref("value"), Some("nyc"));

  // Optional: caret should move to the end of the inserted value.
  let edit = controller
    .interaction_state()
    .text_edit_for(input_node_id)
    .expect("expected text edit state for focused input after datalist choose");
  assert_eq!(edit.caret, "nyc".chars().count());
  assert!(edit.selection.is_none(), "expected no selection after datalist choose");

  Ok(())
}
