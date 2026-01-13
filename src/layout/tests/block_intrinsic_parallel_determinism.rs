use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::engine::LayoutParallelism;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::{Display, FormattingContextType};
use crate::style::float::{Clear, Float};
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

fn approx_eq(a: f32, b: f32) -> bool {
  (a - b).abs() < 0.001
}

#[test]
fn block_intrinsic_sizes_parallel_matches_serial() {
  // Ensure the global Rayon pool is initialized with FastRender's conservative defaults so the
  // parallel intrinsic-sizing path doesn't trip Rayon's lazy init in constrained test runners.
  crate::rayon_global::ensure_global_pool().expect("rayon global pool");

  let text_style = Arc::new(ComputedStyle::default());

  let text = |s: &str| BoxNode::new_text(text_style.clone(), s.to_string());

  let make_block_child = |width: f32, ml: f32, mr: f32, label: &str| {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.width = Some(Length::px(width));
    style.margin_left = Some(Length::px(ml));
    style.margin_right = Some(Length::px(mr));
    let inner = BoxNode::new_text(text_style.clone(), format!("block-inner-{label}"));
    BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![inner])
  };

  let make_float = |width: f32, float: Float, clear: Clear, ml: f32, mr: f32, label: &str| {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.width = Some(Length::px(width));
    style.float = float;
    style.clear = clear;
    style.margin_left = Some(Length::px(ml));
    style.margin_right = Some(Length::px(mr));
    let inner = BoxNode::new_text(text_style.clone(), format!("float-inner-{label}"));
    BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![inner])
  };

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;

  // Build a container with:
  // - multiple inline runs (text nodes),
  // - block children,
  // - floats with margins and a `clear` that forces a new float line.
  let mut children = Vec::new();

  // Inline run #1.
  for i in 0..12 {
    children.push(text(&format!("run1-{i} ")));
  }

  // Float line #1.
  children.push(make_float(60.0, Float::Left, Clear::None, 2.0, 4.0, "f1"));
  children.push(make_float(80.0, Float::Right, Clear::None, 3.0, 1.0, "f2"));

  // Inline run #2.
  for i in 0..8 {
    children.push(text(&format!("run2-{i} ")));
  }

  // Block child #1.
  children.push(make_block_child(120.0, 5.0, 7.0, "b1"));

  // Inline run #3.
  for i in 0..10 {
    children.push(text(&format!("run3-{i} ")));
  }

  // Float line #2, forced by `clear`.
  children.push(make_float(
    40.0,
    Float::Left,
    Clear::Both,
    1.0,
    2.0,
    "f3-clear",
  ));
  children.push(make_float(70.0, Float::Right, Clear::None, 4.0, 5.0, "f4"));

  // Block child #2.
  children.push(make_block_child(90.0, 11.0, 3.0, "b2"));

  // Inline run #4.
  for i in 0..6 {
    children.push(text(&format!("run4-{i} ")));
  }

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    children,
  );

  let serial = BlockFormattingContext::new().with_parallelism(LayoutParallelism::disabled());
  let parallelism = LayoutParallelism::enabled(2).with_max_threads(Some(2));
  assert!(
    parallelism.should_parallelize(container.children.len()),
    "expected test tree to exceed parallel intrinsic sizing threshold (children={})",
    container.children.len()
  );
  let parallel = BlockFormattingContext::new().with_parallelism(parallelism);

  let (serial_min, serial_max) = serial
    .compute_intrinsic_inline_sizes(&container)
    .expect("serial intrinsic sizing");
  let (parallel_min, parallel_max) = parallel
    .compute_intrinsic_inline_sizes(&container)
    .expect("parallel intrinsic sizing");

  assert!(
    approx_eq(serial_min, parallel_min),
    "min-content mismatch: serial={serial_min} parallel={parallel_min}"
  );
  assert!(
    approx_eq(serial_max, parallel_max),
    "max-content mismatch: serial={serial_max} parallel={parallel_max}"
  );
}
