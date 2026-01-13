#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::geometry::Rect;
use fastrender::ui::messages::{
  DatalistOption, PointerButton, RepaintReason, TabId, UiToWorker, WorkerToUi,
};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

fn find_element_by_id<'a>(dom: &'a fastrender::dom::DomNode, element_id: &str) -> Option<&'a fastrender::dom::DomNode> {
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(element_id) {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn find_datalist_opened(
  msgs: &[WorkerToUi],
  tab_id: TabId,
) -> Option<(usize, Vec<DatalistOption>, Rect)> {
  msgs.iter().find_map(|msg| match msg {
    WorkerToUi::DatalistOpened {
      tab_id: got,
      input_node_id,
      options,
      anchor_css,
    } if *got == tab_id => Some((*input_node_id, options.clone(), *anchor_css)),
    _ => None,
  })
}

#[test]
fn browser_tab_controller_datalist_opens_and_filters_on_typing() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (240, 120);
  let url = "https://example.com/index.html";
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #q { position: absolute; left: 0; top: 0; width: 180px; height: 28px; font-family: "Noto Sans Mono"; font-size: 18px; }
        </style>
      </head>
      <body>
        <input id="q" list="choices" value="">
        <datalist id="choices">
          <option value="apple" label="Apple"></option>
          <option value="apricot"></option>
          <option value="banana"></option>
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

  // Ensure the document is laid out before hit-testing the click.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  // Focus the input to open the datalist popup.
  let _ = controller.handle_message(support::pointer_down(tab_id, (10.0, 10.0), PointerButton::Primary))?;
  let msgs = controller.handle_message(support::pointer_up(tab_id, (10.0, 10.0), PointerButton::Primary))?;
  let (_input_node_id, options, anchor_css) =
    find_datalist_opened(&msgs, tab_id).expect("expected DatalistOpened after focusing input");
  assert_eq!(options.len(), 3, "expected all options when query is empty");
  assert!(anchor_css.size.width > 0.0 && anchor_css.size.height > 0.0, "expected non-empty anchor rect");

  // Type "ap" to filter suggestions down to apple + apricot.
  let msgs = controller.handle_message(support::text_input(tab_id, "ap"))?;
  let (_input_node_id, options, _anchor_css) =
    find_datalist_opened(&msgs, tab_id).expect("expected DatalistOpened after typing");
  let values: Vec<&str> = options.iter().map(|o| o.value.as_str()).collect();
  assert_eq!(values, ["apple", "apricot"]);

  // Type an unmatched query to close the popup.
  let msgs = controller.handle_message(support::text_input(tab_id, "zzz"))?;
  assert!(
    msgs.iter().any(|msg| matches!(msg, WorkerToUi::DatalistClosed { tab_id: got } if *got == tab_id)),
    "expected DatalistClosed when no options match"
  );

  Ok(())
}

#[test]
fn browser_tab_controller_datalist_choose_sets_value_and_closes_popup() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (240, 120);
  let url = "https://example.com/index.html";
  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          #q { position: absolute; left: 0; top: 0; width: 180px; height: 28px; font-family: "Noto Sans Mono"; font-size: 18px; }
        </style>
      </head>
      <body>
        <input id="q" list="choices" value="">
        <datalist id="choices">
          <option value="alpha"></option>
          <option value="beta"></option>
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

  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  // Focus the input so the popup opens.
  let _ = controller.handle_message(support::pointer_down(tab_id, (10.0, 10.0), PointerButton::Primary))?;
  let open_msgs = controller.handle_message(support::pointer_up(tab_id, (10.0, 10.0), PointerButton::Primary))?;
  let (input_node_id, options, _anchor_css) =
    find_datalist_opened(&open_msgs, tab_id).expect("expected DatalistOpened after focusing input");
  let chosen = options
    .iter()
    .find(|opt| opt.value == "beta")
    .cloned()
    .expect("expected beta option");

  let choose_msgs =
    controller.handle_message(UiToWorker::datalist_choose(tab_id, input_node_id, chosen.option_node_id))?;

  assert!(
    choose_msgs.iter().any(|msg| matches!(msg, WorkerToUi::DatalistClosed { tab_id: got } if *got == tab_id)),
    "expected DatalistClosed on choose"
  );

  // Choosing a value should apply it to the input like a user edit (value + caret + user validity).
  let input = find_element_by_id(controller.document().dom(), "q").expect("expected input element");
  assert_eq!(input.get_attribute_ref("value"), Some("beta"));

  let state = controller.interaction_state();
  assert!(state.is_focused(input_node_id), "expected focus to remain on input");
  assert!(state.has_user_validity(input_node_id), "expected choose to set user validity");
  let caret = state
    .text_edit_for(input_node_id)
    .map(|e| e.caret)
    .expect("expected text edit state for focused input");
  assert_eq!(caret, "beta".chars().count(), "expected caret to move to end of chosen value");

  Ok(())
}
