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
fn collapsed_row_removal_removes_vertical_gap() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { border-collapse: separate; border-spacing: 0; }
          td { padding: 0; margin: 0; border: 0; height: 10px; font-size: 0; line-height: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr><td>A</td></tr>
          <tr style="visibility: collapse;"><td>B</td></tr>
          <tr><td>C</td></tr>
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
    !cells.contains_key(&'B'),
    "collapsed row cell should not be laid out"
  );

  let a = cells.get(&'A').expect("row 1 cell present");
  let c = cells.get(&'C').expect("row 3 cell present");

  assert!(
    (a.rect.height() - 10.0).abs() < 0.1,
    "row height should match explicit cell height (got {})",
    a.rect.height()
  );

  let expected_c_y = a.rect.y() + a.rect.height();
  let gap = c.rect.y() - expected_c_y;
  assert!(
    gap.abs() < 0.1,
    "collapsed row should not create a gap (expected row 3 y={expected_c_y}, got {}, gap={gap})",
    c.rect.y()
  );
}

#[test]
fn collapsed_column_removal_adjusts_colspans_and_offsets() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            border-collapse: separate;
            border-spacing: 0;
            table-layout: fixed;
          }
          col { width: 20px; }
          td { padding: 0; margin: 0; border: 0; height: 10px; font-size: 0; line-height: 0; }
        </style>
      </head>
      <body>
        <table>
          <col />
          <col style="visibility: collapse;" />
          <col />
          <tr><td colspan="3">S</td></tr>
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
          </tr>
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
    !cells.contains_key(&'B'),
    "collapsed column cell should not be laid out"
  );

  let a = cells.get(&'A').expect("cell in first visible column present");
  let c = cells.get(&'C').expect("cell in last visible column present");
  let s = cells.get(&'S').expect("spanning cell present");

  let expected_c_x = a.rect.x() + a.rect.width();
  let gap = c.rect.x() - expected_c_x;
  assert!(
    gap.abs() < 0.1,
    "collapsed column should be removed from offsets (expected C x={expected_c_x}, got {}, gap={gap})",
    c.rect.x()
  );

  let expected_span_width = a.rect.width() + c.rect.width();
  let span_gap = s.rect.width() - expected_span_width;
  assert!(
    span_gap.abs() < 0.1,
    "colspan should shrink to visible columns (expected S width={expected_span_width}, got {}, gap={span_gap})",
    s.rect.width()
  );
}

#[test]
fn collapsed_column_removal_adjusts_colspans_and_offsets_rtl() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            border-collapse: separate;
            border-spacing: 0;
            table-layout: fixed;
            direction: rtl;
          }
          col { width: 20px; }
          td { padding: 0; margin: 0; border: 0; height: 10px; font-size: 0; line-height: 0; }
        </style>
      </head>
      <body>
        <table>
          <col />
          <col style="visibility: collapse;" />
          <col />
          <tr><td colspan="3">S</td></tr>
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
          </tr>
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
    !cells.contains_key(&'B'),
    "collapsed column cell should not be laid out"
  );

  let a = cells.get(&'A').expect("cell in first visible column present");
  let c = cells.get(&'C').expect("cell in last visible column present");
  let s = cells.get(&'S').expect("spanning cell present");

  assert!(
    a.rect.x() > c.rect.x(),
    "expected RTL order A (right) > C (left), got A.x={} C.x={}",
    a.rect.x(),
    c.rect.x()
  );
  let gap = a.rect.x() - (c.rect.x() + c.rect.width());
  assert!(
    gap.abs() < 0.1,
    "collapsed column should be removed from offsets in RTL (gap={gap})"
  );

  let expected_span_width = a.rect.width() + c.rect.width();
  let span_gap = s.rect.width() - expected_span_width;
  assert!(
    span_gap.abs() < 0.1,
    "colspan should shrink to visible columns in RTL (expected S width={expected_span_width}, got {}, gap={span_gap})",
    s.rect.width()
  );
  assert!(
    (s.rect.x() - c.rect.x()).abs() < 0.1,
    "expected spanning cell to start at leftmost visible column in RTL (S.x={} C.x={})",
    s.rect.x(),
    c.rect.x()
  );
}

