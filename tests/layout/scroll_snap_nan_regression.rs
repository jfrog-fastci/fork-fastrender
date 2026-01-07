use fastrender::geometry::{Point, Rect, Size};
use fastrender::scroll::{
  apply_scroll_snap, ScrollMetadata, ScrollSnapContainer, ScrollSnapTarget, ScrollState,
};
use fastrender::style::types::{ScrollBehavior, ScrollSnapStop, ScrollSnapStrictness};
use fastrender::{FragmentNode, FragmentTree};

fn run_snap_with_targets(targets_x: Vec<f32>, scroll_x: f32) -> fastrender::scroll::ScrollSnapResult {
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 300.0, 100.0), vec![]);
  let viewport = Size::new(100.0, 100.0);
  let mut tree = FragmentTree::with_viewport(root, viewport);

  tree.scroll_metadata = Some(ScrollMetadata {
    containers: vec![ScrollSnapContainer {
      box_id: None,
      viewport,
      strictness: ScrollSnapStrictness::Mandatory,
      behavior: ScrollBehavior::Auto,
      snap_x: true,
      snap_y: false,
      padding_x: (0.0, 0.0),
      padding_y: (0.0, 0.0),
      scroll_bounds: Rect::from_xywh(0.0, 0.0, 300.0, 100.0),
      targets_x: targets_x
        .into_iter()
        .map(|pos| ScrollSnapTarget {
          position: pos,
          stop: ScrollSnapStop::Normal,
        })
        .collect(),
      targets_y: vec![],
      uses_viewport_scroll: true,
    }],
  });

  apply_scroll_snap(&mut tree, &ScrollState::with_viewport(Point::new(scroll_x, 0.0)))
}

#[test]
fn scroll_snap_ignores_non_finite_targets() {
  let result = run_snap_with_targets(
    vec![f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 50.0, 200.0],
    60.0,
  );

  assert!(
    (result.state.viewport.x - 50.0).abs() < 1e-3,
    "expected to snap to the nearest finite target, got {:?}",
    result.state.viewport
  );
}

#[test]
fn scroll_snap_no_update_when_all_targets_non_finite() {
  let result = run_snap_with_targets(vec![f32::NAN, f32::INFINITY, f32::NEG_INFINITY], 60.0);
  assert!(
    (result.state.viewport.x - 60.0).abs() < 1e-3,
    "should leave scroll offset unchanged when all targets are invalid"
  );
  assert!(
    result.updates.is_empty(),
    "no scroll snap update should be emitted when nothing snaps"
  );
}

#[test]
fn scroll_snap_tie_breaking_is_order_independent() {
  let forward = run_snap_with_targets(vec![0.0, 200.0], 100.0);
  let reverse = run_snap_with_targets(vec![200.0, 0.0], 100.0);

  assert!(
    (forward.state.viewport.x - reverse.state.viewport.x).abs() < 1e-3,
    "snap selection should not depend on target iteration order ({} vs {})",
    forward.state.viewport.x,
    reverse.state.viewport.x
  );
}

