use std::collections::HashMap;
use std::sync::Once;

use fastrender::api::FastRender;
use fastrender::geometry::Rect;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};

static SET_RAYON_THREADS: Once = Once::new();

fn ensure_rayon_threads() {
  SET_RAYON_THREADS.call_once(|| {
    if std::env::var("RAYON_NUM_THREADS").is_err() {
      std::env::set_var("RAYON_NUM_THREADS", "4");
    }
  });
}

#[derive(Debug, Clone)]
struct CellInfo {
  rect: Rect,
}

fn find_table<'a>(node: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::Table | Display::InlineTable)
  ) {
    return Some(node);
  }
  node.children.iter().find_map(find_table)
}

fn collect_text(node: &FragmentNode, out: &mut String) {
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push_str(text);
  }
  for child in node.children.iter() {
    collect_text(child, out);
  }
}

fn collect_cells(node: &FragmentNode, origin: (f32, f32), cells: &mut HashMap<char, CellInfo>) {
  let pos = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
  if matches!(
    node.style.as_ref().map(|s| s.display),
    Some(Display::TableCell)
  ) {
    let mut text = String::new();
    collect_text(node, &mut text);
    if let Some(label) = text.trim().chars().find(|c| c.is_ascii_alphabetic()) {
      let rect = Rect::from_xywh(pos.0, pos.1, node.bounds.width(), node.bounds.height());
      cells.insert(label, CellInfo { rect });
    }
  }
  for child in node.children.iter() {
    collect_cells(child, pos, cells);
  }
}

#[test]
fn rowspan_zero_spans_to_end_of_tbody() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { border-collapse: separate; border-spacing: 0 5px; table-layout: fixed; }
          col { width: 20px; }
          td { padding: 0; margin: 0; border: 0; font-size: 0; line-height: 0; }
        </style>
      </head>
      <body>
        <table>
          <col />
          <col />
          <tbody>
            <tr>
              <td rowspan="0" style="height: 10px;">A</td>
              <td style="height: 10px;">B</td>
            </tr>
            <tr><td style="height: 12px;">C</td></tr>
            <tr><td style="height: 14px;">D</td></tr>
          </tbody>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  let a = cells.get(&'A').expect("spanning cell present");
  let b = cells.get(&'B').expect("first row second cell present");
  let c = cells.get(&'C').expect("second row cell present");
  let d = cells.get(&'D').expect("third row cell present");

  assert!(
    (c.rect.x() - b.rect.x()).abs() < 0.1,
    "rowspan=0 should keep later row cells in column 1 (expected C.x={}, got {})",
    b.rect.x(),
    c.rect.x()
  );
  assert!(
    (d.rect.x() - b.rect.x()).abs() < 0.1,
    "rowspan=0 should keep later row cells in column 1 (expected D.x={}, got {})",
    b.rect.x(),
    d.rect.x()
  );

  let expected_a_height = (d.rect.y() + d.rect.height()) - a.rect.y();
  assert!(
    (a.rect.height() - expected_a_height).abs() < 0.1,
    "rowspan=0 should span all remaining tbody rows (expected A.height={expected_a_height}, got {})",
    a.rect.height()
  );
}

#[test]
fn rowspan_zero_does_not_cross_row_group_boundary() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { border-collapse: separate; border-spacing: 0 4px; table-layout: fixed; }
          col { width: 20px; }
          td { padding: 0; margin: 0; border: 0; font-size: 0; line-height: 0; }
        </style>
      </head>
      <body>
        <table>
          <col />
          <col />
          <thead>
            <tr>
              <td rowspan="0" style="height: 10px;">A</td>
              <td style="height: 10px;">B</td>
            </tr>
            <tr><td style="height: 12px;">C</td></tr>
          </thead>
          <tbody>
            <tr>
              <td style="height: 14px;">D</td>
              <td style="height: 14px;">E</td>
            </tr>
          </tbody>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  let a = cells.get(&'A').expect("spanning header cell present");
  let b = cells
    .get(&'B')
    .expect("first header row second cell present");
  let c = cells.get(&'C').expect("second header row cell present");
  let d = cells.get(&'D').expect("first body row first cell present");

  assert!(
    (c.rect.x() - b.rect.x()).abs() < 0.1,
    "rowspan=0 in thead should span within thead, shifting later header row cells (expected C.x={}, got {})",
    b.rect.x(),
    c.rect.x()
  );
  assert!(
    (d.rect.x() - a.rect.x()).abs() < 0.1,
    "rowspan=0 in thead must not span into tbody (expected D.x={}, got {})",
    a.rect.x(),
    d.rect.x()
  );

  let expected_a_height = (c.rect.y() + c.rect.height()) - a.rect.y();
  assert!(
    (a.rect.height() - expected_a_height).abs() < 0.1,
    "rowspan=0 in thead should span all remaining header rows (expected A.height={expected_a_height}, got {})",
    a.rect.height()
  );
}

#[test]
fn rowspan_zero_shrinks_over_collapsed_rows() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { border-collapse: separate; border-spacing: 0 6px; table-layout: fixed; }
          col { width: 20px; }
          td { padding: 0; margin: 0; border: 0; font-size: 0; line-height: 0; }
        </style>
      </head>
      <body>
        <table>
          <col />
          <col />
          <tbody>
            <tr>
              <td rowspan="0" style="height: 10px;">A</td>
              <td style="height: 10px;">B</td>
            </tr>
            <tr style="visibility: collapse;"><td style="height: 12px;">X</td></tr>
            <tr><td style="height: 14px;">C</td></tr>
          </tbody>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(
    !cells.contains_key(&'X'),
    "collapsed row should not be laid out"
  );

  let a = cells.get(&'A').expect("spanning cell present");
  let b = cells.get(&'B').expect("first row second cell present");
  let c = cells.get(&'C').expect("last row cell present");

  assert!(
    (c.rect.x() - b.rect.x()).abs() < 0.1,
    "rowspan=0 should still occupy the first column after collapsing intermediate rows (expected C.x={}, got {})",
    b.rect.x(),
    c.rect.x()
  );

  let gap = c.rect.y() - (b.rect.y() + b.rect.height());
  assert!(
    (gap - 6.0).abs() < 0.1,
    "collapsed row should not add extra border-spacing gaps (expected 6px gap, got {gap})"
  );

  let expected_a_height = (c.rect.y() + c.rect.height()) - a.rect.y();
  assert!(
    (a.rect.height() - expected_a_height).abs() < 0.1,
    "rowspan=0 should shrink to visible rows (expected A.height={expected_a_height}, got {})",
    a.rect.height()
  );
}
