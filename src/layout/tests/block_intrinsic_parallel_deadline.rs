use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::engine::LayoutParallelism;
use crate::layout::formatting_context::{FormattingContext, LayoutError};
use crate::render_control::{with_deadline, RenderDeadline};
use crate::style::display::{Display, FormattingContextType};
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn block_intrinsic_parallel_respects_deadline() {
  // Ensure the global Rayon pool is initialized with FastRender's conservative defaults so the
  // parallel intrinsic-sizing path doesn't panic in constrained environments.
  crate::rayon_global::ensure_global_pool().expect("rayon global pool");

  // Cancel only when the deadline check runs on a Rayon worker thread.
  let cancel = Arc::new(|| rayon::current_thread_index().is_some());
  let deadline = RenderDeadline::new(None, Some(cancel));

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let mut span_style = ComputedStyle::default();
  span_style.display = Display::Inline;
  let span_style = Arc::new(span_style);

  let text = |label: &str| BoxNode::new_text(text_style.clone(), label.to_string());
  let span = |label: &str| BoxNode::new_inline(span_style.clone(), vec![text(label)]);

  let mut separator_style = ComputedStyle::default();
  separator_style.display = Display::Block;
  separator_style.width = Some(Length::px(10.0));
  let separator_style = Arc::new(separator_style);
  let separator = || {
    BoxNode::new_block(
      separator_style.clone(),
      FormattingContextType::Block,
      vec![],
    )
  };

  // Build a container with multiple inline runs separated by block children so the block intrinsic
  // sizing path spawns parallel work items (segments). Each inline run contains enough children to
  // trigger `check_active_periodic` inside the inline intrinsic sizing hot loop.
  let mut children = Vec::new();
  let runs = 4usize;
  let per_run = 64usize;
  for run_idx in 0..runs {
    for i in 0..per_run {
      children.push(span(&format!(
        "run-{run_idx}-{i} lorem ipsum dolor sit amet"
      )));
    }
    if run_idx + 1 < runs {
      children.push(separator());
    }
  }

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    children,
  );

  let serial = BlockFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
  let parallel = BlockFormattingContext::new()
    .with_parallelism(LayoutParallelism::enabled(1).with_max_threads(Some(2)));

  // Serial intrinsic sizing should never observe the cancellation callback (it only fires on Rayon
  // worker threads).
  let serial_result = with_deadline(Some(&deadline), || {
    serial.compute_intrinsic_inline_sizes(&container)
  });
  assert!(
    serial_result.is_ok(),
    "serial intrinsic sizing should succeed under worker-only cancel deadline, got {serial_result:?}"
  );

  // The parallel intrinsic sizing path runs segment work on Rayon workers and should therefore hit
  // the cancellation callback during periodic deadline checks.
  let parallel_result = with_deadline(Some(&deadline), || {
    parallel.compute_intrinsic_inline_sizes(&container)
  });
  assert!(
    matches!(parallel_result, Err(LayoutError::Timeout { .. })),
    "expected timeout from worker-only cancel deadline, got {parallel_result:?}"
  );
}
