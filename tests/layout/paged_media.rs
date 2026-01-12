//! Paged media / pagination regression tests.

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
