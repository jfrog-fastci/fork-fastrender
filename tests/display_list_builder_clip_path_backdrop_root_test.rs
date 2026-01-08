use std::sync::Arc;

use fastrender::paint::display_list::{ClipItem, ClipShape, DisplayItem};
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::style::types::{BasicShape, ClipPath, FillRule};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::Rect;

#[test]
fn clip_path_establishes_backdrop_root_even_when_resolved_none() {
  let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  let child_bounds = Rect::from_xywh(1.0, 1.0, 2.0, 2.0);

  let mut child_style = ComputedStyle::default();
  // Degenerate polygon => `resolve_clip_path` returns `None`, but the computed style is still
  // non-`none` and must trigger Backdrop Root semantics.
  child_style.clip_path = ClipPath::BasicShape(
    Box::new(BasicShape::Polygon {
      fill: FillRule::NonZero,
      points: vec![
        (Length::px(0.0), Length::px(0.0)),
        (Length::px(1.0), Length::px(0.0)),
      ],
    }),
    None,
  );
  let child_style = Arc::new(child_style);

  let child = FragmentNode::new_block_styled(child_bounds, vec![], child_style);
  let root = FragmentNode::new_block_styled(bounds, vec![child], Arc::new(ComputedStyle::default()));

  let list = DisplayListBuilder::new()
    .with_parallelism(&PaintParallelism::disabled())
    .build_with_stacking_tree(&root);

  let child_context = list.items().iter().find_map(|item| match item {
    DisplayItem::PushStackingContext(ctx) if ctx.bounds == child_bounds => Some(ctx),
    _ => None,
  });
  let child_context = child_context.expect("expected a stacking context for clip-path");

  assert!(
    child_context.has_clip_path,
    "non-`none` clip-path should set StackingContextItem.has_clip_path even when it resolves to None"
  );
  assert!(
    child_context.establishes_backdrop_root,
    "non-`none` clip-path should establish a backdrop root even when it resolves to None"
  );

  // Ensure we didn't emit a clip-path clip item, confirming the degenerate polygon resolved to
  // `None` for painting.
  assert!(
    !list.items().iter().any(|item| matches!(
      item,
      DisplayItem::PushClip(ClipItem {
        shape: ClipShape::Path { .. }
      })
    )),
    "expected degenerate clip-path to omit PushClip(Path) emission"
  );
}

