//! Paged media / pagination regression tests.
//!
//! These tests run under the unified integration harness (`tests/integration.rs`). To run only
//! this module, use the standard Rust test filter:
//!
//! ```bash
//! cargo test --test integration paged_media
//! ```

use fastrender::api::FastRender;
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
  page_document_wrapper(page)
    .children
    .first()
    .expect("page content")
}

fn find_text<'a>(node: &'a FragmentNode, needle: &str) -> Option<&'a FragmentNode> {
  if let FragmentContent::Text { text, .. } = &node.content {
    if text.contains(needle) {
      return Some(node);
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_text(child, needle) {
      return Some(found);
    }
  }
  None
}

#[test]
fn multicol_fragmentainer_path_column_metadata_survives_pagination() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 200px; margin: 0; }
          body { margin: 0; }
          .multi { column-count: 2; column-gap: 0; }
          .right { break-before: column; }
        </style>
      </head>
      <body>
        <div class="multi">
          <div>Left</div>
          <div class="right">Right</div>
        </div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();

  let page_roots = pages(&tree);
  assert_eq!(page_roots.len(), 1);

  let page = page_roots[0];
  let left = find_text(page_content(page), "Left").expect("left text fragment");
  let right = find_text(page_content(page), "Right").expect("right text fragment");

  assert_eq!(left.fragmentainer.page_index, 0);
  assert_eq!(right.fragmentainer.page_index, 0);

  assert_eq!(left.fragmentainer.column_set_index, Some(0));
  assert_eq!(left.fragmentainer.column_index, Some(0));
  assert_eq!(right.fragmentainer.column_set_index, Some(0));
  assert_eq!(right.fragmentainer.column_index, Some(1));
}

#[test]
fn footnote_policy_auto_defers_body_without_moving_call() {
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 40px; margin: 0; }
          body { margin: 0; font-size: 10px; line-height: 10px; }
          .line { height: 20px; }
          .note { float: footnote; footnote-policy: auto; display: inline-block; height: 20px; }
        </style>
      </head>
      <body>
        <div class="line">Alpha</div>
        <div class="line">Beta <span class="note">Footnote body</span></div>
      </body>
    </html>
  "#;

  let mut renderer = FastRender::new().unwrap();
  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 40, MediaType::Print)
    .unwrap();

  let page_roots = pages(&tree);
  assert_eq!(page_roots.len(), 2, "expected a deferred footnote to create a second page");

  let page1 = page_roots[0];
  let wrapper1 = page_document_wrapper(page1);
  assert_eq!(wrapper1.children.len(), 1, "page 1 should have no footnote area");
  assert!(find_text(page_content(page1), "1").is_some(), "call marker should stay on page 1");

  let page2 = page_roots[1];
  let wrapper2 = page_document_wrapper(page2);
  assert_eq!(wrapper2.children.len(), 2, "page 2 should have a footnote area");
  let footnote_area2 = wrapper2.children.get(1).expect("page 2 footnote area");
  assert!(
    find_text(footnote_area2, "Footnote body").is_some(),
    "deferred footnote body should be placed on page 2"
  );
  assert!(
    find_text(page_content(page2), "1").is_none(),
    "call marker must not be moved to page 2"
  );
}
