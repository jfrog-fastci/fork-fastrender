#![cfg(feature = "taffy_tree")]

use crate::prelude::*;
use crate::{MeasureOutput, TaffyTree};

fn default<T: Default>() -> T {
  T::default()
}

/// Regression test: when the grid container width is definite, we still need to rerun column sizing
/// when an item's inline intrinsic contribution can change after the row sizing pass.
///
/// The only current cross-axis coupling in `GridItem::known_dimensions` is `aspect_ratio`.
#[test]
#[cfg(feature = "detailed_layout_info")]
fn rerun_column_sizing_still_occurs_for_aspect_ratio_items_with_definite_width() {
  use crate::tree::DetailedLayoutInfo;

  let mut taffy: TaffyTree<()> = TaffyTree::new();

  // This item's min-content width is derived from its stretched height via aspect-ratio.
  let aspect_item = taffy
    .new_leaf(Style {
      grid_row: line(1),
      grid_column: line(1),
      aspect_ratio: Some(2.0),
      ..default()
    })
    .unwrap();

  // This sibling forces the auto row to resolve to a definite height after the block-axis pass.
  let row_sizer = taffy
    .new_leaf(Style {
      grid_row: line(1),
      grid_column: line(2),
      size: Size {
        width: auto(),
        height: length(50.0),
      },
      ..default()
    })
    .unwrap();

  let root = taffy
    .new_with_children(
      Style {
        display: Display::Grid,
        size: Size {
          width: length(200.0),
          height: auto(),
        },
        grid_template_columns: vec![min_content(), min_content()],
        grid_template_rows: vec![auto()],
        ..default()
      },
      &[aspect_item, row_sizer],
    )
    .unwrap();

  taffy
    .compute_layout_with_measure(
      root,
      Size::MAX_CONTENT,
      |known_dimensions, _available_space, _node_id, _context, _style| {
        // If `known_dimensions.width` becomes definite (e.g. from stretched height + aspect ratio),
        // report that as the measured width so it feeds into min-content contributions.
        MeasureOutput::from_size(Size {
          width: known_dimensions.width.unwrap_or(10.0),
          height: known_dimensions.height.unwrap_or(10.0),
        })
      },
    )
    .unwrap();

  let detailed = match taffy.detailed_layout_info(root) {
    DetailedLayoutInfo::Grid(info) => info.as_ref(),
    other => panic!("expected detailed grid info, got {other:?}"),
  };

  let column_width = detailed.columns.sizes[0];
  assert!(
    (column_width - 100.0).abs() < 0.01,
    "expected the min-content column to be sized from the aspect-ratio item's rerun contribution (100px), got {column_width}"
  );
}

/// When the grid container width is already definite, column rerun detection should only remeasure
/// inline min-content contributions for items that can actually depend on row sizes.
///
/// For now, that's aspect-ratio items. This test asserts that we *don't* call the leaf measure
/// function in the "inline min-content, block definite" configuration that would result from the
/// post-row-sizing rerun probe.
#[test]
fn rerun_column_sizing_probe_skips_non_aspect_ratio_items_when_width_definite() {
  use std::cell::Cell;

  let mut taffy: TaffyTree<()> = TaffyTree::new();

  let item_a = taffy
    .new_leaf(Style {
      grid_row: line(1),
      grid_column: line(1),
      ..default()
    })
    .unwrap();

  let item_b = taffy
    .new_leaf(Style {
      grid_row: line(1),
      grid_column: line(2),
      ..default()
    })
    .unwrap();

  let root = taffy
    .new_with_children(
      Style {
        display: Display::Grid,
        size: Size {
          width: length(200.0),
          height: auto(),
        },
        grid_template_columns: vec![min_content(), min_content()],
        grid_template_rows: vec![auto()],
        ..default()
      },
      &[item_a, item_b],
    )
    .unwrap();

  let rerun_probe_calls = Cell::new(0);
  taffy
    .compute_layout_with_measure(
      root,
      Size::MAX_CONTENT,
      |known_dimensions, available_space, _node_id, _context, _style| {
        // The post-row-sizing rerun probe (in `compute/grid/mod.rs`) measures inline min-content
        // contributions with a definite block-axis estimate.
        //
        // If we hit this configuration in this test, it means the probe ran when it shouldn't
        // have (no aspect-ratio items + definite container width).
        if matches!(available_space.width, AvailableSpace::MinContent)
          && matches!(available_space.height, AvailableSpace::Definite(_))
        {
          rerun_probe_calls.set(rerun_probe_calls.get() + 1);
        }

        // Give the items a non-zero intrinsic height so the auto row resolves to a definite size
        // after the first block-axis sizing pass.
        MeasureOutput::from_size(Size {
          width: known_dimensions.width.unwrap_or(10.0),
          height: known_dimensions.height.unwrap_or(50.0),
        })
      },
    )
    .unwrap();

  assert_eq!(
    rerun_probe_calls.get(),
    0,
    "expected no inline min-content rerun-probe measurements when the container width is definite and no items have aspect_ratio"
  );
}
