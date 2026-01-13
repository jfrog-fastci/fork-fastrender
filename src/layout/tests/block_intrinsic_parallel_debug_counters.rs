use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::contexts::factory::FormattingContextFactory;
use crate::layout::engine::LayoutParallelism;
use crate::layout::engine::{
  current_layout_parallel_debug_collector, enable_layout_parallel_debug_counters,
  layout_parallel_debug_counters, reset_layout_parallel_debug_counters,
};
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::{Display, FormattingContextType};
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn block_intrinsic_parallel_records_debug_work_items() {
  crate::rayon_global::ensure_global_pool().expect("rayon global pool");

  enable_layout_parallel_debug_counters(true);
  reset_layout_parallel_debug_counters();

  let parallelism = LayoutParallelism::enabled(1).with_max_threads(Some(2));
  let factory = FormattingContextFactory::new()
    .with_parallelism(parallelism)
    .with_layout_parallel_debug(current_layout_parallel_debug_collector());
  let bfc = BlockFormattingContext::with_factory(factory);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  let child_style = Arc::new(child_style);

  let mut children = Vec::new();
  for _ in 0..16 {
    children.push(BoxNode::new_block(
      child_style.clone(),
      FormattingContextType::Block,
      vec![],
    ));
  }

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    children,
  );

  let _ = bfc
    .compute_intrinsic_inline_sizes(&container)
    .expect("intrinsic sizing should succeed");

  let counters = layout_parallel_debug_counters();
  enable_layout_parallel_debug_counters(false);
  reset_layout_parallel_debug_counters();

  assert!(
    counters.work_items > 0,
    "expected parallel intrinsic sizing to record work items, counters={counters:?}"
  );
  assert!(
    counters.worker_threads > 0,
    "expected parallel intrinsic sizing to observe at least one rayon worker thread, counters={counters:?}"
  );
}
