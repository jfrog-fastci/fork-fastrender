use crate::api::FastRender;
use crate::geometry::{Point, Rect};
use crate::paint::display_list::ClipShape;
use crate::paint::display_list::DisplayItem;
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::style::media::MediaType;
use crate::tree::fragment_tree::FragmentTree;
use crate::tree::fragment_tree::{FragmentNode, TableCollapsedBorders};

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

fn collect_table_fragments<'a>(
  node: &'a FragmentNode,
  offset: Point,
  out: &mut Vec<(&'a FragmentNode, Rect)>,
) {
  let abs_rect = node.bounds.translate(offset);
  if node.table_borders.is_some() {
    out.push((node, abs_rect));
  }
  for child in node.children.iter() {
    collect_table_fragments(child, abs_rect.origin, out);
  }
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

#[test]
fn collapsed_table_borders_use_fragment_local_origin_with_repeated_thead_in_print_pagination() {
  const EPSILON: f32 = 0.1;

  let body_rows = (0..12)
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
          th, td {{ border: 2px solid black; height: 30px; padding: 0; }}
        </style>
      </head>
      <body>
        <table>
          <thead><tr><th></th></tr></thead>
          <tbody>{body_rows}</tbody>
        </table>
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
    let page_offset = Point::new(-page_root.bounds.origin.x, -page_root.bounds.origin.y);
    let translated_page = page_root.translate(page_offset);
    let (table_fragment, table_rect) = find_table_fragment(&translated_page, Point::ZERO)
      .expect("table fragment with border metadata");

    if table_fragment.slice_info.slice_offset <= EPSILON {
      continue;
    }
    saw_continuation = true;

    let list = DisplayListBuilder::new().build(&translated_page);
    let items = list.items();
    let table_items: Vec<_> = items
      .iter()
      .filter_map(|item| match item {
        DisplayItem::TableCollapsedBorders(item) => Some(item),
        _ => None,
      })
      .collect();
    assert_eq!(
      table_items.len(),
      1,
      "expected exactly one TableCollapsedBorders display item on continuation page {page_idx}"
    );
    let item = table_items[0];
    assert_point_eq_eps(item.origin, table_rect.origin, EPSILON);
    assert!(
      item.borders.fragment_local,
      "expected fragment-local borders on continuation page {page_idx}"
    );
  }

  assert!(
    saw_continuation,
    "expected at least one continuation page with a non-zero slice_offset"
  );
}

#[test]
fn collapsed_table_borders_include_outer_edge_spill_with_repeated_thead() {
  // When table header rows are repeated across pages (`display: table-header-group` / `<thead>`),
  // fragmentation builds a derived `TableCollapsedBorders` for each continuation fragment (see
  // `layout::fragmentation::inject_table_headers_and_footers`).
  //
  // This derived border set must compute `paint_bounds` from the *actual* outer-edge segments
  // included in the slice. Otherwise, a later row with a thicker outer border can be culled/clipped
  // when the continuation fragment is painted (CSS 2.1 §17.6.2; WPT `border-collapse-basic-001`).
  const EPSILON: f32 = 0.1;

  let body_rows = (0..12)
    .map(|idx| {
      if idx == 8 {
        r#"<tr><td class="thick"></td></tr>"#.to_string()
      } else {
        "<tr><td></td></tr>".to_string()
      }
    })
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
          th, td {{
            height: 30px;
            padding: 0;
            border-left: 2px solid black;
            /* Ensure corner joins don't capture the thick outer segment. */
            border-top: 2px hidden black;
            border-bottom: 2px hidden black;
            border-right: 2px hidden black;
          }}
          td.thick {{ border-left-width: 20px; border-left-style: solid; border-left-color: black; }}
        </style>
      </head>
      <body>
        <table>
          <thead><tr><th></th></tr></thead>
          <tbody>{body_rows}</tbody>
        </table>
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

  let mut saw_thick_spill = false;
  for page_root in page_roots {
    let page_offset = Point::new(-page_root.bounds.origin.x, -page_root.bounds.origin.y);
    let translated_page = page_root.translate(page_offset);
    let (table_fragment, _) =
      find_table_fragment(&translated_page, Point::ZERO).expect("table fragment");

    if table_fragment.slice_info.slice_offset <= EPSILON {
      continue;
    }

    let borders = table_fragment
      .table_borders
      .as_deref()
      .expect("expected collapsed border metadata");
    assert!(
      borders.fragment_local,
      "expected fragment-local borders on continuation pages"
    );

    let has_thick_left = (0..borders.row_count).any(|row| {
      borders
        .vertical_segment(0, row)
        .is_some_and(|seg| seg.is_visible() && seg.width >= 19.9)
    });
    if !has_thick_left {
      continue;
    }

    saw_thick_spill = true;
    assert!(
      borders.paint_bounds.min_x() <= -17.9,
      "expected paint bounds to include thick outer spill on continuation fragment (min_x={})",
      borders.paint_bounds.min_x()
    );
  }

  assert!(
    saw_thick_spill,
    "expected to find a continuation fragment containing the thick outer border row"
  );
}

