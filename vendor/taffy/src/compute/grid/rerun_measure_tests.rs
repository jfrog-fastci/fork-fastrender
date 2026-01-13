//! Performance regression tests for grid track-sizing rerun logic.
//!
//! These tests are "perf in the small": they validate that we don't fan out into an O(n) set of
//! additional intrinsic measurements when the result cannot change (e.g. when there are no
//! aspect-ratio items that could introduce cross-axis dependencies).

#[cfg(all(test, feature = "taffy_tree"))]
mod tests {
  use crate::compute::{compute_grid_layout, compute_leaf_layout, compute_root_layout};
  use crate::geometry::Point;
  use crate::prelude::*;
  use crate::sys::DefaultCheapStr;
  use crate::tree::{
    Layout, LayoutGridContainer, LayoutInput, LayoutOutput, LayoutPartialTree, MeasureOutput,
    NodeId, TaffyTree, TraversePartialTree,
  };
  use std::cell::Cell;

  /// Minimal test tree that avoids the high-level `TaffyTree` leaf-measure cache canonicalization.
  ///
  /// The perf regression we care about here is extra *tree* measurement fanout caused by the grid
  /// rerun probes. `TaffyTree::compute_layout_with_measure` intentionally deduplicates many of these
  /// probes for leaf nodes; for this unit test we want to observe the raw `measure_child_size`
  /// fanout.
  struct TestTree {
    nodes: Vec<TestNode>,
    intrinsic_probe_count: Cell<usize>,
  }

  struct TestNode {
    style: Style,
    children: Vec<NodeId>,
    layout: Layout,
  }

  impl TestTree {
    fn new() -> Self {
      Self { nodes: Vec::new(), intrinsic_probe_count: Cell::new(0) }
    }

    fn new_leaf(&mut self, style: Style) -> NodeId {
      let id = NodeId::from(self.nodes.len());
      self.nodes.push(TestNode { style, children: Vec::new(), layout: Layout::new() });
      id
    }

    fn new_with_children(&mut self, style: Style, children: Vec<NodeId>) -> NodeId {
      let id = NodeId::from(self.nodes.len());
      self.nodes.push(TestNode { style, children, layout: Layout::new() });
      id
    }
  }

  impl TraversePartialTree for TestTree {
    type ChildIter<'a>
      = core::iter::Copied<core::slice::Iter<'a, NodeId>>
    where
      Self: 'a;

    fn child_ids(&self, parent_node_id: NodeId) -> Self::ChildIter<'_> {
      let idx: usize = parent_node_id.into();
      self.nodes[idx].children.iter().copied()
    }

    fn child_count(&self, parent_node_id: NodeId) -> usize {
      let idx: usize = parent_node_id.into();
      self.nodes[idx].children.len()
    }

