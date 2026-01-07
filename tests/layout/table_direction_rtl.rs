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

fn collect_cells(node: &FragmentNode, origin: (f32, f32), cells: &mut HashMap<char, Rect>) {
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
      .unwrap();
    let rect = Rect::from_xywh(pos.0, pos.1, node.bounds.width(), node.bounds.height());
    cells.insert(label, rect);
  }
  for child in node.children.iter() {
    collect_cells(child, pos, cells);
  }
}

#[test]
fn table_direction_rtl_places_first_column_on_the_right() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { direction: rtl; table-layout: fixed; border-collapse: separate; border-spacing: 0; }
          col.col1 { width: 40px; }
          col.col2 { width: 60px; }
          td { padding: 0; margin: 0; border: 0; font-size: 12px; line-height: 12px; }
        </style>
      </head>
      <body>
        <table>
          <col class="col1" />
          <col class="col2" />
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  let a = cells.get(&'A').expect("cell A");
  let b = cells.get(&'B').expect("cell B");

  assert!(
    a.x() > b.x(),
    "expected RTL column order (A right of B), got A.x={} B.x={}",
    a.x(),
    b.x()
  );
  assert!(
    (a.width() - 40.0).abs() < 0.1,
    "expected A width ~40px from first <col> (got {})",
    a.width()
  );
  assert!(
    (b.width() - 60.0).abs() < 0.1,
    "expected B width ~60px from second <col> (got {})",
    b.width()
  );
}

