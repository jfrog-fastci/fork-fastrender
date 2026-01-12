use crate::api::FastRender;
use crate::style::display::Display;
use crate::tree::fragment_tree::FragmentNode;

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
fn table_baseline_padding_does_not_inflate_row_height() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { border-collapse: separate; border-spacing: 0; table-layout: fixed; width: 100px; }
          td { padding: 0; border: 0; width: 50px; vertical-align: baseline; font-size: 0px; line-height: 10px; }
          td.tall { padding-top: 12px; }
        </style>
      </head>
      <body>
        <table>
          <tr><td class="tall">X</td><td>Y</td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let cell = find_first_cell(&tree.root).expect("table cell fragment present");
  let height = cell.bounds.height();
  assert!(
    (height - 22.0).abs() < 0.1,
    "expected row height ~22px, got {height}"
  );
}
