use fastrender::dom::{enumerate_dom_ids, DomNode};
use fastrender::interaction::{absolute_bounds_for_box_id, InteractionEngine};
use fastrender::tree::box_tree::{FormControlKind, ReplacedType};
use fastrender::ui::messages::{PointerButton, PointerModifiers};
use fastrender::{BoxType, BrowserDocument, Point, RenderOptions, Result};

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

fn collect_option_hidden_selected(dom: &fastrender::dom::DomNode) -> Vec<(bool, bool)> {
  let mut out = Vec::new();
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("option"))
    {
      let hidden = node.get_attribute_ref("hidden").is_some();
      let selected = node.get_attribute_ref("selected").is_some();
      out.push((hidden, selected));
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  out
}

fn find_select(dom: &DomNode) -> Option<&DomNode> {
  let mut stack = vec![dom];
  while let Some(node) = stack.pop() {
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("select"))
    {
      return Some(node);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn select_listbox_hidden_option_click_selects_first_visible_option_and_marks_user_validity(
) -> Result<()> {
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
        <select size="3">
          <option hidden>Hidden</option>
          <option>Visible 1</option>
          <option>Visible 2</option>
          <option>Visible 3</option>
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
    find_listbox_select_box_id(prepared.box_tree()).expect("expected box tree to contain <select>");
  let select_rect =
    absolute_bounds_for_box_id(prepared.fragment_tree(), select_box_id).expect("select rect");
  let row_height = select_rect.height() / 3.0_f32;

  let scroll_state = doc.scroll_state();
  let click_viewport_point = Point::new(10.0, row_height / 2.0);

  let mut engine = InteractionEngine::new();
  doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let _ = engine.pointer_down(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_viewport_point,
    );
    let (changed, _action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_viewport_point,
      PointerButton::Primary,
      PointerModifiers::NONE,
      "",
      "",
    );
    (changed, ())
  })?;

  let option_flags = collect_option_hidden_selected(doc.dom());
  assert!(
    option_flags.len() >= 2,
    "expected at least two <option> elements"
  );

  let selected_indices: Vec<usize> = option_flags
    .iter()
    .enumerate()
    .filter_map(|(idx, (_, selected))| (*selected).then_some(idx))
    .collect();
  assert_eq!(
    selected_indices,
    vec![1],
    "expected click on first visible row to select the first visible <option> (skipping hidden rows)"
  );
  assert_eq!(
    find_select(doc.dom()).and_then(|node| node.get_attribute_ref("data-fastr-user-validity")),
    None,
    "renderer must not inject data-fastr-user-validity onto the DOM"
  );
  let select = find_select(doc.dom()).expect("expected <select> element");
  let select_id = *enumerate_dom_ids(doc.dom())
    .get(&(select as *const DomNode))
    .expect("select node id");
  assert!(
    engine.interaction_state().has_user_validity(select_id),
    "expected <select> to flip internal user validity state after selection change"
  );

  Ok(())
}
