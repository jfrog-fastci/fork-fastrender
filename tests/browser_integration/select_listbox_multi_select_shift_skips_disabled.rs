use super::support::deterministic_renderer;
use fastrender::interaction::{
  absolute_bounds_for_box_id, content_rect_for_border_rect, InteractionEngine,
};
use fastrender::tree::box_tree::{FormControlKind, ReplacedType};
use fastrender::ui::messages::{PointerButton, PointerModifiers};
use fastrender::{BoxType, BrowserDocument, Point, RenderOptions, Result};

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

fn option_disabled_selected(dom: &fastrender::dom::DomNode) -> Vec<(bool, bool)> {
  let mut out = Vec::new();
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
    {
      let disabled = node.get_attribute_ref("disabled").is_some();
      let selected = node.get_attribute_ref("selected").is_some();
      out.push((disabled, selected));
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  out
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
fn select_listbox_multi_select_shift_skips_disabled_options() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          /* Keep the content rect aligned with fragment bounds for deterministic hit-testing. */
          select { border: 0; padding: 0; line-height: 20px; font-size: 20px; }
        </style>
      </head>
      <body>
        <select multiple size="4">
          <option>Option 1</option>
          <option disabled>Option 2</option>
          <option>Option 3</option>
          <option>Option 4</option>
        </select>
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(200, 200);
  let mut doc = BrowserDocument::new(deterministic_renderer(), html, options)?;
  let mut engine = InteractionEngine::new();

  // Render once so we can compute deterministic click coordinates.
  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;

  // ---------------------------------------------------------------------------
  // Click option 1 to set the selection anchor.
  // ---------------------------------------------------------------------------
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after render");
  let select_box_id =
    find_listbox_select_box_id(prepared.box_tree()).expect("expected box tree to contain <select>");
  let select_rect =
    absolute_bounds_for_box_id(prepared.fragment_tree(), select_box_id).expect("select rect");
  let select_style = prepared
    .fragment_tree()
    .iter_fragments()
    .find(|fragment| fragment.box_id() == Some(select_box_id))
    .and_then(|fragment| fragment.get_style())
    .expect("expected <select> fragment to have a computed style");
  let viewport_size = prepared.fragment_tree().viewport_size();
  let content_rect = content_rect_for_border_rect(select_rect, select_style, viewport_size);
  let row_height = content_rect.height().max(0.0) / 4.0;
  assert!(
    row_height.is_finite() && row_height > 0.0,
    "expected non-zero row height"
  );

  let scroll_state = doc.scroll_state();
  let click1_page_point = Point::new(content_rect.x() + 1.0, content_rect.y() + row_height / 2.0);
  let click1_viewport_point = Point::new(
    click1_page_point.x - scroll_state.viewport.x,
    click1_page_point.y - scroll_state.viewport.y,
  );

  doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let down_changed = engine.pointer_down(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click1_viewport_point,
    );
    let (up_changed, _action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click1_viewport_point,
      PointerButton::Primary,
      PointerModifiers::NONE,
      true,
      "",
      "",
    );
    (down_changed || up_changed, ())
  })?;

  let flags_after_click = option_disabled_selected(doc.dom());
  assert_eq!(
    flags_after_click.len(),
    4,
    "expected four <option> elements"
  );
  assert!(flags_after_click[1].0, "expected option 2 to be disabled");
  assert_eq!(
    selected_option_indices(doc.dom()),
    vec![0],
    "expected initial click to select only option 1"
  );

  // Re-render so the box tree's `SelectControl` snapshot reflects the updated selectedness before
  // issuing the shift-click gesture.
  doc.render_frame_with_scroll_state_and_interaction_state(Some(engine.interaction_state()))?;

  // ---------------------------------------------------------------------------
  // Shift-click option 4: range selection should skip disabled option 2.
  // ---------------------------------------------------------------------------
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after re-render");
  let select_box_id =
    find_listbox_select_box_id(prepared.box_tree()).expect("expected box tree to contain <select>");
  let select_rect =
    absolute_bounds_for_box_id(prepared.fragment_tree(), select_box_id).expect("select rect");
  let select_style = prepared
    .fragment_tree()
    .iter_fragments()
    .find(|fragment| fragment.box_id() == Some(select_box_id))
    .and_then(|fragment| fragment.get_style())
    .expect("expected <select> fragment to have a computed style");
  let viewport_size = prepared.fragment_tree().viewport_size();
  let content_rect = content_rect_for_border_rect(select_rect, select_style, viewport_size);
  let row_height = content_rect.height().max(0.0) / 4.0;
  assert!(
    row_height.is_finite() && row_height > 0.0,
    "expected non-zero row height"
  );

  let scroll_state = doc.scroll_state();
  let click4_page_point = Point::new(
    content_rect.x() + 1.0,
    content_rect.y() + row_height * 3.0 + row_height / 2.0,
  );
  let click4_viewport_point = Point::new(
    click4_page_point.x - scroll_state.viewport.x,
    click4_page_point.y - scroll_state.viewport.y,
  );

  doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let down_changed = engine.pointer_down_with_click_count(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click4_viewport_point,
      PointerButton::Primary,
      PointerModifiers::SHIFT,
      1,
    );
    let (up_changed, _action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click4_viewport_point,
      PointerButton::Primary,
      PointerModifiers::SHIFT,
      true,
      "",
      "",
    );
    (down_changed || up_changed, ())
  })?;

  assert_eq!(
    selected_option_indices(doc.dom()),
    vec![0, 2, 3],
    "expected shift-range selection to skip disabled option 2"
  );
  let flags_after_shift = option_disabled_selected(doc.dom());
  assert_eq!(
    flags_after_shift.len(),
    4,
    "expected four <option> elements after shift click"
  );
  assert!(
    flags_after_shift[1].0,
    "expected option 2 to remain disabled"
  );
  assert!(
    !flags_after_shift[1].1,
    "expected disabled option 2 to remain unselected after shift range selection"
  );

  Ok(())
}