#[test]
fn column_visibility_collapse_removes_column_from_layout() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0; table-layout: fixed; }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <col style="width: 40px; visibility: collapse" />
          <col style="width: 50px" />
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(!cells.contains_key(&'B'), "collapsed column cell should not be laid out");

  let a = cells.get(&'A').expect("cell A present");
  let c = cells.get(&'C').expect("cell C present");

  let gap = c.rect.x() - (a.rect.x() + a.rect.width());
  assert!(
    gap.abs() < 0.1,
    "cells adjacent after collapsing column (gap={gap})"
  );
  assert!(
    (c.rect.width() - 50.0).abs() < 0.1,
    "collapsed column should not affect subsequent column width (got {})",
    c.rect.width()
  );
}

#[test]
fn column_visibility_collapse_removes_column_from_layout_rtl() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            display: inline-table;
            border-collapse: separate;
            border-spacing: 0;
            table-layout: fixed;
            direction: rtl;
          }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <col style="width: 40px; visibility: collapse" />
          <col style="width: 50px" />
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(!cells.contains_key(&'B'), "collapsed column cell should not be laid out");

  let a = cells.get(&'A').expect("cell A present");
  let c = cells.get(&'C').expect("cell C present");

  assert!(
    a.rect.x() > c.rect.x(),
    "expected RTL order A (right) > C (left), got A.x={} C.x={}",
    a.rect.x(),
    c.rect.x()
  );
  let gap = a.rect.x() - (c.rect.x() + c.rect.width());
  assert!(
    gap.abs() < 0.1,
    "cells adjacent after collapsing column in RTL (gap={gap})"
  );
  assert!(
    (a.rect.width() - 30.0).abs() < 0.1,
    "expected first source column width 30px (got {})",
    a.rect.width()
  );
  assert!(
    (c.rect.width() - 50.0).abs() < 0.1,
    "collapsed column should not affect subsequent column width (got {})",
    c.rect.width()
  );
}

#[test]
fn row_visibility_collapse_removes_row_from_layout() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0; table-layout: fixed; }
          td { height: 10px; padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <tr><td>A</td></tr>
          <tr style="visibility: collapse"><td>B</td></tr>
          <tr><td>C</td></tr>
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

  assert!(!cells.contains_key(&'B'), "collapsed row cell should not be laid out");

  let a = cells.get(&'A').expect("cell A present");
  let c = cells.get(&'C').expect("cell C present");

  let gap = c.rect.y() - (a.rect.y() + a.rect.height());
  assert!(
    gap.abs() < 0.1,
    "rows adjacent after collapsing middle row (gap={gap})"
  );
}

#[test]
fn rowspan_across_collapsed_row_is_shortened() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0; table-layout: fixed; }
          td { height: 10px; padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 40px" />
          <col style="width: 40px" />
          <tr>
            <td rowspan="2">A</td>
            <td>X</td>
          </tr>
          <tr style="visibility: collapse">
            <td>(collapsed)</td>
          </tr>
          <tr>
            <td>B</td>
            <td>Y</td>
          </tr>
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

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");

  assert!(
    (a.rect.height() - 10.0).abs() < 0.1,
    "rowspan should not include collapsed row (got {})",
    a.rect.height()
  );
  let gap = b.rect.y() - (a.rect.y() + a.rect.height());
  assert!(
    gap.abs() < 0.1,
    "next visible row should start immediately after first row (gap={gap})"
  );
}

