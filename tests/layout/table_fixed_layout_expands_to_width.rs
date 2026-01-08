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

fn collect_cells(node: &FragmentNode, origin: (f32, f32), out: &mut HashMap<char, Rect>) {
  let pos = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::TableCell)
  ) {
    let mut text = String::new();
    collect_text(node, &mut text);
    if let Some(label) = text.trim().chars().find(|c| c.is_ascii_alphabetic()) {
      let rect = Rect::from_xywh(pos.0, pos.1, node.bounds.width(), node.bounds.height());
      out.insert(label, rect);
    }
  }
  for child in node.children.iter() {
    collect_cells(child, pos, out);
  }
}

#[test]
fn table_fixed_layout_expands_to_specified_width() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 300px;
            border-collapse: separate;
            border-spacing: 0;
            padding: 0;
            border: 0;
          }
          col { width: 50px; }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <col />
          <col />
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let table_width = table.bounds.width();
  assert!(
    (table_width - 300.0).abs() < 0.1,
    "expected table width ~300px, got {table_width}"
  );

  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);
  assert_eq!(cells.len(), 2, "expected two table cells");

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");

  assert!(
    a.x() < b.x(),
    "expected LTR order A (left) < B, got A.x={} B.x={}",
    a.x(),
    b.x()
  );

  let widths = [a.width(), b.width()];
  let sum: f32 = widths.iter().sum();
  assert!(
    (sum - 300.0).abs() < 0.1,
    "expected cell widths to sum to ~300px, got {sum} (cells={widths:?})"
  );

  for (idx, width) in widths.iter().enumerate() {
    assert!(
      (*width - 150.0).abs() < 0.1,
      "expected cell {idx} width ~150px, got {width}"
    );
  }

  let gap = b.x() - (a.x() + a.width());
  assert!(gap.abs() < 0.1, "expected cells to be adjacent (gap={gap})");
}

#[test]
fn table_fixed_layout_expands_to_specified_width_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 300px;
            border-collapse: separate;
            border-spacing: 0;
            padding: 0;
            border: 0;
            direction: rtl;
          }
          col { width: 50px; }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <col />
          <col />
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let table_width = table.bounds.width();
  assert!(
    (table_width - 300.0).abs() < 0.1,
    "expected table width ~300px, got {table_width}"
  );

  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);
  assert_eq!(cells.len(), 2, "expected two table cells");

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");

  assert!(
    a.x() > b.x(),
    "expected RTL order A (right) > B (left), got A.x={} B.x={}",
    a.x(),
    b.x()
  );

  let widths = [a.width(), b.width()];
  let sum: f32 = widths.iter().sum();
  assert!(
    (sum - 300.0).abs() < 0.1,
    "expected cell widths to sum to ~300px in RTL, got {sum} (cells={widths:?})"
  );

  for (idx, width) in widths.iter().enumerate() {
    assert!(
      (*width - 150.0).abs() < 0.1,
      "expected cell {idx} width ~150px in RTL, got {width}"
    );
  }

  let gap = a.x() - (b.x() + b.width());
  assert!(gap.abs() < 0.1, "expected cells to be adjacent in RTL (gap={gap})");
}

#[test]
fn table_fixed_layout_expands_to_specified_width_collapsed_border_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 300px;
            border-collapse: collapse;
            border: none;
            padding: 0;
          }
          col { width: 50px; }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <col />
          <col />
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let table_width = table.bounds.width();
  assert!(
    (table_width - 300.0).abs() < 0.1,
    "expected table width ~300px, got {table_width}"
  );

  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);
  assert_eq!(cells.len(), 2, "expected two table cells");

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");

  assert!(
    a.x() < b.x(),
    "expected LTR order A (left) < B, got A.x={} B.x={}",
    a.x(),
    b.x()
  );

  let widths = [a.width(), b.width()];
  let sum: f32 = widths.iter().sum();
  assert!(
    (sum - 300.0).abs() < 0.1,
    "expected cell widths to sum to ~300px, got {sum} (cells={widths:?})"
  );

  for (idx, width) in widths.iter().enumerate() {
    assert!(
      (*width - 150.0).abs() < 0.1,
      "expected cell {idx} width ~150px, got {width}"
    );
  }

  let gap = b.x() - (a.x() + a.width());
  assert!(gap.abs() < 0.1, "expected cells to be adjacent (gap={gap})");
}

#[test]
fn table_fixed_layout_expands_to_specified_width_collapsed_border_model_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 300px;
            border-collapse: collapse;
            border: none;
            padding: 0;
            direction: rtl;
          }
          col { width: 50px; }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <col />
          <col />
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let table_width = table.bounds.width();
  assert!(
    (table_width - 300.0).abs() < 0.1,
    "expected table width ~300px, got {table_width}"
  );

  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);
  assert_eq!(cells.len(), 2, "expected two table cells");

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");

  assert!(
    a.x() > b.x(),
    "expected RTL order A (right) > B (left), got A.x={} B.x={}",
    a.x(),
    b.x()
  );

  let widths = [a.width(), b.width()];
  let sum: f32 = widths.iter().sum();
  assert!(
    (sum - 300.0).abs() < 0.1,
    "expected cell widths to sum to ~300px in RTL, got {sum} (cells={widths:?})"
  );

  for (idx, width) in widths.iter().enumerate() {
    assert!(
      (*width - 150.0).abs() < 0.1,
      "expected cell {idx} width ~150px in RTL, got {width}"
    );
  }

  let gap = a.x() - (b.x() + b.width());
  assert!(gap.abs() < 0.1, "expected cells to be adjacent in RTL (gap={gap})");
}
