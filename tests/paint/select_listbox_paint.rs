use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::scroll::ScrollState;
use fastrender::{DisplayItem, FastRender};

#[test]
fn select_listbox_emits_rows_and_selection_highlight() {
  let html = r#"
    <!doctype html>
    <style>
      body { margin: 0; }
      select { display: block; margin: 0; }
    </style>
    <select id="multi" multiple size="4">
      <optgroup label="Enabled group">
        <option>One</option>
        <option selected>Two</option>
      </optgroup>
      <optgroup label="Disabled group" disabled>
        <option selected>Three</option>
        <option>Four</option>
      </optgroup>
      <option>Outside</option>
    </select>
    <select id="size3" size="3">
      <option>Alpha</option>
      <option selected>Beta</option>
      <option>Gamma</option>
      <option>Delta</option>
    </select>
  "#;

  let mut renderer = FastRender::new().expect("renderer");
  let dom = renderer.parse_html(html).expect("parsed");
  let fragment_tree = renderer.layout_document(&dom, 320, 240).expect("laid out");

  let list = DisplayListBuilder::new()
    .with_scroll_state(ScrollState::default())
    .build_tree_with_stacking(&fragment_tree);

  let text_origins: Vec<f32> = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::Text(text) => Some(text.origin.y),
      _ => None,
    })
    .collect();

  assert!(
    text_origins.len() >= 6,
    "expected listbox selects to paint multiple rows, got {} text items",
    text_origins.len()
  );
  assert!(
    text_origins
      .windows(2)
      .all(|pair| pair[1] + 0.01 >= pair[0]),
    "expected painted text runs to be in row order: {:?}",
    text_origins
  );

  let text_xs: Vec<f32> = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::Text(text) => Some(text.origin.x),
      _ => None,
    })
    .collect();
  let min_x = text_xs
    .iter()
    .copied()
    .fold(f32::INFINITY, |a, b| a.min(b));
  let max_x = text_xs
    .iter()
    .copied()
    .fold(f32::NEG_INFINITY, |a, b| a.max(b));
  assert!(
    (max_x - min_x) >= 6.0,
    "expected options inside optgroups to be indented (x range [{min_x}, {max_x}])"
  );

  let has_selection_highlight = list.items().iter().any(|item| match item {
    DisplayItem::FillRect(fill) => fill.color.a > 0.0 && fill.color.a < 1.0,
    DisplayItem::FillRoundedRect(fill) => fill.color.a > 0.0 && fill.color.a < 1.0,
    _ => false,
  });
  assert!(
    has_selection_highlight,
    "expected listbox select to paint a selection highlight"
  );
}