#[test]
fn collapsed_table_borders_use_fragment_local_origin_with_repeated_tfoot_in_print_pagination() {
  const EPSILON: f32 = 0.1;

  let body_rows = (0..22)
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
          th, td {{ border: 2px solid black; height: 30px; padding: 0; }}
        </style>
      </head>
      <body>
        <table>
          <tbody>{body_rows}</tbody>
          <tfoot><tr><td></td></tr></tfoot>
        </table>
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

  assert!(
    page_roots.len() > 2,
    "table should span at least three pages"
  );

  let mut saw_target_page = false;
  for (page_idx, page_root) in page_roots.iter().enumerate() {
    let page_offset = Point::new(-page_root.bounds.origin.x, -page_root.bounds.origin.y);
    let translated_page = page_root.translate(page_offset);
    let (table_fragment, table_rect) = find_table_fragment(&translated_page, Point::ZERO)
      .expect("table fragment with border metadata");

    if table_fragment.slice_info.slice_offset <= EPSILON || table_fragment.slice_info.is_last {
      continue;
    }
    saw_target_page = true;

    let list = DisplayListBuilder::new().build(&translated_page);
    let items = list.items();
    let table_items: Vec<_> = items
      .iter()
      .filter_map(|item| match item {
        DisplayItem::TableCollapsedBorders(item) => Some(item),
        _ => None,
      })
      .collect();
    assert_eq!(
      table_items.len(),
      1,
      "expected exactly one TableCollapsedBorders display item on page {page_idx}"
    );
    let item = table_items[0];
    assert_point_eq_eps(item.origin, table_rect.origin, EPSILON);
    assert!(
      item.borders.fragment_local,
      "expected fragment-local borders on page {page_idx}"
    );
  }

  assert!(
    saw_target_page,
    "expected at least one continuation page that is not the last table fragment"
  );
}

