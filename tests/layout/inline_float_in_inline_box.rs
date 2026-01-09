use fastrender::style::display::Display;
use fastrender::style::float::Float;
use fastrender::tree::box_tree::{CrossOriginAttribute, ImageDecodingAttribute, ReplacedType};
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{
  BoxNode, BoxTree, ComputedStyle, FormattingContextType, LayoutConfig, LayoutEngine, Size,
};
use std::sync::Arc;

fn collect_replaced<'a>(node: &'a FragmentNode, out: &mut Vec<&'a FragmentNode>) {
  if matches!(&node.content, FragmentContent::Replaced { .. }) {
    out.push(node);
  }
  for child in node.children.iter() {
    collect_replaced(child, out);
  }
}

#[test]
fn floated_replaced_inside_inline_box_creates_fragment() {
  // Regression test: a floated replaced element nested inside an inline box (e.g. `<a><img
  // style="float:left"></a>`) must still be placed into the float context and produce a replaced
  // fragment. Previously, the float placeholder stayed nested inside the inline box item and was
  // ignored by float integration, so the image disappeared entirely.

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut span_style = ComputedStyle::default();
  span_style.display = Display::Inline;

  let mut img_style = ComputedStyle::default();
  img_style.display = Display::Inline;
  img_style.float = Float::Left;

  let img = BoxNode::new_replaced(
    Arc::new(img_style),
    ReplacedType::Image {
      src: "float.png".to_string(),
      alt: None,
      decoding: ImageDecodingAttribute::Auto,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      srcset: Vec::new(),
      sizes: None,
      picture_sources: Vec::new(),
    },
    Some(Size::new(30.0, 20.0)),
    Some(1.5),
  );

  let span = BoxNode::new_inline(Arc::new(span_style), vec![img]);

  let text = BoxNode::new_text(Arc::new(ComputedStyle::default()), "after".to_string());

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![span, text],
  );
  let tree = BoxTree::new(root);

  let engine = LayoutEngine::new(LayoutConfig::for_viewport(Size::new(200.0, 100.0)));
  let fragments = engine.layout_tree(&tree).expect("layout");

  let mut replaced = Vec::new();
  collect_replaced(&fragments.root, &mut replaced);
  let float_frag = replaced.iter().find(|node| match &node.content {
    FragmentContent::Replaced {
      replaced_type: ReplacedType::Image { src, .. },
      ..
    } => src == "float.png",
    _ => false,
  });
  let Some(float_frag) = float_frag else {
    let mut found = Vec::new();
    for node in &replaced {
      if let FragmentContent::Replaced { replaced_type, .. } = &node.content {
        found.push((replaced_type.clone(), node.bounds));
      }
    }
    panic!("floated replaced fragment should exist; found={found:?}");
  };
  assert!(
    float_frag.bounds.width() > 0.0 && float_frag.bounds.height() > 0.0,
    "floated replaced fragment should have non-zero size: bounds={:?}",
    float_frag.bounds
  );
}
