use fastrender::api::FastRender;
use fastrender::geometry::{Point, Rect};
use fastrender::paint::display_list::ClipShape;
use fastrender::paint::display_list::DisplayItem;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::style::media::MediaType;
use fastrender::tree::fragment_tree::FragmentTree;
use fastrender::tree::fragment_tree::{FragmentNode, TableCollapsedBorders};

fn count_fragments(node: &FragmentNode) -> usize {
  1 + node.children.iter().map(count_fragments).sum::<usize>()
}

fn find_table_borders(node: &FragmentNode) -> Option<&TableCollapsedBorders> {
  if let Some(borders) = node.table_borders.as_ref() {
    return Some(borders.as_ref());
  }
  node.children.iter().find_map(find_table_borders)
}

fn build_table_html(rows: usize, cols: usize, collapse: bool) -> String {
  let cells = "<td></td>".repeat(cols);
  let body: String = (0..rows).map(|_| format!("<tr>{cells}</tr>")).collect();
  let collapse_rule = if collapse {
    "collapse;"
  } else {
    "separate; border-spacing: 0;"
  };

  format!(
    r#"
    <html>
      <head>
        <style>
          table {{
            border-collapse: {collapse_rule}
          }}
          td {{
            border: 2px solid black;
            width: 10px;
            height: 10px;
            padding: 0;
            margin: 0;
          }}
        </style>
      </head>
      <body>
        <table>{body}</table>
      </body>
    </html>
  "#
  )
}

fn pages<'a>(tree: &'a FragmentTree) -> Vec<&'a FragmentNode> {
  let mut roots = vec![&tree.root];
  roots.extend(tree.additional_fragments.iter());
  roots
}

fn find_table_fragment<'a>(
  node: &'a FragmentNode,
  offset: Point,
) -> Option<(&'a FragmentNode, Rect)> {
  let abs_rect = node.bounds.translate(offset);
  if node.table_borders.is_some() {
    return Some((node, abs_rect));
  }
  for child in node.children.iter() {
    if let Some(found) = find_table_fragment(child, abs_rect.origin) {
      return Some(found);
    }
  }
  None
}

fn assert_point_eq_eps(actual: Point, expected: Point, eps: f32) {
  assert!(
    (actual.x - expected.x).abs() < eps && (actual.y - expected.y).abs() < eps,
    "expected point ({}, {}), got ({}, {})",
    expected.x,
    expected.y,
    actual.x,
    actual.y
  );
}

fn assert_rect_eq_eps(actual: Rect, expected: Rect, eps: f32) {
  assert_point_eq_eps(actual.origin, expected.origin, eps);
  assert!(
    (actual.width() - expected.width()).abs() < eps
      && (actual.height() - expected.height()).abs() < eps,
    "expected rect size ({}, {}), got ({}, {})",
    expected.width(),
    expected.height(),
    actual.width(),
    actual.height()
  );
}

#[test]
fn collapsed_table_uses_compact_borders() {
  const ROWS: usize = 20;
  const COLS: usize = 20;

  let collapsed_html = build_table_html(ROWS, COLS, true);
  let separate_html = build_table_html(ROWS, COLS, false);

  let mut collapsed = FastRender::new().unwrap();
  let collapsed_tree = collapsed
    .layout_document(&collapsed.parse_html(&collapsed_html).unwrap(), 800, 800)
    .unwrap();

  let mut separate = FastRender::new().unwrap();
  let separate_tree = separate
    .layout_document(&separate.parse_html(&separate_html).unwrap(), 800, 800)
    .unwrap();

  let collapsed_fragments = count_fragments(&collapsed_tree.root);
  let separate_fragments = count_fragments(&separate_tree.root);
  assert!(
    collapsed_fragments <= separate_fragments + 50,
    "collapsed borders should not inflate fragment count (collapsed={collapsed_fragments}, separate={separate_fragments})"
  );
  assert!(
    collapsed_fragments < ROWS * COLS * 5,
    "collapsed border fragment count should stay near cell count (got {collapsed_fragments})"
  );

  let table_borders =
    find_table_borders(&collapsed_tree.root).expect("collapsed table attaches border metadata");
  assert_eq!(table_borders.column_count, COLS);
  assert_eq!(table_borders.row_count, ROWS);
  assert_eq!(table_borders.column_line_positions.len(), COLS + 1);
  assert_eq!(table_borders.row_line_positions.len(), ROWS + 1);
  assert!(
    table_borders
      .vertical_borders
      .iter()
      .chain(table_borders.horizontal_borders.iter())
      .chain(table_borders.corner_borders.iter())
      .any(|b| b.is_visible()),
    "collapsed borders should include visible segments"
  );

  let list = DisplayListBuilder::new().build(&collapsed_tree.root);
  let collapsed_items = list
    .items()
    .iter()
    .filter(|item| matches!(item, DisplayItem::TableCollapsedBorders(_)))
    .count();
  assert_eq!(
    collapsed_items, 1,
    "collapsed borders should be emitted as a single paint primitive"
  );
}

