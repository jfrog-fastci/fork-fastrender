use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn find_table<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.style.as_ref().map(|s| s.display), Some(Display::Table)) {
    return Some(node);
  }
  node.children.iter().find_map(find_table)
}

fn find_first_cell<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::TableCell)
  ) {
    return Some(node);
  }
  node.children.iter().find_map(find_first_cell)
}

#[test]
fn separate_border_spacing_uses_full_edges() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { border-collapse: separate; border-spacing: 10px 6px; padding: 0; border: 0; }
          td { width: 40px; height: 10px; padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table><tr><td></td></tr></table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  // Use a viewport width that matches the expected table width so that the table's
  // resolved width comes from the cell + edge border-spacing geometry.
  let tree = renderer.layout_document(&dom, 60, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment");
  let cell = find_first_cell(table).expect("table cell fragment");

  assert!(
    (cell.bounds.x() - 10.0).abs() < 0.1,
    "expected edge cell to start after full horizontal border-spacing (got x={})",
    cell.bounds.x()
  );
  assert!(
    (cell.bounds.y() - 6.0).abs() < 0.1,
    "expected edge cell to start after full vertical border-spacing (got y={})",
    cell.bounds.y()
  );

  assert!(
    (table.bounds.width() - 60.0).abs() < 0.1,
    "expected table width to include left+right border-spacing (got {})",
    table.bounds.width()
  );
  assert!(
    (cell.bounds.width() - 40.0).abs() < 0.1,
    "expected cell width to exclude edge border-spacing (got {})",
    cell.bounds.width()
  );
  assert!(
    (table.bounds.height() - 22.0).abs() < 0.1,
    "expected table height to include top+bottom border-spacing (got {})",
    table.bounds.height()
  );
}
