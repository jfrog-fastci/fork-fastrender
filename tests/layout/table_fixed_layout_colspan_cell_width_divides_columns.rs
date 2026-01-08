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
fn table_fixed_layout_colspan_cell_width_divides_columns() {
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
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr>
            <td colspan="2" style="width: 250px">A</td>
            <td>B</td>
          </tr>
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

  let spanning = cells.get(&'A').expect("spanning cell A").width();
  let last = cells.get(&'B').expect("remaining cell B").width();
  assert!(
    (spanning - 250.0).abs() < 0.1,
    "expected spanning cell to be ~250px wide, got {spanning}"
  );
  assert!(
    (last - 50.0).abs() < 0.1,
    "expected remaining column to get ~50px, got {last}"
  );
}

#[test]
fn table_fixed_layout_colspan_cell_width_divides_columns_rtl() {
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
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr>
            <td colspan="2" style="width: 250px">A</td>
            <td>B</td>
          </tr>
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

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");

  assert!(
    a.x() > b.x(),
    "expected RTL column order A (right) > B (left), got A.x={} B.x={}",
    a.x(),
    b.x()
  );
  assert!(
    (a.width() - 250.0).abs() < 0.1,
    "expected spanning cell to be ~250px wide in RTL, got {}",
    a.width()
  );
  assert!(
    (b.width() - 50.0).abs() < 0.1,
    "expected remaining column to get ~50px in RTL, got {}",
    b.width()
  );

  let gap = a.x() - (b.x() + b.width());
  assert!(
    gap.abs() < 0.1,
    "expected cells to be adjacent after width division in RTL (gap={gap})"
  );
}

#[test]
fn table_fixed_layout_colspan_cell_width_divides_columns_border_spacing_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 400px;
            border-collapse: separate;
            border-spacing: 10px 0;
            padding: 0;
            border: 0;
            direction: rtl;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr>
            <td colspan="2" style="width: 250px">A</td>
            <td>B</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 600, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let table_width = table.bounds.width();
  let table_left = table.bounds.x();
  assert!(
    (table_width - 400.0).abs() < 0.1,
    "expected table width ~400px, got {table_width}"
  );

  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");

  assert!(
    a.x() > b.x(),
    "expected RTL order A (right) > B (left), got A.x={} B.x={}",
    a.x(),
    b.x()
  );
  assert!(
    (b.x() - table_left - 10.0).abs() < 0.1,
    "expected left edge border-spacing applied in RTL (B.x={} table.x={})",
    b.x(),
    table_left
  );

  // For a cell spanning 2 columns, its width includes one internal border-spacing gap in the
  // separated border model. To compute per-column widths, fixed layout must subtract this
  // internal spacing before dividing.
  assert!(
    (a.width() - 250.0).abs() < 0.1,
    "expected spanning cell width to include internal border-spacing in RTL (A width {})",
    a.width()
  );
  assert!(
    (b.width() - 120.0).abs() < 0.1,
    "expected remaining column to take the leftover width in RTL (B width {})",
    b.width()
  );

  let gap = a.x() - (b.x() + b.width());
  assert!(
    (gap - 10.0).abs() < 0.1,
    "expected border-spacing gap between B and A in RTL (gap={gap})"
  );

  let a_right = a.x() + a.width();
  assert!(
    (a_right - table_left - 390.0).abs() < 0.1,
    "expected right edge border-spacing applied in RTL (A right={a_right} table.x={table_left})"
  );
}

#[test]
fn table_fixed_layout_colspan_cell_width_divides_columns_collapsed_border_model() {
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
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr>
            <td colspan="2" style="width: 250px">A</td>
            <td>B</td>
          </tr>
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

  let a = cells.get(&'A').expect("spanning cell A");
  let b = cells.get(&'B').expect("remaining cell B");
  assert!(
    (a.width() - 250.0).abs() < 0.1,
    "expected spanning cell to be ~250px wide, got {}",
    a.width()
  );
  assert!(
    (b.width() - 50.0).abs() < 0.1,
    "expected remaining column to get ~50px, got {}",
    b.width()
  );
  let gap = b.x() - (a.x() + a.width());
  assert!(
    gap.abs() < 0.1,
    "expected cells to be adjacent after width division (gap={gap})"
  );
}

#[test]
fn table_fixed_layout_colspan_cell_width_divides_columns_collapsed_border_model_rtl() {
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
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr>
            <td colspan="2" style="width: 250px">A</td>
            <td>B</td>
          </tr>
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

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");
  assert!(
    a.x() > b.x(),
    "expected RTL column order A (right) > B (left), got A.x={} B.x={}",
    a.x(),
    b.x()
  );
  assert!(
    (a.width() - 250.0).abs() < 0.1,
    "expected spanning cell to be ~250px wide in RTL, got {}",
    a.width()
  );
  assert!(
    (b.width() - 50.0).abs() < 0.1,
    "expected remaining column to get ~50px in RTL, got {}",
    b.width()
  );

  let gap = a.x() - (b.x() + b.width());
  assert!(
    gap.abs() < 0.1,
    "expected cells to be adjacent after width division in RTL (gap={gap})"
  );
}
