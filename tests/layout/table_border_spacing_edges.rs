use std::collections::HashMap;

use fastrender::api::FastRender;
use fastrender::geometry::Rect;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};

fn find_table<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.style.as_ref().map(|s| s.display), Some(Display::Table)) {
    return Some(node);
  }
  node.children.iter().find_map(find_table)
}

fn collect_text(node: &FragmentNode, out: &mut String) {
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push_str(text);
  }
  for child in node.children.iter() {
    collect_text(child, out);
  }
}

fn collect_cells(node: &FragmentNode, origin: (f32, f32), cells: &mut HashMap<char, Rect>) {
  let pos = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::TableCell)
  ) {
    let mut text = String::new();
    collect_text(node, &mut text);
    let label = text
      .trim()
      .chars()
      .find(|c| c.is_ascii_alphabetic())
      .unwrap();
    let rect = Rect::from_xywh(pos.0, pos.1, node.bounds.width(), node.bounds.height());
    cells.insert(label, rect);
  }
  for child in node.children.iter() {
    collect_cells(child, pos, cells);
  }
}

#[test]
fn separate_border_spacing_uses_full_edges() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { border-collapse: separate; border-spacing: 10px 6px; padding: 0; border: 0; }
          td { width: 40px; height: 10px; padding: 0; margin: 0; border: 0; }
        </style>
      </head>
      <body>
        <table><tr><td>A</td></tr></table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  // Use a viewport width that matches the expected table width so that the table's
  // resolved width comes from the cell + edge border-spacing geometry.
  let tree = renderer.layout_document(&dom, 60, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment");
  let mut cells = HashMap::new();
  for child in table.children.iter() {
    collect_cells(child, (0.0, 0.0), &mut cells);
  }
  let cell = cells.get(&'A').expect("table cell fragment");

  assert!(
    (cell.x() - 10.0).abs() < 0.1,
    "expected edge cell to start after full horizontal border-spacing (got x={})",
    cell.x()
  );
  assert!(
    (cell.y() - 6.0).abs() < 0.1,
    "expected edge cell to start after full vertical border-spacing (got y={})",
    cell.y()
  );

  assert!(
    (table.bounds.width() - 60.0).abs() < 0.1,
    "expected table width to include left+right border-spacing (got {})",
    table.bounds.width()
  );
  assert!(
    (cell.width() - 40.0).abs() < 0.1,
    "expected cell width to exclude edge border-spacing (got {})",
    cell.width()
  );
  assert!(
    (table.bounds.height() - 22.0).abs() < 0.1,
    "expected table height to include top+bottom border-spacing (got {})",
    table.bounds.height()
  );
}

#[test]
fn separate_border_spacing_uses_full_edges_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { border-collapse: separate; border-spacing: 10px 6px; padding: 0; border: 0; direction: rtl; }
          td { width: 40px; height: 10px; padding: 0; margin: 0; border: 0; }
        </style>
      </head>
      <body>
        <table><tr><td>A</td></tr></table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 60, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment");
  let mut cells = HashMap::new();
  for child in table.children.iter() {
    collect_cells(child, (0.0, 0.0), &mut cells);
  }
  let cell = cells.get(&'A').expect("table cell fragment");

  assert!(
    (cell.x() - 10.0).abs() < 0.1,
    "expected edge cell to start after full horizontal border-spacing in RTL (got x={})",
    cell.x()
  );
  assert!(
    (cell.y() - 6.0).abs() < 0.1,
    "expected edge cell to start after full vertical border-spacing in RTL (got y={})",
    cell.y()
  );
  assert!(
    (table.bounds.width() - 60.0).abs() < 0.1,
    "expected table width to include left+right border-spacing in RTL (got {})",
    table.bounds.width()
  );
  assert!(
    (table.bounds.height() - 22.0).abs() < 0.1,
    "expected table height to include top+bottom border-spacing in RTL (got {})",
    table.bounds.height()
  );
}

#[test]
fn border_spacing_applies_at_table_edges_in_separated_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            border-collapse: separate;
            border-spacing: 10px 6px;
            width: 200px;
            table-layout: fixed;
            border: 0;
            padding: 0;
          }
          td { padding: 0; margin: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  for child in table.children.iter() {
    collect_cells(child, (0.0, 0.0), &mut cells);
  }

  let a = cells.get(&'A').expect("A cell");
  let b = cells.get(&'B').expect("B cell");

  assert!(
    (a.x() - 10.0).abs() < 0.1,
    "first cell should start after horizontal border-spacing (expected ~10, got {})",
    a.x()
  );
  assert!(
    (a.y() - 6.0).abs() < 0.1,
    "first row should start after vertical border-spacing (expected ~6, got {})",
    a.y()
  );

  let spacing_x = b.x() - (a.x() + a.width());
  assert!(
    (spacing_x - 10.0).abs() < 0.1,
    "spacing between cells should match border-spacing (expected ~10, got {spacing_x})"
  );
}

