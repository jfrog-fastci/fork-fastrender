use fastrender::interaction::{absolute_bounds_for_box_id, InteractionEngine, KeyAction};
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
fn select_dropdown_arrow_keys_skip_hidden_options_when_box_tree_is_available() -> Result<()> {
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
        <select>
          <option selected>Visible 1</option>
          <option hidden>Hidden</option>
          <option>Visible 2</option>
        </select>
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(200, 200);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  doc.render_frame_with_scroll_state()?;
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after render");

  let select_box_id =
    find_select_box_id(prepared.box_tree()).expect("expected box tree to contain <select>");
  let select_rect =
    absolute_bounds_for_box_id(prepared.fragment_tree(), select_box_id).expect("select rect");

  let scroll_state = doc.scroll_state();
  let click_viewport_point = Point::new(select_rect.x() + 5.0, select_rect.y() + 5.0);

  let mut engine = InteractionEngine::new();
  let (after_down, after_up) =
    doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
      let mut changed = false;

      changed |= engine.pointer_down(
        dom,
        box_tree,
        fragment_tree,
        &scroll_state,
        click_viewport_point,
      );
      let (up_changed, _action) = engine.pointer_up_with_scroll(
        dom,
        box_tree,
        fragment_tree,
        &scroll_state,
        click_viewport_point,
        PointerButton::Primary,
        PointerModifiers::NONE,
        true,
        "",
        "",
      );
      changed |= up_changed;

      changed |= engine.key_action_with_box_tree(dom, Some(box_tree), KeyAction::ArrowDown);
      let after_down = selected_option_indices(dom);

      changed |= engine.key_action_with_box_tree(dom, Some(box_tree), KeyAction::ArrowUp);
      let after_up = selected_option_indices(dom);

      (changed, (after_down, after_up))
    })?;

  assert_eq!(
    after_down,
    vec![2],
    "expected ArrowDown to skip hidden options and select the next visible option"
  );
  assert_eq!(
    after_up,
    vec![0],
    "expected ArrowUp to skip hidden options and return to the previous visible option"
  );

  Ok(())
}

#[test]
fn select_dropdown_page_up_down_moves_selection_by_ten_options() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let mut options_html = String::new();
  for i in 0..25 {
    if i == 0 {
      options_html.push_str(&format!("<option selected>Option {i}</option>"));
    } else {
      options_html.push_str(&format!("<option>Option {i}</option>"));
    }
  }

  let html = format!(
    r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body {{ margin: 0; padding: 0; }}
          /* Keep the content rect aligned with fragment bounds for deterministic hit-testing. */
          select {{ border: 0; padding: 0; line-height: 20px; font-size: 20px; }}
        </style>
      </head>
      <body>
        <select>
          {options_html}
        </select>
      </body>
    </html>"#
  );

  let options = RenderOptions::new().with_viewport(200, 200);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), &html, options)?;
  doc.render_frame_with_scroll_state()?;
  let prepared = doc
    .prepared()
    .expect("expected BrowserDocument to have cached layout after render");

  let select_box_id =
    find_select_box_id(prepared.box_tree()).expect("expected box tree to contain <select>");
  let select_rect =
    absolute_bounds_for_box_id(prepared.fragment_tree(), select_box_id).expect("select rect");

  let scroll_state = doc.scroll_state();
  let click_viewport_point = Point::new(select_rect.x() + 5.0, select_rect.y() + 5.0);

  let mut engine = InteractionEngine::new();
  let (after_down, after_up) = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let mut changed = false;

    changed |= engine.pointer_down(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_viewport_point,
    );
    let (up_changed, _action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_viewport_point,
      PointerButton::Primary,
      PointerModifiers::NONE,
      true,
      "",
      "",
    );
    changed |= up_changed;

    changed |= engine.key_action_with_box_tree(dom, Some(box_tree), KeyAction::PageDown);
    let after_down = selected_option_indices(dom);

    changed |= engine.key_action_with_box_tree(dom, Some(box_tree), KeyAction::PageUp);
    let after_up = selected_option_indices(dom);

    (changed, (after_down, after_up))
  })?;

  assert_eq!(
    after_down,
    vec![10],
    "expected PageDown to move selection forward by 10 options"
  );
  assert_eq!(
    after_up,
    vec![0],
    "expected PageUp to move selection backward by 10 options"
  );

  Ok(())
}
