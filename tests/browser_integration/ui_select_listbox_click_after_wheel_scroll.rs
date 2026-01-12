#![cfg(feature = "browser_ui")]

use fastrender::interaction::absolute_bounds_for_box_id;
use fastrender::tree::box_tree::{FormControlKind, ReplacedType};
use fastrender::ui::messages::{PointerButton, PointerModifiers, RepaintReason, TabId, UiToWorker};
use fastrender::ui::BrowserTabController;
use fastrender::{BoxType, Point, Result};

use super::support;

fn find_listbox_select_box_id(box_tree: &fastrender::BoxTree) -> Option<usize> {
  let mut stack = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if let BoxType::Replaced(replaced) = &node.box_type {
      if let ReplacedType::FormControl(control) = &replaced.replaced_type {
        if matches!(control.control, FormControlKind::Select(_)) {
          return Some(node.id);
        }
      }
    }

    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

fn selected_option_indices(dom: &fastrender::dom::DomNode) -> Vec<usize> {
  let mut indices = Vec::new();
  let mut option_idx = 0usize;
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
    {
      if node.get_attribute_ref("selected").is_some() {
        indices.push(option_idx);
      }
      option_idx += 1;
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  indices
}

#[test]
fn browser_tab_controller_listbox_click_accounts_for_wheel_scroll() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  let _lock = super::stage_listener_test_lock();
  let tab_id = TabId(1);
  let viewport_css = (200, 200);
  let url = "https://example.com/index.html";

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          /* Deterministic listbox row heights. */
          select {
            position: absolute;
            top: 0;
            left: 0;
            border: 0;
            padding: 0;
            line-height: 20px;
            font-size: 20px;
          }
        </style>
      </head>
      <body>
        <select size="3">
          <option>Option 1</option>
          <option>Option 2</option>
          <option>Option 3</option>
          <option>Option 4</option>
          <option>Option 5</option>
          <option>Option 6</option>
          <option>Option 7</option>
          <option>Option 8</option>
          <option>Option 9</option>
          <option>Option 10</option>
          <option>Option 11</option>
          <option>Option 12</option>
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

  // Initial paint to populate cached layout artifacts used by scroll hit-testing.
  let initial_msgs = controller.handle_message(UiToWorker::RequestRepaint {
    tab_id,
    reason: RepaintReason::Explicit,
  })?;
  assert!(
    initial_msgs
      .iter()
      .any(|msg| matches!(msg, fastrender::ui::messages::WorkerToUi::FrameReady { .. })),
    "expected initial repaint to produce a FrameReady message",
  );

  let (select_box_id, select_rect, row_height) = {
    let prepared = controller
      .document()
      .prepared()
      .expect("expected prepared doc after repaint");
    let select_box_id = find_listbox_select_box_id(prepared.box_tree())
      .expect("expected box tree to contain a listbox <select>");
    let select_rect =
      absolute_bounds_for_box_id(prepared.fragment_tree(), select_box_id).expect("select rect");
    let row_height = select_rect.height() / 3.0_f32;
    assert!(
      row_height.is_finite() && row_height > 0.0,
      "expected non-zero row height, got {row_height:?}"
    );
    (select_box_id, select_rect, row_height)
  };

  // Scroll by ~2 rows.
  let _ = controller.handle_message(UiToWorker::Scroll {
    tab_id,
    delta_css: (0.0, row_height * 2.0),
    pointer_css: Some((5.0, 5.0)),
  })?;

  let scroll_state = controller.scroll_state().clone();
  let scroll_y = scroll_state.element_offset(select_box_id).y;
  assert!(
    scroll_y > 0.0,
    "expected listbox wheel scroll to increase element scroll offset"
  );

  // Click the top visible row (in viewport coordinates). The selected option should correspond to
  // the scrolled-to row, not the original first row.
  let click_viewport_point = Point::new(10.0, row_height / 2.0);
  let page_point = click_viewport_point.translate(scroll_state.viewport);
  let local_y = page_point.y - select_rect.y();
  let expected_row_idx = ((local_y + scroll_y) / row_height).floor().max(0.0) as usize;
  assert!(
    expected_row_idx > 0,
    "expected wheel scroll to move the top visible row beyond the first option, got idx={expected_row_idx}"
  );

  let _ = controller.handle_message(UiToWorker::PointerDown {
    tab_id,
    pos_css: (click_viewport_point.x, click_viewport_point.y),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  })?;
  let _ = controller.handle_message(UiToWorker::PointerUp {
    tab_id,
    pos_css: (click_viewport_point.x, click_viewport_point.y),
    button: PointerButton::Primary,
    modifiers: PointerModifiers::NONE,
  })?;

  assert_eq!(
    selected_option_indices(controller.document().dom()),
    vec![expected_row_idx],
    "expected click to select the scrolled-to option row"
  );

  Ok(())
}
