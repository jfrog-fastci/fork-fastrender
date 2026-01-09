use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::InsetValue;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::CrossOriginAttribute;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::box_tree::SrcsetCandidate;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::Size;
use std::sync::Arc;

fn find_fragment_by_box_id<'a>(fragment: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    let matches_id = match &node.content {
      FragmentContent::Block { box_id: Some(id) }
      | FragmentContent::Inline { box_id: Some(id), .. }
      | FragmentContent::Text { box_id: Some(id), .. }
      | FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
      _ => false,
    };
    if matches_id {
      return Some(node);
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn abspos_descendant_inside_inline_wrapper_uses_positioned_ancestor_containing_block() {
  // Regression test for absolute positioning inside inline content.
  //
  // Pattern:
  //   positioned block (definite size)
  //     inline wrapper (e.g. <a>)
  //       abspos "fill" child (e.g. <img style="position:absolute; inset:0">)
  //
  // The abspos child is out-of-flow and its containing block is the nearest positioned ancestor
  // (CSS 2.1 §10.1). It must *not* accidentally use the anonymous inline container that block
  // layout creates for laying out runs of inline-level children, which typically has the height
  // of a single line box (~line-height).
  let mut root_style = ComputedStyle::default();
  root_style.width = Some(Length::px(400.0));

  let mut positioned_style = ComputedStyle::default();
  positioned_style.position = Position::Relative;
  positioned_style.width = Some(Length::px(200.0));
  positioned_style.height = Some(Length::px(100.0));

  let mut link_style = ComputedStyle::default();
  link_style.position = Position::Static;

  let mut img_style = ComputedStyle::default();
  img_style.position = Position::Absolute;
  img_style.width = Some(Length::percent(100.0));
  img_style.top = InsetValue::Length(Length::px(0.0));
  img_style.right = InsetValue::Length(Length::px(0.0));
  img_style.bottom = InsetValue::Length(Length::px(0.0));
  img_style.left = InsetValue::Length(Length::px(0.0));

  let mut img = BoxNode::new_replaced(
    Arc::new(img_style),
    ReplacedType::Image {
      src: String::new(),
      alt: None,
      crossorigin: CrossOriginAttribute::None,
      referrer_policy: None,
      srcset: Vec::<SrcsetCandidate>::new(),
      sizes: None,
      picture_sources: Vec::new(),
    },
    Some(Size::new(50.0, 20.0)),
    None,
  );
  img.id = 3;

  let mut link = BoxNode::new_inline(Arc::new(link_style), vec![img]);
  link.id = 2;

  let mut positioned =
    BoxNode::new_block(Arc::new(positioned_style), FormattingContextType::Block, vec![link]);
  positioned.id = 1;

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![positioned],
  );

  let constraints = LayoutConstraints::definite(400.0, 300.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("block layout");

  let img_fragment = find_fragment_by_box_id(&fragment, 3).expect("image fragment should exist");
  assert!(
    (img_fragment.bounds.width() - 200.0).abs() < 0.1,
    "expected abspos child to fill positioned ancestor width (got {})",
    img_fragment.bounds.width()
  );
  assert!(
    (img_fragment.bounds.height() - 100.0).abs() < 0.1,
    "expected abspos child to fill positioned ancestor height (got {})",
    img_fragment.bounds.height()
  );
}
