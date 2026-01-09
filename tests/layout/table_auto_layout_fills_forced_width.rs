use fastrender::api::FastRender;
use fastrender::geometry::Rect;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn find_table<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::Table) | Some(Display::InlineTable)
  ) {
    return Some(node);
  }
  node.children.iter().find_map(find_table)
}

fn collect_cell_rects(node: &FragmentNode, origin: (f32, f32), out: &mut Vec<Rect>) {
  let pos = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::TableCell)
  ) {
    out.push(Rect::from_xywh(
      pos.0,
      pos.1,
      node.bounds.width(),
      node.bounds.height(),
    ));
  }
  for child in node.children.iter() {
    collect_cell_rects(child, pos, out);
  }
}

fn assert_cells_fill_table_width(table: &FragmentNode, expected_table_width: f32) {
  let table_width = table.bounds.width();
  assert!(
    (table_width - expected_table_width).abs() < 0.1,
    "expected table width ~{expected_table_width}, got {table_width}"
  );

  let mut rects = Vec::new();
  collect_cell_rects(table, (0.0, 0.0), &mut rects);
  assert_eq!(
    rects.len(),
    2,
    "expected exactly 2 cells in the table, got {}",
    rects.len()
  );

  rects.sort_by(|a, b| a.x().partial_cmp(&b.x()).unwrap());
  let left = rects[0].x();
  let right = rects
    .iter()
    .map(|r| r.x() + r.width())
    .fold(f32::NEG_INFINITY, f32::max);
  let gap = rects[1].x() - (rects[0].x() + rects[0].width());

  assert!(
    left.abs() < 0.1,
    "expected first cell to start near the table's left edge (left={left})"
  );
  assert!(
    (right - table_width).abs() < 0.1,
    "expected cells to fill the table width (right={right}, table_width={table_width})"
  );
  assert!(gap.abs() < 0.1, "expected no gap between cells (gap={gap})");

  let sum_widths: f32 = rects.iter().map(|r| r.width()).sum();
  assert!(
    (sum_widths - table_width).abs() < 0.1,
    "expected cell widths to sum to the table width (sum={sum_widths}, table_width={table_width})"
  );
}

#[test]
fn table_auto_layout_fills_flex_grow_used_width() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .flex { display: flex; width: 400px; }
          table {
            flex: 1;
            width: auto;
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
              <td><div style="width:40px;height:10px"></div></td>
              <td><div style="width:40px;height:10px"></div></td>
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
  assert_cells_fill_table_width(table, 400.0);
}

#[test]
fn table_auto_layout_fills_grid_stretch_used_width() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .grid {
            display: grid;
            width: 400px;
            grid-template-columns: 400px;
            justify-items: stretch;
          }
          table {
            width: auto;
            border-collapse: separate;
            border-spacing: 0;
            padding: 0;
            border: 0;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <div class="grid">
          <table>
            <tr>
              <td><div style="width:40px;height:10px"></div></td>
              <td><div style="width:40px;height:10px"></div></td>
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
  assert_cells_fill_table_width(table, 400.0);
}

#[test]
fn table_auto_layout_min_width_expands_columns_to_fill() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            display: inline-table;
            width: auto;
            min-width: 300px;
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
          <tr>
            <td><div style="width:40px;height:10px"></div></td>
            <td><div style="width:40px;height:10px"></div></td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 800, 200).unwrap();

  let table = find_table(&tree.root).expect("expected table fragment");
  assert_cells_fill_table_width(table, 300.0);
}
