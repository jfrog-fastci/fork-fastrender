use fastrender::dom::DomNode;
use fastrender::interaction::{absolute_bounds_for_box_id, InteractionAction, InteractionEngine};
use fastrender::tree::box_tree::{FormControlKind, ReplacedType};
use fastrender::ui::messages::{PointerButton, PointerModifiers};
use fastrender::{BoxType, BrowserDocument, Point, RenderOptions, Result};

use super::support;

fn find_select_box_id(box_tree: &fastrender::BoxTree) -> Option<usize> {
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

fn collect_selected_option_ids(dom: &DomNode) -> Vec<String> {
  let mut out = Vec::new();
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
      && node.get_attribute_ref("selected").is_some()
    {
      out.push(node.get_attribute_ref("id").unwrap_or("").to_string());
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  out
}

fn click_point_for_row(select_rect: fastrender::Rect, row_height: f32, row_idx: usize) -> Point {
  Point::new(
    select_rect.x() + 10.0,
    select_rect.y() + row_height * (row_idx as f32 + 0.5),
  )
}

fn click_with_modifiers(
  doc: &mut BrowserDocument,
  engine: &mut InteractionEngine,
  point: Point,
  modifiers: PointerModifiers,
) -> Result<()> {
  let scroll_state = doc.scroll_state();
  doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let down_changed = engine.pointer_down(dom, box_tree, fragment_tree, &scroll_state, point);
    let (up_changed, _action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      point,
      PointerButton::Primary,
      modifiers,
      "https://example.invalid/page.html",
      "https://example.invalid/page.html",
    );
    (down_changed || up_changed, ())
  })?;
  Ok(())
}

#[test]
fn select_listbox_multiselect_click_semantics_match_native_listboxes() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          /* Deterministic row height for hit-testing. */
          select { display: block; border: 0; padding: 0; font-size: 20px; line-height: 20px; height: 180px; width: 120px; }
        </style>
      </head>
      <body>
        <select multiple size="9" id="sel">
          <option id="o1" value="a">A</option>
          <option id="o2" value="b">B</option>
          <option id="o3" value="c">C</option>
          <option id="o4" value="d" disabled>Disabled</option>
          <option id="o5" value="e">E</option>
          <optgroup label="Disabled Group" disabled>
            <option id="o6" value="f">F</option>
            <option id="o7" value="g">G</option>
          </optgroup>
          <option id="o8" value="h">H</option>
        </select>
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(240, 240);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  doc.render_frame_with_scroll_state()?;
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after render");

  let select_box_id =
    find_select_box_id(prepared.box_tree()).expect("expected box tree to contain <select>");
  let select_rect =
    absolute_bounds_for_box_id(prepared.fragment_tree(), select_box_id).expect("select rect");
  let row_height = select_rect.height() / 9.0_f32;

  let mut engine = InteractionEngine::new();

  // Plain click selects exactly one option (clears others) and establishes the anchor.
  click_with_modifiers(
    &mut doc,
    &mut engine,
    click_point_for_row(select_rect, row_height, 2),
    PointerModifiers::NONE,
  )?;
  assert_eq!(collect_selected_option_ids(doc.dom()), vec!["o3"]);

  // Cmd/Ctrl-click toggles without clearing.
  let command = PointerModifiers::CTRL | PointerModifiers::META;
  click_with_modifiers(
    &mut doc,
    &mut engine,
    click_point_for_row(select_rect, row_height, 0),
    command,
  )?;
  assert_eq!(collect_selected_option_ids(doc.dom()), vec!["o1", "o3"]);

  // Plain click replaces the selection again.
  click_with_modifiers(
    &mut doc,
    &mut engine,
    click_point_for_row(select_rect, row_height, 1),
    PointerModifiers::NONE,
  )?;
  assert_eq!(collect_selected_option_ids(doc.dom()), vec!["o2"]);

  // Shift-click selects a contiguous range from the anchor and ignores disabled options/optgroups.
  click_with_modifiers(
    &mut doc,
    &mut engine,
    click_point_for_row(select_rect, row_height, 8),
    PointerModifiers::SHIFT,
  )?;
  assert_eq!(
    collect_selected_option_ids(doc.dom()),
    vec!["o2", "o3", "o5", "o8"],
    "expected range selection to skip disabled options and optgroup label rows"
  );

  // Clicking a disabled option is a no-op.
  click_with_modifiers(
    &mut doc,
    &mut engine,
    click_point_for_row(select_rect, row_height, 3),
    command,
  )?;
  assert_eq!(collect_selected_option_ids(doc.dom()), vec!["o2", "o3", "o5", "o8"]);

  // Clicking an optgroup label row is a no-op (row index 5 in the flattened list).
  click_with_modifiers(
    &mut doc,
    &mut engine,
    click_point_for_row(select_rect, row_height, 5),
    PointerModifiers::NONE,
  )?;
  assert_eq!(collect_selected_option_ids(doc.dom()), vec!["o2", "o3", "o5", "o8"]);

  // Clicking a disabled optgroup descendant is also a no-op.
  click_with_modifiers(
    &mut doc,
    &mut engine,
    click_point_for_row(select_rect, row_height, 6),
    command,
  )?;
  assert_eq!(collect_selected_option_ids(doc.dom()), vec!["o2", "o3", "o5", "o8"]);

  // Cmd/Ctrl-click still toggles enabled options without clearing.
  click_with_modifiers(
    &mut doc,
    &mut engine,
    click_point_for_row(select_rect, row_height, 2),
    command,
  )?;
  assert_eq!(collect_selected_option_ids(doc.dom()), vec!["o2", "o5", "o8"]);

  Ok(())
}

