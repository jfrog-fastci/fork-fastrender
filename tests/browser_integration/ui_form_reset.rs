#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::dom::{DomNode, DomNodeType};
use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{PointerButton, RepaintReason, TabId, WorkerToUi};
use fastrender::ui::BrowserTabController;
use fastrender::Result;

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

fn textarea_text_content(node: &DomNode) -> String {
  let mut out = String::new();
  let mut stack = vec![node];
  while let Some(node) = stack.pop() {
    if let DomNodeType::Text { content } = &node.node_type {
      out.push_str(content);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  out
}

fn assert_no_navigation(msgs: &[WorkerToUi]) {
  assert!(
    !msgs.iter().any(|msg| matches!(
      msg,
      WorkerToUi::NavigationStarted { .. }
        | WorkerToUi::NavigationCommitted { .. }
        | WorkerToUi::NavigationFailed { .. }
    )),
    "expected no navigation messages, got {msgs:?}"
  );
}

fn assert_defaults(dom: &DomNode) {
  let input = find_element_by_id(dom, "text");
  assert_eq!(input.get_attribute_ref("value"), Some("a"));

  let checkbox = find_element_by_id(dom, "cb");
  assert!(
    checkbox.get_attribute_ref("checked").is_some(),
    "expected checkbox to be restored to checked"
  );

  let option1 = find_element_by_id(dom, "o1");
  let option2 = find_element_by_id(dom, "o2");
  assert!(
    option1.get_attribute_ref("selected").is_some(),
    "expected first option to be selected after reset"
  );
  assert!(
    option2.get_attribute_ref("selected").is_none(),
    "expected second option to be deselected after reset"
  );

  let textarea = find_element_by_id(dom, "ta");
  assert_eq!(
    textarea.get_attribute_ref("data-fastr-value"),
    None,
    "expected textarea override value to be cleared on reset"
  );
  assert_eq!(
    textarea_text_content(textarea),
    "hello",
    "expected textarea to fall back to its original text content"
  );
}

#[test]
fn form_reset_restores_defaults_for_pointer_and_keyboard_activation() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (360, 240);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; font-family: "Noto Sans Mono"; font-size: 16px; }
          #text { position: absolute; left: 10px; top: 10px; width: 180px; height: 22px; }
          #cb { position: absolute; left: 10px; top: 40px; width: 18px; height: 18px; }
          #sel { position: absolute; left: 10px; top: 70px; width: 180px; height: 22px; }
          #ta { position: absolute; left: 10px; top: 100px; width: 220px; height: 60px; }
          #r1 { position: absolute; left: 10px; top: 170px; width: 80px; height: 26px; }
          #r2 { position: absolute; left: 110px; top: 170px; width: 80px; height: 26px; }
        </style>
      </head>
      <body>
        <form id="f">
          <input id="text" value="a">
          <input id="cb" type="checkbox" checked>
          <select id="sel">
            <option id="o1" selected>One</option>
            <option id="o2">Two</option>
          </select>
          <textarea id="ta">hello</textarea>
          <input id="r1" type="reset" value="Reset">
          <button id="r2" type="reset">Reset2</button>
        </form>
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

  // Mutate controls.
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    (15.0, 15.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    (15.0, 15.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::text_input(tab_id, "b"))?;

  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    (15.0, 45.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    (15.0, 45.0),
    PointerButton::Primary,
  ))?;

  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    (15.0, 75.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    (15.0, 75.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowDown))?;

  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    (15.0, 120.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    (15.0, 120.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  // Pointer-click the <input type=reset>.
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    (15.0, 180.0),
    PointerButton::Primary,
  ))?;
  let msgs = controller.handle_message(support::pointer_up(
    tab_id,
    (15.0, 180.0),
    PointerButton::Primary,
  ))?;
  assert!(
    msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected reset click to repaint"
  );
  assert_no_navigation(&msgs);
  assert_defaults(controller.document().dom());

  // Mutate controls again, then keyboard-activate the <button type=reset>.
  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    (15.0, 15.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    (15.0, 15.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::text_input(tab_id, "b"))?;

  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    (15.0, 45.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    (15.0, 45.0),
    PointerButton::Primary,
  ))?;

  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    (15.0, 75.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    (15.0, 75.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::ArrowDown))?;

  let _ = controller.handle_message(support::pointer_down(
    tab_id,
    (15.0, 120.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::pointer_up(
    tab_id,
    (15.0, 120.0),
    PointerButton::Primary,
  ))?;
  let _ = controller.handle_message(support::text_input(tab_id, "X"))?;

  // Tab from textarea -> input reset -> button reset, then press Space.
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Tab))?;
  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Tab))?;
  let msgs = controller.handle_message(support::key_action(tab_id, KeyAction::Space))?;
  assert!(
    msgs
      .iter()
      .any(|msg| matches!(msg, WorkerToUi::FrameReady { .. })),
    "expected keyboard reset activation to repaint"
  );
  assert_no_navigation(&msgs);
  assert_defaults(controller.document().dom());

  Ok(())
}
