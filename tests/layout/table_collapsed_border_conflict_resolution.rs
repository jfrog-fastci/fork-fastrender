use fastrender::api::FastRender;
use fastrender::style::color::Rgba;
use fastrender::style::types::BorderStyle;
use fastrender::tree::fragment_tree::{FragmentNode, TableCollapsedBorders};

fn find_table_borders(node: &FragmentNode) -> Option<&TableCollapsedBorders> {
  if let Some(borders) = node.table_borders.as_ref() {
    return Some(borders.as_ref());
  }
  node.children.iter().find_map(find_table_borders)
}

fn table_borders_from_html(html: &str) -> TableCollapsedBorders {
  let mut renderer = FastRender::new().unwrap();
  let document = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&document, 200, 200).unwrap();
  find_table_borders(&tree.root)
    .expect("expected table collapsed border metadata")
    .clone()
}

#[test]
fn collapsed_border_conflict_width_beats_style() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr>
            <td style="border-right: 1px double red;"></td>
            <td style="border-left: 4px solid blue;"></td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let borders = table_borders_from_html(html);
  let segment = borders
    .vertical_segment(1, 0)
    .expect("expected internal vertical border segment");
  assert!(
    (segment.width - 4.0).abs() < 0.01,
    "expected wider 4px border to win, got {:?}",
    segment
  );
  assert_eq!(segment.style, BorderStyle::Solid);
}

#[test]
fn collapsed_border_conflict_style_breaks_width_ties() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr>
            <td style="border-right: 2px dashed red;"></td>
            <td style="border-left: 2px solid blue;"></td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let borders = table_borders_from_html(html);
  let segment = borders
    .vertical_segment(1, 0)
    .expect("expected internal vertical border segment");
  assert!(
    (segment.width - 2.0).abs() < 0.01,
    "expected 2px border, got {:?}",
    segment
  );
  assert_eq!(segment.style, BorderStyle::Solid);
}

#[test]
fn collapsed_border_conflict_color_tie_break_respects_direction() {
  let html = |direction: &str| {
    format!(
      r#"
        <html>
          <head>
            <style>
              table {{ border-collapse: collapse; border: none; direction: {direction}; }}
              td {{ border: none; width: 10px; height: 10px; padding: 0; margin: 0; }}
            </style>
          </head>
          <body>
            <table>
              <tr>
                <td style="border-left: 2px solid blue; border-right: 2px solid red;"></td>
                <td style="border-left: 2px solid blue; border-right: 2px solid red;"></td>
              </tr>
            </table>
          </body>
        </html>
      "#
    )
  };

  let ltr = table_borders_from_html(&html("ltr"));
  let ltr_segment = ltr
    .vertical_segment(1, 0)
    .expect("expected internal vertical border segment");
  assert_eq!(
    ltr_segment.color,
    Rgba::RED,
    "expected LTR to prefer the left cell border color"
  );

  let rtl = table_borders_from_html(&html("rtl"));
  let rtl_segment = rtl
    .vertical_segment(1, 0)
    .expect("expected internal vertical border segment");
  assert_eq!(
    rtl_segment.color,
    Rgba::BLUE,
    "expected RTL to prefer the right cell border color"
  );
}

#[test]
fn collapsed_border_resolution_honors_colgroup_span() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; }
          colgroup { border-right: 3px solid red; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <colgroup span="2"></colgroup>
          <tr><td></td><td></td></tr>
        </table>
      </body>
    </html>
  "#;

  let borders = table_borders_from_html(html);
  assert!(
    (borders.vertical_line_width(2) - 3.0).abs() < 0.01,
    "expected the colgroup right border to land on the table's outer edge, got {}",
    borders.vertical_line_width(2)
  );
  assert!(
    borders.vertical_line_width(1) < 0.01,
    "expected the internal divider to remain borderless, got {}",
    borders.vertical_line_width(1)
  );
}

#[test]
fn collapsed_border_resolution_honors_col_span() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; }
          col { border: none; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <col span="2" style="border-right: 3px solid red;">
          <tr><td></td><td></td></tr>
        </table>
      </body>
    </html>
  "#;

  let borders = table_borders_from_html(html);
  assert!(
    borders.vertical_line_width(0) < 0.01,
    "expected no border on the left edge, got {}",
    borders.vertical_line_width(0)
  );
  assert!(
    (borders.vertical_line_width(1) - 3.0).abs() < 0.01,
    "expected the <col span> border-right to apply between the two columns, got {}",
    borders.vertical_line_width(1)
  );
  assert!(
    (borders.vertical_line_width(2) - 3.0).abs() < 0.01,
    "expected the <col span> border-right to also apply on the outer edge, got {}",
    borders.vertical_line_width(2)
  );
}

#[test]
fn collapsed_border_hidden_suppresses_all() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr>
            <td style="border-right: 4px solid red;"></td>
            <td style="border-left: 4px hidden blue;"></td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let borders = table_borders_from_html(html);
  let segment = borders
    .vertical_segment(1, 0)
    .expect("expected internal vertical border segment");
  assert!(
    segment.width < 0.01,
    "expected hidden to suppress the border, got {:?}",
    segment
  );
  assert_eq!(segment.style, BorderStyle::None);
}

#[test]
fn collapsed_border_origin_priority_cell_over_column() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <col style="border-right: 2px solid red;">
          <col>
          <tr>
            <td style="border-right: 2px solid blue;"></td>
            <td></td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let borders = table_borders_from_html(html);
  let segment = borders
    .vertical_segment(1, 0)
    .expect("expected internal vertical border segment");
  assert_eq!(
    segment.color,
    Rgba::BLUE,
    "expected cell border to win over column border"
  );
  assert!(
    (segment.width - 2.0).abs() < 0.01,
    "expected 2px border, got {:?}",
    segment
  );
  assert_eq!(segment.style, BorderStyle::Solid);
}

#[test]
fn collapsed_border_origin_priority_column_over_colgroup() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <colgroup style="border-right: 2px solid red;">
            <col style="border-right: 2px solid blue;">
          </colgroup>
          <col>
          <tr><td></td><td></td></tr>
        </table>
      </body>
    </html>
  "#;

  let borders = table_borders_from_html(html);
  let segment = borders
    .vertical_segment(1, 0)
    .expect("expected internal vertical border segment");
  assert_eq!(
    segment.color,
    Rgba::BLUE,
    "expected column border to win over colgroup border"
  );
  assert!(
    (segment.width - 2.0).abs() < 0.01,
    "expected 2px border, got {:?}",
    segment
  );
  assert_eq!(segment.style, BorderStyle::Solid);
}

#[test]
fn collapsed_border_origin_priority_row_over_table() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; border-top: 2px solid red; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <tr style="border-top: 2px solid blue;">
            <td></td>
          </tr>
        </table>
      </body>
    </html>
  "#;

  let borders = table_borders_from_html(html);
  let segment = borders
    .horizontal_segment(0, 0)
    .expect("expected top horizontal border segment");
  assert_eq!(
    segment.color,
    Rgba::BLUE,
    "expected row border to win over table border"
  );
  assert!(
    (segment.width - 2.0).abs() < 0.01,
    "expected 2px border, got {:?}",
    segment
  );
  assert_eq!(segment.style, BorderStyle::Solid);
}