#[test]
fn rowspan_across_collapsed_row_is_shortened_rtl() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            display: inline-table;
            border-collapse: separate;
            border-spacing: 0;
            table-layout: fixed;
            direction: rtl;
          }
          td { height: 10px; padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 40px" />
          <col style="width: 50px" />
          <tr>
            <td rowspan="2">A</td>
            <td>X</td>
          </tr>
          <tr style="visibility: collapse">
            <td>(collapsed)</td>
          </tr>
          <tr>
            <td>B</td>
            <td>Y</td>
          </tr>
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

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");
  let x = cells.get(&'X').expect("cell X present");
  let y = cells.get(&'Y').expect("cell Y present");

  assert!(
    (a.rect.height() - 10.0).abs() < 0.1,
    "rowspan should not include collapsed row in RTL (got {})",
    a.rect.height()
  );

  assert!(
    a.rect.x() > x.rect.x(),
    "expected RTL order A (right) > X (left), got A.x={} X.x={}",
    a.rect.x(),
    x.rect.x()
  );
  assert!(
    b.rect.x() > y.rect.x(),
    "expected RTL order B (right) > Y (left), got B.x={} Y.x={}",
    b.rect.x(),
    y.rect.x()
  );
  assert!(
    (a.rect.x() - b.rect.x()).abs() < 0.1,
    "expected A and B to share the same column x in RTL, got A.x={} B.x={}",
    a.rect.x(),
    b.rect.x()
  );

  let gap = b.rect.y() - (a.rect.y() + a.rect.height());
  assert!(
    gap.abs() < 0.1,
    "next visible row should start immediately after first row in RTL (gap={gap})"
  );
}

#[test]
fn colspan_over_collapsed_column_is_shortened() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0; table-layout: fixed; }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <col style="width: 40px; visibility: collapse" />
          <col style="width: 50px" />
          <tr>
            <td colspan="2">A</td>
            <td>C</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  let a = cells.get(&'A').expect("cell A present");
  let c = cells.get(&'C').expect("cell C present");

  assert!(
    (a.rect.width() - 30.0).abs() < 0.1,
    "colspan should not include collapsed column (got {})",
    a.rect.width()
  );
  let gap = c.rect.x() - (a.rect.x() + a.rect.width());
  assert!(
    gap.abs() < 0.1,
    "cells adjacent after collapsing column (gap={gap})"
  );
}

#[test]
fn colspan_over_collapsed_column_is_shortened_rtl() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            display: inline-table;
            border-collapse: separate;
            border-spacing: 0;
            table-layout: fixed;
            direction: rtl;
          }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <col style="width: 40px; visibility: collapse" />
          <col style="width: 50px" />
          <tr>
            <td colspan="2">A</td>
            <td>C</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  let a = cells.get(&'A').expect("cell A present");
  let c = cells.get(&'C').expect("cell C present");

  assert!(
    (a.rect.width() - 30.0).abs() < 0.1,
    "colspan should not include collapsed column in RTL (got {})",
    a.rect.width()
  );
  assert!(
    a.rect.x() > c.rect.x(),
    "expected RTL order A (right) > C (left), got A.x={} C.x={}",
    a.rect.x(),
    c.rect.x()
  );
  let gap = a.rect.x() - (c.rect.x() + c.rect.width());
  assert!(
    gap.abs() < 0.1,
    "cells adjacent after collapsing column in RTL (gap={gap})"
  );
}

#[test]
fn row_group_visibility_collapse_removes_rows_from_layout() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0; table-layout: fixed; }
          td { height: 10px; padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <tbody><tr><td>A</td></tr></tbody>
          <tbody style="visibility: collapse"><tr><td>B</td></tr></tbody>
          <tbody><tr><td>C</td></tr></tbody>
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
    !cells.contains_key(&'B'),
    "collapsed row-group cell should not be laid out"
  );

  let a = cells.get(&'A').expect("cell A present");
  let c = cells.get(&'C').expect("cell C present");

  let gap = c.rect.y() - (a.rect.y() + a.rect.height());
  assert!(
    gap.abs() < 0.1,
    "rows adjacent after collapsing middle row-group (gap={gap})"
  );
}

