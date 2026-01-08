use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn find_display<'a>(node: &'a FragmentNode, display: Display) -> Option<&'a FragmentNode> {
  if node.style.as_ref().map(|s| s.display) == Some(display) {
    return Some(node);
  }
  node
    .children
    .iter()
    .find_map(|child| find_display(child, display))
}

fn find_inline_table<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  find_display(node, Display::InlineTable).or_else(|| find_display(node, Display::Table))
}

#[test]
fn table_layout_fixed_with_width_auto_inline_table_uses_auto_layout() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            display: inline-table;
            table-layout: fixed;
            border-collapse: separate;
            border-spacing: 0;
            padding: 0;
            border: 0;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body><table><tr><td><div style="width:10px;height:10px"></div></td></tr><tr><td><div style="width:200px;height:10px"></div></td></tr></table></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 800, 200).unwrap();

  let table = find_inline_table(&tree.root).expect("expected inline-table fragment");
  let width = table.bounds.width();
  assert!(
    (width - 200.0).abs() < 0.5,
    "expected inline-table width ~200px, got {width}",
  );
}

#[test]
fn table_layout_fixed_with_width_auto_inline_table_uses_auto_layout_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            display: inline-table;
            table-layout: fixed;
            border-collapse: separate;
            border-spacing: 0;
            padding: 0;
            border: 0;
            direction: rtl;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body><table><tr><td><div style="width:10px;height:10px"></div></td></tr><tr><td><div style="width:200px;height:10px"></div></td></tr></table></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 800, 200).unwrap();

  let table = find_inline_table(&tree.root).expect("expected inline-table fragment");
  let width = table.bounds.width();
  assert!(
    (width - 200.0).abs() < 0.5,
    "expected inline-table width ~200px in RTL, got {width}",
  );
}
