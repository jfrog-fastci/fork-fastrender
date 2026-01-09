use fastrender::tree::box_tree::{FormControlKind, ReplacedType};
use fastrender::interaction::InteractionEngine;
use fastrender::interaction::absolute_bounds_for_box_id;
use fastrender::{BrowserDocument, BoxType, Overflow, Point, RenderOptions, Result};
use super::support::deterministic_renderer;

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

#[test]
fn select_listbox_wheel_scroll_updates_element_scroll_state() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
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

  let options = RenderOptions::new().with_viewport(200, 200);
  let mut doc = BrowserDocument::new(deterministic_renderer(), html, options)?;
  doc.render_frame_with_scroll_state()?;
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after render");

  let select_box_id =
    find_listbox_select_box_id(prepared.box_tree()).expect("expected box tree to contain <select>");

  let select_is_scroll_container = prepared.fragment_tree().iter_fragments().any(|fragment| {
    fragment.box_id() == Some(select_box_id)
      && fragment.get_style().is_some_and(|style| {
        matches!(style.overflow_y, Overflow::Auto | Overflow::Scroll)
      })
  });
  assert!(
    select_is_scroll_container,
    "expected <select size> listbox to be a scroll container"
  );

  let changed = doc.wheel_scroll_at_viewport_point(Point::new(5.0, 5.0), (0.0, 40.0))?;
  assert!(changed, "expected wheel scroll to update the scroll state");
  assert!(
    doc.scroll_state().element_offset(select_box_id).y > 0.0,
    "expected element scroll offset for select to increase"
  );

  Ok(())
}

#[test]
fn select_listbox_wheel_scroll_affects_click_row_mapping() -> Result<()> {
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();
  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          select { border: 5px solid black; padding: 7px; line-height: 20px; font-size: 20px; }
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

  let options = RenderOptions::new().with_viewport(200, 200);
  let mut doc = BrowserDocument::new(deterministic_renderer(), html, options)?;
  doc.render_frame_with_scroll_state()?;
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after render");

  let select_box_id =
    find_listbox_select_box_id(prepared.box_tree()).expect("expected box tree to contain <select>");

  let select_rect =
    absolute_bounds_for_box_id(prepared.fragment_tree(), select_box_id).expect("select rect");
  let row_height = 20.0_f32;
  let border = 5.0_f32;
  let padding = 7.0_f32;
  let content_rect_y = select_rect.y() + border + padding;
  let content_rect_x = select_rect.x() + border + padding;

  // Scroll by ~2 rows, then click within the top visible row. The click should select the
  // scrolled-to option, not the original first row.
  doc.wheel_scroll_at_viewport_point(Point::new(5.0, 5.0), (0.0, row_height * 2.0))?;
  let scroll_state = doc.scroll_state();
  let scroll_y = scroll_state.element_offset(select_box_id).y;
  assert!(scroll_y > 0.0, "expected listbox select to scroll");

  let click_viewport_point = Point::new(content_rect_x + 1.0, content_rect_y + row_height / 2.0);
  let page_point = click_viewport_point.translate(scroll_state.viewport);

  // Expected row index based on the same math as the select listbox painter.
  let local_y = page_point.y - content_rect_y;
  let expected_row_idx = ((local_y + scroll_y) / row_height).floor().max(0.0) as usize;

  let mut engine = InteractionEngine::new();
  doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let _ = engine.pointer_down(dom, box_tree, fragment_tree, &scroll_state, click_viewport_point);
    let (changed, _action) =
      engine.pointer_up_with_scroll(dom, box_tree, fragment_tree, &scroll_state, click_viewport_point, "", "");
    (changed, ())
  })?;

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

  assert_eq!(
    selected_option_indices(doc.dom()),
    vec![expected_row_idx],
    "expected click to select the scrolled-to option row"
  );

  Ok(())
}