#[test]
fn column_group_visibility_collapse_removes_columns_from_layout() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0; table-layout: fixed; }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <colgroup style="visibility: collapse"><col style="width: 40px" /></colgroup>
          <col style="width: 50px" />
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(
    !cells.contains_key(&'B'),
    "collapsed column-group cell should not be laid out"
  );

  let a = cells.get(&'A').expect("cell A present");
  let c = cells.get(&'C').expect("cell C present");

  let gap = c.rect.x() - (a.rect.x() + a.rect.width());
  assert!(
    gap.abs() < 0.1,
    "cells adjacent after collapsing column-group (gap={gap})"
  );
  assert!(
    (c.rect.width() - 50.0).abs() < 0.1,
    "collapsed column-group should not affect subsequent column width (got {})",
    c.rect.width()
  );
}

#[test]
fn column_group_visibility_collapse_removes_columns_from_layout_rtl() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            display: inline-table;
            border-collapse: separate;
            border-spacing: 0;
            table-layout: fixed;
            direction: rtl;
          }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <colgroup style="visibility: collapse"><col style="width: 40px" /></colgroup>
          <col style="width: 50px" />
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(
    !cells.contains_key(&'B'),
    "collapsed column-group cell should not be laid out"
  );

  let a = cells.get(&'A').expect("cell A present");
  let c = cells.get(&'C').expect("cell C present");

  assert!(
    a.rect.x() > c.rect.x(),
    "expected RTL order A (right) > C (left), got A.x={} C.x={}",
    a.rect.x(),
    c.rect.x()
  );
  let gap = a.rect.x() - (c.rect.x() + c.rect.width());
  assert!(
    gap.abs() < 0.1,
    "cells adjacent after collapsing column-group in RTL (gap={gap})"
  );
  assert!(
    (a.rect.width() - 30.0).abs() < 0.1,
    "expected first source column width 30px (got {})",
    a.rect.width()
  );
  assert!(
    (c.rect.width() - 50.0).abs() < 0.1,
    "collapsed column-group should not affect subsequent column width (got {})",
    c.rect.width()
  );
}

#[test]
fn column_group_span_visibility_collapse_removes_columns_from_layout() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0; table-layout: fixed; }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <colgroup span="2" style="visibility: collapse"></colgroup>
          <col style="width: 50px" />
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
            <td>D</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(!cells.contains_key(&'B'), "collapsed column-group cell should not be laid out");
  assert!(!cells.contains_key(&'C'), "collapsed column-group cell should not be laid out");

  let a = cells.get(&'A').expect("cell A present");
  let d = cells.get(&'D').expect("cell D present");

  let gap = d.rect.x() - (a.rect.x() + a.rect.width());
  assert!(
    gap.abs() < 0.1,
    "cells adjacent after collapsing column-group span (gap={gap})"
  );
  assert!(
    (a.rect.width() - 30.0).abs() < 0.1,
    "expected first source column width 30px (got {})",
    a.rect.width()
  );
  assert!(
    (d.rect.width() - 50.0).abs() < 0.1,
    "collapsed column-group should not affect subsequent column width (got {})",
    d.rect.width()
  );
}

#[test]
fn column_group_span_visibility_collapse_removes_columns_from_layout_rtl() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            display: inline-table;
            border-collapse: separate;
            border-spacing: 0;
            table-layout: fixed;
            direction: rtl;
          }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <colgroup span="2" style="visibility: collapse"></colgroup>
          <col style="width: 50px" />
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
            <td>D</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(!cells.contains_key(&'B'), "collapsed column-group cell should not be laid out");
  assert!(!cells.contains_key(&'C'), "collapsed column-group cell should not be laid out");

  let a = cells.get(&'A').expect("cell A present");
  let d = cells.get(&'D').expect("cell D present");

  assert!(
    a.rect.x() > d.rect.x(),
    "expected RTL order A (right) > D (left), got A.x={} D.x={}",
    a.rect.x(),
    d.rect.x()
  );
  let gap = a.rect.x() - (d.rect.x() + d.rect.width());
  assert!(
    gap.abs() < 0.1,
    "cells adjacent after collapsing column-group span in RTL (gap={gap})"
  );
  assert!(
    (a.rect.width() - 30.0).abs() < 0.1,
    "expected first source column width 30px (got {})",
    a.rect.width()
  );
  assert!(
    (d.rect.width() - 50.0).abs() < 0.1,
    "collapsed column-group should not affect subsequent column width (got {})",
    d.rect.width()
  );
}

