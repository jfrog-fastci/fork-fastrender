use taffy::{prelude::*, MeasureOutput};

#[test]
fn flex_multiline_auto_height_accumulates_remeasured_line_cross_sizes() {
  // Regression test: when flex items are remeasured for cross size after their main size is
  // resolved (e.g. text wrapping after shrink-to-fit), the flex container's auto cross size must
  // include *all* resulting flex lines, not just the last line.

  const CONTAINER_WIDTH: f32 = 100.0;
  const UNCONSTRAINED_WIDTH: f32 = 200.0;
  const SINGLE_LINE_HEIGHT: f32 = 10.0;
  const WRAPPED_HEIGHT: f32 = 20.0;
  const ROW_GAP: f32 = 5.0;

  let mut taffy: TaffyTree<()> = TaffyTree::new();

  let leaf_style = Style {
    flex_grow: 0.0,
    flex_shrink: 1.0,
    ..Default::default()
  };
  let leaf1 = taffy.new_leaf(leaf_style.clone()).expect("leaf node");
  let leaf2 = taffy.new_leaf(leaf_style.clone()).expect("leaf node");
  let leaf3 = taffy.new_leaf(leaf_style).expect("leaf node");

  let root = taffy
    .new_with_children(
      Style {
        display: Display::Flex,
        flex_wrap: FlexWrap::Wrap,
        align_items: Some(AlignItems::FlexStart),
        size: Size {
          width: Dimension::length(CONTAINER_WIDTH),
          height: Dimension::auto(),
        },
        gap: Size {
          width: length(0.0),
          height: length(ROW_GAP),
        },
        ..Default::default()
      },
      &[leaf1, leaf2, leaf3],
    )
    .expect("root node");

  taffy
    .compute_layout_with_measure(
      root,
      Size {
        width: AvailableSpace::Definite(CONTAINER_WIDTH),
        height: AvailableSpace::MaxContent,
      },
      |known_dimensions, available_space, _node_id, _node_context, _style| {
        // When the container clamps the used width to 100px, pretend content "wraps" and becomes
        // taller. Otherwise, report an unconstrained max-content width of 200px and a shorter
        // height.
        let size = if known_dimensions.width == Some(CONTAINER_WIDTH) {
          Size {
            width: CONTAINER_WIDTH,
            height: WRAPPED_HEIGHT,
          }
        } else {
          match available_space.width {
            AvailableSpace::MinContent => Size {
              width: CONTAINER_WIDTH,
              height: SINGLE_LINE_HEIGHT,
            },
            AvailableSpace::MaxContent => Size {
              width: UNCONSTRAINED_WIDTH,
              height: SINGLE_LINE_HEIGHT,
            },
            AvailableSpace::Definite(width) => Size {
              width,
              height: SINGLE_LINE_HEIGHT,
            },
          }
        };
        MeasureOutput::from_size(size)
      },
    )
    .expect("compute layout");

  // Each leaf reports a max-content width of 200px, so under `flex-wrap: wrap` and a 100px
  // container, each leaf gets its own line. After shrinking to 100px, each leaf is remeasured and
  // becomes 20px tall. The container's auto height must include all three lines plus the row gaps:
  // 20 + 5 + 20 + 5 + 20 = 70.
  let layout = taffy.layout(root).expect("root layout");
  let expected_height = WRAPPED_HEIGHT * 3.0 + ROW_GAP * 2.0;
  let eps = 1e-3;
  assert!(
    (layout.size.height - expected_height).abs() < eps,
    "expected container auto height to accumulate all remeasured line heights (got {}, expected {expected_height})",
    layout.size.height
  );
}

