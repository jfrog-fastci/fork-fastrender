//! Dedicated test target for focused table fragmentation regressions.
//!
//! The full integration suite is linked via `tests/integration.rs`. This target exists so automation
//! can validate table header repetition / pagination behavior without executing the entire
//! integration harness.

use fastrender::api::FastRender;
use fastrender::style::media::MediaType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

fn pages<'a>(tree: &'a FragmentTree) -> Vec<&'a FragmentNode> {
  let mut roots = vec![&tree.root];
  roots.extend(tree.additional_fragments.iter());
  roots
}

fn contains_text(node: &FragmentNode, needle: &str) -> bool {
  if let FragmentContent::Text { text, .. } = &node.content {
    if text.contains(needle) {
      return true;
    }
  }
  node.children.iter().any(|child| contains_text(child, needle))
}

#[test]
fn table_headers_repeat_across_pages() {
  let mut renderer = FastRender::new().unwrap();
  let html = r#"
    <html>
      <head>
        <style>
          @page { size: 200px 60px; margin: 0; }
          body { margin: 0; }
          table { border-collapse: collapse; width: 200px; }
          td { height: 20px; }
        </style>
      </head>
      <body>
        <table>
          <thead>
            <tr><td>Header</td></tr>
          </thead>
          <tbody>
            <tr><td>Row 1</td></tr>
            <tr><td>Row 2</td></tr>
            <tr><td>Row 3</td></tr>
            <tr><td>Row 4</td></tr>
            <tr><td>Row 5</td></tr>
            <tr><td>Row 6</td></tr>
            <tr><td>Row 7</td></tr>
            <tr><td>Row 8</td></tr>
            <tr><td>Row 9</td></tr>
            <tr><td>Row 10</td></tr>
          </tbody>
        </table>
      </body>
    </html>
  "#;

  let dom = renderer.parse_html(html).unwrap();
  let tree = renderer
    .layout_document_for_media(&dom, 200, 200, MediaType::Print)
    .unwrap();
  let page_roots = pages(&tree);

  assert!(
    page_roots.len() > 1,
    "expected table to fragment across multiple pages"
  );
  assert!(
    page_roots.len() <= 10,
    "expected reasonable page count; got {}",
    page_roots.len()
  );

  for (idx, page) in page_roots.iter().enumerate() {
    assert!(
      contains_text(page, "Header"),
      "expected repeated header on page {idx}"
    );
  }
}

