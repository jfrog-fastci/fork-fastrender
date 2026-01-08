use std::collections::HashMap;

use fastrender::api::FastRender;
use fastrender::geometry::Rect;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};

fn collect_text(node: &FragmentNode, out: &mut String) {
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push_str(text);
  }
  for child in node.children.iter() {
    collect_text(child, out);
  }
}

fn collect_cells(node: &FragmentNode, origin: (f32, f32), out: &mut HashMap<String, Rect>) {
  let pos = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::TableCell)
  ) {
    let mut text = String::new();
    collect_text(node, &mut text);
    out.insert(
      text.trim().to_string(),
      Rect::from_xywh(pos.0, pos.1, node.bounds.width(), node.bounds.height()),
    );
  }
  for child in node.children.iter() {
    collect_cells(child, pos, out);
  }
}

fn layout_cells(html: &str) -> HashMap<String, Rect> {
  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 800, 600).unwrap();

  let mut cells = HashMap::new();
  collect_cells(&tree.root, (0.0, 0.0), &mut cells);
  cells
}

#[test]
fn inline_table_fixed_layout_width_auto_uses_auto_algorithm() {
  let long = "W".repeat(40);
  let html = format!(
    r#"
      <html>
        <head>
          <style>
            body {{ margin: 0; }}
            table {{ display: inline-table; table-layout: fixed; border-collapse: separate; border-spacing: 0; }}
            td {{ padding: 0; border: 0; white-space: nowrap; }}
          </style>
        </head>
        <body>
          <table>
            <tr><td>A</td><td>B</td></tr>
            <tr><td>{}</td><td>C</td></tr>
          </table>
        </body>
      </html>
    "#,
    long
  );

  let cells = layout_cells(&html);
  let a = cells.get("A").expect("A cell present");
  let b = cells.get("B").expect("B cell present");
  let long_width = cells.get(&long).expect("long word cell present").width();
  let b_width = b.width();

  assert!(
    a.x() < b.x(),
    "expected LTR column order A (left) < B, got A.x={:.2} B.x={:.2}",
    a.x(),
    b.x()
  );
  let gap = b.x() - (a.x() + a.width());
  assert!(gap.abs() < 0.1, "expected cells to be adjacent (gap={gap})");

  // CSS2.1 §17.5.2.1: `width:auto` disables the fixed table layout algorithm (even
  // when `table-layout: fixed` is set). The long word in the *second* row must
  // therefore influence the column width.
  assert!(
    long_width > 200.0,
    "expected long-word column to contribute to inline-table shrink-to-fit width (got {long_width:.2})"
  );
  assert!(
    long_width > b_width * 2.0,
    "expected first column to be substantially wider than the second (got long={long_width:.2}, B={b_width:.2})"
  );
}

#[test]
fn inline_table_fixed_layout_with_specified_width_stays_fixed() {
  let long = "W".repeat(40);
  let html = format!(
    r#"
      <html>
        <head>
          <style>
            body {{ margin: 0; }}
            table {{ display: inline-table; table-layout: fixed; width: 200px; border-collapse: separate; border-spacing: 0; }}
            td {{ padding: 0; border: 0; white-space: nowrap; }}
          </style>
        </head>
        <body>
          <table>
            <tr><td>A</td><td>B</td></tr>
            <tr><td>{}</td><td>C</td></tr>
          </table>
        </body>
      </html>
    "#,
    long
  );

  let cells = layout_cells(&html);
  let a = cells.get("A").expect("A cell present");
  let b = cells.get("B").expect("B cell present");
  let long_width = cells.get(&long).expect("long word cell present").width();
  let b_width = b.width();

  assert!(
    a.x() < b.x(),
    "expected LTR column order A (left) < B, got A.x={:.2} B.x={:.2}",
    a.x(),
    b.x()
  );
  let gap = b.x() - (a.x() + a.width());
  assert!(gap.abs() < 0.1, "expected cells to be adjacent (gap={gap})");

  // With a definite width, fixed layout remains active and the second row's
  // content should not redistribute column widths.
  assert!(
    (long_width - b_width).abs() < 1.0,
    "expected fixed layout to keep columns equal-width (got long={long_width:.2}, B={b_width:.2})"
  );
  assert!(
    long_width < 120.0,
    "expected fixed layout to keep the first column near 100px (got {long_width:.2})"
  );
}

#[test]
fn inline_table_fixed_layout_width_auto_uses_auto_algorithm_rtl() {
  let long = "W".repeat(40);
  let html = format!(
    r#"
      <html>
        <head>
          <style>
            body {{ margin: 0; }}
            table {{ display: inline-table; table-layout: fixed; border-collapse: separate; border-spacing: 0; direction: rtl; }}
            td {{ padding: 0; border: 0; white-space: nowrap; }}
          </style>
        </head>
        <body>
          <table>
            <tr><td>A</td><td>B</td></tr>
            <tr><td>{}</td><td>C</td></tr>
          </table>
        </body>
      </html>
    "#,
    long
  );

  let cells = layout_cells(&html);
  let a = cells.get("A").expect("A cell present");
  let b = cells.get("B").expect("B cell present");
  let long_width = cells.get(&long).expect("long word cell present").width();
  let b_width = b.width();

  assert!(
    a.x() > b.x(),
    "expected RTL column order A (right) > B, got A.x={:.2} B.x={:.2}",
    a.x(),
    b.x()
  );
  let gap = a.x() - (b.x() + b.width());
  assert!(gap.abs() < 0.1, "expected cells to be adjacent in RTL (gap={gap})");

  assert!(
    long_width > 200.0,
    "expected long-word column to contribute to inline-table shrink-to-fit width in RTL (got {long_width:.2})"
  );
  assert!(
    long_width > b_width * 2.0,
    "expected first column to be substantially wider than the second in RTL (got long={long_width:.2}, B={b_width:.2})"
  );
}

#[test]
fn inline_table_fixed_layout_with_specified_width_stays_fixed_rtl() {
  let long = "W".repeat(40);
  let html = format!(
    r#"
      <html>
        <head>
          <style>
            body {{ margin: 0; }}
            table {{ display: inline-table; table-layout: fixed; width: 200px; border-collapse: separate; border-spacing: 0; direction: rtl; }}
            td {{ padding: 0; border: 0; white-space: nowrap; }}
          </style>
        </head>
        <body>
          <table>
            <tr><td>A</td><td>B</td></tr>
            <tr><td>{}</td><td>C</td></tr>
          </table>
        </body>
      </html>
    "#,
    long
  );

  let cells = layout_cells(&html);
  let a = cells.get("A").expect("A cell present");
  let b = cells.get("B").expect("B cell present");
  let long_width = cells.get(&long).expect("long word cell present").width();
  let b_width = b.width();

  assert!(
    a.x() > b.x(),
    "expected RTL column order A (right) > B, got A.x={:.2} B.x={:.2}",
    a.x(),
    b.x()
  );
  let gap = a.x() - (b.x() + b.width());
  assert!(gap.abs() < 0.1, "expected cells to be adjacent in RTL (gap={gap})");

  assert!(
    (long_width - b_width).abs() < 1.0,
    "expected fixed layout to keep columns equal-width in RTL (got long={long_width:.2}, B={b_width:.2})"
  );
  assert!(
    long_width < 120.0,
    "expected fixed layout to keep the first column near 100px in RTL (got {long_width:.2})"
  );
}
