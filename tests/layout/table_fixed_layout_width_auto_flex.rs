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
fn flex_item_table_layout_fixed_width_auto_uses_auto_layout() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .flex {
            display: flex;
            width: 400px;
          }
          table {
            flex: 1 1 0px;
            table-layout: fixed;
            border-collapse: separate;
            border-spacing: 0;
            padding: 0;
            border: 0;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <div class="flex">
          <table>
            <tr>
              <td><div style="width:10px;height:10px"></div></td>
              <td><div style="width:10px;height:10px"></div></td>
            </tr>
            <tr>
              <td><div style="width:300px;height:10px"></div></td>
              <td><div style="width:10px;height:10px"></div></td>
            </tr>
          </table>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 800, 200).unwrap();

  let table = find_table(&tree.root).expect("expected table fragment");
  let mut cells = Vec::new();
  collect_cells(table, &mut cells);
  assert_eq!(cells.len(), 4, "expected 4 table cells, got {}", cells.len());

  let max_cell_width = cells
    .iter()
    .map(|cell| cell.bounds.width())
    .fold(0.0f32, f32::max);
  assert!(
    max_cell_width > 250.0,
    "expected wide second-row content to influence column width in flex context (max cell width {max_cell_width:.2})"
  );
}