#[test]
fn collapsed_table_borders_align_with_repeated_headers_in_multicol() {
  const EPSILON: f32 = 0.1;

  let body_rows: String = (1..=16)
    .map(|i| format!(r#"<tr><td>{i}</td></tr>"#))
    .collect();
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          html, body {{ margin: 0; padding: 0; }}
          div.columns {{
            column-count: 2;
            column-gap: 20px;
            width: 260px;
          }}
          table {{ width: 100%; border-collapse: collapse; box-decoration-break: slice; }}
          td, th {{ border: 2px solid black; height: 32px; padding: 0; }}
        </style>
      </head>
      <body>
        <div class="columns">
          <table>
            <thead><tr><th>Header</th></tr></thead>
            <tbody>{body_rows}</tbody>
          </table>
        </div>
      </body>
    </html>
  "#
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer.layout_document(&dom, 320, 400).unwrap();

  let mut table_fragments = Vec::new();
  collect_table_fragments(&tree.root, Point::ZERO, &mut table_fragments);
  assert!(
    !table_fragments.is_empty(),
    "expected to find table fragments with collapsed border metadata"
  );

  let continuation_fragments: Vec<_> = table_fragments
    .iter()
    .filter(|(node, _)| node.slice_info.slice_offset > EPSILON)
    .collect();
  assert!(
    !continuation_fragments.is_empty(),
    "expected at least one continuation column fragment (table fragments={:?})",
    table_fragments
      .iter()
      .map(|(node, rect)| (rect.origin.x, rect.origin.y, node.slice_info.slice_offset))
      .collect::<Vec<_>>()
  );

  // Disable viewport-based culling so we also validate offscreen column sets.
  let list = DisplayListBuilder::new()
    .with_culling_viewport_size(10_000.0, 10_000.0)
    .build(&tree.root);
  let collapsed_items: Vec<_> = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::TableCollapsedBorders(item) => Some(item),
      _ => None,
    })
    .collect();
  assert!(
    collapsed_items.len() >= 2,
    "expected the table to fragment into multiple column slices (got {} TableCollapsedBorders items)",
    collapsed_items.len()
  );

  for (table_fragment, table_rect) in continuation_fragments {
    let origin = table_rect.origin;
    let found = collapsed_items.iter().any(|item| {
      item.borders.fragment_local
        && (item.origin.x - origin.x).abs() < EPSILON
        && (item.origin.y - origin.y).abs() < EPSILON
    });
    assert!(
      found,
      "expected TableCollapsedBorders item origin to match continuation fragment origin ({}, {}) \
       (slice_offset={}); got origins={:?}",
      origin.x,
      origin.y,
      table_fragment.slice_info.slice_offset,
      collapsed_items
        .iter()
        .map(|item| (item.origin.x, item.origin.y))
        .collect::<Vec<_>>()
    );
  }
}

#[test]
fn collapsed_table_borders_align_with_repeated_footers_in_multicol() {
  const EPSILON: f32 = 0.1;

  let body_rows: String = (1..=48)
    .map(|i| format!(r#"<tr><td>{i}</td></tr>"#))
    .collect();
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          html, body {{ margin: 0; padding: 0; }}
          div.columns {{
            column-count: 2;
            column-gap: 20px;
            width: 260px;
            height: 160px;
          }}
          table {{ width: 100%; border-collapse: collapse; box-decoration-break: slice; }}
          td, th {{ border: 2px solid black; height: 32px; padding: 0; }}
        </style>
      </head>
      <body>
        <div class="columns">
          <table>
            <tbody>{body_rows}</tbody>
            <tfoot><tr><td>Footer</td></tr></tfoot>
          </table>
        </div>
      </body>
    </html>
  "#
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer.layout_document(&dom, 320, 400).unwrap();

  let mut table_fragments = Vec::new();
  collect_table_fragments(&tree.root, Point::ZERO, &mut table_fragments);
  assert!(
    !table_fragments.is_empty(),
    "expected to find table fragments with collapsed border metadata"
  );

  let continuation_non_last_fragments: Vec<_> = table_fragments
    .iter()
    .filter(|(node, _)| node.slice_info.slice_offset > EPSILON && !node.slice_info.is_last)
    .collect();
  assert!(
    !continuation_non_last_fragments.is_empty(),
    "expected at least one continuation column fragment that is not last \
     (table fragments={:?})",
    table_fragments
      .iter()
      .map(|(node, rect)| {
        (
          rect.origin.x,
          rect.origin.y,
          node.slice_info.slice_offset,
          node.slice_info.is_last,
        )
      })
      .collect::<Vec<_>>()
  );

  // Disable viewport-based culling so we also validate offscreen column sets.
  let list = DisplayListBuilder::new()
    .with_culling_viewport_size(10_000.0, 10_000.0)
    .build(&tree.root);
  let collapsed_items: Vec<_> = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::TableCollapsedBorders(item) => Some(item),
      _ => None,
    })
    .collect();
  assert!(
    collapsed_items.len() >= 2,
    "expected the table to fragment into multiple column slices (got {} TableCollapsedBorders items)",
    collapsed_items.len()
  );

  for (table_fragment, table_rect) in continuation_non_last_fragments {
    let origin = table_rect.origin;
    let found = collapsed_items.iter().any(|item| {
      item.borders.fragment_local
        && (item.origin.x - origin.x).abs() < EPSILON
        && (item.origin.y - origin.y).abs() < EPSILON
    });
    assert!(
      found,
      "expected TableCollapsedBorders item origin to match continuation fragment origin ({}, {}) \
       (slice_offset={}, is_last={}); got origins={:?}",
      origin.x,
      origin.y,
      table_fragment.slice_info.slice_offset,
      table_fragment.slice_info.is_last,
      collapsed_items
        .iter()
        .map(|item| (item.origin.x, item.origin.y))
        .collect::<Vec<_>>()
    );
  }
}

