use fastrender::interaction::{scrollbar_reservation_for_box_id, scrollport_rect_for_padding_rect};
use fastrender::tree::fragment_tree::ScrollbarReservation;
use fastrender::{FragmentNode, FragmentTree, Rect};

#[test]
fn scrollbar_reservation_for_box_id_collects_fragment_reservation() {
  let target_box_id = 1;

  let mut target_fragment =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), target_box_id, vec![]);
  target_fragment.scrollbar_reservation = ScrollbarReservation {
    right: 10.0,
    bottom: 5.0,
    ..ScrollbarReservation::default()
  };

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![target_fragment]);
  let tree = FragmentTree::new(root);

  let reservation = scrollbar_reservation_for_box_id(&tree, target_box_id).expect("reservation");
  assert_eq!(
    reservation,
    ScrollbarReservation {
      right: 10.0,
      bottom: 5.0,
      ..ScrollbarReservation::default()
    }
  );
}

#[test]
fn scrollbar_reservation_for_box_id_combines_multiple_fragments_conservatively() {
  let target_box_id = 1;

  let mut frag_a =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 10.0, 10.0), target_box_id, vec![]);
  frag_a.scrollbar_reservation = ScrollbarReservation {
    right: 10.0,
    bottom: 5.0,
    ..ScrollbarReservation::default()
  };

  let mut frag_b =
    FragmentNode::new_block_with_id(Rect::from_xywh(20.0, 0.0, 10.0, 10.0), target_box_id, vec![]);
  frag_b.scrollbar_reservation = ScrollbarReservation {
    left: 4.0,
    right: 3.0,
    bottom: 12.0,
    ..ScrollbarReservation::default()
  };

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![frag_a, frag_b]);
  let tree = FragmentTree::new(root);

  let reservation = scrollbar_reservation_for_box_id(&tree, target_box_id).expect("reservation");
  assert_eq!(
    reservation,
    ScrollbarReservation {
      left: 4.0,
      right: 10.0,
      bottom: 12.0,
      ..ScrollbarReservation::default()
    }
  );
}

#[test]
fn scrollbar_reservation_for_box_id_returns_none_when_missing() {
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 200.0, 200.0), vec![]);
  let tree = FragmentTree::new(root);

  assert_eq!(scrollbar_reservation_for_box_id(&tree, 42), None);
}

#[test]
fn scrollport_rect_for_padding_rect_insets_by_reservation() {
  let reservation = ScrollbarReservation {
    right: 10.0,
    bottom: 5.0,
    ..ScrollbarReservation::default()
  };

  let padding_rect = Rect::from_xywh(0.0, 0.0, 100.0, 50.0);
  let scrollport = scrollport_rect_for_padding_rect(padding_rect, reservation);
  assert_eq!(scrollport, Rect::from_xywh(0.0, 0.0, 90.0, 45.0));
}
