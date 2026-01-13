//! Tests for subgrid inheritance/virtual-item propagation when parent/child have differing
//! `axes_swapped` (writing-mode mismatch in the integrator).
//!
//! FastRender transposes axis-dependent grid style properties into Taffy's physical axes. When a
//! parent and subgrid child disagree on `axes_swapped`, the subgrid pipeline must remap which
//! parent axis provides the overridden tracks/gaps and how descendant virtual items map into the
//! ancestor grid.

#![cfg(all(test, feature = "taffy_tree"))]

use crate::prelude::*;

#[cfg(feature = "detailed_layout_info")]
use crate::tree::DetailedLayoutInfo;

fn assert_approx_eq(actual: f32, expected: f32) {
  let diff = (actual - expected).abs();
  assert!(
    diff <= 0.001,
    "expected {expected} but got {actual} (diff {diff})"
  );
}

#[test]
fn subgrid_override_swaps_tracks_and_gap_when_axes_swapped_differs() {
  let mut taffy: TaffyTree<()> = TaffyTree::new();

  // Items inside the (axes_swapped=true) subgrid specify CSS `grid-column: 1/2` and `2/3`.
  // FastRender maps those to Taffy `grid_row` placements when the inline axis is vertical.
  let item_a = taffy
    .new_leaf(Style {
      display: Display::Block,
      grid_row: Line {
        start: line(1),
        end: line(2),
      },
      ..Default::default()
    })
    .unwrap();
  let item_b = taffy
    .new_leaf(Style {
      display: Display::Block,
      grid_row: Line {
        start: line(2),
        end: line(3),
      },
      ..Default::default()
    })
    .unwrap();

  let subgrid = taffy
    .new_with_children(
      Style {
        display: Display::Grid,
        // Writing-mode mismatch relative to the parent grid.
        axes_swapped: true,
        // `grid-template-columns/rows: subgrid` (after FastRender transposition) => both physical axes are subgrids.
        subgrid_rows: true,
        subgrid_columns: true,
        // Avoid stretch alignment changing the resolved track sizes in this test: we want to
        // observe the overridden track sizes/gaps directly.
        justify_content: Some(JustifyContent::Start),
        align_content: Some(AlignContent::Start),
        // Place the subgrid in the parent: span both parent columns.
        grid_column: Line {
          start: line(1),
          end: line(3),
        },
        grid_row: Line {
          start: line(1),
          end: line(2),
        },
        ..Default::default()
      },
      &[item_a, item_b],
    )
    .unwrap();

  let root = taffy
    .new_with_children(
      Style {
        display: Display::Grid,
        axes_swapped: false,
        // Parent has two columns with a column-gap; one row.
        grid_template_columns: vec![
          GridTemplateComponent::Single(TrackSizingFunction::from_length(40.0)),
          GridTemplateComponent::Single(TrackSizingFunction::from_length(60.0)),
        ],
        grid_template_rows: vec![GridTemplateComponent::Single(TrackSizingFunction::from_length(
          70.0,
        ))],
        gap: Size {
          width: LengthPercentage::length(10.0),
          height: LengthPercentage::length(0.0),
        },
        // Fix the parent size so track sizing is stable.
        size: Size {
          width: Dimension::length(110.0),
          height: Dimension::length(70.0),
        },
        ..Default::default()
      },
      &[subgrid],
    )
    .unwrap();

  taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();

  let a_layout = taffy.layout(item_a).unwrap();
  let b_layout = taffy.layout(item_b).unwrap();

  // With axes_swapped mismatch, the subgrid's physical Y axis inherits the parent's physical X
  // tracks (40px, 60px) and the parent's column-gap (10px) becomes the subgrid's row-gap.
  //
  // The subgrid's physical X axis inherits the parent's physical Y tracks (70px).
  assert_approx_eq(a_layout.size.width, 70.0);
  assert_approx_eq(a_layout.size.height, 40.0);
  assert_approx_eq(a_layout.location.x, 0.0);
  assert_approx_eq(a_layout.location.y, 0.0);

  assert_approx_eq(b_layout.size.width, 70.0);
  assert_approx_eq(b_layout.size.height, 60.0);
  assert_approx_eq(b_layout.location.x, 0.0);
  assert_approx_eq(b_layout.location.y, 50.0); // 40px track + 10px inherited gap
}

#[cfg(feature = "detailed_layout_info")]
#[test]
fn subgrid_virtual_items_map_into_correct_ancestor_tracks_with_axes_swap() {
  let mut taffy: TaffyTree<()> = TaffyTree::new();

  // Two leaves with different intrinsic widths. They are placed into different *rows* of the
  // axes-swapped subgrid, which should map into different columns of the ancestor grid when the
  // subgrid axes are remapped.
  let item_a = taffy
    .new_leaf(Style {
      display: Display::Block,
      size: Size {
        width: Dimension::length(10.0),
        height: Dimension::length(10.0),
      },
      grid_row: Line {
        start: line(1),
        end: line(2),
      },
      ..Default::default()
    })
    .unwrap();
  let item_b = taffy
    .new_leaf(Style {
      display: Display::Block,
      size: Size {
        width: Dimension::length(100.0),
        height: Dimension::length(10.0),
      },
      grid_row: Line {
        start: line(2),
        end: line(3),
      },
      ..Default::default()
    })
    .unwrap();

  let subgrid = taffy
    .new_with_children(
      Style {
        display: Display::Grid,
        axes_swapped: true,
        subgrid_rows: true,
        subgrid_columns: true,
        justify_content: Some(JustifyContent::Start),
        align_content: Some(AlignContent::Start),
        grid_column: Line {
          start: line(1),
          end: line(3),
        },
        grid_row: Line {
          start: line(1),
          end: line(2),
        },
        ..Default::default()
      },
      &[item_a, item_b],
    )
    .unwrap();

  let root = taffy
    .new_with_children(
      Style {
        display: Display::Grid,
        axes_swapped: false,
        grid_template_columns: vec![
          GridTemplateComponent::Single(TrackSizingFunction::AUTO),
          GridTemplateComponent::Single(TrackSizingFunction::AUTO),
        ],
        // Give the ancestor a definite single row so we're testing column sizing only.
        grid_template_rows: vec![GridTemplateComponent::Single(TrackSizingFunction::from_length(
          10.0,
        ))],
        justify_content: Some(JustifyContent::Start),
        align_content: Some(AlignContent::Start),
        ..Default::default()
      },
      &[subgrid],
    )
    .unwrap();

  taffy.compute_layout(root, Size::MAX_CONTENT).unwrap();

  let DetailedLayoutInfo::Grid(grid_info) = taffy.detailed_layout_info(root) else {
    panic!("expected DetailedLayoutInfo::Grid");
  };

  // The two subgrid items are placed in different rows of the axes-swapped subgrid; virtual item
  // mapping should propagate those into different ancestor columns.
  assert_eq!(grid_info.columns.sizes.len(), 2);
  assert_approx_eq(grid_info.columns.sizes[0], 10.0);
  assert_approx_eq(grid_info.columns.sizes[1], 100.0);
}