#[test]
fn collapsed_table_borders_align_with_repeated_headers_and_footers_in_multicol() {
  const EPSILON: f32 = 0.1;

  let body_rows: String = (1..=48)
    .map(|i| format!(r#"<tr><td>{i}</td></tr>"#))
    .collect();
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          html, body {{ margin: 0; padding: 0; }}
          div.columns {{
            column-count: 2;
            column-gap: 20px;
            width: 260px;
            height: 160px;
          }}
          table {{ width: 100%; border-collapse: collapse; box-decoration-break: slice; }}
          td, th {{ border: 2px solid black; height: 32px; padding: 0; }}
        </style>
      </head>
      <body>
        <div class="columns">
          <table>
            <thead><tr><th>Header</th></tr></thead>
            <tbody>{body_rows}</tbody>
            <tfoot><tr><td>Footer</td></tr></tfoot>
          </table>
        </div>
      </body>
    </html>
  "#
  );

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(&html).unwrap();
  let tree = renderer.layout_document(&dom, 320, 400).unwrap();

  let mut table_fragments = Vec::new();
  collect_table_fragments(&tree.root, Point::ZERO, &mut table_fragments);
  assert!(
    !table_fragments.is_empty(),
    "expected to find table fragments with collapsed border metadata"
  );

  let middle_fragments: Vec<_> = table_fragments
    .iter()
    .filter(|(node, _)| {
      !node.slice_info.is_first
        && !node.slice_info.is_last
        && node.slice_info.slice_offset > EPSILON
    })
    .collect();
  assert!(
    !middle_fragments.is_empty(),
    "expected at least one table fragment that is neither first nor last and intersects the viewport \
     (table fragments={:?})",
    table_fragments
      .iter()
      .map(|(node, rect)| {
        (
          rect.origin.x,
          rect.origin.y,
          node.slice_info.slice_offset,
          node.slice_info.is_first,
          node.slice_info.is_last
        )
      })
      .collect::<Vec<_>>()
  );

  // Disable viewport-based culling so we also validate offscreen column sets.
  let list = DisplayListBuilder::new()
    .with_culling_viewport_size(10_000.0, 10_000.0)
    .build(&tree.root);
  let collapsed_items: Vec<_> = list
    .items()
    .iter()
    .filter_map(|item| match item {
      DisplayItem::TableCollapsedBorders(item) => Some(item),
      _ => None,
    })
    .collect();
  assert!(
    collapsed_items.len() >= 2,
    "expected the table to fragment into multiple column slices (got {} TableCollapsedBorders items)",
    collapsed_items.len()
  );

  for (table_fragment, table_rect) in middle_fragments {
    let origin = table_rect.origin;
    let found = collapsed_items.iter().any(|item| {
      item.borders.fragment_local
        && (item.origin.x - origin.x).abs() < EPSILON
        && (item.origin.y - origin.y).abs() < EPSILON
    });
    assert!(
      found,
      "expected TableCollapsedBorders item origin to match middle fragment origin ({}, {}) \
       (slice_offset={}, is_first={}, is_last={}); got origins={:?}",
      origin.x,
      origin.y,
      table_fragment.slice_info.slice_offset,
      table_fragment.slice_info.is_first,
      table_fragment.slice_info.is_last,
      collapsed_items
        .iter()
        .map(|item| (item.origin.x, item.origin.y))
        .collect::<Vec<_>>()
    );
  }
}

#[test]
fn collapsed_table_borders_keep_horizontal_edges_visible_with_repeated_headers() {
  const EPSILON: f32 = 0.1;

  let body_rows = (0..6)
    .map(|_| "<tr><td></td></tr>")
    .collect::<Vec<_>>()
    .join("");
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 200px 80px; margin: 0; }}
          html, body {{ margin: 0; padding: 0; }}
          table {{ border-collapse: collapse; width: 100%; }}
          th, td {{ border: 2px solid black; height: 30px; padding: 0; }}
        </style>
      </head>
      <body>
        <table>
          <thead><tr><th>H</th></tr></thead>
          <tbody>{body_rows}</tbody>
        </table>
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
  assert!(
    page_roots.len() >= 3,
    "table should span at least three pages to cover an intermediate continuation fragment"
  );

  let mut saw_intermediate = false;
  for (page_idx, page_root) in page_roots.iter().enumerate() {
    let page_offset = Point::new(-page_root.bounds.origin.x, -page_root.bounds.origin.y);
    let translated_page = page_root.translate(page_offset);

    let (table_fragment, _) = find_table_fragment(&translated_page, Point::ZERO)
      .expect("table fragment with border metadata");
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
    let DisplayItem::TableCollapsedBorders(item) = &items[table_indices[0]] else {
      unreachable!();
    };

    if table_fragment.slice_info.slice_offset > EPSILON && !table_fragment.slice_info.is_last {
      saw_intermediate = true;
      assert!(
        item
          .borders
          .horizontal_segment(0, 0)
          .expect("expected top horizontal segment")
          .is_visible(),
        "expected top border line to be visible on page {page_idx}"
      );
      assert!(
        item
          .borders
          .horizontal_segment(item.borders.row_count, 0)
          .expect("expected bottom horizontal segment")
          .is_visible(),
        "expected bottom border line to be visible on page {page_idx}"
      );
    }
  }

  assert!(
    saw_intermediate,
    "expected to find an intermediate continuation fragment with a non-zero slice_offset"
  );
}

