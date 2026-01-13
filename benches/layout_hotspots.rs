//! Microbenchmarks for layout hotspots.
//!
//! These benchmarks are intentionally small and synthetic: they run quickly, stay offline,
//! and isolate known layout hotspots so regressions show up in a targeted `cargo bench`.
//!
//! The goal is not to mimic full-page rendering. Instead, each benchmark stresses a tight loop
//! that has historically been performance sensitive:
//! - Flex item measurement (Taffy measure callback)
//! - Flex intrinsic sizing (min/max-content width)
//! - Block intrinsic sizing (min/max-content width)
//! - Grid track sizing + intrinsic measurement fanout (Taffy grid measure callback)
//! - Table cell intrinsic sizing + column width distribution

use std::sync::Arc;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, Criterion, SamplingMode, Throughput};
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::engine::{layout_parallelism_workload, LayoutParallelism};
use fastrender::layout::table::{TableFormattingContext, TableStructure};
use fastrender::layout::taffy_integration::{
  enable_taffy_counters, reset_taffy_counters, taffy_counters, taffy_perf_counters,
  TaffyPerfCountersGuard,
};
use fastrender::style::display::Display;
use fastrender::style::float::Float;
use fastrender::style::types::{
  BorderCollapse, BorderStyle, FlexWrap, GridTrack, TableLayout, TextWrap, WhiteSpace,
};
use fastrender::style::values::Length;
use fastrender::{
  BoxNode, BoxTree, ComputedStyle, FormattingContext, FormattingContextFactory,
  FormattingContextType, IntrinsicSizingMode, LayoutConfig, LayoutConstraints, LayoutEngine, Size,
};
use rayon::ThreadPoolBuilder;

mod common;

fn micro_criterion() -> Criterion {
  // Keep this bench target quick to run locally. Some of these hotspots can take hundreds
  // of milliseconds per iteration, so prefer fewer samples over long measurement windows.
  Criterion::default()
    .sample_size(10)
    .warm_up_time(Duration::from_millis(200))
    .measurement_time(Duration::from_millis(600))
    .configure_from_args()
}

fn ensure_rayon_global_pool_for_bench(max_threads: usize) -> bool {
  let desired = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(1)
    .max(1)
    .min(max_threads.max(1));
  if ThreadPoolBuilder::new()
    .num_threads(desired)
    .thread_name(|idx| format!("fastr-bench-rayon-{idx}"))
    .build_global()
    .is_ok()
  {
    return true;
  }

  // If the pool was already initialized elsewhere, we can treat this as success. Avoid
  // unguarded calls that could trigger Rayon's lazy init with an excessive thread count.
  std::panic::catch_unwind(|| rayon::current_num_threads()).is_ok()
}

