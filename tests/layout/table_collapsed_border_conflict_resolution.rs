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
    "expected LTR to prefer the left-side cell border color"
  );
  assert!(
    (ltr_segment.width - 2.0).abs() < 0.01,
    "expected 2px border, got {:?}",
    ltr_segment
  );
  assert_eq!(ltr_segment.style, BorderStyle::Solid);

  let rtl = table_borders_from_html(&html("rtl"));
  let rtl_segment = rtl
    .vertical_segment(1, 0)
    .expect("expected internal vertical border segment");
  assert_eq!(
    rtl_segment.color,
    Rgba::BLUE,
    "expected RTL to prefer the right-side cell border color"
  );
  assert!(
    (rtl_segment.width - 2.0).abs() < 0.01,
    "expected 2px border, got {:?}",
    rtl_segment
  );
  assert_eq!(rtl_segment.style, BorderStyle::Solid);
}

#[test]
fn collapsed_border_cell_outer_edges_respect_direction_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; direction: rtl; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
          td.a { border-right: 3px solid red; }
          td.b { border-left: 5px solid blue; }
        </style>
      </head>
      <body>
        <table>
          <tr><td class="a"></td><td class="b"></td></tr>
        </table>
      </body>
    </html>
  "#;

  // In RTL, the first source column (cell `.a`) is laid out on the right, so its *physical*
  // `border-right` should land on the table's right outer edge (line 2 in a 2-column table).
  // Similarly, the second source column (cell `.b`) is laid out on the left, so its `border-left`
  // should land on the table's left outer edge (line 0).
  let borders = table_borders_from_html(html);
  let left = borders
    .vertical_segment(0, 0)
    .expect("expected left outer vertical border segment");
  let inner = borders
    .vertical_segment(1, 0)
    .expect("expected internal vertical border segment");
  let right = borders
    .vertical_segment(2, 0)
    .expect("expected right outer vertical border segment");

  assert!(
    (left.width - 5.0).abs() < 0.01,
    "expected the left outer edge to use the 5px border from the leftmost (second source) cell, got {left:?}"
  );
  assert_eq!(left.color, Rgba::BLUE);
  assert_eq!(left.style, BorderStyle::Solid);

  assert!(
    inner.width < 0.01,
    "expected no internal border between the two columns, got {inner:?}"
  );
  assert_eq!(inner.style, BorderStyle::None);

  assert!(
    (right.width - 3.0).abs() < 0.01,
    "expected the right outer edge to use the 3px border from the rightmost (first source) cell, got {right:?}"
  );
  assert_eq!(right.color, Rgba::RED);
  assert_eq!(right.style, BorderStyle::Solid);
}

#[test]
fn collapsed_border_resolution_honors_colgroup_span() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; }
          #cg { border-right: 3px solid red; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <colgroup id="cg" span="2"></colgroup>
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
fn collapsed_border_resolution_honors_colgroup_span_in_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; direction: rtl; }
          #cg { border-right: 3px solid red; }
          col { border: none; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <colgroup id="cg" span="2"></colgroup>
          <col>
          <tr><td></td><td></td><td></td></tr>
        </table>
      </body>
    </html>
  "#;

  let borders = table_borders_from_html(html);
  let line0 = borders.vertical_line_width(0);
  let line1 = borders.vertical_line_width(1);
  let line2 = borders.vertical_line_width(2);
  let line3 = borders.vertical_line_width(3);
  assert!(
    (line3 - 3.0).abs() < 0.01,
    "expected the colgroup right border to land on the table's outer edge in RTL, got {line3} (vertical lines: [{line0}, {line1}, {line2}, {line3}])",
  );
  assert!(
    line2 < 0.01,
    "expected the internal divider between colgroup columns to remain borderless, got {line2} (vertical lines: [{line0}, {line1}, {line2}, {line3}])",
  );
  assert!(
    line1 < 0.01,
    "expected the divider between the colgroup and remaining column to remain borderless, got {line1} (vertical lines: [{line0}, {line1}, {line2}, {line3}])",
  );
}

