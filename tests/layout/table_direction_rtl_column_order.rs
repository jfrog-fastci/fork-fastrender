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
  for child in &node.children {
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
    let label = text
      .trim()
      .chars()
      .find(|c| c.is_ascii_alphabetic())
      .expect("expected cell label");
    out.insert(
      label,
      Rect::from_xywh(pos.0, pos.1, node.bounds.width(), node.bounds.height()),
    );
  }
  for child in &node.children {
    collect_cells(child, pos, out);
  }
}

fn layout_table_cells(html: &str) -> HashMap<char, Rect> {
  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);
  cells
}

fn assert_rtl_column_order(cells: &HashMap<char, Rect>) {
  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");
  let c = cells.get(&'C').expect("cell C present");

  assert!(
    a.x() > b.x() && b.x() > c.x(),
    "expected RTL visual order A (right) > B > C (left), got A.x={:.2} B.x={:.2} C.x={:.2}",
    a.x(),
    b.x(),
    c.x()
  );

  assert!(
    (a.width() - 40.0).abs() < 0.1,
    "expected A width 40px, got {:.2}",
    a.width()
  );
  assert!(
    (b.width() - 60.0).abs() < 0.1,
    "expected B width 60px, got {:.2}",
    b.width()
  );
  assert!(
    (c.width() - 50.0).abs() < 0.1,
    "expected C width 50px, got {:.2}",
    c.width()
  );
}

fn assert_rtl_colspan_mapping(cells: &HashMap<char, Rect>) {
  let a = cells.get(&'A').expect("cell A present");
  let c = cells.get(&'C').expect("cell C present");

  assert!(
    a.x() > c.x(),
    "expected RTL colspan cell A to be to the right of C, got A.x={:.2} C.x={:.2}",
    a.x(),
    c.x()
  );
  assert!(
    (a.width() - 100.0).abs() < 0.1,
    "expected A width 100px (40px+60px), got {:.2}",
    a.width()
  );
  assert!(
    (c.width() - 50.0).abs() < 0.1,
    "expected C width 50px, got {:.2}",
    c.width()
  );
}

fn assert_rtl_rowspan_mapping(cells: &HashMap<char, Rect>) {
  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");
  let c = cells.get(&'C').expect("cell C present");
  let d = cells.get(&'D').expect("cell D present");
  let e = cells.get(&'E').expect("cell E present");

  assert!(
    a.x() > b.x() && b.x() > c.x(),
    "expected RTL visual order A (right) > B > C (left), got A.x={:.2} B.x={:.2} C.x={:.2}",
    a.x(),
    b.x(),
    c.x()
  );
  assert!(
    a.x() > d.x() && d.x() > e.x(),
    "expected RTL visual order A (right) > D > E (left), got A.x={:.2} D.x={:.2} E.x={:.2}",
    a.x(),
    d.x(),
    e.x()
  );
  assert!(
    (b.x() - d.x()).abs() < 0.1,
    "expected B and D to share the same column x, got B.x={:.2} D.x={:.2}",
    b.x(),
    d.x()
  );
  assert!(
    (c.x() - e.x()).abs() < 0.1,
    "expected C and E to share the same column x, got C.x={:.2} E.x={:.2}",
    c.x(),
    e.x()
  );

  assert!((a.width() - 40.0).abs() < 0.1, "expected A width 40px");
  assert!((b.width() - 60.0).abs() < 0.1, "expected B width 60px");
  assert!((c.width() - 50.0).abs() < 0.1, "expected C width 50px");
  assert!((d.width() - 60.0).abs() < 0.1, "expected D width 60px");
  assert!((e.width() - 50.0).abs() < 0.1, "expected E width 50px");
}

#[test]
fn table_direction_rtl_column_order_separate_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            width: 150px;
            table-layout: fixed;
            border-collapse: separate;
            border-spacing: 0;
            direction: rtl;
            padding: 0;
            border: 0;
          }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          col.col3 { width: 50px; }
          td { padding: 0; margin: 0; border: 0; font-size: 12px; line-height: 12px; }
        </style>
      </head>
      <body>
        <table>
          <col class="col1" />
          <col class="col2" />
          <col class="col3" />
          <tr><td>A</td><td>B</td><td>C</td></tr>
        </table>
      </body>
    </html>
  "#;

  let cells = layout_table_cells(html);
  assert_rtl_column_order(&cells);
}

#[test]
fn table_direction_rtl_column_order_inherits_direction_from_body() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; direction: rtl; }
          table {
            width: 150px;
            table-layout: fixed;
            border-collapse: separate;
            border-spacing: 0;
            padding: 0;
            border: 0;
          }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          col.col3 { width: 50px; }
          td { padding: 0; margin: 0; border: 0; font-size: 12px; line-height: 12px; }
        </style>
      </head>
      <body>
        <table>
          <col class="col1" />
          <col class="col2" />
          <col class="col3" />
          <tr><td>A</td><td>B</td><td>C</td></tr>
        </table>
      </body>
    </html>
  "#;

  let cells = layout_table_cells(html);
  assert_rtl_column_order(&cells);
}

#[test]
fn table_direction_rtl_column_order_with_caption_separate_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            width: 150px;
            table-layout: fixed;
            border-collapse: separate;
            border-spacing: 0;
            direction: rtl;
            padding: 0;
            border: 0;
          }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          col.col3 { width: 50px; }
          caption { caption-side: top; }
          td { padding: 0; margin: 0; border: 0; font-size: 12px; line-height: 12px; }
        </style>
      </head>
      <body>
        <table>
          <caption>caption</caption>
          <col class="col1" />
          <col class="col2" />
          <col class="col3" />
          <tr><td>A</td><td>B</td><td>C</td></tr>
        </table>
      </body>
    </html>
  "#;

  let cells = layout_table_cells(html);
  assert_rtl_column_order(&cells);
}

