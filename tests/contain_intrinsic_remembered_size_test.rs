use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::types::ContainIntrinsicSizeAxis;
use fastrender::style::types::ContentVisibility;
use fastrender::{BoxNode, BoxTree, ComputedStyle, Display, FontContext, FormattingContext};
use fastrender::{FormattingContextType, LayoutConstraints, Size};
use std::sync::Arc;

fn block_style() -> ComputedStyle {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style
}

#[test]
fn contain_intrinsic_size_auto_uses_remembered_size_when_skipped() {
  // Layout structure:
  // root
  //  ├─ spacer (1000px tall)     -> pushes the auto element below a small viewport
  //  ├─ auto (content-visibility:auto)
  //  │    └─ tall child (500px)  -> establishes a non-zero remembered size
  //  └─ after (10px tall)        -> should not shift between layout passes

  let root_style = Arc::new(block_style());

  let mut spacer_style = block_style();
  spacer_style.height = Some(fastrender::Length::px(1000.0));

  let mut auto_style = block_style();
  auto_style.content_visibility = ContentVisibility::Auto;

  let mut tall_child_style = block_style();
  tall_child_style.height = Some(fastrender::Length::px(500.0));

  let mut after_style = block_style();
  after_style.height = Some(fastrender::Length::px(10.0));

  let auto = BoxNode::new_block(
    Arc::new(auto_style),
    FormattingContextType::Block,
    vec![BoxNode::new_block(
      Arc::new(tall_child_style),
      FormattingContextType::Block,
      vec![],
    )],
  );

  let tree = BoxTree::new(BoxNode::new_block(
    root_style,
    FormattingContextType::Block,
    vec![
      BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]),
      auto,
      BoxNode::new_block(Arc::new(after_style), FormattingContextType::Block, vec![]),
    ],
  ));

  let constraints = LayoutConstraints::definite_width(800.0);

  // Pass #1: large viewport so `content-visibility:auto` does NOT skip.
  let viewport_large = Size::new(800.0, 3000.0);
  let fc =
    BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport_large);
  let root_frag = fc.layout(&tree.root, &constraints).expect("layout pass #1");

  assert_eq!(root_frag.children.len(), 3);
  let auto_frag = &root_frag.children[1];
  assert!(
    !auto_frag.children.is_empty(),
    "expected auto element contents to be laid out in pass #1"
  );
  let remembered_height = auto_frag.bounds.height();
  assert!(
    remembered_height > 0.0,
    "expected a non-zero laid out block-size for the auto element"
  );
  let after_y_pass1 = root_frag.children[2].bounds.y();

  // Pass #2: small viewport so the auto element is considered out-of-viewport and skipped.
  let viewport_small = Size::new(800.0, 600.0);
  let fc =
    BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport_small);
  let root_frag = fc.layout(&tree.root, &constraints).expect("layout pass #2");

  assert_eq!(root_frag.children.len(), 3);
  let auto_frag = &root_frag.children[1];
  assert!(
    auto_frag.children.is_empty(),
    "expected auto element contents to be skipped in pass #2"
  );

  let placeholder_height = auto_frag.bounds.height();
  assert!(
    (placeholder_height - remembered_height).abs() < 0.01,
    "expected skipped placeholder block-size {placeholder_height} to match remembered size {remembered_height}"
  );

  let after_y_pass2 = root_frag.children[2].bounds.y();
  assert!(
    (after_y_pass2 - after_y_pass1).abs() < 0.01,
    "expected following content to keep the same offset (pass1={after_y_pass1}, pass2={after_y_pass2})"
  );
}

