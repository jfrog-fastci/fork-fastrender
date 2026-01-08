use std::sync::Arc;

use fastrender::geometry::{Rect, Size};
use fastrender::style::types::{
  Direction, ScrollSnapAlign, ScrollSnapAxis, ScrollSnapStrictness, WritingMode,
};
use fastrender::{ComputedStyle, FragmentContent, FragmentNode, FragmentTree};

fn snap_target_y(writing_mode: WritingMode, direction: Direction, align: ScrollSnapAlign) -> f32 {
  let mut container_style = ComputedStyle::default();
  container_style.writing_mode = writing_mode;
  container_style.direction = direction;
  container_style.scroll_snap_type.axis = ScrollSnapAxis::Inline;
  container_style.scroll_snap_type.strictness = ScrollSnapStrictness::Mandatory;
  let container_style = Arc::new(container_style);

  let mut target_style = ComputedStyle::default();
  target_style.scroll_snap_align.inline = align;
  let target_style = Arc::new(target_style);

  // In vertical writing modes the inline axis is vertical. Place a target just below the viewport
  // so that both inline-start (top/bottom) alignments can produce non-zero scroll offsets.
  let target = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 100.0, 100.0, 10.0),
    FragmentContent::Block { box_id: Some(2) },
    vec![],
    target_style,
  );

  let root = FragmentNode::new_with_style(
    Rect::from_xywh(0.0, 0.0, 100.0, 100.0),
    FragmentContent::Block { box_id: Some(1) },
    vec![target],
    container_style,
  );

  let mut tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));
  tree.ensure_scroll_metadata();

  let metadata = tree.scroll_metadata.expect("scroll metadata computed");
  assert_eq!(metadata.containers.len(), 1, "expected one snap container");
  let container = &metadata.containers[0];
  assert!(
    container.snap_y,
    "inline axis should map to the Y axis under {writing_mode:?}",
  );
  assert_eq!(
    container.targets_y.len(),
    1,
    "expected one inline-axis snap target under {writing_mode:?}"
  );
  container.targets_y[0].position
}

#[test]
fn scroll_snap_inline_start_end_flip_in_vertical_inline_axes_under_rtl() {
  // The `direction` property flips the inline-start/inline-end edges even when the inline axis is
  // vertical. Scroll snap uses the inline axis to decide which physical edge is aligned for the
  // `start`/`end` keywords.
  //
  // For a 100px-tall snapport and a 10px-tall target at y=100..110:
  // - LTR inline-start is the top edge => scroll to y=100.
  // - RTL inline-start is the bottom edge => scroll to y=10.
  for writing_mode in [
    WritingMode::VerticalRl,
    WritingMode::VerticalLr,
    WritingMode::SidewaysRl,
    WritingMode::SidewaysLr,
  ] {
    let (expected_ltr_start, expected_rtl_start, expected_ltr_end, expected_rtl_end) =
      if writing_mode == WritingMode::SidewaysLr {
        (10.0, 100.0, 100.0, 10.0)
      } else {
        (100.0, 10.0, 10.0, 100.0)
      };

    let ltr_start = snap_target_y(writing_mode, Direction::Ltr, ScrollSnapAlign::Start);
    let rtl_start = snap_target_y(writing_mode, Direction::Rtl, ScrollSnapAlign::Start);
    assert!(
      (ltr_start - expected_ltr_start).abs() < 1e-3,
      "expected inline-start target to resolve to y={expected_ltr_start} under {writing_mode:?} + ltr, got {ltr_start}"
    );
    assert!(
      (rtl_start - expected_rtl_start).abs() < 1e-3,
      "expected inline-start target to resolve to y={expected_rtl_start} under {writing_mode:?} + rtl, got {rtl_start}"
    );

    let ltr_end = snap_target_y(writing_mode, Direction::Ltr, ScrollSnapAlign::End);
    let rtl_end = snap_target_y(writing_mode, Direction::Rtl, ScrollSnapAlign::End);
    assert!(
      (ltr_end - expected_ltr_end).abs() < 1e-3,
      "expected inline-end target to resolve to y={expected_ltr_end} under {writing_mode:?} + ltr, got {ltr_end}"
    );
    assert!(
      (rtl_end - expected_rtl_end).abs() < 1e-3,
      "expected inline-end target to resolve to y={expected_rtl_end} under {writing_mode:?} + rtl, got {rtl_end}"
    );
  }
}
