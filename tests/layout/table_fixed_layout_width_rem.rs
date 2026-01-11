use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};

fn collect_text(node: &FragmentNode, out: &mut String) {
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push_str(text);
  }
  for child in node.children.iter() {
    collect_text(child, out);
  }
}

fn find_table_cell_containing<'a>(
  node: &'a FragmentNode,
  needle: &str,
) -> Option<&'a FragmentNode> {
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::TableCell)
  ) {
    let mut text = String::new();
    collect_text(node, &mut text);
    if text.contains(needle) {
      return Some(node);
    }
  }
  node
    .children
    .iter()
    .find_map(|child| find_table_cell_containing(child, needle))
}

#[test]
fn table_fixed_layout_column_width_rem_resolves() {
  // Regression test: fixed table layout must resolve font-relative `<col>` widths (e.g. `rem`) when
  // distributing column widths. Previously these were treated as raw numeric values via
  // `Length::to_px()`, shrinking columns by ~16x.
  let html = r#"
    <html>
      <head>
        <style>
          html { font-size: 16px; }
          body { margin: 0; }
          table { table-layout: fixed; width: 200px; border-collapse: collapse; }
          col:first-child { width: 10rem; }
          td { padding: 0; border: none; }
        </style>
      </head>
      <body>
        <table><colgroup><col><col></colgroup><tr><td>col-rem</td><td>other</td></tr></table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 100).unwrap();

  let cell =
    find_table_cell_containing(&tree.root, "col-rem").expect("expected to find table cell");
  assert!(
    (cell.bounds.width() - 160.0).abs() < 0.5,
    "expected first column to be 10rem (160px) wide; got {:.2}px",
    cell.bounds.width()
  );
}

#[test]
fn table_fixed_layout_cell_width_rem_resolves() {
  // Regression test: first-row cell `width` hints in fixed layout must also resolve font-relative
  // lengths.
  let html = r#"
    <html>
      <head>
        <style>
          html { font-size: 16px; }
          body { margin: 0; }
          table { table-layout: fixed; width: 200px; border-collapse: collapse; }
          td { padding: 0; border: none; }
          td:first-child { width: 10rem; }
        </style>
      </head>
      <body>
        <table><tr><td>cell-rem</td><td>other</td></tr></table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 100).unwrap();

  let cell =
    find_table_cell_containing(&tree.root, "cell-rem").expect("expected to find table cell");
  assert!(
    (cell.bounds.width() - 160.0).abs() < 0.5,
    "expected first column to be 10rem (160px) wide; got {:.2}px",
    cell.bounds.width()
  );
}