fn build_flex_measure_tree(item_count: usize) -> BoxTree {
  // Regression protected:
  // - Flex layout can become dominated by repeated item measurement (Taffy calling the measure
  //   callback for leaf nodes). This tree is constructed so every flex item requires intrinsic
  //   measurement (text + inline formatting context).
  const TEXT: &str = "lorem ipsum dolor sit amet consectetur adipiscing elit";

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_wrap = FlexWrap::Wrap;
  // A narrow container forces wrap decisions and tends to increase measurement pressure.
  flex_style.width = Some(Length::px(420.0));
  let flex_style = Arc::new(flex_style);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.padding_left = Length::px(2.0);
  item_style.padding_right = Length::px(2.0);
  let item_style = Arc::new(item_style);

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Block;
  let inline_style = Arc::new(inline_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let mut children = Vec::with_capacity(item_count);
  for idx in 0..item_count {
    let text = BoxNode::new_text(text_style.clone(), format!("item-{idx} {TEXT}"));
    let inline = BoxNode::new_block(
      inline_style.clone(),
      FormattingContextType::Inline,
      vec![text],
    );
    children.push(BoxNode::new_block(
      item_style.clone(),
      FormattingContextType::Block,
      vec![inline],
    ));
  }

  let root = BoxNode::new_block(flex_style, FormattingContextType::Flex, children);
  BoxTree::new(root)
}

fn build_flex_intrinsic_tree(item_count: usize) -> BoxTree {
  // Regression protected:
  // - Flex intrinsic sizing (min/max-content width) can become dominated by scanning child items.
  //   This tree is constructed so each child requires non-trivial intrinsic sizing (inline text),
  //   but stable ids allow child results to be cached across iterations.
  const WORD: &str = "supercalifragilisticexpialidocious";
  const FILL: &str = "lorem ipsum dolor sit amet consectetur adipiscing elit";

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  // Keep the container's preferred size auto so intrinsic sizing must inspect children.
  flex_style.width = None;
  flex_style.flex_wrap = FlexWrap::NoWrap;
  // Include a non-zero gap so intrinsic sizing has to account for it.
  flex_style.grid_column_gap = Length::px(4.0);
  let flex_style = Arc::new(flex_style);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.padding_left = Length::px(2.0);
  item_style.padding_right = Length::px(2.0);
  item_style.margin_left = Some(Length::px(1.0));
  item_style.margin_right = Some(Length::px(1.0));
  let item_style = Arc::new(item_style);

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Block;
  let inline_style = Arc::new(inline_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let mut children = Vec::with_capacity(item_count);
  for idx in 0..item_count {
    let text = BoxNode::new_text(
      text_style.clone(),
      format!("item-{idx} {WORD} {FILL} {FILL}"),
    );
    let inline = BoxNode::new_block(
      inline_style.clone(),
      FormattingContextType::Inline,
      vec![text],
    );
    children.push(BoxNode::new_block(
      item_style.clone(),
      FormattingContextType::Block,
      vec![inline],
    ));
  }

  let root = BoxNode::new_block(flex_style, FormattingContextType::Flex, children);
  BoxTree::new(root)
}

fn build_grid_track_sizing_measure_tree(item_count: usize) -> BoxTree {
  // Regression protected:
  // - Grid track sizing can become dominated by intrinsic measurement of grid items (Taffy invoking
  //   the measure callback repeatedly for min/max-content probes). This tree is constructed to
  //   force auto minimum track sizing (`1fr` -> `minmax(auto, 1fr)`) across many text-heavy items.
  // Keep this short but non-trivial so we still hit intrinsic sizing paths without turning the
  // benchmark into a text shaping benchmark.
  const TEXT: &str = "supercalifragilisticexpialidocious lorem";

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  // A definite inline size keeps the benchmark focused on track sizing rather than container sizing.
  grid_style.width = Some(Length::px(420.0));
  // `repeat(12, 1fr)` (min track sizing defaults to `auto`, which triggers intrinsic sizing).
  grid_style.grid_template_columns = vec![GridTrack::Fr(1.0); 12];
  // Keep the block axis simple so we mainly exercise column sizing.
  grid_style.grid_auto_rows = vec![GridTrack::Length(Length::px(10.0))].into();
  let grid_style = Arc::new(grid_style);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.padding_left = Length::px(2.0);
  item_style.padding_right = Length::px(2.0);
  let item_style = Arc::new(item_style);

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Block;
  let inline_style = Arc::new(inline_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let mut children = Vec::with_capacity(item_count);
  for idx in 0..item_count {
    // Keep the payload non-trivial (forces intrinsic sizing) but avoid extremely long strings so
    // the benchmark stays quick under the micro Criterion settings.
    let text = BoxNode::new_text(text_style.clone(), format!("cell-{idx} {TEXT}"));
    let inline = BoxNode::new_block(
      inline_style.clone(),
      FormattingContextType::Inline,
      vec![text],
    );
    children.push(BoxNode::new_block(
      item_style.clone(),
      FormattingContextType::Block,
      vec![inline],
    ));
  }

  let root = BoxNode::new_block(grid_style, FormattingContextType::Grid, children);
  BoxTree::new(root)
}

fn build_block_intrinsic_tree(span_count: usize) -> BoxTree {
  // Regression protected:
  // - Block intrinsic sizing relies heavily on inline item collection + text measurement.
  //   Changes that accidentally re-shape text repeatedly or allocate excessively will show up
  //   here without the noise of full layout.
  const WORD: &str = "supercalifragilisticexpialidocious";
  const FILL: &str = "the quick brown fox jumps over the lazy dog";

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  let root_style = Arc::new(root_style);

  let mut span_style = ComputedStyle::default();
  span_style.display = Display::Inline;
  let span_style = Arc::new(span_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let mut children = Vec::with_capacity(span_count * 2);
  for idx in 0..span_count {
    let payload = if idx % 8 == 0 {
      format!("{WORD} {FILL} {FILL}")
    } else {
      format!("{FILL} {FILL}")
    };
    let text = BoxNode::new_text(text_style.clone(), payload);
    let span = BoxNode::new_inline(span_style.clone(), vec![text]);
    children.push(span);
    // Explicit separator so max-content width includes multiple segments.
    children.push(BoxNode::new_text(text_style.clone(), " ".to_string()));
  }

  let root = BoxNode::new_block(root_style, FormattingContextType::Block, children);
  BoxTree::new(root)
}

fn build_block_intrinsic_block_children_tree(child_count: usize) -> BoxTree {
  // Regression protected:
  // - The block intrinsic sizing hot path can fan out over many block-level children (e.g. tables,
  //   multi-column flows). This tree is constructed so the parallel segment path has many
  //   block-child segments to process.
  const LONG_WORD: &str = "supercalifragilisticexpialidocious";
  const FILL: &str = "lorem ipsum dolor sit amet consectetur adipiscing elit";

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  let root_style = Arc::new(root_style);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  let child_style = Arc::new(child_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let mut children = Vec::with_capacity(child_count);
  for idx in 0..child_count {
    let payload = if idx % 7 == 0 {
      format!("child-{idx} {LONG_WORD} {FILL} {FILL}")
    } else {
      format!("child-{idx} {FILL} {FILL}")
    };
    let text = BoxNode::new_text(text_style.clone(), payload);
    children.push(BoxNode::new_block(
      child_style.clone(),
      FormattingContextType::Block,
      vec![text],
    ));
  }

  let root = BoxNode::new_block(root_style, FormattingContextType::Block, children);
  BoxTree::new(root)
}

fn build_block_intrinsic_tree_nowrap(span_count: usize) -> BoxTree {
  // Regression protected:
  // - Inline intrinsic sizing runs `find_break_opportunities` (UAX#14) to locate soft wrap points.
  //   When wrapping is disabled (e.g. `white-space: nowrap`), we should be able to skip the scan.
  //
  // This mirrors `build_block_intrinsic_tree` but forces `allow_soft_wrap_for_style == false` on
  // the inline spans/text nodes so it becomes a targeted guardrail for the nowrap fast path.
  const WORD: &str = "supercalifragilisticexpialidocious";
  const FILL: &str = "the quick brown fox jumps over the lazy dog";

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  let root_style = Arc::new(root_style);

  let mut span_style = ComputedStyle::default();
  span_style.display = Display::Inline;
  span_style.white_space = WhiteSpace::Nowrap;
  span_style.text_wrap = TextWrap::NoWrap;
  let span_style = Arc::new(span_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  text_style.white_space = WhiteSpace::Nowrap;
  text_style.text_wrap = TextWrap::NoWrap;
  let text_style = Arc::new(text_style);

  let mut children = Vec::with_capacity(span_count * 2);
  for idx in 0..span_count {
    let payload = if idx % 8 == 0 {
      format!("{WORD} {FILL} {FILL}")
    } else {
      format!("{FILL} {FILL}")
    };
    let text = BoxNode::new_text(text_style.clone(), payload);
    let span = BoxNode::new_inline(span_style.clone(), vec![text]);
    children.push(span);
    // Explicit separator so max-content width includes multiple segments.
    children.push(BoxNode::new_text(text_style.clone(), " ".to_string()));
  }

  let root = BoxNode::new_block(root_style, FormattingContextType::Block, children);
  BoxTree::new(root)
}

fn build_block_intrinsic_many_runs_tree(run_count: usize) -> BoxTree {
  // Regression protected:
  // - Block intrinsic sizing may have to flush many short inline runs when block-level boxes are
  //   mixed into otherwise inline content. This stresses the inline-run cache keying path in
  //   `BlockFormattingContext::compute_intrinsic_inline_size`.

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  let root_style = Arc::new(root_style);

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Inline;
  let inline_style = Arc::new(inline_style);

  let mut separator_style = ComputedStyle::default();
  separator_style.display = Display::Block;
  separator_style.containment.inline_size = true;
  let separator_style = Arc::new(separator_style);

  let mut children = Vec::with_capacity(run_count.saturating_mul(2).saturating_sub(1));
  for idx in 0..run_count {
    children.push(BoxNode::new_inline(inline_style.clone(), Vec::new()));
    if idx + 1 < run_count {
      children.push(BoxNode::new_block(
        separator_style.clone(),
        FormattingContextType::Block,
        vec![],
      ));
    }
  }

  let root = BoxNode::new_block(root_style, FormattingContextType::Block, children);
  BoxTree::new(root)
}

fn build_block_intrinsic_mixed_segments_tree(section_count: usize) -> BoxTree {
  // Regression protected:
  // - `BlockFormattingContext::compute_intrinsic_inline_sizes` can become dominated by walking many
  //   heterogeneous children (inline runs, block-level children, and floats). Profiling on large
  //   pages shows the per-child intrinsic sizing + inline-run shaping can dominate wall time, so
  //   this tree is designed to stress that fan-out.
  const RUN_TEXT: &str = "lorem ipsum dolor sit amet consectetur adipiscing elit";
  const BLOCK_TEXT: &str = "supercalifragilisticexpialidocious the quick brown fox jumps";

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  let root_style = Arc::new(root_style);

  let mut block_style = ComputedStyle::default();
  block_style.display = Display::Block;
  // Add some margins so the outer-size path is exercised too.
  block_style.margin_left = Some(Length::px(4.0));
  block_style.margin_right = Some(Length::px(6.0));
  let block_style = Arc::new(block_style);

  let mut float_left_style = ComputedStyle::default();
  float_left_style.display = Display::Block;
  float_left_style.float = Float::Left;
  float_left_style.margin_right = Some(Length::px(6.0));
  let float_left_style = Arc::new(float_left_style);

  let mut float_right_style = ComputedStyle::default();
  float_right_style.display = Display::Block;
  float_right_style.float = Float::Right;
  float_right_style.margin_left = Some(Length::px(6.0));
  let float_right_style = Arc::new(float_right_style);

  // Use a dedicated inline FC wrapper so block children invoke inline layout/shaping.
  let mut inline_fc_style = ComputedStyle::default();
  inline_fc_style.display = Display::Block;
  let inline_fc_style = Arc::new(inline_fc_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let mut children = Vec::with_capacity(section_count * 16);
  for idx in 0..section_count {
    // Inline run (multiple text nodes) to force inline-item collection/shaping.
    for j in 0..6 {
      children.push(BoxNode::new_text(
        text_style.clone(),
        format!("run-{idx}-{j} {RUN_TEXT} "),
      ));
    }

    // Periodic floats so intrinsic sizing has to account for float line accumulation.
    if idx % 8 == 0 {
      let left_text = BoxNode::new_text(
        text_style.clone(),
        format!("float-left-{idx} {RUN_TEXT} {RUN_TEXT}"),
      );
      children.push(BoxNode::new_block(
        float_left_style.clone(),
        FormattingContextType::Block,
        vec![left_text],
      ));
      let right_text = BoxNode::new_text(
        text_style.clone(),
        format!("float-right-{idx} {RUN_TEXT} {RUN_TEXT}"),
      );
      children.push(BoxNode::new_block(
        float_right_style.clone(),
        FormattingContextType::Block,
        vec![right_text],
      ));
    }

    // In-flow block child.
    let text = BoxNode::new_text(
      text_style.clone(),
      format!("block-{idx} {BLOCK_TEXT} {BLOCK_TEXT} {RUN_TEXT}"),
    );
    let inline = BoxNode::new_block(
      inline_fc_style.clone(),
      FormattingContextType::Inline,
      vec![text],
    );
    children.push(BoxNode::new_block(
      block_style.clone(),
      FormattingContextType::Block,
      vec![inline],
    ));
  }

  let root = BoxNode::new_block(root_style, FormattingContextType::Block, children);
  BoxTree::new(root)
}

fn build_inline_cache_tree(span_count: usize) -> BoxTree {
  const TEXT: &str = "lorem ipsum dolor sit amet consectetur adipiscing elit";

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  let root_style = Arc::new(root_style);

  let mut span_style = ComputedStyle::default();
  span_style.display = Display::Inline;
  let span_style = Arc::new(span_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let mut children = Vec::with_capacity(span_count * 2);
  for idx in 0..span_count {
    let text = BoxNode::new_text(text_style.clone(), format!("span-{idx} {TEXT} {TEXT}"));
    children.push(BoxNode::new_inline(span_style.clone(), vec![text]));
    children.push(BoxNode::new_text(text_style.clone(), " ".to_string()));
  }

  let root = BoxNode::new_block(root_style, FormattingContextType::Inline, children);
  BoxTree::new(root)
}

fn assign_box_ids(node: &mut BoxNode, next: &mut usize) {
  let id = *next;
  *next = id.saturating_add(1);
  node.id = id;
  for child in &mut node.children {
    assign_box_ids(child, next);
  }
}

fn assign_text_box_ids(node: &mut BoxNode, next: &mut usize) {
  use fastrender::tree::box_tree::BoxType;
  if matches!(node.box_type, BoxType::Text(_) | BoxType::Marker(_)) {
    let id = *next;
    *next = id.saturating_add(1);
    node.id = id;
  }
  for child in &mut node.children {
    assign_text_box_ids(child, next);
  }
  if let Some(body) = node.footnote_body.as_deref_mut() {
    assign_text_box_ids(body, next);
  }
}

fn build_table_tree(rows: usize, cols: usize) -> BoxNode {
  // Regression protected:
  // - Table auto layout measures min/max-content widths for every cell, then distributes
  //   widths across columns. The complexity tends to scale with cell count, so keep the
  //   table moderate but non-trivial.
  const CELL_TEXT: &str = "Table cell text: lorem ipsum dolor sit amet";

  let mut table_style = ComputedStyle::default();
  table_style.display = Display::Table;
  table_style.table_layout = TableLayout::Auto;
  table_style.border_collapse = BorderCollapse::Separate;
  table_style.width = Some(Length::px(960.0));
  let table_style = Arc::new(table_style);

  let mut group_style = ComputedStyle::default();
  group_style.display = Display::TableRowGroup;
  let group_style = Arc::new(group_style);

  let mut row_style = ComputedStyle::default();
  row_style.display = Display::TableRow;
  let row_style = Arc::new(row_style);

  let mut cell_style = ComputedStyle::default();
  cell_style.display = Display::TableCell;
  cell_style.padding_left = Length::px(4.0);
  cell_style.padding_right = Length::px(4.0);
  cell_style.padding_top = Length::px(2.0);
  cell_style.padding_bottom = Length::px(2.0);
  cell_style.border_left_width = Length::px(1.0);
  cell_style.border_right_width = Length::px(1.0);
  cell_style.border_top_width = Length::px(1.0);
  cell_style.border_bottom_width = Length::px(1.0);
  cell_style.border_left_style = BorderStyle::Solid;
  cell_style.border_right_style = BorderStyle::Solid;
  cell_style.border_top_style = BorderStyle::Solid;
  cell_style.border_bottom_style = BorderStyle::Solid;
  let cell_style = Arc::new(cell_style);

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Block;
  let inline_style = Arc::new(inline_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let mut row_nodes = Vec::with_capacity(rows);
  for r in 0..rows {
    let mut cells = Vec::with_capacity(cols);
    for c in 0..cols {
      let text = BoxNode::new_text(
        text_style.clone(),
        format!("r{r}c{c} {CELL_TEXT} {CELL_TEXT}"),
      );
      let inline = BoxNode::new_block(
        inline_style.clone(),
        FormattingContextType::Inline,
        vec![text],
      );
      cells.push(BoxNode::new_block(
        cell_style.clone(),
        FormattingContextType::Block,
        vec![inline],
      ));
    }
    row_nodes.push(BoxNode::new_block(
      row_style.clone(),
      FormattingContextType::Block,
      cells,
    ));
  }

  let row_group = BoxNode::new_block(group_style, FormattingContextType::Block, row_nodes);
  BoxNode::new_block(table_style, FormattingContextType::Table, vec![row_group])
}

fn build_float_shrink_to_fit_tree(float_count: usize) -> BoxTree {
  // Regression protected:
  // - Shrink-to-fit sizing for floats uses min/max-content intrinsic widths. Those intrinsic widths
  //   are often computed earlier during parent intrinsic sizing / layout probes (flex, grid, etc.).
  //   This tree warms the intrinsic cache first, then measures how much work float layout can reuse.
  const FLOAT_TEXT: &str = "lorem ipsum dolor sit amet consectetur adipiscing elit";

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  let root_style = Arc::new(root_style);

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::Block;
  float_style.float = Float::Left;
  float_style.width_keyword = None;
  float_style.height_keyword = None;
  let float_style = Arc::new(float_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let mut children = Vec::with_capacity(float_count);
  for idx in 0..float_count {
    let text = BoxNode::new_text(text_style.clone(), format!("float-{idx} {FLOAT_TEXT}"));
    let mut float = BoxNode::new_block(
      float_style.clone(),
      FormattingContextType::Block,
      vec![text],
    );
    // Use stable, non-zero ids so intrinsic sizing can be cached.
    float.id = 10_000 + idx;
    children.push(float);
  }

  let root = BoxNode::new_block(root_style, FormattingContextType::Block, children);
  BoxTree::new(root)
}

fn build_grid_spanning_distribution_tree(item_count: usize, columns: usize) -> BoxTree {
  // Regression protected:
  // - CSS Grid track sizing for spanning items can become dominated by the
  //   `distribute_space_up_to_limits` loop in Taffy's implementation of
  //   https://www.w3.org/TR/css-grid-1/#extra-space (11.5.1).
  //
  // This tree is designed to:
  // - produce many spanning items (2-6 columns),
  // - mix track sizing functions that introduce growth limits (fit-content),
  // - keep the per-item subtree shallow so track sizing dominates.
  const TEXT: &str = "supercalifragilisticexpialidocious lorem ipsum dolor sit amet";

  let columns = columns.max(6);

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(800.0));
  // Fixed implicit row size keeps the benchmark focused on *column* track sizing.
  grid_style.grid_auto_rows = vec![GridTrack::Length(Length::px(24.0))].into();

  // Mixture of intrinsic mins + capped maxes so spanning items repeatedly distribute space up to
  // limits. Vary limits so `distribute_space_up_to_limits` requires multiple iterations.
  let mut template = Vec::with_capacity(columns);
  for idx in 0..columns {
    let track = match idx % 6 {
      // fit-content tracks introduce growth limits that force repeated distribution iterations.
      0 => GridTrack::FitContent(Length::px(60.0)),
      1 => GridTrack::FitContent(Length::px(120.0)),
      2 => GridTrack::FitContent(Length::px(200.0)),
      // Alternate intrinsic mins to ensure min/max-content contributions are computed.
      3 => GridTrack::MinMax(
        Box::new(GridTrack::MinContent),
        Box::new(GridTrack::FitContent(Length::px(160.0))),
      ),
      4 => GridTrack::MinMax(
        Box::new(GridTrack::Length(Length::px(0.0))),
        Box::new(GridTrack::FitContent(Length::px(140.0))),
      ),
      _ => GridTrack::MinMax(
        Box::new(GridTrack::MinContent),
        Box::new(GridTrack::FitContent(Length::px(240.0))),
      ),
    };
    template.push(track);
  }
  grid_style.grid_template_columns = template;
  let grid_style = Arc::new(grid_style);

  let mut base_item_style = ComputedStyle::default();
  base_item_style.display = Display::Block;
  base_item_style.padding_left = Length::px(2.0);
  base_item_style.padding_right = Length::px(2.0);

  // Pre-build a small pool of item styles for each possible (start, span) combination we use so
  // Taffy style conversions can be cached and the benchmark stays dominated by track sizing.
  let mut style_pool: Vec<Vec<Arc<ComputedStyle>>> = Vec::with_capacity(5);
  for span in 2..=6 {
    let max_start = columns.saturating_sub(span) + 1;
    let mut styles = Vec::with_capacity(max_start);
    for start in 1..=max_start {
      let mut style = base_item_style.clone();
      style.grid_column_raw = Some(format!("{start} / span {span}"));
      styles.push(Arc::new(style));
    }
    style_pool.push(styles);
  }

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Block;
  let inline_style = Arc::new(inline_style);

  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let mut children = Vec::with_capacity(item_count);
  for idx in 0..item_count {
    let span = 2 + (idx % 5);
    let span_idx = span - 2;
    let styles = &style_pool[span_idx];
    let max_start = styles.len();
    // Stable pseudo-random start that spreads spans across many columns.
    let start_idx = (idx.wrapping_mul(17) + span * 3) % max_start;
    let item_style = styles[start_idx].clone();

    let text = BoxNode::new_text(text_style.clone(), TEXT.to_string());
    let inline = BoxNode::new_block(
      inline_style.clone(),
      FormattingContextType::Inline,
      vec![text],
    );
    children.push(BoxNode::new_block(
      item_style,
      FormattingContextType::Block,
      vec![inline],
    ));
  }

  let root = BoxNode::new_block(grid_style, FormattingContextType::Grid, children);
  BoxTree::new(root)
}

fn bench_flex_measure_hot_path(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  let viewport = Size::new(800.0, 600.0);
  let font_ctx = common::fixed_font_context();
  let engine = LayoutEngine::with_font_context(
    LayoutConfig::for_viewport(viewport).with_parallelism(LayoutParallelism::disabled()),
    font_ctx.clone(),
  );
  let mut cached_config =
    LayoutConfig::for_viewport(viewport).with_parallelism(LayoutParallelism::disabled());
  cached_config.enable_cache = true;
  let cached_engine = LayoutEngine::with_font_context(cached_config, font_ctx);
  // 96 flex items * ~3 nodes/item ~= 289 nodes total.
  let box_tree = build_flex_measure_tree(96);

  // Capture some counters once so the benchmark documents its own workload.
  {
    let _taffy_stats_guard = enable_taffy_counters(true);
    reset_taffy_counters();
    let _taffy_perf_guard = TaffyPerfCountersGuard::new();
    let _ = engine
      .layout_tree(black_box(&box_tree))
      .expect("flex layout should succeed");
    let perf = taffy_perf_counters();
    let usage = taffy_counters();
    // If these ever hit zero, this benchmark is no longer exercising the flex measurement path.
    assert!(perf.flex_measure_calls > 0);
    eprintln!(
      "layout_hotspots flex_measure: taffy_flex_measure_calls={} taffy_flex_compute_cpu_ms={:.2} taffy_nodes_built={} taffy_nodes_reused={}",
      perf.flex_measure_calls,
      perf.flex_compute_ns as f64 / 1_000_000.0,
      usage.flex_nodes_built,
      usage.flex_nodes_reused,
    );
  }

  let mut group = c.benchmark_group("layout_hotspots_flex_measure");
  // Keep the overall measurement window bounded under the micro Criterion configuration.
  // Linear sampling can choose an increasing iterations/sample schedule that exceeds the target
  // time for these medium-weight layout passes.
  group.sampling_mode(SamplingMode::Flat);
  group.bench_function("flex_layout_single_pass", |b| {
    b.iter(|| {
      let fragments = engine
        .layout_tree(black_box(&box_tree))
        .expect("flex layout should succeed");
      black_box(fragments);
    })
  });
  group.bench_function("flex_layout_single_pass_layout_cache", |b| {
    b.iter(|| {
      let fragments = cached_engine
        .layout_tree(black_box(&box_tree))
        .expect("flex layout should succeed");
      black_box(fragments);
    })
  });
  group.finish();
}

fn bench_flex_intrinsic_sizing(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  let viewport = Size::new(800.0, 600.0);
  let font_ctx = common::fixed_font_context();
  let factory = FormattingContextFactory::with_font_context_and_viewport(font_ctx, viewport)
    .with_parallelism(LayoutParallelism::disabled());
  let ffc = FlexFormattingContext::with_factory(factory);

  // Large enough to make the child-scan overhead measurable while remaining a microbench.
  let mut tree = build_flex_intrinsic_tree(512);
  // Disable global intrinsic caching for the root so each iteration recomputes the intrinsic widths.
  tree.root.id = 0;
  let node = &tree.root;

  // Warm intrinsic caches for descendants so the steady-state benchmark isolates the flex
  // container's child scanning.
  let _ = ffc
    .compute_intrinsic_inline_sizes(node)
    .expect("intrinsic sizing warmup should succeed");

  // Capture cache activity for a single intrinsic probe so the benchmark documents its workload.
  {
    let stats_engine = LayoutEngine::new(LayoutConfig::for_viewport(viewport));
    let before = stats_engine.stats();
    let (min, max) = ffc
      .compute_intrinsic_inline_sizes(node)
      .expect("intrinsic sizing probe should succeed");
    let after = stats_engine.stats();
    let delta_hits = after.cache_hits.saturating_sub(before.cache_hits);
    let delta_misses = after.cache_misses.saturating_sub(before.cache_misses);
    let delta_lookups = delta_hits + delta_misses;
    assert!(min > 0.0 && max > 0.0);
    assert!(delta_lookups > 0, "expected intrinsic cache activity");
    eprintln!(
      "layout_hotspots flex_intrinsic: items={} min={:.2} max={:.2} intrinsic_cache lookups={} hits={} misses={}",
      node.children.len(),
      min,
      max,
      delta_lookups,
      delta_hits,
      delta_misses
    );
  }

  let mut group = c.benchmark_group("layout_hotspots_flex_intrinsic");
  group.bench_function("min_content", |b| {
    b.iter(|| {
      let width = ffc
        .compute_intrinsic_inline_size(black_box(node), IntrinsicSizingMode::MinContent)
        .expect("intrinsic sizing should succeed");
      black_box(width);
    })
  });
  group.bench_function("max_content", |b| {
    b.iter(|| {
      let width = ffc
        .compute_intrinsic_inline_size(black_box(node), IntrinsicSizingMode::MaxContent)
        .expect("intrinsic sizing should succeed");
      black_box(width);
    })
  });
  group.bench_function("min_and_max", |b| {
    b.iter(|| {
      let min = ffc
        .compute_intrinsic_inline_size(black_box(node), IntrinsicSizingMode::MinContent)
        .expect("intrinsic sizing should succeed");
      let max = ffc
        .compute_intrinsic_inline_size(black_box(node), IntrinsicSizingMode::MaxContent)
        .expect("intrinsic sizing should succeed");
      black_box((min, max));
    })
  });
  // Exercise the combined min/max intrinsic sizing API (single child scan).
  group.bench_function("min_and_max_combined_api", |b| {
    b.iter(|| {
      let widths = ffc
        .compute_intrinsic_inline_sizes(black_box(node))
        .expect("intrinsic sizing should succeed");
      black_box(widths);
    })
  });
  group.finish();
}

fn bench_grid_spanning_distribution_hotspot(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  let viewport = Size::new(800.0, 600.0);
  let font_ctx = common::fixed_font_context();
  let engine = LayoutEngine::with_font_context(
    LayoutConfig::for_viewport(viewport).with_parallelism(LayoutParallelism::disabled()),
    font_ctx,
  );

  // Moderate-but-non-trivial item count keeps the benchmark fast under micro settings while still
  // producing enough spanning work to highlight regressions in `distribute_space_up_to_limits`.
  let mut box_tree = build_grid_spanning_distribution_tree(512, 30);
  // Use stable, non-zero ids so intrinsic/layout caches can participate within a single iteration.
  let mut next_id = 69_001usize;
  assign_box_ids(&mut box_tree.root, &mut next_id);

  // Capture counters once so the benchmark documents its workload.
  {
    let _taffy_stats_guard = enable_taffy_counters(true);
    reset_taffy_counters();
    let _taffy_perf_guard = TaffyPerfCountersGuard::new();
    let _ = engine
      .layout_tree(black_box(&box_tree))
      .expect("grid layout should succeed");
    let perf = taffy_perf_counters();
    let usage = taffy_counters();
    assert!(perf.grid_compute_ns > 0);
    eprintln!(
      "layout_hotspots grid_spanning_distribution: taffy_grid_measure_calls={} taffy_grid_compute_cpu_ms={:.2} taffy_nodes_built={} taffy_nodes_reused={}",
      perf.grid_measure_calls,
      perf.grid_compute_ns as f64 / 1_000_000.0,
      usage.grid_nodes_built,
      usage.grid_nodes_reused,
    );
  }

  let mut group = c.benchmark_group("layout_hotspots_grid");
  group.bench_function("grid_spanning_distribution_hotspot", |b| {
    b.iter(|| {
      let _taffy_perf_guard = TaffyPerfCountersGuard::new();
      let fragments = engine
       .layout_tree(black_box(&box_tree))
       .expect("grid layout should succeed");
      let perf = taffy_perf_counters();
      black_box((fragments, perf.grid_measure_calls, perf.grid_compute_ns));
    })
  });
  group.finish();
}

fn bench_float_shrink_to_fit_intrinsic_cache_reuse(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  let viewport = Size::new(800.0, 600.0);
  let font_ctx = common::fixed_font_context();
  let factory = FormattingContextFactory::with_font_context_and_viewport(font_ctx, viewport)
    .with_parallelism(LayoutParallelism::disabled());
  let bfc = BlockFormattingContext::with_factory(factory);
  // Enough floats to make intrinsic sizing meaningful without turning this into a full-page bench.
  let tree = build_float_shrink_to_fit_tree(64);
  let constraints = LayoutConstraints::definite(viewport.width, viewport.height);

  // Warm intrinsic caches as if a parent intrinsic probe (or flex/grid measurement pass) had
  // already walked the subtree.
  bfc
    .compute_intrinsic_inline_sizes(&tree.root)
    .expect("intrinsic sizing should succeed");

  // Capture cache usage for a single cached layout pass so the benchmark documents what it's
  // measuring (and ensures this scenario keeps producing cache hits).
  {
    let stats_engine = LayoutEngine::new(LayoutConfig::for_viewport(viewport));
    let before = stats_engine.stats();
    let _ = bfc
      .layout(&tree.root, &constraints)
      .expect("layout should succeed");
    let after = stats_engine.stats();
    let delta_hits = after.cache_hits.saturating_sub(before.cache_hits);
    let delta_misses = after.cache_misses.saturating_sub(before.cache_misses);
    let delta_lookups = delta_hits + delta_misses;
    let hit_rate = if delta_lookups > 0 {
      (delta_hits as f64 / delta_lookups as f64) * 100.0
    } else {
      0.0
    };
    assert!(
      delta_hits > 0,
      "expected intrinsic cache hits from cached float sizing"
    );
    eprintln!(
      "layout_hotspots float_shrink_to_fit_cached: intrinsic_cache lookups={} hits={} misses={} hit_rate={:.2}%",
      delta_lookups, delta_hits, delta_misses, hit_rate
    );
  }

  let mut group = c.benchmark_group("layout_hotspots_float_shrink_to_fit");
  group.bench_function("layout_cached_intrinsics", |b| {
    b.iter(|| {
      let fragment = bfc
        .layout(black_box(&tree.root), black_box(&constraints))
        .expect("layout should succeed");
      black_box(fragment);
    })
  });
  group.finish();
}

fn bench_block_intrinsic_sizing(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  let viewport = Size::new(800.0, 600.0);
  let font_ctx = common::fixed_font_context();
  let factory = FormattingContextFactory::with_font_context_and_viewport(font_ctx, viewport)
    .with_parallelism(LayoutParallelism::disabled());
  let bfc = BlockFormattingContext::with_factory(factory);
  let mut tree = build_block_intrinsic_tree(64);
  // Disable global intrinsic caching for the root so each iteration recomputes the intrinsic width.
  tree.root.id = 0;
  let node = &tree.root;

  let mut group = c.benchmark_group("layout_hotspots_block_intrinsic");
  group.bench_function("min_content", |b| {
    b.iter(|| {
      let width = bfc
        .compute_intrinsic_inline_size(black_box(node), IntrinsicSizingMode::MinContent)
        .expect("intrinsic sizing should succeed");
      black_box(width);
    })
  });
  group.bench_function("max_content", |b| {
    b.iter(|| {
      let width = bfc
        .compute_intrinsic_inline_size(black_box(node), IntrinsicSizingMode::MaxContent)
        .expect("intrinsic sizing should succeed");
      black_box(width);
    })
  });
  group.bench_function("min_and_max", |b| {
    b.iter(|| {
      let min = bfc
        .compute_intrinsic_inline_size(black_box(node), IntrinsicSizingMode::MinContent)
        .expect("intrinsic sizing should succeed");
      let max = bfc
        .compute_intrinsic_inline_size(black_box(node), IntrinsicSizingMode::MaxContent)
        .expect("intrinsic sizing should succeed");
      black_box((min, max));
    })
  });
  // Exercise the combined min/max intrinsic sizing API (single inline-item collection pass).
  group.bench_function("min_and_max_combined_api", |b| {
    b.iter(|| {
      let widths = bfc
        .compute_intrinsic_inline_sizes(black_box(node))
        .expect("intrinsic sizing should succeed");
      black_box(widths);
    })
  });
  group.finish();
}

fn bench_block_intrinsic_sizing_parallel(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  // Ensure the rayon global pool is installed with a conservative thread count so this benchmark
  // can run in constrained environments without panicking during Rayon's lazy initialization.
  if !ensure_rayon_global_pool_for_bench(4) || rayon::current_num_threads() <= 1 {
    eprintln!("layout_hotspots block_intrinsic_parallel: rayon pool unavailable; skipping");
    return;
  }
  let viewport = Size::new(800.0, 600.0);
  let font_ctx = common::fixed_font_context();
  let parallelism = LayoutParallelism::enabled(1).with_max_threads(Some(4));
  let factory = FormattingContextFactory::with_font_context_and_viewport(font_ctx, viewport)
    .with_parallelism(parallelism);
  let bfc = BlockFormattingContext::with_factory(factory);

  let mut tree = build_block_intrinsic_block_children_tree(256);
  // Disable global intrinsic caching so each iteration recomputes intrinsic widths.
  tree.root.id = 0;
  let node = &tree.root;
  assert!(
    parallelism.should_parallelize(node.children.len()),
    "expected benchmark tree to exceed parallel intrinsic sizing threshold (children={})",
    node.children.len()
  );

  let mut group = c.benchmark_group("layout_hotspots_block_intrinsic_parallel_block_children");
  group.bench_function("min_and_max_combined_api", |b| {
    b.iter(|| {
      let widths = bfc
        .compute_intrinsic_inline_sizes(black_box(node))
        .expect("intrinsic sizing should succeed");
      black_box(widths);
    })
  });
  group.finish();
}

fn bench_block_intrinsic_sizing_nowrap(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  let viewport = Size::new(800.0, 600.0);
  let font_ctx = common::fixed_font_context();
  let factory = FormattingContextFactory::with_font_context_and_viewport(font_ctx, viewport)
    .with_parallelism(LayoutParallelism::disabled());
  let bfc = BlockFormattingContext::with_factory(factory);
  let mut tree = build_block_intrinsic_tree_nowrap(64);
  // Disable global intrinsic caching for the root so each iteration recomputes the intrinsic width.
  tree.root.id = 0;
  // NOTE: `InlineFormattingContext` has a text inline-item cache keyed by box id. If the box ids stay
  // stable across iterations (as they would for a real DOM), the benchmark would measure steady-state
  // cache hits rather than text item construction / line-break scanning.
  //
  // To make this a targeted guardrail for the nowrap fast path (skipping `find_break_opportunities`
  // when `allow_soft_wrap == false`), reassign ids for text nodes each iteration so the cache misses.
  let mut id_seed = 1usize;

  let mut group = c.benchmark_group("layout_hotspots_block_intrinsic_nowrap");
  group.bench_function("min_content", |b| {
    b.iter(|| {
      let mut next_id = id_seed;
      assign_text_box_ids(&mut tree.root, &mut next_id);
      id_seed = next_id;
      let width = bfc
        .compute_intrinsic_inline_size(black_box(&tree.root), IntrinsicSizingMode::MinContent)
        .expect("intrinsic sizing should succeed");
      black_box(width);
    })
  });
  group.bench_function("max_content", |b| {
    b.iter(|| {
      let mut next_id = id_seed;
      assign_text_box_ids(&mut tree.root, &mut next_id);
      id_seed = next_id;
      let width = bfc
        .compute_intrinsic_inline_size(black_box(&tree.root), IntrinsicSizingMode::MaxContent)
        .expect("intrinsic sizing should succeed");
      black_box(width);
    })
  });
  group.finish();
}

fn bench_block_intrinsic_many_inline_runs(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  let viewport = Size::new(800.0, 600.0);
  let font_ctx = common::fixed_font_context();
  let factory = FormattingContextFactory::with_font_context_and_viewport(font_ctx, viewport)
    .with_parallelism(LayoutParallelism::disabled());
  let bfc = BlockFormattingContext::with_factory(factory);
  let mut tree = build_block_intrinsic_many_runs_tree(512);
  // Disable global intrinsic caching for the root so each iteration recomputes the intrinsic width.
  tree.root.id = 0;
  let node = &tree.root;

  let mut group = c.benchmark_group("layout_hotspots_block_intrinsic_many_runs");
  group.bench_function("min_content", |b| {
    b.iter(|| {
      let width = bfc
        .compute_intrinsic_inline_size(black_box(node), IntrinsicSizingMode::MinContent)
        .expect("intrinsic sizing should succeed");
      black_box(width);
    })
  });
  group.bench_function("max_content", |b| {
    b.iter(|| {
      let width = bfc
        .compute_intrinsic_inline_size(black_box(node), IntrinsicSizingMode::MaxContent)
        .expect("intrinsic sizing should succeed");
      black_box(width);
    })
  });
  group.finish();
}

fn bench_block_intrinsic_sizing_parallel_fanout(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  // Ensure the rayon global pool is installed with a conservative thread count so this benchmark
  // can run in constrained environments without panicking during Rayon's lazy initialization.
  if !ensure_rayon_global_pool_for_bench(4) {
    eprintln!("layout_hotspots block_intrinsic_parallel: rayon pool unavailable; skipping");
    return;
  }

  let viewport = Size::new(800.0, 600.0);
  let font_ctx = common::fixed_font_context();

  let serial_factory =
    FormattingContextFactory::with_font_context_and_viewport(font_ctx.clone(), viewport)
      .with_parallelism(LayoutParallelism::disabled());
  let serial_bfc = BlockFormattingContext::with_factory(serial_factory);

  let parallelism = LayoutParallelism::enabled(8).with_max_threads(Some(4));
  let parallel_factory =
    FormattingContextFactory::with_font_context_and_viewport(font_ctx, viewport)
      .with_parallelism(parallelism);
  let parallel_bfc = BlockFormattingContext::with_factory(parallel_factory);

  let mut tree = build_block_intrinsic_mixed_segments_tree(64);
  // Disable intrinsic caching for the root so each iteration recomputes intrinsic widths.
  tree.root.id = 0;
  let node = &tree.root;

  assert!(
    parallelism.should_parallelize(node.children.len()),
    "expected block intrinsic parallel bench tree to exceed fanout threshold (children={})",
    node.children.len()
  );

  // Sanity check: serial and parallel intrinsic sizing must match.
  let (serial_min, serial_max) = serial_bfc
    .compute_intrinsic_inline_sizes(node)
    .expect("serial intrinsic sizing should succeed");
  let (parallel_min, parallel_max) = parallel_bfc
    .compute_intrinsic_inline_sizes(node)
    .expect("parallel intrinsic sizing should succeed");
  let eps = 0.001;
  assert!(
    (serial_min - parallel_min).abs() < eps && (serial_max - parallel_max).abs() < eps,
    "intrinsic mismatch: serial=({serial_min},{serial_max}) parallel=({parallel_min},{parallel_max})"
  );

  let mut group = c.benchmark_group("layout_hotspots_block_intrinsic_parallel");
  group.bench_function("serial_min_and_max_combined_api", |b| {
    b.iter(|| {
      let widths = serial_bfc
        .compute_intrinsic_inline_sizes(black_box(node))
        .expect("intrinsic sizing should succeed");
      black_box(widths);
    })
  });
  group.bench_function("parallel_min_and_max_combined_api", |b| {
    b.iter(|| {
      let widths = parallel_bfc
        .compute_intrinsic_inline_sizes(black_box(node))
        .expect("intrinsic sizing should succeed");
      black_box(widths);
    })
  });
  group.finish();
}

fn bench_block_intrinsic_sizing_parallel_auto(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  // Ensure the rayon global pool is installed with a conservative thread count so this benchmark
  // can run in constrained environments without panicking during Rayon's lazy initialization.
  if !ensure_rayon_global_pool_for_bench(4) {
    eprintln!("layout_hotspots block_intrinsic_parallel_auto: rayon pool unavailable; skipping");
    return;
  }

  let viewport = Size::new(800.0, 600.0);
  let font_ctx = common::fixed_font_context();

  let mut tree = build_block_intrinsic_mixed_segments_tree(64);
  // Disable intrinsic caching for the root so each iteration recomputes intrinsic widths.
  tree.root.id = 0;
  let node = &tree.root;

  let min_fanout = 8;
  let workload = layout_parallelism_workload(&tree, min_fanout);
  let parallelism = LayoutParallelism::auto(min_fanout)
    .with_auto_min_nodes(1)
    .with_max_threads(Some(4))
    .resolve_for_workload(workload);

  if !parallelism.should_parallelize(node.children.len()) {
    eprintln!(
      "layout_hotspots block_intrinsic_parallel_auto: auto parallelism not activated (children={} nodes={} workers={}); skipping",
      node.children.len(),
      workload.nodes,
      parallelism.expected_workers()
    );
    return;
  }

  let serial_factory =
    FormattingContextFactory::with_font_context_and_viewport(font_ctx.clone(), viewport)
      .with_parallelism(LayoutParallelism::disabled());
  let serial_bfc = BlockFormattingContext::with_factory(serial_factory);

  let auto_factory = FormattingContextFactory::with_font_context_and_viewport(font_ctx, viewport)
    .with_parallelism(parallelism);
  let auto_bfc = BlockFormattingContext::with_factory(auto_factory);

  // Sanity check: serial and auto-parallel intrinsic sizing must match.
  let (serial_min, serial_max) = serial_bfc
    .compute_intrinsic_inline_sizes(node)
    .expect("serial intrinsic sizing should succeed");
  let (auto_min, auto_max) = auto_bfc
    .compute_intrinsic_inline_sizes(node)
    .expect("auto intrinsic sizing should succeed");
  let eps = 0.001;
  assert!(
    (serial_min - auto_min).abs() < eps && (serial_max - auto_max).abs() < eps,
    "intrinsic mismatch: serial=({serial_min},{serial_max}) auto=({auto_min},{auto_max})"
  );

  let mut group = c.benchmark_group("layout_hotspots_block_intrinsic_parallel_auto");
  group.bench_function("auto_min_and_max_combined_api", |b| {
    b.iter(|| {
      let widths = auto_bfc
        .compute_intrinsic_inline_sizes(black_box(node))
        .expect("intrinsic sizing should succeed");
      black_box(widths);
    })
  });
  group.finish();
}

fn bench_float_shrink_to_fit_sizing(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  let viewport = Size::new(800.0, 600.0);
  let font_ctx = common::fixed_font_context();
  let bfc = BlockFormattingContext::with_font_context_and_viewport(font_ctx, viewport);
  let box_tree = build_float_shrink_to_fit_tree(128);
  let constraints = LayoutConstraints::definite(800.0, 600.0);

  // Warm intrinsic caches for a steady-state benchmark.
  let _ = bfc.layout(&box_tree.root, &constraints);

  c.bench_function("layout_hotspots_float_shrink_to_fit_cached", |b| {
    b.iter(|| {
      let fragment = bfc
        .layout(black_box(&box_tree.root), black_box(&constraints))
        .expect("layout should succeed");
      black_box(fragment);
    })
  });
}

fn bench_table_cell_intrinsic_and_distribution(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  let viewport = Size::new(960.0, 720.0);
  let font_ctx = common::fixed_font_context();
  let factory = FormattingContextFactory::with_font_context_and_viewport(font_ctx, viewport)
    .with_parallelism(LayoutParallelism::disabled());
  let tfc = TableFormattingContext::with_factory(factory);

  let table_box = build_table_tree(12, 12);
  let structure = TableStructure::from_box_tree(&table_box);
  let available_content_width = 960.0;
  let percent_base = Some(available_content_width);

  let mut group = c.benchmark_group("layout_hotspots_table_intrinsic");
  group.bench_function("cell_intrinsic_and_distribution_12x12", |b| {
    b.iter(|| {
      let widths = match tfc.bench_column_constraints_and_distribute(
        black_box(&table_box),
        black_box(&structure),
        black_box(available_content_width),
        percent_base,
      ) {
        Ok(widths) => widths,
        Err(err) => {
          eprintln!("Skipping bench iteration due to table distribution error: {err}");
          Vec::new()
        }
      };
      black_box(widths);
    })
  });
  group.finish();
}

fn bench_grid_track_sizing_measure_fanout(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  // Keep this large enough to stress grid intrinsic sizing fanout, but small enough to stay fast
  // under the micro Criterion settings on typical developer machines/CI.
  const GRID_ITEM_COUNT: usize = 192;
  let viewport = Size::new(800.0, 600.0);
  let font_ctx = common::fixed_font_context();
  let engine = LayoutEngine::with_font_context(
    LayoutConfig::for_viewport(viewport).with_parallelism(LayoutParallelism::disabled()),
    font_ctx,
  );

  // `GRID_ITEM_COUNT` grid items * ~3 nodes/item ~= O(700) nodes total.
  let box_tree = build_grid_track_sizing_measure_tree(GRID_ITEM_COUNT);

  // Document the workload once (and assert we are actually exercising grid measurement).
  {
    let _taffy_usage_guard = enable_taffy_counters(true);
    reset_taffy_counters();
    let _taffy_perf_guard = TaffyPerfCountersGuard::new();
    let _ = engine
      .layout_tree(black_box(&box_tree))
      .expect("grid layout should succeed");
    let perf = taffy_perf_counters();
    let usage = taffy_counters();
    assert!(perf.grid_measure_calls > 0);
    let calls_per_item = perf.grid_measure_calls as f64 / GRID_ITEM_COUNT as f64;
    eprintln!(
      "layout_hotspots grid_track_sizing: grid_items={} taffy_grid_measure_calls={} taffy_grid_measure_calls_per_item={:.2} taffy_grid_compute_cpu_ms={:.2} taffy_nodes_built={} taffy_nodes_reused={}",
      GRID_ITEM_COUNT,
      perf.grid_measure_calls,
      calls_per_item,
      perf.grid_compute_ns as f64 / 1_000_000.0,
      usage.grid_nodes_built,
      usage.grid_nodes_reused,
    );
  }

  // Enable counters for the benchmark body. (We reset them per-iteration below.)
  let _taffy_usage_guard = enable_taffy_counters(true);

  let mut group = c.benchmark_group("layout_hotspots_grid_track_sizing");
  // This benchmark tends to be heavier than the other "micro" hotspots. Keep its measurement
  // window smaller so `cargo bench --bench layout_hotspots` stays quick.
  group.warm_up_time(Duration::from_millis(100));
  group.measurement_time(Duration::from_millis(400));
  // Use flat sampling so Criterion doesn't vary the iterations/sample across samples. That keeps
  // the per-iteration counter resets + measure fanout easier to interpret.
  group.sampling_mode(SamplingMode::Flat);
  group.throughput(Throughput::Elements(GRID_ITEM_COUNT as u64));
  group.bench_function("grid_track_sizing_measure_fanout", |b| {
    b.iter(|| {
      reset_taffy_counters();
      let _taffy_perf_guard = TaffyPerfCountersGuard::new();
      let fragments = engine
        .layout_tree(black_box(&box_tree))
        .expect("grid layout should succeed");
      let perf = taffy_perf_counters();
      let usage = taffy_counters();
      black_box(fragments);
      // Keep the counters live so compiler/LTO cannot elide the instrumentation path.
      black_box(perf.grid_measure_calls);
      black_box(perf.grid_compute_ns);
      black_box((usage.grid_nodes_built, usage.grid_nodes_reused));
    })
  });
  group.finish();
}

fn bench_inline_layout_cache(c: &mut Criterion) {
  common::bench_print_config_once("layout_hotspots", &[]);
  let viewport = Size::new(480.0, 600.0);
  let font_ctx = common::fixed_font_context();
  let mut config =
    LayoutConfig::for_viewport(viewport).with_parallelism(LayoutParallelism::disabled());
  config.enable_cache = true;
  let engine = LayoutEngine::with_font_context(config, font_ctx);

  let mut box_tree = build_inline_cache_tree(128);
  let mut next_id = 1usize;
  assign_box_ids(&mut box_tree.root, &mut next_id);

  let _ = engine
    // Warm using `layout_tree_reuse_caches` so the engine stores a stable run fingerprint.
    // `layout_tree()` runs with `reset_caches=true` (because `enable_incremental` is false by
    // default) which skips the fingerprint computation. That would cause the next reuse-caches
    // run to detect a "fingerprint change", bump the cache epoch, and yield 0 cache hits.
    .layout_tree_reuse_caches(black_box(&box_tree))
    .expect("inline layout warmup should succeed");
  let before = engine.stats();
  let _ = engine
    .layout_tree_reuse_caches(black_box(&box_tree))
    .expect("inline layout cache probe should succeed");
  let after = engine.stats();
  assert!(
    after.layout_cache_hits > before.layout_cache_hits,
    "expected inline layout cache hit after warmup (before={} after={})",
    before.layout_cache_hits,
    after.layout_cache_hits
  );
  eprintln!(
    "layout_hotspots inline_layout_cache: layout_cache_hits={} layout_cache_misses={} layout_cache_clones={}",
    after.layout_cache_hits, after.layout_cache_misses, after.layout_cache_clones
  );

  let mut group = c.benchmark_group("layout_hotspots_inline_cache");
  group.bench_function("inline_layout_cached", |b| {
    b.iter(|| {
      let fragments = engine
        .layout_tree_reuse_caches(black_box(&box_tree))
        .expect("inline layout should succeed");
      black_box(fragments);
    })
  });
  group.finish();
}

criterion_group!(
  name = benches;
  config = micro_criterion();
  targets =
    bench_flex_measure_hot_path,
    bench_flex_intrinsic_sizing,
    bench_grid_track_sizing_measure_fanout,
    bench_grid_spanning_distribution_hotspot,
    bench_float_shrink_to_fit_intrinsic_cache_reuse,
    bench_block_intrinsic_sizing,
    bench_block_intrinsic_sizing_parallel,
    bench_block_intrinsic_sizing_nowrap,
    bench_block_intrinsic_many_inline_runs,
    bench_block_intrinsic_sizing_parallel_fanout,
    bench_block_intrinsic_sizing_parallel_auto,
    bench_float_shrink_to_fit_sizing,
    bench_table_cell_intrinsic_and_distribution,
    bench_inline_layout_cache
);
criterion_main!(benches);
