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

/// Regression test: after the row sizing pass, Taffy may rerun column sizing if an aspect-ratio
/// item's inline min-content contribution depends on the resolved row height. If that rerun changes
/// column sizes, then row sizing may also need to rerun because an aspect-ratio item's block
/// min-content contribution can depend on the resolved column width.
#[test]
#[cfg(feature = "detailed_layout_info")]
fn rerun_row_sizing_still_occurs_for_aspect_ratio_items_with_definite_height() {
  use crate::tree::DetailedLayoutInfo;

  let mut taffy: TaffyTree<()> = TaffyTree::new();

  // This item's min-content width is derived from its stretched height via aspect-ratio. After the
  // block-axis sizing pass resolves row 1 to a definite height, the inline-axis rerun should grow
  // column 1 based on this item's updated width contribution.
  let width_from_height = taffy
    .new_leaf(Style {
      grid_row: line(1),
      grid_column: line(1),
      aspect_ratio: Some(2.0),
      ..default()
    })
    .unwrap();

  // Force row 1 to resolve to a definite height.
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

  // This item's min-content height is derived from its stretched width via aspect-ratio. If column
  // 1 changes after the inline-axis rerun, row sizing must rerun so row 2 sees the updated
  // contribution.
  let height_from_width = taffy
    .new_leaf(Style {
      grid_row: line(2),
      grid_column: line(1),
      aspect_ratio: Some(1.0),
      ..default()
    })
    .unwrap();

  let root = taffy
    .new_with_children(
      Style {
        display: Display::Grid,
        // Make the container height definite so we exercise the rerun logic even when there are
        // no percentage-based reasons to rerun row sizing.
        size: Size {
          width: length(200.0),
          height: length(200.0),
        },
        grid_template_columns: vec![min_content(), min_content()],
        grid_template_rows: vec![min_content(), min_content()],
        ..default()
      },
      &[width_from_height, row_sizer, height_from_width],
    )
    .unwrap();

  taffy
    .compute_layout_with_measure(
      root,
      Size::MAX_CONTENT,
      |known_dimensions, _available_space, _node_id, _context, _style| {
        // Surface any definite known dimensions into the reported size so intrinsic contributions
        // can observe aspect-ratio derived sizes.
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
    "expected column 1 to be sized from the aspect-ratio item's rerun contribution (100px), got {column_width}"
  );

  let row_height = detailed.rows.sizes[1];
  assert!(
    (row_height - 100.0).abs() < 0.01,
    "expected row 2 to be sized from the aspect-ratio item's rerun contribution (100px), got {row_height}"
  );
}

/// Regression test: the column rerun probe must refresh intrinsic caches for *all* probed items
/// before rerun sizing occurs.
///
/// The probe historically used `.any(...)` while mutating per-item intrinsic caches. Because `.any`
/// short-circuits, only the first changed item would have its cache refreshed. If another probed
/// item also changes (e.g. multiple aspect-ratio items in different columns), rerun column sizing
/// would reuse stale cached contributions and compute incorrect track sizes.
#[test]
#[cfg(feature = "detailed_layout_info")]
fn rerun_column_sizing_refreshes_all_aspect_ratio_items_before_rerun() {
  use crate::tree::DetailedLayoutInfo;

  let mut taffy: TaffyTree<()> = TaffyTree::new();

  // Both items derive their inline intrinsic contributions from the (stretched) row height via
  // aspect ratio. After the block-axis sizing pass, the row becomes definite (from container
  // height + stretch), so both items' min-content widths change and must be reflected in the rerun
  // column sizing pass.
  let col_1_item = taffy
    .new_leaf(Style {
      grid_row: line(1),
      grid_column: line(1),
      aspect_ratio: Some(2.0), // 50px height -> 100px width
      ..default()
    })
    .unwrap();

  let col_2_item = taffy
    .new_leaf(Style {
      grid_row: line(1),
      grid_column: line(2),
      aspect_ratio: Some(3.0), // 50px height -> 150px width
      ..default()
    })
    .unwrap();

  let root = taffy
    .new_with_children(
      Style {
        display: Display::Grid,
        // Definite container size:
        // - width is large enough that min-content columns don't get clamped.
        // - height is used to stretch the auto row to a definite 50px in the block-axis pass.
        size: Size {
          width: length(500.0),
          height: length(50.0),
        },
        grid_template_columns: vec![min_content(), min_content()],
        grid_template_rows: vec![auto()],
        ..default()
      },
      &[col_1_item, col_2_item],
    )
    .unwrap();

  taffy
    .compute_layout_with_measure(
      root,
      Size::MAX_CONTENT,
      |known_dimensions, _available_space, _node_id, _context, _style| {
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

  let col1 = detailed.columns.sizes[0];
  let col2 = detailed.columns.sizes[1];
  assert!(
    (col1 - 100.0).abs() < 0.01,
    "expected column 1 to size to 100px from aspect-ratio rerun contribution, got {col1}"
  );
  assert!(
    (col2 - 150.0).abs() < 0.01,
    "expected column 2 to size to 150px from aspect-ratio rerun contribution, got {col2}"
  );
}