#[test]
fn column_visibility_collapse_removes_extra_border_spacing_gap() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 10px 0; table-layout: fixed; }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <col style="width: 40px; visibility: collapse" />
          <col style="width: 50px" />
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(!cells.contains_key(&'B'), "collapsed column cell should not be laid out");

  let a = cells.get(&'A').expect("cell A present");
  let c = cells.get(&'C').expect("cell C present");

  let gap = c.rect.x() - (a.rect.x() + a.rect.width());
  assert!(
    (gap - 10.0).abs() < 0.1,
    "border-spacing should be applied once between adjacent columns (gap={gap})"
  );
}

#[test]
fn column_visibility_collapse_removes_extra_border_spacing_gap_rtl() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            display: inline-table;
            border-collapse: separate;
            border-spacing: 10px 0;
            table-layout: fixed;
            direction: rtl;
          }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <col style="width: 40px; visibility: collapse" />
          <col style="width: 50px" />
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(!cells.contains_key(&'B'), "collapsed column cell should not be laid out");

  let a = cells.get(&'A').expect("cell A present");
  let c = cells.get(&'C').expect("cell C present");

  assert!(
    a.rect.x() > c.rect.x(),
    "expected RTL order A (right) > C (left), got A.x={} C.x={}",
    a.rect.x(),
    c.rect.x()
  );
  let gap = a.rect.x() - (c.rect.x() + c.rect.width());
  assert!(
    (gap - 10.0).abs() < 0.1,
    "border-spacing should be applied once between adjacent columns in RTL (gap={gap})"
  );
}

#[test]
fn row_visibility_collapse_removes_extra_border_spacing_gap() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0 8px; table-layout: fixed; }
          td { height: 10px; padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <tr><td>A</td></tr>
          <tr style="visibility: collapse"><td>B</td></tr>
          <tr><td>C</td></tr>
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

  assert!(!cells.contains_key(&'B'), "collapsed row cell should not be laid out");

  let a = cells.get(&'A').expect("cell A present");
  let c = cells.get(&'C').expect("cell C present");

  let gap = c.rect.y() - (a.rect.y() + a.rect.height());
  assert!(
    (gap - 8.0).abs() < 0.1,
    "border-spacing should be applied once between adjacent rows (gap={gap})"
  );
}

#[test]
fn rowspan_across_collapsed_row_removes_extra_border_spacing_gap() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0 8px; table-layout: fixed; }
          td { height: 10px; padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 40px" />
          <col style="width: 40px" />
          <tr>
            <td rowspan="2">A</td>
            <td>X</td>
          </tr>
          <tr style="visibility: collapse">
            <td>(collapsed)</td>
          </tr>
          <tr>
            <td>B</td>
            <td>Y</td>
          </tr>
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

  let a = cells.get(&'A').expect("cell A present");
  let b = cells.get(&'B').expect("cell B present");

  assert!(
    (a.rect.height() - 10.0).abs() < 0.1,
    "rowspan should not include collapsed row (got {})",
    a.rect.height()
  );
  let gap = b.rect.y() - (a.rect.y() + a.rect.height());
  assert!(
    (gap - 8.0).abs() < 0.1,
    "border-spacing should be applied once between adjacent rows (gap={gap})"
  );
}

#[test]
fn colspan_across_collapsed_middle_column_keeps_remaining_columns_and_spacing() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 10px 0; table-layout: fixed; }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <col style="width: 40px; visibility: collapse" />
          <col style="width: 50px" />
          <tr>
            <td colspan="3">A</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  let a = cells.get(&'A').expect("cell A present");
  // Only the first and third columns remain (30px and 50px) with a single border-spacing gap (10px)
  // between them.
  assert!(
    (a.rect.width() - 90.0).abs() < 0.1,
    "colspan should collapse away hidden column + spacing (expected 90, got {})",
    a.rect.width()
  );
}

