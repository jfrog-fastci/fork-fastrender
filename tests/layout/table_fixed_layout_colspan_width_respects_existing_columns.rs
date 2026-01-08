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
fn table_fixed_layout_colspan_width_respects_existing_columns() {
  // Column 2 has an explicit width via <col>. A spanning first-row cell that
  // covers columns 2-3 should only allocate the remaining width to the
  // unassigned column, instead of forcing all remaining columns to take an
  // equal share and expanding the table.
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            table-layout: fixed;
            width: 400px;
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
          <col style="width: 100px" />
          <col style="width: 80px" />
          <tr>
            <td>A</td>
            <td colspan="2" style="width: 200px">B</td>
            <td></td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 800, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let table_width = table.bounds.width();
  assert!(
    (table_width - 400.0).abs() < 0.1,
    "expected table width ~400px, got {table_width}"
  );

  let mut cells = Vec::new();
  collect_cells(table, &mut cells);
  assert_eq!(cells.len(), 3, "expected three table cells");

  let a = cells[0].bounds.width();
  let b = cells[1].bounds.width();
  let c = cells[2].bounds.width();

  assert!((a - 100.0).abs() < 0.1, "expected col1 ~100px, got {a}");
  assert!((b - 200.0).abs() < 0.1, "expected colspan cell ~200px, got {b}");
  assert!(
    (c - 100.0).abs() < 0.1,
    "expected remaining column ~100px, got {c}"
  );
}

