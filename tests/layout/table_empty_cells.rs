use fastrender::api::FastRender;
use fastrender::style::color::Rgba;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};

fn count_table_cells(node: &FragmentNode) -> usize {
  let mut count = 0;
  if node
    .style
    .as_ref()
    .map(|s| matches!(s.display, Display::TableCell))
    .unwrap_or(false)
  {
    count += 1;
  }
  for child in node.children.iter() {
    count += count_table_cells(child);
  }
  count
}

fn fragment_contains_text(node: &FragmentNode, needle: &str) -> bool {
  match &node.content {
    FragmentContent::Text { text, .. } if text.contains(needle) => true,
    _ => node
      .children
      .iter()
      .any(|child| fragment_contains_text(child, needle)),
  }
}

fn find_cell_fragment_with_text<'a>(
  node: &'a FragmentNode,
  needle: &str,
) -> Option<&'a FragmentNode> {
  if fragment_contains_text(node, needle)
    && node
      .style
      .as_ref()
      .map(|s| matches!(s.display, Display::TableCell))
      .unwrap_or(false)
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_cell_fragment_with_text(child, needle) {
      return Some(found);
    }
  }
  None
}

fn collect_table_cell_fragments<'a>(node: &'a FragmentNode, out: &mut Vec<&'a FragmentNode>) {
  if node
    .style
    .as_ref()
    .map(|s| matches!(s.display, Display::TableCell))
    .unwrap_or(false)
  {
    out.push(node);
  }
  for child in node.children.iter() {
    collect_table_cell_fragments(child, out);
  }
}

fn cell_fragments<'a>(tree: &'a fastrender::tree::fragment_tree::FragmentTree) -> Vec<&'a FragmentNode> {
  let mut cells = Vec::new();
  collect_table_cell_fragments(&tree.root, &mut cells);
  for fragment in &tree.additional_fragments {
    collect_table_cell_fragments(fragment, &mut cells);
  }
  cells
}

#[test]
fn empty_cells_hide_layouts_many_empty_cells() {
  let empty_row: String = (0..32).map(|_| "<td></td>").collect();
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          table {{ empty-cells: hide; border-collapse: separate; border-spacing: 1px; }}
          td {{ width: 8px; height: 8px; border: 1px solid black; }}
        </style>
      </head>
      <body>
        <table>
          <tr>{row}</tr>
          <tr>{row}</tr>
          <tr>{row}</tr>
        </table>
      </body>
    </html>
  "#,
    row = empty_row
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 300).unwrap();

  let mut cell_count = count_table_cells(&tree.root);
  for fragment in &tree.additional_fragments {
    cell_count += count_table_cells(fragment);
  }

  assert!(
    cell_count >= 96,
    "expected empty cells to produce fragments, saw {cell_count}"
  );
}

#[test]
fn non_empty_cells_keep_background_with_empty_cells_hide() {
  let html = r#"
    <html>
      <head>
        <style>
          table { empty-cells: hide; border-collapse: separate; }
          td { background: rgb(200, 20, 30); border: 1px solid black; }
        </style>
      </head>
      <body>
        <table>
          <tr><td></td><td>filled</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let filled_cell = find_cell_fragment_with_text(&tree.root, "filled")
    .or_else(|| {
      tree
        .additional_fragments
        .iter()
        .find_map(|fragment| find_cell_fragment_with_text(fragment, "filled"))
    })
    .expect("non-empty cell fragment should be present");

  let style = filled_cell
    .style
    .as_ref()
    .expect("table cell fragments should carry styles");
  assert!(
    !style.background_color.is_transparent(),
    "non-empty cell should not be treated as visually empty when empty-cells: hide is used"
  );
  assert!(
    !matches!(style.background_color, Rgba::TRANSPARENT),
    "non-empty cell background should be preserved"
  );
}

