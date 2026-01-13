use super::support::deterministic_renderer;
use fastrender::dom::DomNode;
use fastrender::interaction::{absolute_bounds_for_box_id, InteractionEngine};
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

fn find_dom_by_id<'a>(node: &'a DomNode, id: &str) -> Option<&'a DomNode> {
  if node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  node
    .children
    .iter()
    .find_map(|child| find_dom_by_id(child, id))
}

fn option_has_selected(dom: &DomNode, option_id: &str) -> bool {
  find_dom_by_id(dom, option_id)
    .and_then(|node| node.get_attribute_ref("selected"))
    .is_some()
}

fn assert_selected(dom: &DomNode, expected: &[&str]) {
  for id in ["opt1", "opt2", "opt3", "opt4"] {
    assert_eq!(
      option_has_selected(dom, id),
      expected.contains(&id),
      "expected {id} selected={} (DOM selected attributes did not match)",
      expected.contains(&id)
    );
  }
}

#[test]
fn select_listbox_multi_select_respects_pointer_modifiers() -> Result<()> {
  let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
  #[cfg(feature = "browser_ui")]
  let _lock = super::stage_listener_test_lock();

  let html = r#"<!doctype html>
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          #sel {
            line-height: 30px;
            height: 120px; /* 4 rows */
            padding: 0;
            border: 0;
          }
        </style>
      </head>
      <body>
        <select id="sel" multiple size="4">
          <option id="opt1" selected>Option 1</option>
          <option id="opt2">Option 2</option>
          <option id="opt3">Option 3</option>
          <option id="opt4">Option 4</option>
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
  let row_height = select_rect.height() / 4.0_f32;

  let click_point = |row_index: usize| -> Point {
    Point::new(
      select_rect.x() + 10.0,
      select_rect.y() + row_height * (row_index as f32 + 0.5),
    )
  };

  let scroll_state = doc.scroll_state();
  let mut engine = InteractionEngine::new();

  // 1) Plain click option 3 → only option 3 selected.
  let click_opt3 = click_point(2);
  doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let _ = engine.pointer_down_with_click_count(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_opt3,
      PointerButton::Primary,
      PointerModifiers::NONE,
      1,
    );
    let (changed, _action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_opt3,
      PointerButton::Primary,
      PointerModifiers::NONE,
      "",
      "",
    );
    (changed, ())
  })?;
  assert_selected(doc.dom(), &["opt3"]);

  // 2) Ctrl/Cmd click option 1 → options 1 and 3 selected.
  let click_opt1 = click_point(0);
  let cmd = if cfg!(target_os = "macos") {
    PointerModifiers::META
  } else {
    PointerModifiers::CTRL
  };
  doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let _ = engine.pointer_down_with_click_count(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_opt1,
      PointerButton::Primary,
      cmd,
      1,
    );
    let (changed, _action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_opt1,
      PointerButton::Primary,
      cmd,
      "",
      "",
    );
    (changed, ())
  })?;
  assert_selected(doc.dom(), &["opt1", "opt3"]);

  // 3) Shift click option 4 → options 1..4 selected (range from anchor=option1).
  let click_opt4 = click_point(3);
  doc.mutate_dom_with_layout_artifacts(|dom, box_tree, fragment_tree| {
    let _ = engine.pointer_down_with_click_count(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_opt4,
      PointerButton::Primary,
      PointerModifiers::SHIFT,
      1,
    );
    let (changed, _action) = engine.pointer_up_with_scroll(
      dom,
      box_tree,
      fragment_tree,
      &scroll_state,
      click_opt4,
      PointerButton::Primary,
      PointerModifiers::SHIFT,
      "",
      "",
    );
    (changed, ())
  })?;
  assert_selected(doc.dom(), &["opt1", "opt2", "opt3", "opt4"]);

  Ok(())
}
