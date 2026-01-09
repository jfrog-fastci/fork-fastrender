use fastrender::tree::box_tree::{FormControlKind, ReplacedType};
use fastrender::{BrowserDocument, BoxType, Overflow, Point, RenderOptions, Result};

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
  let mut doc = BrowserDocument::from_html(html, options)?;
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

