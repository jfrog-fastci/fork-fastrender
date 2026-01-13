//! Performance regression tests for grid track-sizing rerun logic.
//!
//! These tests are "perf in the small": they validate that we don't fan out into an O(n) set of
//! additional intrinsic measurements when the result cannot change (e.g. when there are no
//! aspect-ratio items that could introduce cross-axis dependencies).

#[cfg(all(test, feature = "taffy_tree"))]
mod tests {
  use crate::geometry::Point;
  use crate::prelude::*;
  use crate::tree::MeasureOutput;

  #[test]
  fn grid_intrinsic_rerun_scan_skips_non_aspect_ratio_items() {
    // Large enough for the rerun scan to be meaningful, but small enough for a unit test.
    const COLUMN_COUNT: usize = 8;
    const CHILD_COUNT: usize = 128;

    let mut taffy: TaffyTree<()> = TaffyTree::new();

    let child_style = Style { ..Default::default() };
    let mut children = Vec::with_capacity(CHILD_COUNT);
    for _ in 0..CHILD_COUNT {
      children.push(taffy.new_leaf(child_style.clone()).unwrap());
    }

    let root = taffy
      .new_with_children(
        Style {
          display: Display::Grid,
          // Make the container size definite to reduce unrelated intrinsic sizing work.
          size: Size::from_lengths(800.0, 600.0),
          // Intrinsic columns: forces the inline-axis track sizing pass to query min-content sizes.
          grid_template_columns: vec![min_content(); COLUMN_COUNT],
          // Intrinsic implicit rows: ensures the initial inline-axis measurements happen with
          // unknown block-axis sizes (so the rerun scan would historically remeasure every item).
          grid_auto_rows: vec![min_content()],
          ..Default::default()
        },
        &children,
      )
      .unwrap();

    let mut intrinsic_probe_count = 0usize;

    taffy
      .compute_layout_with_measure(
        root,
        Size {
          width: AvailableSpace::Definite(800.0),
          height: AvailableSpace::Definite(600.0),
        },
        |known_dimensions, available_space, _, _, _| {
          let is_intrinsic_probe = matches!(
            available_space.width,
            AvailableSpace::MinContent | AvailableSpace::MaxContent
          ) || matches!(
            available_space.height,
            AvailableSpace::MinContent | AvailableSpace::MaxContent
          );

          if is_intrinsic_probe {
            intrinsic_probe_count += 1;
          }

          // A simple, deterministic leaf measure function.
          let measured = Size {
            width: known_dimensions.width.unwrap_or(10.0),
            height: known_dimensions.height.unwrap_or(10.0),
          };

          MeasureOutput {
            size: measured,
            first_baselines: Point { x: None, y: None },
          }
        },
      )
      .unwrap();

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
}

