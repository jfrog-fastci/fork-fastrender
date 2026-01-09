use std::collections::HashMap;

use fastrender::api::FastRender;
use fastrender::geometry::Rect;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};

fn find_table<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::Table | Display::InlineTable)
  ) {
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
fn table_fixed_layout_splits_remaining_width_across_columns_missing_from_first_row() {
  // Fixed layout uses only the first row to determine widths, but the table structure still
  // includes columns introduced by later rows. Columns that have no `<col>`/first-row width hint
  // should split remaining width evenly.
  let html = r#"
    <html>
      <head>
        <style>
          html, body { margin: 0; }
          table {
            table-layout: fixed;
            width: 300px;
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
          <tr><td style="width: 50px">A</td><td>B</td></tr>
          <tr><td>C</td><td>D</td><td>E</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  assert!(
    (table.bounds.width() - 300.0).abs() < 0.1,
    "expected table width ~300px, got {}",
    table.bounds.width()
  );

  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);
  assert_eq!(cells.len(), 5, "expected five table cells");

  let c = cells.get(&'C').expect("cell C present");
  let d = cells.get(&'D').expect("cell D present");
  let e = cells.get(&'E').expect("cell E present");

  assert!(
    (c.width() - 50.0).abs() < 0.1,
    "expected first column width ~50px, got {}",
    c.width()
  );
  assert!(
    (d.width() - 125.0).abs() < 0.1,
    "expected second column width ~125px, got {}",
    d.width()
  );
  assert!(
    (e.width() - 125.0).abs() < 0.1,
    "expected third column width ~125px, got {}",
    e.width()
  );

  let gap_cd = d.x() - (c.x() + c.width());
  let gap_de = e.x() - (d.x() + d.width());
  assert!(
    gap_cd.abs() < 0.1,
    "expected C/D cells adjacent (gap={gap_cd})"
  );
  assert!(
    gap_de.abs() < 0.1,
    "expected D/E cells adjacent (gap={gap_de})"
  );
}

#[test]
fn table_fixed_layout_overconstrained_columns_expand_table() {
  // CSS 2.1 §17.5.2.1: the used table width is the max of the specified width and the sum of
  // column widths (plus spacing/edges). When columns over-constrain, the table should expand.
  let html = r#"
    <html>
      <head>
        <style>
          html, body { margin: 0; }
          table {
            table-layout: fixed;
            width: 100px;
            border-collapse: separate;
            border-spacing: 0;
            padding: 0;
            border: 0;
          }
          col.a { width: 80px; }
          col.b { width: 80px; }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <colgroup>
            <col class="a" />
            <col class="b" />
          </colgroup>
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  assert!(
    (table.bounds.width() - 160.0).abs() < 0.1,
    "expected table to expand to ~160px, got {}",
    table.bounds.width()
  );

  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);
  assert_eq!(cells.len(), 2, "expected two table cells");

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");

  assert!(
    (a.width() - 80.0).abs() < 0.1,
    "expected first column width ~80px, got {}",
    a.width()
  );
  assert!(
    (b.width() - 80.0).abs() < 0.1,
    "expected second column width ~80px, got {}",
    b.width()
  );
}
