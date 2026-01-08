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
fn table_fixed_layout_colgroup_width_applies_to_col_children() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 100px;
            border-collapse: separate;
            border-spacing: 0;
            padding: 0;
            border: 0;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <colgroup style="width: 40px">
            <col />
            <col />
          </colgroup>
          <col />
          <tr><td>A</td><td>B</td><td>C</td></tr>
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
    (table_width - 100.0).abs() < 0.1,
    "expected table width ~100px, got {table_width}"
  );

  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);
  assert_eq!(cells.len(), 3, "expected three table cells");

  let a = cells.get(&'A').expect("cell A");
  let b = cells.get(&'B').expect("cell B");
  let c = cells.get(&'C').expect("cell C");

  assert!(
    (a.width() - 40.0).abs() < 0.1,
    "expected first column width ~40px from <colgroup>, got {}",
    a.width()
  );
  assert!(
    (b.width() - 40.0).abs() < 0.1,
    "expected second column width ~40px from <colgroup>, got {}",
    b.width()
  );
  assert!(
    (c.width() - 20.0).abs() < 0.1,
    "expected third column to take remaining width (~20px), got {}",
    c.width()
  );

  let gap_ab = b.x() - (a.x() + a.width());
  assert!(gap_ab.abs() < 0.1, "expected cells A/B adjacent (gap={gap_ab})");
  let gap_bc = c.x() - (b.x() + b.width());
  assert!(gap_bc.abs() < 0.1, "expected cells B/C adjacent (gap={gap_bc})");
}

#[test]
fn table_fixed_layout_colgroup_width_applies_to_col_children_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 100px;
            border-collapse: separate;
            border-spacing: 0;
            padding: 0;
            border: 0;
            direction: rtl;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <colgroup style="width: 40px">
            <col />
            <col />
          </colgroup>
          <col />
          <tr><td>A</td><td>B</td><td>C</td></tr>
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
    (table_width - 100.0).abs() < 0.1,
    "expected table width ~100px, got {table_width}"
  );

  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);
  assert_eq!(cells.len(), 3, "expected three table cells");

  let a = cells.get(&'A').expect("cell A");
  let b = cells.get(&'B').expect("cell B");
  let c = cells.get(&'C').expect("cell C");

  assert!(
    a.x() > b.x() && b.x() > c.x(),
    "expected RTL order A > B > C, got A.x={} B.x={} C.x={}",
    a.x(),
    b.x(),
    c.x()
  );

  assert!(
    (a.width() - 40.0).abs() < 0.1,
    "expected first source column width ~40px from <colgroup> in RTL, got {}",
    a.width()
  );
  assert!(
    (b.width() - 40.0).abs() < 0.1,
    "expected second source column width ~40px from <colgroup> in RTL, got {}",
    b.width()
  );
  assert!(
    (c.width() - 20.0).abs() < 0.1,
    "expected third source column to take remaining width (~20px) in RTL, got {}",
    c.width()
  );

  let gap_ab = a.x() - (b.x() + b.width());
  assert!(gap_ab.abs() < 0.1, "expected cells A/B adjacent (gap={gap_ab})");
  let gap_bc = b.x() - (c.x() + c.width());
  assert!(gap_bc.abs() < 0.1, "expected cells B/C adjacent (gap={gap_bc})");
}

