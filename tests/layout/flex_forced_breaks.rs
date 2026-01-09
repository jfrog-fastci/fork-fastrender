use fastrender::api::{FastRender, LayoutDocumentOptions, PageStacking};
use fastrender::style::media::MediaType;
use fastrender::tree::fragment_tree::{FragmentNode, FragmentTree};
use fastrender::Rgba;

fn pages<'a>(tree: &'a FragmentTree) -> Vec<&'a FragmentNode> {
  let mut roots = vec![&tree.root];
  roots.extend(tree.additional_fragments.iter());
  roots
}

fn find_fragment_by_background(
  node: &FragmentNode,
  origin: (f32, f32),
  color: Rgba,
) -> Option<(f32, f32)> {
  let abs_x = origin.0 + node.bounds.x();
  let abs_y = origin.1 + node.bounds.y();
  if node
    .style
    .as_ref()
    .is_some_and(|style| style.background_color == color)
  {
    return Some((abs_x, abs_y));
  }
  for child in node.children.iter() {
    if let Some(found) = find_fragment_by_background(child, (abs_x, abs_y), color) {
      return Some(found);
    }
  }
  None
}

#[test]
fn flex_item_forced_break_after_right_propagates_to_line_boundary_and_inserts_blank_page() {
  // Regression: for row flex containers, forced page breaks authored on flex items propagate to
  // the flex *line* boundary (not the individual item's border box). Page-side constraints like
  // `break-after:right` must be attached to that line boundary so pagination can insert a blank
  // page when needed.
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 100px 100px; margin: 0; }
          body { margin: 0; }
          .flex {
            display: flex;
            flex-wrap: wrap;
            width: 100px;
            align-content: flex-start;
            align-items: flex-start;
          }
          .a {
            width: 50px;
            height: 10px;
            break-after: right;
            background: rgb(255, 0, 0);
          }
          .b {
            width: 50px;
            height: 30px;
            background: rgb(0, 255, 0);
          }
          .c {
            width: 100px;
            height: 10px;
            background: rgb(0, 0, 255);
          }
        </style>
      </head>
      <body>
        <div class="flex">
          <div class="a"></div>
          <div class="b"></div>
          <div class="c"></div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 200, 200, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    3,
    "expected a blank page to be inserted to satisfy break-after:right",
  );

  let red = Rgba::rgb(255, 0, 0);
  let green = Rgba::rgb(0, 255, 0);
  let blue = Rgba::rgb(0, 0, 255);

  // Page 1 contains the first flex line (both items).
  assert!(find_fragment_by_background(page_roots[0], (0.0, 0.0), red).is_some());
  assert!(find_fragment_by_background(page_roots[0], (0.0, 0.0), green).is_some());
  assert!(find_fragment_by_background(page_roots[0], (0.0, 0.0), blue).is_none());

  // Page 2 is blank (inserted to satisfy the right-side requirement for the next page).
  assert!(find_fragment_by_background(page_roots[1], (0.0, 0.0), red).is_none());
  assert!(find_fragment_by_background(page_roots[1], (0.0, 0.0), green).is_none());
  assert!(find_fragment_by_background(page_roots[1], (0.0, 0.0), blue).is_none());

  // Page 3 contains the second flex line, rebased to the top of the page.
  assert!(find_fragment_by_background(page_roots[2], (0.0, 0.0), red).is_none());
  assert!(find_fragment_by_background(page_roots[2], (0.0, 0.0), green).is_none());
  let blue_pos =
    find_fragment_by_background(page_roots[2], (0.0, 0.0), blue).expect("blue item on page 3");
  assert!(
    blue_pos.1 < 1.0,
    "expected the second flex line to appear at the top of page 3; got y={}",
    blue_pos.1
  );
}

#[test]
fn flex_item_forced_break_after_does_not_create_gap_only_pages() {
  // Regression: similar to the grid `row-gap` case, forced breaks that are propagated to the start
  // of the next flex line (after the row-gap) can create a page containing only the gap when the
  // fragmentainer size ends exactly at the previous line boundary. Breaks should align to the end
  // edge of the preceding line band instead.
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 100px 30px; margin: 0; }
          body { margin: 0; }
          .flex {
            display: flex;
            flex-wrap: wrap;
            width: 100px;
            row-gap: 10px;
            align-content: flex-start;
            align-items: flex-start;
          }
          .a {
            width: 50px;
            height: 10px;
            break-after: page;
            background: rgb(255, 0, 0);
          }
          .b {
            width: 50px;
            height: 30px;
            background: rgb(0, 255, 0);
          }
          .c {
            width: 100px;
            height: 20px;
            background: rgb(0, 0, 255);
          }
        </style>
      </head>
      <body>
        <div class="flex">
          <div class="a"></div>
          <div class="b"></div>
          <div class="c"></div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 200, 200, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "expected the forced break to land at the first line boundary (before the row-gap) without inserting a gap-only page",
  );

  let blue = Rgba::rgb(0, 0, 255);
  let blue_pos =
    find_fragment_by_background(page_roots[1], (0.0, 0.0), blue).expect("blue item on page 2");
  assert!(
    (blue_pos.1 - 10.0).abs() < 1.0,
    "expected the second line to appear after the 10px row-gap at the start of page 2; got y={}",
    blue_pos.1
  );
}

