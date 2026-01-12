use taffy::{prelude::*, MeasureOutput};

#[test]
fn flex_leaf_is_remeasured_for_cross_size_after_main_size_flexing() {
  const CONTAINER_WIDTH: f32 = 100.0;
  const SINGLE_LINE_HEIGHT: f32 = 10.0;
  const WRAPPED_HEIGHT: f32 = 20.0;

  let mut taffy: TaffyTree<()> = TaffyTree::new();

  let leaf = taffy
    .new_leaf(Style {
      flex_grow: 0.0,
      flex_shrink: 1.0,
      ..Default::default()
    })
    .expect("leaf node");

  let root = taffy
    .new_with_children(
      Style {
        display: Display::Flex,
        align_items: Some(AlignItems::FlexStart),
        size: Size {
          width: Dimension::length(CONTAINER_WIDTH),
          height: Dimension::auto(),
        },
        ..Default::default()
      },
      &[leaf],
    )
    .expect("root node");

  taffy
    .compute_layout_with_measure(
      root,
      Size {
        width: AvailableSpace::Definite(CONTAINER_WIDTH),
        height: AvailableSpace::MaxContent,
      },
      |known_dimensions, available_space, node_id, _node_context, _style| {
        assert_eq!(node_id, leaf, "unexpected leaf measured");

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
              width: 200.0,
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

  let layout = taffy.layout(leaf).expect("leaf layout");
  let eps = 1e-3;
  assert!(
    (layout.size.width - CONTAINER_WIDTH).abs() < eps,
    "expected leaf to shrink to 100px width, got {}",
    layout.size.width
  );
  assert!(
    (layout.size.height - WRAPPED_HEIGHT).abs() < eps,
    "expected leaf height to be remeasured at 100px width, got {}",
    layout.size.height
  );
}
