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
fn table_percentage_padding_left_resolves_against_containing_block_width() {
  // Table padding percentages resolve against the *containing block* width (not the table width).
  let html = r#"
    <html>
      <head>
        <style>
          html, body { margin: 0; }
          table {
            width: 200px;
            border-collapse: separate;
            border-spacing: 0;
            border: 0;
            padding-left: 10%;
            padding-top: 0;
            padding-right: 0;
            padding-bottom: 0;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table><tr><td>A</td></tr></table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  assert!(
    (table.bounds.width() - 240.0).abs() < 0.1,
    "expected table border box width ~240px, got {}",
    table.bounds.width()
  );

  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);
  let a = cells.get(&'A').expect("cell A present");

  // 10% of the viewport (400px) = 40px.
  assert!(
    (a.x() - 40.0).abs() < 0.1,
    "expected cell content to start at x≈40px, got x={}",
    a.x()
  );
}

#[test]
fn table_percentage_padding_top_affects_percent_row_heights() {
  // Percent row heights should be based on the table's content box height after subtracting
  // padding (and borders/spacing when present).
  let html = r#"
    <html>
      <head>
        <style>
          html, body { margin: 0; }
          table {
            width: 200px;
            height: 200px;
            border-collapse: separate;
            border-spacing: 0;
            border: 0;
            padding-top: 10%;
            padding-left: 0;
            padding-right: 0;
            padding-bottom: 0;
          }
          td { padding: 0; border: 0; }
          tr.first { height: 50%; }
        </style>
      </head>
      <body>
        <table>
          <tr class="first"><td>A</td></tr>
          <tr><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 300).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  assert!(
    (table.bounds.height() - 240.0).abs() < 0.1,
    "expected table border box height ~240px, got {}",
    table.bounds.height()
  );

  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);
  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");

  // 10% padding-top = 10% of containing block width (400px) = 40px.
  // The table's `height: 200px` is a content-box height. Row percentage heights are based on the
  // table's content box height (200px), so first row target = 50% of 200 = 100px.
  assert!(
    (a.y() - 40.0).abs() < 0.1,
    "expected first row cell to start at y≈40px, got y={}",
    a.y()
  );
  assert!(
    (a.height() - 100.0).abs() < 0.1,
    "expected first row height ≈100px, got {}",
    a.height()
  );
  assert!(
    (b.y() - 140.0).abs() < 0.1,
    "expected second row cell to start at y≈140px, got y={}",
    b.y()
  );
  assert!(
    (b.height() - 100.0).abs() < 0.1,
    "expected second row height ≈100px, got {}",
    b.height()
  );
}
