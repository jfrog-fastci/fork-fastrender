use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::engine::LayoutParallelism;
use crate::layout::formatting_context::FormattingContext;
use crate::render_control::{
  active_heartbeat, with_deadline, RenderDeadline, StageHeartbeat, StageHeartbeatGuard,
};
use crate::style::display::{Display, FormattingContextType};
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[test]
fn block_intrinsic_parallel_propagates_stage_heartbeat() {
  crate::rayon_global::ensure_global_pool().expect("rayon global pool");

  // Install a stage heartbeat on the calling thread; the parallel intrinsic sizing path should
  // propagate this into rayon workers so deadline checks and budget attribution see a consistent
  // stage heartbeat.
  let _heartbeat_guard = StageHeartbeatGuard::install(Some(StageHeartbeat::Layout));

  let calls = Arc::new(AtomicUsize::new(0));
  let worker_calls = Arc::new(AtomicUsize::new(0));

  let cancel = {
    let calls = Arc::clone(&calls);
    let worker_calls = Arc::clone(&worker_calls);
    Arc::new(move || {
      calls.fetch_add(1, Ordering::Relaxed);
      if rayon::current_thread_index().is_some() {
        worker_calls.fetch_add(1, Ordering::Relaxed);
        return active_heartbeat() != Some(StageHeartbeat::Layout);
      }
      false
    })
  };
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

  // Multiple inline runs separated by block children so the block intrinsic sizing logic produces
  // multiple segments, which can be processed in parallel by Rayon.
  let mut children = Vec::new();
  // Keep this test reasonably small: we only need enough inline children per run to trigger at
  // least one `check_active_periodic(..., 32, ...)` inside inline intrinsic sizing so the cancel
  // callback runs on Rayon worker threads.
  let runs = 2usize;
  let per_run = 32usize;
  for run_idx in 0..runs {
    for i in 0..per_run {
      children.push(span(&format!(
        "run-{run_idx}-{i} supercalifragilisticexpialidocious"
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

  let parallelism = LayoutParallelism::enabled(1).with_max_threads(Some(2));
  assert!(
    parallelism.should_parallelize(container.children.len()),
    "expected heartbeat test tree to exceed parallel intrinsic sizing threshold (children={})",
    container.children.len()
  );
  let parallel = BlockFormattingContext::new().with_parallelism(parallelism);

  let result = with_deadline(Some(&deadline), || {
    parallel.compute_intrinsic_inline_sizes(&container)
  });
  assert!(
    result.is_ok(),
    "expected intrinsic sizing to complete when stage heartbeat is propagated into workers, got {result:?}"
  );
  assert!(
    worker_calls.load(Ordering::Relaxed) > 0,
    "expected cancellation callback to run on at least one rayon worker thread"
  );
}
