use fastrender::api::FastRender;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn collect_table_cells<'a>(node: &'a FragmentNode, out: &mut Vec<&'a FragmentNode>) {
  if matches!(node.style.as_ref().map(|s| s.display), Some(Display::TableCell)) {
    out.push(node);
  }
  for child in node.children.iter() {
    collect_table_cells(child, out);
  }
}

#[test]
fn table_cell_height_behaves_like_min_height() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { border-collapse: separate; border-spacing: 0; }
          td { padding: 0; border: 0; vertical-align: middle; width: 50px; }

          td.short { height: 10px; }
          td.short > div { height: 30px; }

          td.tall > div { height: 40px; }
        </style>
      </head>
      <body>
        <table><tr><td class="short"><div></div></td><td class="tall"><div></div></td></tr></table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let mut cells = Vec::new();
  collect_table_cells(&tree.root, &mut cells);
  assert_eq!(cells.len(), 2, "expected exactly two table-cell fragments");

  cells.sort_by(|a, b| {
    a.bounds
      .x()
      .partial_cmp(&b.bounds.x())
      .unwrap_or(std::cmp::Ordering::Equal)
  });
  let short_cell = cells[0];

  let content = short_cell
    .children
    .iter()
    .max_by(|a, b| {
      a.bounds
        .height()
        .partial_cmp(&b.bounds.height())
        .unwrap_or(std::cmp::Ordering::Equal)
    })
    .expect("short cell should have a child fragment");

  let expected_y = ((short_cell.bounds.height() - content.bounds.height()) / 2.0).max(0.0);
  let actual_y = content.bounds.y();
  assert!(
    (actual_y - expected_y).abs() < 0.1,
    "expected table-cell `height` to act as min-height when computing vertical-align offsets \
     (expected child y={expected_y}, got {actual_y})"
  );
}