#[test]
fn collapsed_table_borders_keep_horizontal_edges_visible_with_repeated_footers() {
  const EPSILON: f32 = 0.1;

  let body_rows = (0..6)
    .map(|_| "<tr><td></td></tr>")
    .collect::<Vec<_>>()
    .join("");
  let html = format!(
    r#"
    <html>
      <head>
        <style>
          @page {{ size: 200px 80px; margin: 0; }}
          html, body {{ margin: 0; padding: 0; }}
          table {{ border-collapse: collapse; width: 100%; }}
          td {{ border: 2px solid black; height: 30px; padding: 0; }}
        </style>
      </head>
      <body>
        <table>
          <tbody>{body_rows}</tbody>
          <tfoot><tr><td>F</td></tr></tfoot>
        </table>
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
  assert!(
    page_roots.len() >= 3,
    "table should span at least three pages to cover an intermediate continuation fragment"
  );

  let mut saw_intermediate = false;
  for (page_idx, page_root) in page_roots.iter().enumerate() {
    let page_offset = Point::new(-page_root.bounds.origin.x, -page_root.bounds.origin.y);
    let translated_page = page_root.translate(page_offset);

    let (table_fragment, _) = find_table_fragment(&translated_page, Point::ZERO)
      .expect("table fragment with border metadata");
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
    let DisplayItem::TableCollapsedBorders(item) = &items[table_indices[0]] else {
      unreachable!();
    };

    if table_fragment.slice_info.slice_offset > EPSILON && !table_fragment.slice_info.is_last {
      saw_intermediate = true;
      assert!(
        item
          .borders
          .horizontal_segment(0, 0)
          .expect("expected top horizontal segment")
          .is_visible(),
        "expected top border line to be visible on page {page_idx}"
      );
      assert!(
        item
          .borders
          .horizontal_segment(item.borders.row_count, 0)
          .expect("expected bottom horizontal segment")
          .is_visible(),
        "expected bottom border line to be visible on page {page_idx}"
      );
    }
  }

  assert!(
    saw_intermediate,
    "expected to find an intermediate continuation fragment with a non-zero slice_offset"
  );
}

#[test]
fn collapsed_table_outer_border_spills_into_margin() {
  // CSS 2.1 §17.6.2: later rows/columns with thicker *outer* collapsed border winners must not
  // widen the table's layout box; the excess must spill outward into the margin area.
  //
  // Our collapsed-border coordinate convention aligns the table fragment origin (`x=0`) with the
  // baseline outer border paint edge. The baseline border therefore paints fully inside the table
  // fragment bounds, and `paint_bounds` goes negative only when a thicker outer-edge winner spills
  // outward beyond that baseline (WPT `border-collapse-basic-001`).
  const EPSILON: f32 = 0.1;

  let html = r#"
    <html>
      <head>
        <style>
          html, body { margin: 0; padding: 0; }
          table { border-collapse: collapse; border: 0; margin: 0; padding: 0; }
          td { width: 40px; height: 40px; padding: 0; border: 0; }
          td.thin { border-left: 2px solid black; }
          td.thick { border-left: 20px solid black; }
        </style>
      </head>
      <body>
        <table>
          <tr><td class="thin"></td></tr>
          <tr><td class="thick"></td></tr>
        </table>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer.layout_document(&dom, 200, 200).unwrap();

  let (table_fragment, table_rect) =
    find_table_fragment(&tree.root, Point::ZERO).expect("table fragment with border metadata");
  let borders = table_fragment
    .table_borders
    .as_deref()
    .expect("expected collapsed border metadata");

  // 1) Baseline outer-left border width comes from the first row.
  assert!(
    (borders.vertical_line_base.first().copied().unwrap_or(0.0) - 2.0).abs() < EPSILON,
    "expected baseline outer-left width of 2px, got {:?}",
    borders.vertical_line_base.first()
  );

  // 2) Later rows with thicker outer borders should spill outward by the *excess width* beyond the
  // baseline edge. The inside extent is clamped to the baseline half-width, so any additional
  // thickness is painted entirely on the outside (CSS 2.1 §17.6.2).
  let baseline_width = borders.vertical_line_width(0);
  let baseline_half = baseline_width * 0.5;
  let baseline_outer_edge = borders
    .column_line_positions
    .first()
    .copied()
    .unwrap_or(0.0)
    - baseline_half;
  let expected_spill = 20.0 - baseline_width; // 18px
  let expected_min_x = baseline_outer_edge - expected_spill;
  assert!(
    (borders.paint_bounds.min_x() - expected_min_x).abs() < EPSILON,
    "expected paint_bounds.min_x to reflect a {}px spill beyond the baseline outer edge \
     (baseline_outer_edge={:.2}, expected_min_x={:.2}, got min_x={:.2})",
    expected_spill,
    baseline_outer_edge,
    expected_min_x,
    borders.paint_bounds.min_x()
  );

  // 3) The table fragment's own bounds should stay anchored at the baseline edge (x=0); only the
  // paint bounds should extend into negative coordinates.
  assert!(
    table_fragment.bounds.min_x().abs() < EPSILON,
    "expected table fragment bounds.min_x to remain 0, got {} (absolute min_x={})",
    table_fragment.bounds.min_x(),
    table_rect.min_x()
  );
}
