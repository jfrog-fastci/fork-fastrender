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
fn table_fixed_layout_col_width_overrides_first_row_cell_width() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 200px;
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
          <col style="width: 50px" />
          <col />
          <tr><td style="width: 50%">A</td><td>B</td></tr>
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
    (table_width - 200.0).abs() < 0.1,
    "expected table width ~200px, got {table_width}"
  );

  let mut cells = Vec::new();
  collect_cells(table, &mut cells);
  assert_eq!(cells.len(), 2, "expected two table cells");

  let first = cells[0].bounds.width();
  let second = cells[1].bounds.width();
  assert!(
    (first - 50.0).abs() < 0.1,
    "expected first column width ~50px from <col>, got {first}"
  );
  assert!(
    (second - 150.0).abs() < 0.1,
    "expected second column to take remaining width (~150px), got {second}"
  );
}