#[test]
fn table_direction_rtl_column_order_auto_layout_separate_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            width: 150px;
            table-layout: auto;
            border-collapse: separate;
            border-spacing: 0;
            direction: rtl;
            padding: 0;
            border: 0;
          }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          col.col3 { width: 50px; }
          td { padding: 0; margin: 0; border: 0; font-size: 12px; line-height: 12px; }
        </style>
      </head>
      <body>
        <table>
          <col class="col1" />
          <col class="col2" />
          <col class="col3" />
          <tr><td>A</td><td>B</td><td>C</td></tr>
        </table>
      </body>
    </html>
  "#;

  let cells = layout_table_cells(html);
  assert_rtl_column_order(&cells);
}

#[test]
fn table_direction_rtl_column_order_auto_layout_collapsed_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            width: 150px;
            table-layout: auto;
            border-collapse: collapse;
            border-spacing: 0;
            direction: rtl;
            padding: 0;
            border: 0;
          }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          col.col3 { width: 50px; }
          td { padding: 0; margin: 0; border: 0; font-size: 12px; line-height: 12px; }
        </style>
      </head>
      <body>
        <table>
          <col class="col1" />
          <col class="col2" />
          <col class="col3" />
          <tr><td>A</td><td>B</td><td>C</td></tr>
        </table>
      </body>
    </html>
  "#;

  let cells = layout_table_cells(html);
  assert_rtl_column_order(&cells);
}

#[test]
fn table_direction_rtl_column_order_collapsed_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            width: 150px;
            table-layout: fixed;
            border-collapse: collapse;
            border-spacing: 0;
            direction: rtl;
            padding: 0;
            border: 0;
          }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          col.col3 { width: 50px; }
          td { padding: 0; margin: 0; border: 0; font-size: 12px; line-height: 12px; }
        </style>
      </head>
      <body>
        <table>
          <col class="col1" />
          <col class="col2" />
          <col class="col3" />
          <tr><td>A</td><td>B</td><td>C</td></tr>
        </table>
      </body>
    </html>
  "#;

  let cells = layout_table_cells(html);
  assert_rtl_column_order(&cells);
}

#[test]
fn table_direction_rtl_colspan_separate_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            width: 150px;
            table-layout: fixed;
            border-collapse: separate;
            border-spacing: 0;
            direction: rtl;
            padding: 0;
            border: 0;
          }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          col.col3 { width: 50px; }
          td { padding: 0; margin: 0; border: 0; font-size: 12px; line-height: 12px; }
        </style>
      </head>
      <body>
        <table>
          <col class="col1" />
          <col class="col2" />
          <col class="col3" />
          <tr><td colspan="2">A</td><td>C</td></tr>
        </table>
      </body>
    </html>
  "#;

  let cells = layout_table_cells(html);
  assert_rtl_colspan_mapping(&cells);
}

#[test]
fn table_direction_rtl_colspan_collapsed_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            width: 150px;
            table-layout: fixed;
            border-collapse: collapse;
            border-spacing: 0;
            direction: rtl;
            padding: 0;
            border: 0;
          }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          col.col3 { width: 50px; }
          td { padding: 0; margin: 0; border: 0; font-size: 12px; line-height: 12px; }
        </style>
      </head>
      <body>
        <table>
          <col class="col1" />
          <col class="col2" />
          <col class="col3" />
          <tr><td colspan="2">A</td><td>C</td></tr>
        </table>
      </body>
    </html>
  "#;

  let cells = layout_table_cells(html);
  assert_rtl_colspan_mapping(&cells);
}

#[test]
fn table_direction_rtl_rowspan_separate_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            width: 150px;
            table-layout: fixed;
            border-collapse: separate;
            border-spacing: 0;
            direction: rtl;
            padding: 0;
            border: 0;
          }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          col.col3 { width: 50px; }
          td { padding: 0; margin: 0; border: 0; font-size: 12px; line-height: 12px; }
        </style>
      </head>
      <body>
        <table>
          <col class="col1" />
          <col class="col2" />
          <col class="col3" />
          <tr><td rowspan="2">A</td><td>B</td><td>C</td></tr>
          <tr><td>D</td><td>E</td></tr>
        </table>
      </body>
    </html>
  "#;

  let cells = layout_table_cells(html);
  assert_rtl_rowspan_mapping(&cells);
}

#[test]
fn table_direction_rtl_rowspan_collapsed_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            width: 150px;
            table-layout: fixed;
            border-collapse: collapse;
            border-spacing: 0;
            direction: rtl;
            padding: 0;
            border: 0;
          }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          col.col3 { width: 50px; }
          td { padding: 0; margin: 0; border: 0; font-size: 12px; line-height: 12px; }
        </style>
      </head>
      <body>
        <table>
          <col class="col1" />
          <col class="col2" />
          <col class="col3" />
          <tr><td rowspan="2">A</td><td>B</td><td>C</td></tr>
          <tr><td>D</td><td>E</td></tr>
        </table>
      </body>
    </html>
  "#;

  let cells = layout_table_cells(html);
  assert_rtl_rowspan_mapping(&cells);
}
