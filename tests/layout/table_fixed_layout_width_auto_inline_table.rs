use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn find_inline_table<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(
    node.style.as_ref().map(|style| style.display),
    Some(Display::InlineTable)
  ) {
    return Some(node);
  }
  node.children.iter().find_map(find_inline_table)
}

#[test]
fn inline_table_fixed_layout_width_auto_falls_back_to_auto_layout() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          div { width: 100px; }
          table {
            display: inline-table;
            table-layout: fixed;
            width: auto;
            border-collapse: separate;
            border-spacing: 0;
          }
          td {
            white-space: nowrap;
            font: 10px/10px monospace;
            padding: 0;
          }
        </style>
      </head>
      <body>
        <div>
          <table>
            <tr><td>short</td></tr>
            <tr><td>xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx</td></tr>
          </table>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_inline_table(&tree.root).expect("inline-table fragment present");
  let width = table.bounds.width();
  assert!(
    width > 100.0 + 0.5,
    "expected inline-table shrink-to-fit width to exceed 100px when non-first-row content is wider (got {width:.2})"
  );
}

