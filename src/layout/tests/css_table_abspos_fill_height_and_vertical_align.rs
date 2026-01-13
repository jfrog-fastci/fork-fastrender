use crate::api::FastRender;
use crate::geometry::Rect;
use crate::style::display::Display;
use crate::tree::fragment_tree::FragmentNode;

fn find_table<'a>(node: &'a FragmentNode, origin: (f32, f32)) -> Option<(&'a FragmentNode, Rect)> {
  let rect = Rect::from_xywh(origin.0, origin.1, node.bounds.width(), node.bounds.height());
  if matches!(node.style.as_ref().map(|s| s.display), Some(Display::Table)) {
    return Some((node, rect));
  }
  for child in node.children.iter() {
    let child_origin = (origin.0 + child.bounds.x(), origin.1 + child.bounds.y());
    if let Some(found) = find_table(child, child_origin) {
      return Some(found);
    }
  }
  None
}

fn collect_by_display<'a>(
  node: &'a FragmentNode,
  origin: (f32, f32),
  display: Display,
  out: &mut Vec<(&'a FragmentNode, Rect)>,
) {
  if matches!(node.style.as_ref().map(|s| s.display), Some(d) if d == display) {
    out.push((
      node,
      Rect::from_xywh(origin.0, origin.1, node.bounds.width(), node.bounds.height()),
    ));
  }
  for child in node.children.iter() {
    let child_origin = (origin.0 + child.bounds.x(), origin.1 + child.bounds.y());
    collect_by_display(child, child_origin, display, out);
  }
}

#[test]
fn css_table_abspos_fill_height_and_vertical_align() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .cb { position: relative; width: 300px; height: 200px; }
          .table { position: absolute; inset: 0; display: table; border-spacing: 0; padding: 0; border: 0; }
          .row { display: table-row; }
          .cell { display: table-cell; padding: 0; border: 0; }
        </style>
      </head>
      <body><div class=cb><div class=table><div class=row style="height:20px"><div class=cell><div style="height:20px"></div></div></div><div class=row><div class=cell style="vertical-align:bottom"><div class=inner style="height:10px"></div></div></div></div></div></body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 800, 600).unwrap();

  let (table, table_rect) = find_table(&tree.root, (0.0, 0.0)).expect("table fragment present");
  assert!(
    (table_rect.height() - 200.0).abs() < 0.5,
    "expected absolutely positioned CSS table to use definite height 200px, got {:.2}",
    table_rect.height()
  );

  let mut cells = Vec::new();
  collect_by_display(table, (table_rect.x(), table_rect.y()), Display::TableCell, &mut cells);
  assert_eq!(cells.len(), 2, "expected two table-cell fragments");

  let (bottom_cell, bottom_cell_rect) = cells
    .iter()
    .copied()
    .max_by(|a, b| a.1.y().total_cmp(&b.1.y()))
    .expect("bottom cell");

  let cell_bottom = bottom_cell_rect.y() + bottom_cell_rect.height();
  let table_bottom = table_rect.y() + table_rect.height();
  assert!(
    (cell_bottom - table_bottom).abs() < 0.5,
    "expected bottom row to reach table bottom (cell_bottom={cell_bottom:.2} vs table_bottom={table_bottom:.2})"
  );
  assert!(
    bottom_cell_rect.height() > 50.0,
    "expected bottom row to receive extra table height (got cell height {:.2})",
    bottom_cell_rect.height()
  );

  // Find the inner block fragment (the `<div class=inner style="height:10px">`) within the bottom
  // cell, measuring coordinates relative to the cell itself.
  let mut blocks = Vec::new();
  for child in bottom_cell.children.iter() {
    let child_origin = (child.bounds.x(), child.bounds.y());
    collect_by_display(child, child_origin, Display::Block, &mut blocks);
  }
  let (inner, inner_rect) = blocks
    .into_iter()
    .filter(|(_, rect)| (rect.height() - 10.0).abs() < 0.25)
    .min_by(|a, b| a.1.height().total_cmp(&b.1.height()))
    .expect("expected to find inner block fragment with height ~10px");
  let _ = inner; // keep reference alive for debug assertions if needed.

  let inner_bottom = inner_rect.y() + inner_rect.height();
  assert!(
    (inner_bottom - bottom_cell.bounds.height()).abs() < 0.5,
    "expected `vertical-align: bottom` to shift cell contents within the inflated cell (inner_bottom={inner_bottom:.2} vs cell_height={:.2})",
    bottom_cell.bounds.height(),
  );
}

