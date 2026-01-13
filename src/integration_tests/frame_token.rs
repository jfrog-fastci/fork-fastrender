use crate::debug::runtime::RuntimeToggles;
use crate::tree::box_tree::ReplacedType;
use crate::{FastRender, FontConfig, FragmentContent, FragmentNode, FragmentTree};

fn collect_iframe_frame_tokens(tree: &FragmentTree) -> Vec<Option<u64>> {
  let mut tokens = Vec::new();
  let mut stack: Vec<&FragmentNode> = Vec::new();
  stack.push(&tree.root);
  for extra in &tree.additional_fragments {
    stack.push(extra);
  }

  while let Some(node) = stack.pop() {
    if let FragmentContent::Replaced { replaced_type, .. } = &node.content {
      if let ReplacedType::Iframe { frame_token, .. } = replaced_type {
        tokens.push(*frame_token);
      }
    }

    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  tokens
}

#[test]
fn iframe_frame_tokens_are_stable_and_unique_across_layouts() {
  let mut renderer = FastRender::builder()
    .font_sources(FontConfig::bundled_only())
    // Avoid host `FASTR_*` env vars affecting deterministic id generation.
    .runtime_toggles(RuntimeToggles::default())
    .build()
    .expect("init renderer");

  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; padding: 0; }
      iframe { display: block; border: 0; width: 10px; height: 10px; }
    </style>
    <iframe srcdoc="<p>one</p>"></iframe>
    <iframe srcdoc="<p>two</p>"></iframe>
  "#;

  let dom = renderer.parse_html(html).expect("parse html");

  let tree_first = renderer.layout_document(&dom, 100, 100).expect("layout 1");
  let mut tokens_first = collect_iframe_frame_tokens(&tree_first);
  tokens_first.sort_unstable();

  assert_eq!(tokens_first.len(), 2, "expected two iframe fragments");
  assert!(
    tokens_first.iter().all(|t| t.is_some()),
    "iframe fragments should carry stable frame tokens"
  );
  assert_ne!(
    tokens_first[0], tokens_first[1],
    "iframe frame tokens must be unique within the document"
  );

  let tree_second = renderer.layout_document(&dom, 100, 100).expect("layout 2");
  let mut tokens_second = collect_iframe_frame_tokens(&tree_second);
  tokens_second.sort_unstable();

  assert_eq!(
    tokens_first, tokens_second,
    "re-layout with unchanged DOM should preserve iframe frame tokens"
  );
}

