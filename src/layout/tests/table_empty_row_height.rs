use crate::api::FastRender;
use crate::geometry::Rect;
use crate::style::display::Display;
use crate::tree::fragment_tree::FragmentNode;

fn find_table<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.style.as_ref().map(|s| s.display), Some(Display::Table)) {
    return Some(node);
  }
  node.children.iter().find_map(find_table)
}

fn collect_table_cells(node: &FragmentNode, origin: (f32, f32), out: &mut Vec<Rect>) {
  let pos = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::TableCell)
  ) {
    out.push(Rect::from_xywh(
      pos.0,
      pos.1,
      node.bounds.width(),
      node.bounds.height(),
    ));
  }
  for child in node.children.iter() {
    collect_table_cells(child, pos, out);
  }
}

#[test]
fn empty_tr_height_contributes_to_table_layout() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { border-spacing: 0; border-collapse: separate; }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table><tr><td><div style="height:20px;width:20px"></div></td></tr><tr style="height:10px"></tr><tr><td><div style="height:20px;width:20px"></div></td></tr></table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = Vec::new();
  // Collect coordinates relative to the table's border box, not the viewport.
  for child in table.children.iter() {
    collect_table_cells(child, (0.0, 0.0), &mut cells);
  }

  assert_eq!(
    cells.len(),
    2,
    "expected exactly two table-cell fragments (only the rows with <td> content)"
  );

  cells.sort_by(|a, b| {
    a.y()
      .partial_cmp(&b.y())
      .unwrap_or(std::cmp::Ordering::Equal)
  });

  let second_cell_y = cells[1].y();
  assert!(
    (second_cell_y - 30.0).abs() < 0.1,
    "expected second row's cell to be pushed down by the empty <tr>'s height (expected y≈30, got {second_cell_y})"
  );

  assert!(
    (table.bounds.height() - 50.0).abs() < 0.1,
    "expected table height to include the empty row's height (expected ≈50, got {})",
    table.bounds.height()
  );
}

