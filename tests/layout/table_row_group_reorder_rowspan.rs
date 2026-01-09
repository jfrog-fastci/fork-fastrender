use std::collections::HashMap;
use std::sync::Once;

use fastrender::api::FastRender;
use fastrender::geometry::Rect;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};

static SET_RAYON_THREADS: Once = Once::new();

fn ensure_rayon_threads() {
  SET_RAYON_THREADS.call_once(|| {
    if std::env::var("RAYON_NUM_THREADS").is_err() {
      std::env::set_var("RAYON_NUM_THREADS", "4");
    }
  });
}

#[derive(Debug, Clone)]
struct CellInfo {
  rect: Rect,
}

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

fn collect_cells(node: &FragmentNode, origin: (f32, f32), cells: &mut HashMap<char, CellInfo>) {
  let pos = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::TableCell)
  ) {
    let mut text = String::new();
    collect_text(node, &mut text);
    if let Some(label) = text.trim().chars().find(|c| c.is_ascii_alphabetic()) {
      let rect = Rect::from_xywh(pos.0, pos.1, node.bounds.width(), node.bounds.height());
      cells.insert(label, CellInfo { rect });
    }
  }
  for child in node.children.iter() {
    collect_cells(child, pos, cells);
  }
}

#[test]
fn table_footer_group_before_body_does_not_shift_body_cells() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { border-collapse: separate; border-spacing: 0; table-layout: fixed; }
          col { width: 20px; }
          td { padding: 0; margin: 0; border: 0; font-size: 0; line-height: 0; }
        </style>
      </head>
      <body>
        <table>
          <col />
          <col />
          <tfoot>
            <tr>
              <td rowspan="2" style="height: 10px;">A</td>
              <td style="height: 10px;">B</td>
            </tr>
          </tfoot>
          <tbody>
            <tr>
              <td style="height: 12px;">C</td>
              <td style="height: 12px;">D</td>
            </tr>
          </tbody>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  let a = cells.get(&'A').expect("footer cell A present");
  let c = cells.get(&'C').expect("body cell C present");

  assert!(
    (c.rect.x() - a.rect.x()).abs() < 0.1,
    "body cells should start at column 0 even if a tfoot with rowspan appears before tbody (expected C.x={}, got {})",
    a.rect.x(),
    c.rect.x()
  );
}

