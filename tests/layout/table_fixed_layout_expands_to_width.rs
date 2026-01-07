use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn find_table<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.style.as_ref().map(|s| s.display), Some(Display::Table)) {
    return Some(node);
  }
  node.children.iter().find_map(find_table)
}

fn collect_cells<'a>(node: &'a FragmentNode, out: &mut Vec<&'a FragmentNode>) {
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::TableCell)
  ) {
    out.push(node);
  }
  for child in node.children.iter() {
    collect_cells(child, out);
  }
}

#[test]
fn table_fixed_layout_expands_to_specified_width() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0 }
          table {
            table-layout: fixed;
            width: 300px;
            border-collapse: separate;
            border-spacing: 0px;
            padding: 0;
            border: 0;
          }
          col { width: 50px; }
        </style>
      </head>
      <body>
        <table>
          <col />
          <col />
          <tr><td>A</td><td>B</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("expected table fragment");
  let table_width = table.bounds.width();
  assert!(
    (table_width - 300.0).abs() < 0.1,
    "expected table width ~300px, got {table_width}"
  );

  let mut cells = Vec::new();
  collect_cells(table, &mut cells);
  assert_eq!(cells.len(), 2, "expected two table cells");

  let cells_width: f32 = cells.iter().map(|cell| cell.bounds.width()).sum();
  assert!(
    (cells_width - 300.0).abs() < 0.1,
    "expected cell widths to sum to ~300px, got {cells_width}"
  );

  for (idx, cell) in cells.iter().enumerate() {
    let width = cell.bounds.width();
    assert!(
      (width - 150.0).abs() < 0.1,
      "expected cell {idx} width ~150px, got {width}"
    );
  }
}
