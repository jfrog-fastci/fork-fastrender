#![cfg(feature = "browser_ui")]

use super::support;
use fastrender::interaction::KeyAction;
use fastrender::ui::messages::{PointerButton, RepaintReason, TabId};
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

fn node_id_by_id_attr(dom: &DomNode, id_attr: &str) -> usize {
  let ids = fastrender::dom::enumerate_dom_ids(dom);
  let mut stack: Vec<&DomNode> = vec![dom];
  while let Some(node) = stack.pop() {
    if node.get_attribute_ref("id") == Some(id_attr) {
      return *ids
        .get(&(node as *const DomNode))
        .unwrap_or_else(|| panic!("node id missing for element with id={id_attr:?}"));
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  panic!("expected element with id attribute {id_attr:?}");
}

fn details_fixture_html() -> &'static str {
  r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          summary { position: absolute; left: 0; top: 0; width: 200px; height: 40px; }
          #content { position: absolute; left: 0; top: 50px; width: 200px; height: 40px; }
        </style>
      </head>
      <body>
        <details id="d">
          <summary id="s">Title</summary>
          <div id="content">Hidden</div>
        </details>
     </body>
    </html>"#
}

fn details_nested_span_fixture_html() -> &'static str {
  r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          summary { position: absolute; left: 0; top: 0; width: 200px; height: 40px; }
          #inner { display: block; width: 40px; height: 40px; }
          #content { position: absolute; left: 0; top: 50px; width: 200px; height: 40px; }
        </style>
      </head>
      <body>
        <details id="d">
          <summary id="s"><span id="inner">Title</span></summary>
          <div id="content">Hidden</div>
        </details>
      </body>
    </html>"#
}

#[test]
fn details_summary_pointer_click_toggles_open() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 120);
  let url = "https://example.com/index.html";

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    details_fixture_html(),
    url,
    viewport_css,
    1.0,
  )?;

  // Render once so hit-testing works.
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  assert!(
    find_element_by_id(controller.document().dom(), "d")
      .get_attribute_ref("open")
      .is_none(),
    "fixture <details> should start closed"
  );

  let click = (10.0, 10.0);
  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  assert!(
    find_element_by_id(controller.document().dom(), "d")
      .get_attribute_ref("open")
      .is_some(),
    "expected click on <summary> to add open attribute to parent <details>"
  );

  let _ =
    controller.handle_message(support::pointer_down(tab_id, click, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, click, PointerButton::Primary))?;

  assert!(
    find_element_by_id(controller.document().dom(), "d")
      .get_attribute_ref("open")
      .is_none(),
    "expected second click on <summary> to remove open attribute from parent <details>"
  );

  Ok(())
}

#[test]
fn details_summary_is_tab_focusable_and_space_toggles_open() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 120);
  let url = "https://example.com/index.html";

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    details_fixture_html(),
    url,
    viewport_css,
    1.0,
  )?;
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  let summary_id = node_id_by_id_attr(controller.document().dom(), "s");

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Tab))?;
  assert_eq!(
    controller.interaction_state().focused,
    Some(summary_id),
    "Tab should focus the first <summary> of <details>"
  );

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Space))?;
  assert!(
    find_element_by_id(controller.document().dom(), "d")
      .get_attribute_ref("open")
      .is_some(),
    "Space on focused <summary> should toggle parent <details open>"
  );

  Ok(())
}

#[test]
fn details_summary_enter_toggles_open() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 120);
  let url = "https://example.com/index.html";

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    details_fixture_html(),
    url,
    viewport_css,
    1.0,
  )?;
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  let summary_id = node_id_by_id_attr(controller.document().dom(), "s");

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Tab))?;
  assert_eq!(
    controller.interaction_state().focused,
    Some(summary_id),
    "Tab should focus the first <summary> of <details>"
  );

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Enter))?;
  assert!(
    find_element_by_id(controller.document().dom(), "d")
      .get_attribute_ref("open")
      .is_some(),
    "Enter on focused <summary> should toggle parent <details open>"
  );

  let _ = controller.handle_message(support::key_action(tab_id, KeyAction::Enter))?;
  assert!(
    find_element_by_id(controller.document().dom(), "d")
      .get_attribute_ref("open")
      .is_none(),
    "a second Enter on focused <summary> should close the <details>"
  );

  Ok(())
}

#[test]
fn details_summary_click_on_descendant_and_release_elsewhere_still_toggles_and_focuses() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (240, 120);
  let url = "https://example.com/index.html";

  let mut controller = BrowserTabController::from_html_with_renderer(
    support::deterministic_renderer(),
    tab_id,
    details_nested_span_fixture_html(),
    url,
    viewport_css,
    1.0,
  )?;
  let _ = controller.handle_message(support::request_repaint(tab_id, RepaintReason::Explicit))?;

  let summary_id = node_id_by_id_attr(controller.document().dom(), "s");

  assert!(
    find_element_by_id(controller.document().dom(), "d")
      .get_attribute_ref("open")
      .is_none(),
    "fixture <details> should start closed"
  );

  let down = (10.0, 10.0); // inside #inner
  let up = (190.0, 10.0); // inside <summary> but outside #inner
  let _ =
    controller.handle_message(support::pointer_down(tab_id, down, PointerButton::Primary))?;
  let _ = controller.handle_message(support::pointer_up(tab_id, up, PointerButton::Primary))?;

  assert!(
    find_element_by_id(controller.document().dom(), "d")
      .get_attribute_ref("open")
      .is_some(),
    "expected click within <summary> (even across descendants) to toggle parent <details open>"
  );
  assert_eq!(
    controller.interaction_state().focused,
    Some(summary_id),
    "clicking the details summary should focus the <summary> element"
  );

  Ok(())
}
