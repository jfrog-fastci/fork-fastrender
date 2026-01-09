use fastrender::paint::display_list::Transform3D;
use fastrender::paint::preserve3d::{cull_backfaces, depth_sort_scene, SceneItem};
use fastrender::style::types::BackfaceVisibility;
use fastrender::Rect;

#[test]
fn rotated_y_backface_hidden_culls_plane() {
  let rect = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  let plane = SceneItem {
    item: "culled",
    accumulated_transform: Transform3D::rotate_y(std::f32::consts::PI),
    plane_rect: rect,
    backface_visibility: BackfaceVisibility::Hidden,
  };

  assert!(cull_backfaces(vec![plane]).is_empty());
}

#[test]
fn rotated_y_backface_visible_keeps_plane() {
  let rect = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  let plane = SceneItem {
    item: "kept",
    accumulated_transform: Transform3D::rotate_y(std::f32::consts::PI),
    plane_rect: rect,
    backface_visibility: BackfaceVisibility::Visible,
  };

  let kept = cull_backfaces(vec![plane.clone()]);
  assert_eq!(kept.len(), 1);
  assert_eq!(kept[0].item, plane.item);
}

#[test]
fn depth_sort_scene_orders_overlapping_planes_back_to_front() {
  let rect = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  let front = SceneItem {
    item: "front",
    accumulated_transform: Transform3D::translate(0.0, 0.0, 10.0),
    plane_rect: rect,
    backface_visibility: BackfaceVisibility::Visible,
  };
  let back = SceneItem {
    item: "back",
    accumulated_transform: Transform3D::identity(),
    plane_rect: rect,
    backface_visibility: BackfaceVisibility::Visible,
  };

  let sorted = depth_sort_scene(vec![front, back]);
  assert_eq!(
    sorted.iter().map(|item| item.item).collect::<Vec<_>>(),
    vec!["back", "front"]
  );
}

#[test]
fn depth_sort_scene_keeps_authored_order_for_non_overlapping_planes() {
  let rect = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  let front_left = SceneItem {
    item: "front_left",
    accumulated_transform: Transform3D::translate(0.0, 0.0, 10.0),
    plane_rect: rect,
    backface_visibility: BackfaceVisibility::Visible,
  };
  let back_right = SceneItem {
    item: "back_right",
    accumulated_transform: Transform3D::translate(20.0, 0.0, 0.0),
    plane_rect: rect,
    backface_visibility: BackfaceVisibility::Visible,
  };

  // Even though the second plane is farther away, it cannot occlude the first due to lack of
  // overlap, so the renderer preserves the authored order.
  let sorted = depth_sort_scene(vec![front_left, back_right]);
  assert_eq!(
    sorted.iter().map(|item| item.item).collect::<Vec<_>>(),
    vec!["front_left", "back_right"]
  );
}

#[test]
fn culled_backface_plane_does_not_occlude_in_depth_sort() {
  let rect = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  let hidden_front = SceneItem {
    item: "front",
    accumulated_transform: Transform3D::translate(0.0, 0.0, 10.0)
      .multiply(&Transform3D::rotate_y(std::f32::consts::PI)),
    plane_rect: rect,
    backface_visibility: BackfaceVisibility::Hidden,
  };
  let visible_back = SceneItem {
    item: "behind",
    accumulated_transform: Transform3D::translate(0.0, 0.0, 1.0),
    plane_rect: rect,
    backface_visibility: BackfaceVisibility::Visible,
  };

  let sorted = depth_sort_scene(vec![hidden_front, visible_back.clone()]);
  assert_eq!(
    sorted.len(),
    1,
    "hidden plane should be culled before sorting"
  );
  assert_eq!(sorted[0].item, visible_back.item);
}