#[test]
fn border_spacing_applies_at_table_edges_in_separated_model_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            border-collapse: separate;
            border-spacing: 10px 6px;
            width: 200px;
            table-layout: fixed;
            border: 0;
            padding: 0;
            direction: rtl;
          }
          td { padding: 0; margin: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  for child in table.children.iter() {
    collect_cells(child, (0.0, 0.0), &mut cells);
  }

  let a = cells.get(&'A').expect("A cell");
  let b = cells.get(&'B').expect("B cell");

  assert!(
    a.x() > b.x(),
    "expected RTL order A (right) > B (left), got A.x={} B.x={}",
    a.x(),
    b.x()
  );
  assert!(
    (b.x() - 10.0).abs() < 0.1,
    "leftmost cell should start after horizontal edge border-spacing in RTL (expected ~10, got {})",
    b.x()
  );
  assert!(
    (a.y() - 6.0).abs() < 0.1,
    "first row should start after vertical border-spacing in RTL (expected ~6, got {})",
    a.y()
  );

  let spacing_x = a.x() - (b.x() + b.width());
  assert!(
    (spacing_x - 10.0).abs() < 0.1,
    "spacing between cells should match border-spacing in RTL (expected ~10, got {spacing_x})"
  );
}

#[test]
fn collapsed_border_model_ignores_border_spacing() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            border-collapse: collapse;
            border-spacing: 10px 6px;
            table-layout: fixed;
            width: 100px;
            border: none;
            padding: 0;
          }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          td { padding: 0; margin: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <col class="col1" />
          <col class="col2" />
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  for child in table.children.iter() {
    collect_cells(child, (0.0, 0.0), &mut cells);
  }

  let a = cells.get(&'A').expect("A cell");
  let b = cells.get(&'B').expect("B cell");

  assert!(
    (a.x() - 0.0).abs() < 0.1,
    "expected border-spacing to be ignored at the leading edge in collapsed model (A.x={})",
    a.x()
  );
  assert!(
    (a.y() - 0.0).abs() < 0.1,
    "expected border-spacing to be ignored at the top edge in collapsed model (A.y={})",
    a.y()
  );
  assert!(
    (b.y() - 0.0).abs() < 0.1,
    "expected border-spacing to be ignored at the top edge in collapsed model (B.y={})",
    b.y()
  );
  let spacing_x = b.x() - (a.x() + a.width());
  assert!(
    spacing_x.abs() < 0.1,
    "expected border-spacing to be ignored between cells in collapsed model (gap={spacing_x})"
  );
}

#[test]
fn collapsed_border_model_ignores_border_spacing_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            border-collapse: collapse;
            border-spacing: 10px 6px;
            table-layout: fixed;
            width: 100px;
            border: none;
            padding: 0;
            direction: rtl;
          }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          td { padding: 0; margin: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <col class="col1" />
          <col class="col2" />
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  for child in table.children.iter() {
    collect_cells(child, (0.0, 0.0), &mut cells);
  }

  let a = cells.get(&'A').expect("A cell");
  let b = cells.get(&'B').expect("B cell");

  assert!(
    a.x() > b.x(),
    "expected RTL order A (right) > B (left), got A.x={} B.x={}",
    a.x(),
    b.x()
  );
  assert!(
    (b.x() - 0.0).abs() < 0.1,
    "expected border-spacing to be ignored at the leading edge in RTL collapsed model (B.x={})",
    b.x()
  );
  assert!(
    (a.y() - 0.0).abs() < 0.1,
    "expected border-spacing to be ignored at the top edge in RTL collapsed model (A.y={})",
    a.y()
  );
  assert!(
    (b.y() - 0.0).abs() < 0.1,
    "expected border-spacing to be ignored at the top edge in RTL collapsed model (B.y={})",
    b.y()
  );

  let spacing_x = a.x() - (b.x() + b.width());
  assert!(
    spacing_x.abs() < 0.1,
    "expected border-spacing to be ignored between cells in RTL collapsed model (gap={spacing_x})"
  );
}