#[test]
fn collapsed_table_borders_respect_fragment_slice_info_in_print_pagination() {
  const EPSILON: f32 = 0.1;

  let rows = (0..12)
    .map(|_| "<tr><td></td></tr>")
    .collect::<Vec<_>>()
    .join("");
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 200px 120px; margin: 0; }}
          html, body {{ margin: 0; padding: 0; }}
          table {{ border-collapse: collapse; box-decoration-break: slice; width: 100%; }}
          td {{ border: 2px solid black; height: 30px; padding: 0; }}
        </style>
      </head>
      <body>
        <table>{rows}</table>
      </body>
    </html>
  "#
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 300, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(page_roots.len() > 1, "table should span multiple pages");

  let mut saw_continuation = false;
  for (page_idx, page_root) in page_roots.iter().enumerate() {
    // Page roots can be positioned in a global coordinate space (e.g. stacked vertically). The
    // display list builder assumes the visible viewport is anchored at (0,0), so translate each
    // page to local coordinates before building a page-scoped display list.
    let page_offset = Point::new(-page_root.bounds.origin.x, -page_root.bounds.origin.y);
    let translated_page = page_root.translate(page_offset);

    let (table_fragment, table_rect) = find_table_fragment(&translated_page, Point::ZERO)
      .expect("table fragment with border metadata");
    if table_fragment.slice_info.slice_offset > EPSILON {
      saw_continuation = true;
    }

    let list = DisplayListBuilder::new().build(&translated_page);
    let items = list.items();
    let table_indices: Vec<usize> = items
      .iter()
      .enumerate()
      .filter_map(|(idx, item)| {
        matches!(item, DisplayItem::TableCollapsedBorders(_)).then_some(idx)
      })
      .collect();
    assert_eq!(
      table_indices.len(),
      1,
      "expected exactly one TableCollapsedBorders display item on page {page_idx}"
    );
    let idx = table_indices[0];

    let DisplayItem::TableCollapsedBorders(item) = &items[idx] else {
      unreachable!();
    };

    let info = table_fragment.slice_info;
    let original_block_size = info.original_block_size.max(0.0);
    let slice_offset = info.slice_offset.clamp(0.0, original_block_size);
    let expected_origin = Point::new(table_rect.origin.x, table_rect.origin.y - slice_offset);
    assert_point_eq_eps(item.origin, expected_origin, EPSILON);

    let expected_bounds = table_fragment
      .table_borders
      .as_ref()
      .unwrap()
      .paint_bounds
      .translate(expected_origin);
    assert_rect_eq_eps(item.bounds, expected_bounds, EPSILON);

    assert!(
      idx >= 1 && idx + 1 < items.len(),
      "expected clip items around TableCollapsedBorders on page {page_idx}"
    );
    let DisplayItem::PushClip(clip) = &items[idx - 1] else {
      panic!(
        "expected PushClip immediately before TableCollapsedBorders on page {page_idx}, got {:?}",
        items[idx - 1]
      );
    };
    let rect = match &clip.shape {
      ClipShape::Rect { rect, .. } => *rect,
      other => {
        panic!(
          "expected rect clip immediately before TableCollapsedBorders on page {page_idx}, got {other:?}"
        );
      }
    };
    assert_rect_eq_eps(rect, table_rect, EPSILON);

    assert!(
      matches!(&items[idx + 1], DisplayItem::PopClip),
      "expected PopClip immediately after TableCollapsedBorders on page {page_idx}, got {:?}",
      items[idx + 1]
    );
  }

  assert!(
    saw_continuation,
    "expected at least one continuation page with a non-zero slice_offset"
  );
}
