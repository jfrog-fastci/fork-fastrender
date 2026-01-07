use fastrender::api::FastRender;
use fastrender::style::color::Rgba;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn find_table<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(node.style.as_ref().map(|s| s.display), Some(Display::Table)) {
    return Some(node);
  }
  node.children.iter().find_map(find_table)
}

fn find_colgroup_fragment(node: &FragmentNode, origin: (f32, f32), bg: Rgba) -> Option<(f32, f32)> {
  if let Some(style) = node.style.as_ref() {
    if style.display == Display::TableColumnGroup && style.background_color == bg {
      return Some((origin.0, node.bounds.width()));
    }
  }
  for child in node.children.iter() {
    let child_origin = (origin.0 + child.bounds.x(), origin.1 + child.bounds.y());
    if let Some(found) = find_colgroup_fragment(child, child_origin, bg) {
      return Some(found);
    }
  }
  None
}

#[test]
fn table_direction_rtl_colgroup_background_spans_correct_columns() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            direction: rtl;
            border-collapse: separate;
            border-spacing: 0 0;
            table-layout: fixed;
            width: 120px;
            border: 0;
            padding: 0;
          }
          td { padding: 0; margin: 0; border: 0; height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <colgroup span="2" style="background: rgb(10, 20, 30)"></colgroup>
          <tr>
            <td style="width: 30px"></td>
            <td style="width: 40px"></td>
            <td style="width: 50px"></td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 400, 200).unwrap();

  let expected_bg = Rgba::rgb(10, 20, 30);
  let table = find_table(&tree.root).expect("table fragment present");
  let (x, width) =
    find_colgroup_fragment(table, (0.0, 0.0), expected_bg).expect("colgroup fragment");

  assert!(
    (x - 50.0).abs() < 0.5,
    "expected colgroup fragment to start around x=50px, got {x}"
  );
  assert!(
    (width - 70.0).abs() < 0.5,
    "expected colgroup fragment to span two columns (~70px total), got {width}"
  );
}

#[test]
fn table_direction_rtl_colgroup_with_col_children_background_spans_correct_columns() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            direction: rtl;
            border-collapse: separate;
            border-spacing: 0 0;
            table-layout: fixed;
            width: 120px;
            border: 0;
            padding: 0;
          }
          td { padding: 0; margin: 0; border: 0; height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <colgroup style="background: rgb(10, 20, 30)">
            <col span="2">
          </colgroup>
          <tr>
            <td style="width: 30px"></td>
            <td style="width: 40px"></td>
            <td style="width: 50px"></td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 400, 200).unwrap();

  let expected_bg = Rgba::rgb(10, 20, 30);
  let table = find_table(&tree.root).expect("table fragment present");
  let (x, width) =
    find_colgroup_fragment(table, (0.0, 0.0), expected_bg).expect("colgroup fragment");

  assert!(
    (x - 50.0).abs() < 0.5,
    "expected colgroup fragment to start around x=50px, got {x}"
  );
  assert!(
    (width - 70.0).abs() < 0.5,
    "expected colgroup fragment to span two columns (~70px total), got {width}"
  );
}

#[test]
fn table_direction_rtl_colgroup_background_spans_correct_columns_in_collapsed_border_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            direction: rtl;
            border-collapse: collapse;
            table-layout: fixed;
            width: 120px;
            border: none;
            padding: 0;
          }
          td { padding: 0; margin: 0; border: 0; height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <colgroup span="2" style="background: rgb(10, 20, 30)"></colgroup>
          <tr>
            <td style="width: 30px"></td>
            <td style="width: 40px"></td>
            <td style="width: 50px"></td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 400, 200).unwrap();

  let expected_bg = Rgba::rgb(10, 20, 30);
  let table = find_table(&tree.root).expect("table fragment present");
  let (x, width) =
    find_colgroup_fragment(table, (0.0, 0.0), expected_bg).expect("colgroup fragment");

  assert!(
    (x - 50.0).abs() < 0.5,
    "expected colgroup fragment to start around x=50px, got {x}"
  );
  assert!(
    (width - 70.0).abs() < 0.5,
    "expected colgroup fragment to span two columns (~70px total), got {width}"
  );
}
