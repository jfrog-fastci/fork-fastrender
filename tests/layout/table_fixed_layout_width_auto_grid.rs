use fastrender::api::FastRender;
use fastrender::geometry::Rect;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::FragmentNode;

fn find_table<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  // Avoid recursive traversal: fragment trees can be deep enough to overflow the default Rust test
  // thread stack on some platforms.
  let mut stack = vec![node];
  while let Some(node) = stack.pop() {
    if matches!(node.style.as_ref().map(|s| s.display), Some(Display::Table)) {
      return Some(node);
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  None
}

fn collect_cells<'a>(node: &'a FragmentNode, out: &mut Vec<&'a FragmentNode>) {
  // Avoid recursion for the same reason as `find_table`.
  let mut stack = vec![node];
  while let Some(node) = stack.pop() {
    if matches!(
      node.style.as_ref().map(|s| s.display),
      Some(Display::TableCell)
    ) {
      out.push(node);
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
}

fn collect_cell_rects(node: &FragmentNode, origin: (f32, f32), out: &mut Vec<Rect>) {
  // Avoid recursion for the same reason as `find_table`.
  let mut stack = vec![(node, origin)];
  while let Some((node, origin)) = stack.pop() {
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
      stack.push((child, pos));
    }
  }
}

#[test]
fn grid_item_table_layout_fixed_width_auto_uses_auto_layout() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .grid {
            display: grid;
            width: 400px;
            grid-template-columns: 1fr;
            justify-items: stretch;
          }
          table {
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
        <div class="grid">
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
    "expected wide second-row content to influence column width in grid context (max cell width {max_cell_width:.2})"
  );
}

#[test]
fn grid_item_table_layout_fixed_width_auto_uses_auto_layout_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .grid {
            display: grid;
            width: 400px;
            grid-template-columns: 1fr;
            justify-items: stretch;
          }
          table {
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
        <div class="grid">
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
  let mut cell_rects = Vec::new();
  collect_cell_rects(table, (0.0, 0.0), &mut cell_rects);
  assert_eq!(
    cell_rects.len(),
    4,
    "expected 4 table cells, got {}",
    cell_rects.len()
  );

  let max_cell_width = cell_rects
    .iter()
    .map(|cell| cell.width())
    .fold(0.0f32, f32::max);
  assert!(
    max_cell_width > 250.0,
    "expected wide second-row content to influence column width in grid context RTL (max cell width {max_cell_width:.2})"
  );

  // Ensure the wide (source-first) column is placed on the right in RTL.
  let min_y = cell_rects
    .iter()
    .map(|cell| cell.y())
    .fold(f32::INFINITY, f32::min);
  let top_row: Vec<&Rect> = cell_rects
    .iter()
    .filter(|cell| (cell.y() - min_y).abs() < 0.1)
    .collect();
  assert_eq!(
    top_row.len(),
    2,
    "expected two cells in the first row, got {}",
    top_row.len()
  );

  let (cell0, cell1) = (top_row[0], top_row[1]);
  let (wide, narrow) = if cell0.width() >= cell1.width() {
    (cell0, cell1)
  } else {
    (cell1, cell0)
  };
  assert!(
    wide.x() > narrow.x(),
    "expected RTL column order to place the wide column on the right (wide.x={} narrow.x={})",
    wide.x(),
    narrow.x()
  );
  let gap = wide.x() - (narrow.x() + narrow.width());
  assert!(gap.abs() < 0.1, "expected columns to be adjacent in RTL (gap={gap})");
}

#[test]
fn grid_item_table_layout_fixed_width_auto_uses_auto_layout_collapsed_border_model() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .grid {
            display: grid;
            width: 400px;
            grid-template-columns: 1fr;
            justify-items: stretch;
          }
          table {
            table-layout: fixed;
            border-collapse: collapse;
            border: none;
            padding: 0;
          }
          td { padding: 0; border: 0; }
        </style>
      </head>
      <body>
        <div class="grid">
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
    "expected wide second-row content to influence column width in grid collapsed model (max cell width {max_cell_width:.2})"
  );
}

#[test]
fn grid_item_table_layout_fixed_width_auto_uses_auto_layout_collapsed_border_model_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          .grid {
            display: grid;
            width: 400px;
            grid-template-columns: 1fr;
            justify-items: stretch;
          }
          table {
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
        <div class="grid">
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
  let mut cell_rects = Vec::new();
  collect_cell_rects(table, (0.0, 0.0), &mut cell_rects);
  assert_eq!(
    cell_rects.len(),
    4,
    "expected 4 table cells, got {}",
    cell_rects.len()
  );

  let max_cell_width = cell_rects
    .iter()
    .map(|cell| cell.width())
    .fold(0.0f32, f32::max);
  assert!(
    max_cell_width > 250.0,
    "expected wide second-row content to influence column width in grid collapsed model RTL (max cell width {max_cell_width:.2})"
  );

  // Ensure the wide (source-first) column is placed on the right in RTL.
  let min_y = cell_rects
    .iter()
    .map(|cell| cell.y())
    .fold(f32::INFINITY, f32::min);
  let top_row: Vec<&Rect> = cell_rects
    .iter()
    .filter(|cell| (cell.y() - min_y).abs() < 0.1)
    .collect();
  assert_eq!(
    top_row.len(),
    2,
    "expected two cells in the first row, got {}",
    top_row.len()
  );

  let (cell0, cell1) = (top_row[0], top_row[1]);
  let (wide, narrow) = if cell0.width() >= cell1.width() {
    (cell0, cell1)
  } else {
    (cell1, cell0)
  };
  assert!(
    wide.x() > narrow.x(),
    "expected RTL column order to place the wide column on the right (wide.x={} narrow.x={})",
    wide.x(),
    narrow.x()
  );
  let gap = wide.x() - (narrow.x() + narrow.width());
  assert!(gap.abs() < 0.1, "expected columns to be adjacent in RTL (gap={gap})");
}
