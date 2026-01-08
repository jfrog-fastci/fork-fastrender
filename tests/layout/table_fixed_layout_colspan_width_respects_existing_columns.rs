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
fn table_fixed_layout_colspan_width_respects_existing_columns() {
  // Column 2 has an explicit width via <col>. A spanning first-row cell that
  // covers columns 2-3 should only allocate the remaining width to the
  // unassigned column, instead of forcing all remaining columns to take an
  // equal share and expanding the table.
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
          <col style="width: 100px" />
          <col style="width: 80px" />
          <tr>
            <td>A</td>
            <td colspan="2" style="width: 200px">B</td>
            <td>C</td>
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
  assert_eq!(cells.len(), 3, "expected three table cells");

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");
  let c = cells.get(&'C').expect("cell C present");

  assert!(
    a.x() < b.x(),
    "expected LTR order A (left) < B, got A.x={} B.x={}",
    a.x(),
    b.x()
  );
  assert!(
    b.x() < c.x(),
    "expected LTR order B (left) < C, got B.x={} C.x={}",
    b.x(),
    c.x()
  );

  assert!(
    (a.width() - 100.0).abs() < 0.1,
    "expected col1 ~100px, got {}",
    a.width()
  );
  assert!(
    (b.width() - 200.0).abs() < 0.1,
    "expected colspan cell ~200px, got {}",
    b.width()
  );
  assert!(
    (c.width() - 100.0).abs() < 0.1,
    "expected remaining column ~100px, got {}",
    c.width()
  );

  let gap_ab = b.x() - (a.x() + a.width());
  assert!(gap_ab.abs() < 0.1, "expected A/B to be adjacent (gap={gap_ab})");
  let gap_bc = c.x() - (b.x() + b.width());
  assert!(gap_bc.abs() < 0.1, "expected B/C to be adjacent (gap={gap_bc})");
}

#[test]
fn table_fixed_layout_colspan_width_respects_existing_columns_rtl() {
  // Same scenario as the LTR test, but ensure RTL column ordering doesn't break width distribution
  // when some columns have explicit widths.
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
          <col style="width: 100px" />
          <col style="width: 80px" />
          <tr>
            <td>A</td>
            <td colspan="2" style="width: 200px">B</td>
            <td>C</td>
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
  assert_eq!(cells.len(), 3, "expected three table cells");

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");
  let c = cells.get(&'C').expect("cell C present");

  assert!(
    a.x() > b.x(),
    "expected RTL order A (right) > B, got A.x={} B.x={}",
    a.x(),
    b.x()
  );
  assert!(
    b.x() > c.x(),
    "expected RTL order B (right) > C, got B.x={} C.x={}",
    b.x(),
    c.x()
  );

  assert!(
    (a.width() - 100.0).abs() < 0.1,
    "expected first source column to stay 100px in RTL (A width {})",
    a.width()
  );
  assert!(
    (b.width() - 200.0).abs() < 0.1,
    "expected colspan cell width ~200px in RTL (B width {})",
    b.width()
  );
  assert!(
    (c.width() - 100.0).abs() < 0.1,
    "expected remaining column to take 100px in RTL (C width {})",
    c.width()
  );

  let gap_ab = a.x() - (b.x() + b.width());
  assert!(gap_ab.abs() < 0.1, "expected A/B to be adjacent in RTL (gap={gap_ab})");
  let gap_bc = b.x() - (c.x() + c.width());
  assert!(gap_bc.abs() < 0.1, "expected B/C to be adjacent in RTL (gap={gap_bc})");
}

#[test]
fn table_fixed_layout_colspan_width_respects_existing_columns_collapsed_border_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 400px;
            border-collapse: collapse;
            border: none;
            padding: 0;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 100px" />
          <col style="width: 80px" />
          <tr>
            <td>A</td>
            <td colspan="2" style="width: 200px">B</td>
            <td>C</td>
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
  assert_eq!(cells.len(), 3, "expected three table cells");

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");
  let c = cells.get(&'C').expect("cell C present");

  assert!(
    (a.width() - 100.0).abs() < 0.1,
    "expected col1 ~100px, got {}",
    a.width()
  );
  assert!(
    (b.width() - 200.0).abs() < 0.1,
    "expected colspan cell ~200px, got {}",
    b.width()
  );
  assert!(
    (c.width() - 100.0).abs() < 0.1,
    "expected remaining column ~100px, got {}",
    c.width()
  );

  let gap_ab = b.x() - (a.x() + a.width());
  assert!(gap_ab.abs() < 0.1, "expected A/B to be adjacent (gap={gap_ab})");
  let gap_bc = c.x() - (b.x() + b.width());
  assert!(gap_bc.abs() < 0.1, "expected B/C to be adjacent (gap={gap_bc})");
}

#[test]
fn table_fixed_layout_colspan_width_respects_existing_columns_collapsed_border_model_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 400px;
            border-collapse: collapse;
            border: none;
            padding: 0;
            direction: rtl;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 100px" />
          <col style="width: 80px" />
          <tr>
            <td>A</td>
            <td colspan="2" style="width: 200px">B</td>
            <td>C</td>
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
  assert_eq!(cells.len(), 3, "expected three table cells");

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");
  let c = cells.get(&'C').expect("cell C present");

  assert!(
    a.x() > b.x(),
    "expected RTL order A (right) > B, got A.x={} B.x={}",
    a.x(),
    b.x()
  );
  assert!(
    b.x() > c.x(),
    "expected RTL order B (right) > C, got B.x={} C.x={}",
    b.x(),
    c.x()
  );

  assert!(
    (a.width() - 100.0).abs() < 0.1,
    "expected first source column to stay 100px in RTL (A width {})",
    a.width()
  );
  assert!(
    (b.width() - 200.0).abs() < 0.1,
    "expected colspan cell width ~200px in RTL (B width {})",
    b.width()
  );
  assert!(
    (c.width() - 100.0).abs() < 0.1,
    "expected remaining column to take 100px in RTL (C width {})",
    c.width()
  );

  let gap_ab = a.x() - (b.x() + b.width());
  assert!(gap_ab.abs() < 0.1, "expected A/B to be adjacent in RTL (gap={gap_ab})");
  let gap_bc = b.x() - (c.x() + c.width());
  assert!(gap_bc.abs() < 0.1, "expected B/C to be adjacent in RTL (gap={gap_bc})");
}