#[test]
fn select_listbox_multiselect_form_submission_includes_multiple_values() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <meta charset="utf-8">
        <style>
          html, body { margin: 0; padding: 0; }
          select { position: absolute; left: 0; top: 0; border: 0; padding: 0; font-size: 20px; line-height: 20px; height: 80px; width: 120px; }
          #submit { position: absolute; left: 0; top: 120px; width: 120px; height: 40px; }
        </style>
      </head>
      <body>
        <form action="https://example.invalid/submit" method="get">
          <select multiple size="4" name="v" id="sel">
            <option id="a" value="a">A</option>
            <option id="b" value="b">B</option>
            <option id="c" value="c">C</option>
            <option id="d" value="d">D</option>
          </select>
          <input id="submit" type="submit" value="Go">
        </form>
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(240, 200);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  doc.render_frame_with_scroll_state()?;
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after render");

  let select_box_id =
    find_select_box_id(prepared.box_tree()).expect("expected box tree to contain <select>");
  let select_rect =
    absolute_bounds_for_box_id(prepared.fragment_tree(), select_box_id).expect("select rect");
  let row_height = select_rect.height() / 4.0_f32;

  let mut engine = InteractionEngine::new();

  // Select two options using plain click + Cmd/Ctrl-click.
  click_with_modifiers(
    &mut doc,
    &mut engine,
    click_point_for_row(select_rect, row_height, 1),
    PointerModifiers::NONE,
  )?;
  let command = PointerModifiers::CTRL | PointerModifiers::META;
  click_with_modifiers(
    &mut doc,
    &mut engine,
    click_point_for_row(select_rect, row_height, 3),
    command,
  )?;

  // Submit the form.
  let scroll_state = doc.scroll_state();
  let submit_point = Point::new(10.0, 130.0);
  let action = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let down_changed = engine.pointer_down(dom, box_tree, fragment_tree, &scroll_state, submit_point);
    let (up_changed, action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      submit_point,
      PointerButton::Primary,
      PointerModifiers::NONE,
      "https://example.invalid/page.html",
      "https://example.invalid/page.html",
    );
    (down_changed || up_changed, action)
  })?;

  let InteractionAction::Navigate { href } = action else {
    panic!("expected Navigate, got {action:?}");
  };
  assert_eq!(
    href,
    "https://example.invalid/submit?v=b&v=d",
    "expected multiple selected values to submit as repeated name/value pairs"
  );

  Ok(())
}

