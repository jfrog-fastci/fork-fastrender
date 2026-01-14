#![cfg(feature = "browser_ui")]

use fastrender::ui::messages::{KeyAction, RepaintReason, TabId, UiToWorker, WorkerToUi};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

fn find_element_by_id<'a>(
  dom: &'a fastrender::dom::DomNode,
  element_id: &str,
) -> Option<&'a fastrender::dom::DomNode> {
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

#[test]
fn color_picker_choose_updates_dom_value_and_repaints() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId(1);
  let viewport_css = (200, 80);
  let url = "https://example.com/index.html";
  let html = r##"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
        </style>
      </head>
      <body>
        <input type="color" id="c" value="#ff0000">
      </body>
    </html>
  "##;

  let mut controller = BrowserTabController::from_html_with_renderer(
    super::support::deterministic_renderer(),
    tab_id,
    html,
    url,
    viewport_css,
    1.0,
  )?;

  // Ensure the document is laid out before keyboard activation.
  let initial_msgs = controller.handle_message(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })?;
  assert!(
    initial_msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected initial FrameReady"
  );

  // Focus the color input via Tab.
  let _ = controller.handle_message(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::Tab,
  })?;

  // Activate (Space) to request opening the picker.
  let open_msgs = controller.handle_message(UiToWorker::KeyAction {
    tab_id,
    key: KeyAction::Space,
  })?;
  let (input_node_id, opened_value) = open_msgs
    .iter()
    .find_map(|msg| match msg {
      WorkerToUi::ColorPickerOpened {
        tab_id: msg_tab,
        input_node_id,
        value,
        ..
      } if *msg_tab == tab_id => Some((*input_node_id, value.clone())),
      _ => None,
    })
    .expect("expected ColorPickerOpened after keyboard activation");
  assert_eq!(opened_value, "#ff0000");

  let choose_msgs = controller.handle_message(UiToWorker::ColorPickerChoose {
    tab_id,
    input_node_id,
    value: "#00ff00".to_string(),
  })?;
  assert!(
    choose_msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected FrameReady after ColorPickerChoose"
  );

  let dom = controller.document().dom();
  let input = find_element_by_id(dom, "c").expect("expected <input id=c>");
  assert_eq!(
    input.get_attribute_ref("value"),
    Some("#00ff00"),
    "expected input value to update after ColorPickerChoose"
  );

  Ok(())
}