#[test]
fn collapsed_border_resolution_honors_colgroup_span_border_left_in_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; direction: rtl; }
          #cg { border-left: 3px solid red; }
          col { border: none; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <colgroup id="cg" span="2"></colgroup>
          <col>
          <tr><td></td><td></td><td></td></tr>
        </table>
      </body>
    </html>
  "#;

  let borders = table_borders_from_html(html);
  let line0 = borders.vertical_line_width(0);
  let line1 = borders.vertical_line_width(1);
  let line2 = borders.vertical_line_width(2);
  let line3 = borders.vertical_line_width(3);
  assert!(
    (line1 - 3.0).abs() < 0.01,
    "expected the colgroup border-left to land on the divider between the remaining column and the colgroup in RTL, got {line1} (vertical lines: [{line0}, {line1}, {line2}, {line3}])",
  );
  assert!(
    line0 < 0.01,
    "expected no border on the left outer edge, got {line0} (vertical lines: [{line0}, {line1}, {line2}, {line3}])",
  );
  assert!(
    line2 < 0.01,
    "expected no border on the internal divider within the colgroup, got {line2} (vertical lines: [{line0}, {line1}, {line2}, {line3}])",
  );
  assert!(
    line3 < 0.01,
    "expected no border on the right outer edge, got {line3} (vertical lines: [{line0}, {line1}, {line2}, {line3}])",
  );

  let segment = borders
    .vertical_segment(1, 0)
    .expect("expected the colgroup left-edge segment");
  assert_eq!(segment.color, Rgba::RED);
  assert_eq!(segment.style, BorderStyle::Solid);
}

#[test]
fn collapsed_border_resolution_honors_colgroup_with_col_children_in_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; direction: rtl; }
          #cg { border-right: 3px solid red; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <colgroup id="cg">
            <col span="2">
          </colgroup>
          <col>
          <tr><td></td><td></td><td></td></tr>
        </table>
      </body>
    </html>
  "#;

  let borders = table_borders_from_html(html);
  let line0 = borders.vertical_line_width(0);
  let line1 = borders.vertical_line_width(1);
  let line2 = borders.vertical_line_width(2);
  let line3 = borders.vertical_line_width(3);
  assert!(
    (line3 - 3.0).abs() < 0.01,
    "expected the colgroup right border to land on the table's outer edge in RTL, got {line3} (vertical lines: [{line0}, {line1}, {line2}, {line3}])",
  );
  assert!(
    line2 < 0.01,
    "expected the internal divider between colgroup columns to remain borderless, got {line2} (vertical lines: [{line0}, {line1}, {line2}, {line3}])",
  );
  assert!(
    line1 < 0.01,
    "expected the divider between the colgroup and remaining column to remain borderless, got {line1} (vertical lines: [{line0}, {line1}, {line2}, {line3}])",
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
fn collapsed_border_resolution_honors_col_span_in_rtl() {
  let html = r#"
    <html>
      <head>
        <style>
          table { border-collapse: collapse; border: none; direction: rtl; }
          col { border: none; }
          td { border: none; width: 10px; height: 10px; padding: 0; margin: 0; }
        </style>
      </head>
      <body>
        <table>
          <col span="2" style="border-right: 3px solid red;">
          <col>
          <tr><td></td><td></td><td></td></tr>
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
    borders.vertical_line_width(1) < 0.01,
    "expected no border between the non-spanned column and the spanned columns, got {}",
    borders.vertical_line_width(1)
  );
  assert!(
    (borders.vertical_line_width(2) - 3.0).abs() < 0.01,
    "expected the <col span> border-right to apply between the spanned columns, got {}",
    borders.vertical_line_width(2)
  );
  assert!(
    (borders.vertical_line_width(3) - 3.0).abs() < 0.01,
    "expected the <col span> border-right to also apply on the outer edge, got {}",
    borders.vertical_line_width(3)
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
