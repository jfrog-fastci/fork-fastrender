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
fn grid_item_forced_break_after_propagates_to_row_boundary_in_paged_media() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 100px 100px; margin: 0; }
          body { margin: 0; }
          .grid {
            display: grid;
            grid-template-rows: 30px 30px;
            grid-template-columns: 1fr;
            align-items: start;
          }
          .row1 {
            height: 10px;
            break-after: page;
            background: rgb(255, 0, 0);
          }
          .row2 {
            height: 10px;
            background: rgb(0, 0, 255);
          }
        </style>
      </head>
      <body>
        <div class="grid">
          <div class="row1"></div>
          <div class="row2"></div>
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

  assert_eq!(page_roots.len(), 2, "expected forced break to create two pages");

  let red = Rgba::rgb(255, 0, 0);
  let blue = Rgba::rgb(0, 0, 255);

  assert!(
    find_fragment_by_background(page_roots[0], (0.0, 0.0), red).is_some(),
    "row 1 item should appear on the first page"
  );
  assert!(
    find_fragment_by_background(page_roots[0], (0.0, 0.0), blue).is_none(),
    "row 2 item should be pushed to the second page"
  );

  let blue_pos =
    find_fragment_by_background(page_roots[1], (0.0, 0.0), blue).expect("row 2 item on page 2");
  assert!(
    blue_pos.1 < 1.0,
    "expected break to occur at the row boundary (~30px), placing row 2 at the top of page 2; got y={}",
    blue_pos.1
  );
}
