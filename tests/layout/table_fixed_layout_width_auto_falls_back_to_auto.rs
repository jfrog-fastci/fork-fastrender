use std::collections::HashMap;

use fastrender::api::FastRender;
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

fn collect_cell_widths(node: &FragmentNode, cells: &mut HashMap<String, f32>) {
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::TableCell)
  ) {
    let mut text = String::new();
    collect_text(node, &mut text);
    cells.insert(text.trim().to_string(), node.bounds.width());
  }
  for child in node.children.iter() {
    collect_cell_widths(child, cells);
  }
}

fn layout_cell_widths(html: &str) -> HashMap<String, f32> {
  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 800, 600).unwrap();

  let mut cells = HashMap::new();
  collect_cell_widths(&tree.root, &mut cells);
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

  let cells = layout_cell_widths(&html);
  let long_width = *cells.get(&long).expect("long word cell present");
  let b_width = *cells.get("B").expect("B cell present");

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

  let cells = layout_cell_widths(&html);
  let long_width = *cells.get(&long).expect("long word cell present");
  let b_width = *cells.get("B").expect("B cell present");

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