#[test]
fn empty_cells_hide_suppresses_background_and_border_for_empty_cells() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: separate; empty-cells: hide; border-spacing: 0; }
          td { background: rgb(200, 20, 30); border: 2px solid black; padding: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr><td></td><td>filled</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let filled = find_cell_fragment_with_text(&tree.root, "filled")
    .or_else(|| {
      tree
        .additional_fragments
        .iter()
        .find_map(|fragment| find_cell_fragment_with_text(fragment, "filled"))
    })
    .expect("filled cell fragment");

  let mut cells = cell_fragments(&tree);
  assert_eq!(cells.len(), 2, "expected 2 table-cell fragments");
  let empty = cells
    .drain(..)
    .find(|cell| !fragment_contains_text(cell, "filled"))
    .expect("empty cell fragment");

  let empty_style = empty.style.as_ref().expect("cell style");
  assert!(
    empty_style.background_color.is_transparent(),
    "empty cell background should be suppressed"
  );
  assert_eq!(empty_style.border_top_color, Rgba::TRANSPARENT);
  assert_eq!(empty_style.border_right_color, Rgba::TRANSPARENT);
  assert_eq!(empty_style.border_bottom_color, Rgba::TRANSPARENT);
  assert_eq!(empty_style.border_left_color, Rgba::TRANSPARENT);

  let filled_style = filled.style.as_ref().expect("cell style");
  assert!(
    !filled_style.background_color.is_transparent(),
    "non-empty cell background should not be suppressed"
  );
  assert_ne!(filled_style.border_top_color, Rgba::TRANSPARENT);
}

#[test]
fn empty_element_counts_as_cell_content_for_empty_cells() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: separate; empty-cells: hide; border-spacing: 0; }
          td { background: rgb(200, 20, 30); border: 2px solid black; padding: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr><td><span></span></td><td></td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let cells = cell_fragments(&tree);
  assert_eq!(cells.len(), 2, "expected 2 table-cell fragments");

  let suppressed = cells
    .iter()
    .filter(|cell| {
      cell
        .style
        .as_ref()
        .map(|s| s.background_color.is_transparent())
        .unwrap_or(false)
    })
    .count();
  assert_eq!(suppressed, 1, "expected only the truly empty cell to be suppressed");

  let preserved = cells
    .iter()
    .filter(|cell| {
      cell
        .style
        .as_ref()
        .map(|s| !s.background_color.is_transparent())
        .unwrap_or(false)
    })
    .count();
  assert_eq!(preserved, 1, "expected the empty element cell to keep its background");
}

#[test]
fn white_space_pre_makes_whitespace_only_cells_non_empty_for_empty_cells() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: separate; empty-cells: hide; border-spacing: 0; }
          td { background: rgb(200, 20, 30); border: 2px solid black; padding: 0; }
          td.pre { white-space: pre; }
        </style>
      </head>
      <body>
        <table>
          <tr><td class="pre">   </td><td>   </td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let cells = cell_fragments(&tree);
  assert_eq!(cells.len(), 2, "expected 2 table-cell fragments");

  let suppressed = cells
    .iter()
    .filter(|cell| {
      cell
        .style
        .as_ref()
        .map(|s| s.background_color.is_transparent())
        .unwrap_or(false)
    })
    .count();
  assert_eq!(suppressed, 1, "expected only the collapsed-whitespace cell to be suppressed");
}

#[test]
fn empty_cells_hide_collapses_empty_rows_and_dedupes_border_spacing() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: separate; empty-cells: hide; border-spacing: 0 10px; }
          td { height: 20px; padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr><td>A</td></tr>
          <tr><td></td></tr>
          <tr><td>C</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 300).unwrap();

  let first = find_cell_fragment_with_text(&tree.root, "A")
    .or_else(|| {
      tree
        .additional_fragments
        .iter()
        .find_map(|fragment| find_cell_fragment_with_text(fragment, "A"))
    })
    .expect("row 1 cell fragment");
  let third = find_cell_fragment_with_text(&tree.root, "C")
    .or_else(|| {
      tree
        .additional_fragments
        .iter()
        .find_map(|fragment| find_cell_fragment_with_text(fragment, "C"))
    })
    .expect("row 3 cell fragment");

  let gap = third.bounds.y() - (first.bounds.y() + first.bounds.height());
  assert!(
    (gap - 10.0).abs() < 0.75,
    "expected only one 10px border-spacing gap between non-empty rows (got {gap:.2})"
  );
}
