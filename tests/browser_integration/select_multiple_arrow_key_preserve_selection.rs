use fastrender::interaction::{InteractionEngine, KeyAction};
use fastrender::{BrowserDocument, RenderOptions, Result};

use super::support;

fn find_first_select_node_id(dom: &fastrender::dom::DomNode) -> usize {
  fn dfs(node: &fastrender::dom::DomNode, next_id: &mut usize) -> Option<usize> {
    let id = *next_id;
    if node
      .tag_name()
      .is_some_and(|tag| tag.eq_ignore_ascii_case("select"))
    {
      return Some(id);
    }
    for child in &node.children {
      *next_id = next_id.saturating_add(1);
      if let Some(found) = dfs(child, next_id) {
        return Some(found);
      }
    }
    None
  }

  let mut next_id = 1usize;
  dfs(dom, &mut next_id).expect("expected HTML to contain a <select>")
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
fn select_multiple_arrow_down_preserves_other_selections() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <body>
        <select multiple size="4">
          <option selected>Option 0</option>
          <option>Option 1</option>
          <option selected>Option 2</option>
          <option>Option 3</option>
        </select>
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(200, 200);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  doc.render_frame_with_scroll_state()?;

  let mut engine = InteractionEngine::new();
  let after_down = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, _fragment_tree| {
    let mut changed = false;

    let select_id = find_first_select_node_id(dom);
    let (focus_changed, _action) = engine.focus_node_id(dom, Some(select_id), true);
    changed |= focus_changed;

    assert_eq!(selected_option_indices(dom), vec![0, 2]);

    changed |= engine.key_action_with_box_tree(dom, Some(box_tree), KeyAction::ArrowDown);
    let after_down = selected_option_indices(dom);

    (changed, after_down)
  })?;

  assert!(
    after_down.contains(&0),
    "expected ArrowDown in <select multiple> to preserve unrelated selections (option 0)"
  );
  assert_eq!(
    after_down,
    vec![0, 3],
    "expected ArrowDown in <select multiple> to move the active selection (last selected option) \
     down without clearing other selections"
  );

  Ok(())
}

#[test]
fn select_multiple_arrow_up_preserves_other_selections() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <body>
        <select multiple size="4">
          <option selected>Option 0</option>
          <option>Option 1</option>
          <option selected>Option 2</option>
          <option>Option 3</option>
        </select>
      </body>
    </html>
  "#;

  let options = RenderOptions::new().with_viewport(200, 200);
  let mut doc = BrowserDocument::new(support::deterministic_renderer(), html, options)?;
  doc.render_frame_with_scroll_state()?;

  let mut engine = InteractionEngine::new();
  let after_up = doc.mutate_dom_with_layout_artifacts(|dom, box_tree, _fragment_tree| {
    let mut changed = false;

    let select_id = find_first_select_node_id(dom);
    let (focus_changed, _action) = engine.focus_node_id(dom, Some(select_id), true);
    changed |= focus_changed;

    assert_eq!(selected_option_indices(dom), vec![0, 2]);

    changed |= engine.key_action_with_box_tree(dom, Some(box_tree), KeyAction::ArrowUp);
    let after_up = selected_option_indices(dom);

    (changed, after_up)
  })?;

  assert!(
    after_up.contains(&0),
    "expected ArrowUp in <select multiple> to preserve unrelated selections (option 0)"
  );
  assert_eq!(
    after_up,
    vec![0, 1],
    "expected ArrowUp in <select multiple> to move the active selection (last selected option) \
     up without clearing other selections"
  );

  Ok(())
}