    fn get_child_id(&self, parent_node_id: NodeId, child_index: usize) -> NodeId {
      let idx: usize = parent_node_id.into();
      self.nodes[idx].children[child_index]
    }
  }

  impl LayoutPartialTree for TestTree {
    type CoreContainerStyle<'a>
      = &'a Style
    where
      Self: 'a;

    type CustomIdent = DefaultCheapStr;

    fn get_core_container_style(&self, node_id: NodeId) -> Self::CoreContainerStyle<'_> {
      let idx: usize = node_id.into();
      &self.nodes[idx].style
    }

    fn set_unrounded_layout(&mut self, node_id: NodeId, layout: &Layout) {
      let idx: usize = node_id.into();
      self.nodes[idx].layout = *layout;
    }

    fn compute_child_layout(&mut self, node_id: NodeId, inputs: LayoutInput) -> LayoutOutput {
      let idx: usize = node_id.into();
      let style = self.nodes[idx].style.clone();
      let has_children = !self.nodes[idx].children.is_empty();

      match (style.display, has_children) {
        (Display::Grid, _) => compute_grid_layout(self, node_id, inputs),
        (_, false) => {
          let counter = &self.intrinsic_probe_count;
          compute_leaf_layout(inputs, &style, |_, _| 0.0, |known_dimensions, available_space| {
            let is_intrinsic_probe = matches!(
              available_space.width,
              AvailableSpace::MinContent | AvailableSpace::MaxContent
            ) || matches!(
              available_space.height,
              AvailableSpace::MinContent | AvailableSpace::MaxContent
            );
            if is_intrinsic_probe {
              counter.set(counter.get() + 1);
            }

            MeasureOutput {
              size: Size {
                width: known_dimensions.width.unwrap_or(10.0),
                height: known_dimensions.height.unwrap_or(10.0),
              },
              first_baselines: Point::NONE,
            }
          })
        }
        // Not needed for this test.
        (_, true) => compute_leaf_layout(inputs, &style, |_, _| 0.0, |_, _| {
          MeasureOutput::from_size(Size::ZERO)
        }),
      }
    }
  }

  impl LayoutGridContainer for TestTree {
    type GridContainerStyle<'a>
      = &'a Style
    where
      Self: 'a;

    type GridItemStyle<'a>
      = &'a Style
    where
      Self: 'a;

    fn get_grid_container_style(&self, node_id: NodeId) -> Self::GridContainerStyle<'_> {
      self.get_core_container_style(node_id)
    }

    fn get_grid_child_style(&self, child_node_id: NodeId) -> Self::GridItemStyle<'_> {
      self.get_core_container_style(child_node_id)
    }

    fn clone_grid_container_style(&self, node_id: NodeId) -> Style {
      self.get_core_container_style(node_id).clone()
    }

    fn clone_grid_child_style(&self, child_node_id: NodeId) -> Style {
      self.get_core_container_style(child_node_id).clone()
    }
  }

  #[test]
  fn grid_intrinsic_rerun_scan_skips_non_aspect_ratio_items() {
    // Large enough for the rerun scan to be meaningful, but small enough for a unit test.
    const COLUMN_COUNT: usize = 8;
    const CHILD_COUNT: usize = 128;

    let mut tree = TestTree::new();

    let child_style = Style { ..Default::default() };
    let mut children = Vec::with_capacity(CHILD_COUNT);
    for _ in 0..CHILD_COUNT {
      children.push(tree.new_leaf(child_style.clone()));
    }

    let root = tree.new_with_children(
      Style {
        display: Display::Grid,
        // Make the container size definite to reduce unrelated intrinsic sizing work.
        size: Size::from_lengths(800.0, 600.0),
        // Intrinsic columns: forces the inline-axis track sizing pass to query min-content sizes.
        grid_template_columns: vec![min_content(); COLUMN_COUNT],
        // Intrinsic implicit rows: ensures the initial inline-axis measurements happen with unknown
        // block-axis sizes (so the rerun scan would historically remeasure every item).
        grid_auto_rows: vec![min_content()],
        ..Default::default()
      },
      children,
    );

    compute_root_layout(
      &mut tree,
      root,
      Size {
        width: AvailableSpace::Definite(800.0),
        height: AvailableSpace::Definite(600.0),
      },
    );

    let intrinsic_probe_count = tree.intrinsic_probe_count.get();

    // With no aspect-ratio children, the rerun scan should be skipped, avoiding an additional
    // intrinsic measurement per child in the inline axis.
    //
    // Empirically, we expect ~2 intrinsic probes per child (one for inline sizing, one for block
    // sizing). If the O(n) rerun scan remeasures every item, this count jumps by ~CHILD_COUNT.
    let upper_bound = CHILD_COUNT * 5 / 2; // 2.5x
    assert!(
      intrinsic_probe_count < upper_bound,
      "unexpected intrinsic probe fanout: {intrinsic_probe_count} probes for {CHILD_COUNT} children (expected < {upper_bound})"
    );
  }

  #[test]
  fn grid_intrinsic_rerun_scan_skips_aspect_ratio_items_when_inline_tracks_are_unaffected() {
    // The inline-axis rerun scan only needs to probe aspect-ratio items if their min-content
    // contributions could affect track sizing. In the flex batch, that requires a flexible track
    // whose *minimum* sizing function is intrinsic.
    //
    // If an aspect-ratio item spans a flexible track with a definite min (e.g. `minmax(0, 1fr)`),
    // then the scan is guaranteed to be a no-op and should not trigger any intrinsic probes.
    let mut taffy: TaffyTree<()> = TaffyTree::new();

    let child = taffy
      .new_leaf(Style {
        aspect_ratio: Some(1.0),
        grid_column: Line {
          start: line(1),
          end: span(2),
        },
        grid_row: Line {
          start: line(1),
          end: span(1),
        },
        ..Default::default()
      })
      .unwrap();

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          size: Size::from_lengths(200.0, 200.0),
          // First track is flexible but has a definite min (minmax(0, 1fr)).
          // Second track is intrinsic so the item crosses intrinsic columns and triggers rerun logic.
          grid_template_columns: vec![flex(1.0), auto()],
          grid_template_rows: vec![length(10.0); 1],
          ..Default::default()
        },
        &[child],
      )
      .unwrap();

    let mut inline_intrinsic_probe_count = 0usize;
    taffy
      .compute_layout_with_measure(
        root,
        Size {
          width: AvailableSpace::Definite(200.0),
          height: AvailableSpace::Definite(200.0),
        },
        |_, available_space, _, _, _| {
          if matches!(
            available_space.width,
            AvailableSpace::MinContent | AvailableSpace::MaxContent
          ) {
            inline_intrinsic_probe_count += 1;
          }

          MeasureOutput {
            size: Size { width: 10.0, height: 10.0 },
            first_baselines: Point { x: None, y: None },
          }
        },
      )
      .unwrap();

    assert_eq!(inline_intrinsic_probe_count, 0);
  }

  #[test]
  fn grid_intrinsic_rerun_scan_skips_aspect_ratio_items_when_block_tracks_are_unaffected() {
    // Same as the inline-axis test above, but for the block-axis rerun scan.
    let mut taffy: TaffyTree<()> = TaffyTree::new();

    let child = taffy
      .new_leaf(Style {
        aspect_ratio: Some(1.0),
        grid_row: Line {
          start: line(1),
          end: span(2),
        },
        grid_column: Line {
          start: line(1),
          end: span(1),
        },
        ..Default::default()
      })
      .unwrap();

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          size: Size::from_lengths(200.0, 200.0),
          grid_template_columns: vec![length(10.0); 1],
          // First row is flexible but has a definite min (minmax(0, 1fr)).
          // Second row is intrinsic so the item crosses intrinsic rows and triggers rerun logic.
          grid_template_rows: vec![flex(1.0), auto()],
          ..Default::default()
        },
        &[child],
      )
      .unwrap();

    let mut block_intrinsic_probe_count = 0usize;
    taffy
      .compute_layout_with_measure(
        root,
        Size {
          width: AvailableSpace::Definite(200.0),
          height: AvailableSpace::Definite(200.0),
        },
        |_, available_space, _, _, _| {
          if matches!(
            available_space.height,
            AvailableSpace::MinContent | AvailableSpace::MaxContent
          ) {
            block_intrinsic_probe_count += 1;
          }

          MeasureOutput {
            size: Size { width: 10.0, height: 10.0 },
            first_baselines: Point { x: None, y: None },
          }
        },
      )
      .unwrap();

    assert_eq!(block_intrinsic_probe_count, 0);
  }
}
