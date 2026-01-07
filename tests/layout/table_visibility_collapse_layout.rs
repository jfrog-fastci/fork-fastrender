use std::collections::HashMap;

use fastrender::api::FastRender;
use fastrender::geometry::Rect;
use fastrender::style::display::Display;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};

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
fn column_visibility_collapse_removes_column_from_layout() {
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
fn row_visibility_collapse_removes_row_from_layout() {
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
fn colspan_over_collapsed_column_is_shortened() {
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
fn row_group_visibility_collapse_removes_rows_from_layout() {
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