#[test]
fn colspan_across_collapsed_middle_column_keeps_remaining_columns_and_spacing_rtl() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            display: inline-table;
            border-collapse: separate;
            border-spacing: 10px 0;
            table-layout: fixed;
            direction: rtl;
          }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <col style="width: 40px; visibility: collapse" />
          <col style="width: 50px" />
          <tr>
            <td colspan="3">A</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  let a = cells.get(&'A').expect("cell A present");
  assert!(
    (a.rect.width() - 90.0).abs() < 0.1,
    "colspan should collapse away hidden column + spacing in RTL (expected 90, got {})",
    a.rect.width()
  );
}

#[test]
fn rowspan_over_collapsed_middle_row_keeps_later_rows_in_span() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0 8px; table-layout: fixed; }
          td { height: 10px; padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 40px" />
          <col style="width: 40px" />
          <tr>
            <td rowspan="3">A</td>
            <td>X</td>
          </tr>
          <tr style="visibility: collapse">
            <td>(collapsed)</td>
          </tr>
          <tr>
            <td>B</td>
          </tr>
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

  let a = cells.get(&'A').expect("cell A present");

  // The original rowspan=3 covers rows 1 and 3 (with row 2 collapsed), so after collapse it should
  // span 2 visible rows, including the single vertical border-spacing (8px) between them.
  assert!(
    (a.rect.height() - 28.0).abs() < 0.1,
    "rowspan should include later visible row but skip collapsed row (expected 28, got {})",
    a.rect.height()
  );
}

#[test]
fn row_group_visibility_collapse_removes_extra_border_spacing_gap() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 0 8px; table-layout: fixed; }
          td { height: 10px; padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <tbody><tr><td>A</td></tr></tbody>
          <tbody style="visibility: collapse">
            <tr><td>B</td></tr>
            <tr><td>C</td></tr>
          </tbody>
          <tbody><tr><td>D</td></tr></tbody>
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
    !cells.contains_key(&'B') && !cells.contains_key(&'C'),
    "collapsed row-group cells should not be laid out"
  );

  let a = cells.get(&'A').expect("cell A present");
  let d = cells.get(&'D').expect("cell D present");

  let gap = d.rect.y() - (a.rect.y() + a.rect.height());
  assert!(
    (gap - 8.0).abs() < 0.1,
    "border-spacing should be applied once between adjacent rows after collapsing row-group (gap={gap})"
  );
}

#[test]
fn column_group_visibility_collapse_removes_extra_border_spacing_gap() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 10px 0; table-layout: fixed; }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <colgroup style="visibility: collapse">
            <col style="width: 40px" />
            <col style="width: 40px" />
          </colgroup>
          <col style="width: 50px" />
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
            <td>D</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(
    !cells.contains_key(&'B') && !cells.contains_key(&'C'),
    "collapsed column-group cells should not be laid out"
  );

  let a = cells.get(&'A').expect("cell A present");
  let d = cells.get(&'D').expect("cell D present");

  let gap = d.rect.x() - (a.rect.x() + a.rect.width());
  assert!(
    (gap - 10.0).abs() < 0.1,
    "border-spacing should be applied once between adjacent columns after collapsing column-group (gap={gap})"
  );
  assert!(
    (d.rect.width() - 50.0).abs() < 0.1,
    "collapsed column-group should not affect remaining column width (got {})",
    d.rect.width()
  );
}