#[test]
fn contain_intrinsic_size_auto_falls_back_to_length_then_remembered_size() {
  // Layout structure:
  // root
  //  ├─ spacer (1000px tall)     -> pushes the auto element below a small viewport
  //  ├─ auto (content-visibility:auto; contain-intrinsic-size:auto 30px)
  //  │    └─ tall child (500px)  -> establishes a non-zero remembered size once laid out
  //  └─ after (10px tall)        -> should shift from fallback (30px) to remembered (500px)
  //
  // Pass #1: element skipped => use fallback length (30px)
  // Pass #2: element laid out => remember 500px
  // Pass #3: element skipped again => use remembered 500px (not fallback)

  let root_style = Arc::new(block_style());

  let mut spacer_style = block_style();
  spacer_style.height = Some(fastrender::Length::px(1000.0));

  let mut auto_style = block_style();
  auto_style.content_visibility = ContentVisibility::Auto;
  auto_style.contain_intrinsic_height = ContainIntrinsicSizeAxis {
    auto: true,
    length: Some(fastrender::Length::px(30.0)),
  };

  let mut tall_child_style = block_style();
  tall_child_style.height = Some(fastrender::Length::px(500.0));

  let mut after_style = block_style();
  after_style.height = Some(fastrender::Length::px(10.0));

  let auto = BoxNode::new_block(
    Arc::new(auto_style),
    FormattingContextType::Block,
    vec![BoxNode::new_block(
      Arc::new(tall_child_style),
      FormattingContextType::Block,
      vec![],
    )],
  );

  let tree = BoxTree::new(BoxNode::new_block(
    root_style,
    FormattingContextType::Block,
    vec![
      BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]),
      auto,
      BoxNode::new_block(Arc::new(after_style), FormattingContextType::Block, vec![]),
    ],
  ));

  let constraints = LayoutConstraints::definite_width(800.0);

  // Pass #1: small viewport => auto element is skipped, using fallback length (30px).
  let viewport_small = Size::new(800.0, 600.0);
  let fc =
    BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport_small);
  let root_frag = fc.layout(&tree.root, &constraints).expect("layout pass #1");

  assert_eq!(root_frag.children.len(), 3);
  let auto_frag = &root_frag.children[1];
  assert!(
    auto_frag.children.is_empty(),
    "expected auto element contents to be skipped in pass #1"
  );
  assert!(
    (auto_frag.bounds.height() - 30.0).abs() < 0.01,
    "expected pass #1 placeholder height to use fallback length"
  );
  let after_y_pass1 = root_frag.children[2].bounds.y();

  // Pass #2: large viewport => auto element is laid out and its 500px block-size is remembered.
  let viewport_large = Size::new(800.0, 3000.0);
  let fc =
    BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport_large);
  let root_frag = fc.layout(&tree.root, &constraints).expect("layout pass #2");
  assert_eq!(root_frag.children.len(), 3);
  let auto_frag = &root_frag.children[1];
  assert!(
    !auto_frag.children.is_empty(),
    "expected auto element contents to be laid out in pass #2"
  );
  let remembered_height = auto_frag.bounds.height();
  assert!(
    (remembered_height - 500.0).abs() < 0.01,
    "expected pass #2 laid out height to match the tall child"
  );
  let after_y_pass2 = root_frag.children[2].bounds.y();
  assert!(
    after_y_pass2 > after_y_pass1 + 100.0,
    "expected following content to shift down once the element is laid out (pass1={after_y_pass1}, pass2={after_y_pass2})"
  );

  // Pass #3: small viewport again => auto element is skipped, using remembered size (500px).
  let fc =
    BlockFormattingContext::with_font_context_and_viewport(FontContext::new(), viewport_small);
  let root_frag = fc.layout(&tree.root, &constraints).expect("layout pass #3");
  assert_eq!(root_frag.children.len(), 3);
  let auto_frag = &root_frag.children[1];
  assert!(
    auto_frag.children.is_empty(),
    "expected auto element contents to be skipped in pass #3"
  );
  let placeholder_height = auto_frag.bounds.height();
  assert!(
    (placeholder_height - remembered_height).abs() < 0.01,
    "expected pass #3 placeholder height to use remembered size (placeholder={placeholder_height}, remembered={remembered_height})"
  );
  let after_y_pass3 = root_frag.children[2].bounds.y();
  assert!(
    (after_y_pass3 - after_y_pass2).abs() < 0.01,
    "expected following content to keep the remembered offset (pass2={after_y_pass2}, pass3={after_y_pass3})"
  );
}
