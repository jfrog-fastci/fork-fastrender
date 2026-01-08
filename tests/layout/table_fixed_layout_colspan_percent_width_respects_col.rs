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
fn table_fixed_layout_colspan_percent_width_respects_col() {
  // The spanning cell requests 50% of the table width (200px). Since the first
  // column is fixed at 50px via <col>, the remaining spanned column should get
  // 150px so the total span width is 200px.
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 400px;
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
          <col style="width: 50px" />
          <tr>
            <td colspan="2" style="width: 50%">A</td>
            <td>B</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 800, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let table_width = table.bounds.width();
  assert!(
    (table_width - 400.0).abs() < 0.1,
    "expected table width ~400px, got {table_width}"
  );

  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);
  assert_eq!(cells.len(), 2, "expected two table cells");

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");
  let span = a.width();
  let last = b.width();
  assert!(
    (span - 200.0).abs() < 0.1,
    "expected colspan cell width ~200px, got {span}"
  );
  assert!(
    (last - 200.0).abs() < 0.1,
    "expected remaining column width ~200px, got {last}"
  );

  let gap = b.x() - (a.x() + a.width());
  assert!(
    gap.abs() < 0.1,
    "expected cells to be adjacent after resolving percentage width (gap={gap})"
  );
}

#[test]
fn table_fixed_layout_colspan_percent_width_respects_col_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 400px;
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
          <col style="width: 50px" />
          <tr>
            <td colspan="2" style="width: 50%">A</td>
            <td>B</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 800, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let table_width = table.bounds.width();
  assert!(
    (table_width - 400.0).abs() < 0.1,
    "expected table width ~400px, got {table_width}"
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
  assert!(
    (a.width() - 200.0).abs() < 0.1,
    "expected colspan cell width ~200px in RTL, got {}",
    a.width()
  );
  assert!(
    (b.width() - 200.0).abs() < 0.1,
    "expected remaining column width ~200px in RTL, got {}",
    b.width()
  );

  let gap = a.x() - (b.x() + b.width());
  assert!(
    gap.abs() < 0.1,
    "expected cells to be adjacent after resolving percentage width in RTL (gap={gap})"
  );
}