#[test]
fn column_group_visibility_collapse_removes_extra_border_spacing_gap_rtl() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            display: inline-table;
            border-collapse: separate;
            border-spacing: 10px 0;
            table-layout: fixed;
            direction: rtl;
          }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <colgroup style="visibility: collapse">
            <col style="width: 40px" />
            <col style="width: 40px" />
          </colgroup>
          <col style="width: 50px" />
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
            <td>D</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(
    !cells.contains_key(&'B') && !cells.contains_key(&'C'),
    "collapsed column-group cells should not be laid out"
  );

  let a = cells.get(&'A').expect("cell A present");
  let d = cells.get(&'D').expect("cell D present");

  assert!(
    a.rect.x() > d.rect.x(),
    "expected RTL order A (right) > D (left), got A.x={} D.x={}",
    a.rect.x(),
    d.rect.x()
  );
  let gap = a.rect.x() - (d.rect.x() + d.rect.width());
  assert!(
    (gap - 10.0).abs() < 0.1,
    "border-spacing should be applied once between adjacent columns after collapsing column-group in RTL (gap={gap})"
  );
  assert!(
    (a.rect.width() - 30.0).abs() < 0.1,
    "expected first source column width 30px (got {})",
    a.rect.width()
  );
  assert!(
    (d.rect.width() - 50.0).abs() < 0.1,
    "collapsed column-group should not affect remaining column width in RTL (got {})",
    d.rect.width()
  );
}

#[test]
fn column_span_attribute_with_visibility_collapse_removes_multiple_columns() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table { display: inline-table; border-collapse: separate; border-spacing: 10px 0; table-layout: fixed; }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <col span="2" style="width: 40px; visibility: collapse" />
          <col style="width: 50px" />
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
            <td>D</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(
    !cells.contains_key(&'B') && !cells.contains_key(&'C'),
    "collapsed columns (via span) should not be laid out"
  );

  let a = cells.get(&'A').expect("cell A present");
  let d = cells.get(&'D').expect("cell D present");

  let gap = d.rect.x() - (a.rect.x() + a.rect.width());
  assert!(
    (gap - 10.0).abs() < 0.1,
    "border-spacing should be applied once after collapsing multiple columns (gap={gap})"
  );
  assert!(
    (d.rect.width() - 50.0).abs() < 0.1,
    "collapsed columns should not affect remaining column width (got {})",
    d.rect.width()
  );
}

#[test]
fn column_span_attribute_with_visibility_collapse_removes_multiple_columns_rtl() {
  ensure_rayon_threads();
  let html = r#"
    <html>
      <head>
        <style>
          body { margin: 0; }
          table {
            display: inline-table;
            border-collapse: separate;
            border-spacing: 10px 0;
            table-layout: fixed;
            direction: rtl;
          }
          td { padding: 0; margin: 0; border: 0; font-size: 10px; line-height: 10px; }
        </style>
      </head>
      <body>
        <table>
          <col style="width: 30px" />
          <col span="2" style="width: 40px; visibility: collapse" />
          <col style="width: 50px" />
          <tr>
            <td>A</td>
            <td>B</td>
            <td>C</td>
            <td>D</td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 400, 200).unwrap();

  let table = find_table(&tree.root).expect("table fragment present");
  let mut cells = HashMap::new();
  collect_cells(table, (0.0, 0.0), &mut cells);

  assert!(
    !cells.contains_key(&'B') && !cells.contains_key(&'C'),
    "collapsed columns (via span) should not be laid out"
  );

  let a = cells.get(&'A').expect("cell A present");
  let d = cells.get(&'D').expect("cell D present");

  assert!(
    a.rect.x() > d.rect.x(),
    "expected RTL order A (right) > D (left), got A.x={} D.x={}",
    a.rect.x(),
    d.rect.x()
  );

  let gap = a.rect.x() - (d.rect.x() + d.rect.width());
  assert!(
    (gap - 10.0).abs() < 0.1,
    "border-spacing should be applied once after collapsing multiple columns in RTL (gap={gap})"
  );
  assert!(
    (a.rect.width() - 30.0).abs() < 0.1,
    "collapsed columns should not affect remaining column width in RTL (A width {})",
    a.rect.width()
  );
  assert!(
    (d.rect.width() - 50.0).abs() < 0.1,
    "collapsed columns should not affect remaining column width in RTL (D width {})",
    d.rect.width()
  );
}
