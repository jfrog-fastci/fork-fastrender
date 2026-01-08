use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn find_table<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.style.as_ref().map(|s| s.display), Some(Display::Table)) {
    return Some(node);
  }
  node.children.iter().find_map(find_table)
}

#[test]
fn floated_table_layout_fixed_width_auto_uses_auto_layout() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            float: left;
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
        <table>
          <tr><td><div style="width:10px;height:10px"></div></td></tr>
          <tr><td><div style="width:200px;height:10px"></div></td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 800, 200).unwrap();

  let table = find_table(&tree.root).expect("expected table fragment");
  let width = table.bounds.width();
  assert!(
    (width - 200.0).abs() < 0.5,
    "expected floated table width ~200px, got {width}",
  );
}

#[test]
fn floated_table_layout_fixed_width_auto_uses_auto_layout_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            float: left;
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
      <body>
        <table>
          <tr><td><div style="width:10px;height:10px"></div></td></tr>
          <tr><td><div style="width:200px;height:10px"></div></td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 800, 200).unwrap();

  let table = find_table(&tree.root).expect("expected table fragment");
  let width = table.bounds.width();
  assert!(
    (width - 200.0).abs() < 0.5,
    "expected floated table width ~200px in RTL, got {width}",
  );
}

#[test]
fn floated_table_layout_fixed_width_auto_uses_auto_layout_collapsed_border_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            float: left;
            table-layout: fixed;
            border-collapse: collapse;
            border: none;
            padding: 0;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr><td><div style="width:10px;height:10px"></div></td></tr>
          <tr><td><div style="width:200px;height:10px"></div></td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 800, 200).unwrap();

  let table = find_table(&tree.root).expect("expected table fragment");
  let width = table.bounds.width();
  assert!(
    (width - 200.0).abs() < 0.5,
    "expected floated table width ~200px in collapsed model, got {width}",
  );
}

#[test]
fn floated_table_layout_fixed_width_auto_uses_auto_layout_collapsed_border_model_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            float: left;
            table-layout: fixed;
            border-collapse: collapse;
            border: none;
            padding: 0;
            direction: rtl;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr><td><div style="width:10px;height:10px"></div></td></tr>
          <tr><td><div style="width:200px;height:10px"></div></td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 800, 200).unwrap();

  let table = find_table(&tree.root).expect("expected table fragment");
  let width = table.bounds.width();
  assert!(
    (width - 200.0).abs() < 0.5,
    "expected floated table width ~200px in RTL collapsed model, got {width}",
  );
}
