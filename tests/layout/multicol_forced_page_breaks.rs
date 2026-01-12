//! Multi-column forced page/side break regression tests.

use fastrender::api::{FastRender, LayoutDocumentOptions, PageStacking};
use fastrender::style::media::MediaType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

fn pages<'a>(tree: &'a FragmentTree) -> Vec<&'a FragmentNode> {
  let mut roots = vec![&tree.root];
  roots.extend(tree.additional_fragments.iter());
  roots
}

fn page_document_wrapper<'a>(page: &'a FragmentNode) -> &'a FragmentNode {
  page.children.first().expect("page document wrapper")
}

fn page_content<'a>(page: &'a FragmentNode) -> &'a FragmentNode {
  let wrapper = page_document_wrapper(page);
  wrapper.children.first().unwrap_or(wrapper)
}

fn page_content_start_y(page: &FragmentNode) -> f32 {
  let wrapper = page_document_wrapper(page);
  let content_y = wrapper
    .children
    .first()
    .map(|node| node.bounds.y())
    .unwrap_or(0.0);
  page.bounds.y() + wrapper.bounds.y() + content_y
}

fn find_text_eq<'a>(node: &'a FragmentNode, needle: &str) -> Option<&'a FragmentNode> {
  if let FragmentContent::Text { text, .. } = &node.content {
    if text.as_ref() == needle {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_text_eq(child, needle) {
      return Some(found);
    }
  }
  None
}

fn find_text_position(node: &FragmentNode, needle: &str, origin: (f32, f32)) -> Option<(f32, f32)> {
  let current = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
  if let FragmentContent::Text { text, .. } = &node.content {
    if text.contains(needle) {
      return Some(current);
    }
  }
  for child in node.children.iter() {
    if let Some(pos) = find_text_position(child, needle, current) {
      return Some(pos);
    }
  }
  None
}

#[test]
fn multicol_break_after_page_on_last_child_does_not_add_empty_set() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 140px; margin: 20px; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .blk { height: 20px; margin: 0; }
          #b { break-after: page; }
          .after { height: 10px; margin: 0; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div class="blk" id="a">A</div>
          <div class="blk" id="b">B</div>
        </div>
        <div class="after">C</div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let options = LayoutDocumentOptions::new().with_page_stacking(PageStacking::Untranslated);
  let tree = renderer
    .layout_document_for_media_with_options(&dom, 400, 400, MediaType::Print, options, None)
    .unwrap();
  let page_roots = pages(&tree);

  assert_eq!(
    page_roots.len(),
    2,
    "forced break at the end of multicol content must not create a trailing empty column-set page",
  );

  let page1 = page_roots[0];
  let page2 = page_roots[1];

  let page1_content = page_content(page1);
  assert!(find_text_eq(page1_content, "A").is_some());
  assert!(find_text_eq(page1_content, "B").is_some());
  assert!(find_text_eq(page1_content, "C").is_none());

  let pos_c = find_text_position(page2, "C", (0.0, 0.0)).expect("C should be on page 2");
  assert!(
    pos_c.1 <= page_content_start_y(page2) + 1.0,
    "expected following content to start at the top of page 2 (y≈{}), got y={}",
    page_content_start_y(page2),
    pos_c.1
  );
}

