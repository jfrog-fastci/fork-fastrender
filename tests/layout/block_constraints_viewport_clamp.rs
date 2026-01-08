use fastrender::geometry::Size;
use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, ComputedStyle, FontContext};
use std::sync::Arc;

fn line_count(fragment: &FragmentNode) -> usize {
  fragment
    .iter_fragments()
    .filter(|f| matches!(f.content, FragmentContent::Line { .. }))
    .count()
}

#[test]
fn block_constraints_do_not_clamp_definite_width_to_viewport() {
  let viewport = Size::new(200.0, 200.0);
  let fc = BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport);

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.width = Some(Length::px(1000.0));

  let text = "aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa aaaaaaaaa";
  let text_node = BoxNode::new_text(Arc::new(ComputedStyle::default()), text.to_string());
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Inline,
    vec![text_node],
  );

  let root = BoxNode::new_block(Arc::new(root_style), FormattingContextType::Block, vec![container]);

  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(viewport.width), AvailableSpace::Indefinite);
  let fragment = fc.layout(&root, &constraints).expect("layout should succeed");

  let container_fragment = fragment.children.first().expect("container fragment");
  assert!(
    (container_fragment.bounds.width() - 1000.0).abs() < 0.5,
    "block child width should not be clamped to the viewport (got {:.1})",
    container_fragment.bounds.width()
  );

  let lines = line_count(container_fragment);
  assert_eq!(
    lines, 1,
    "child should not wrap when block has a definite width larger than the viewport (child_width={:.1})",
    container_fragment.bounds.width()
  );
}
