use crate::api::PreparedDocument;
use crate::geometry::Point;
use crate::scroll::ScrollState;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::FragmentTree;

use super::hit_test::{hit_test_dom, HitTestResult};

/// Clone a fragment tree and apply element scroll offsets from `scroll`.
///
/// Layout produces fragment trees in an unscrolled coordinate space; to hit-test pointer events in
/// page coordinates, callers must translate scroll container contents by their scroll offsets
/// before calling `FragmentTree::hit_test` / `hit_test_dom`.
///
/// Note: viewport scroll is not applied here; callers should translate the input point by
/// `scroll.viewport` (or use [`hit_test_dom_viewport_point`]).
pub fn fragment_tree_with_scroll(fragment_tree: &FragmentTree, scroll: &ScrollState) -> FragmentTree {
  let mut tree = fragment_tree.clone();
  crate::scroll::apply_scroll_offsets(&mut tree, scroll);
  tree
}

pub fn hit_test_with_scroll(
  prepared: &PreparedDocument,
  scroll: &ScrollState,
  page_point_css: Point,
) -> Vec<FragmentNode> {
  let tree = fragment_tree_with_scroll(prepared.fragment_tree(), scroll);
  tree.hit_test(page_point_css).into_iter().cloned().collect()
}

pub fn hit_test_dom_with_scroll(
  prepared: &PreparedDocument,
  scroll: &ScrollState,
  page_point_css: Point,
) -> Option<HitTestResult> {
  let tree = fragment_tree_with_scroll(prepared.fragment_tree(), scroll);
  hit_test_dom(prepared.dom(), prepared.box_tree(), &tree, page_point_css)
}

pub fn hit_test_dom_viewport_point(
  prepared: &PreparedDocument,
  scroll: &ScrollState,
  viewport_point_css: Point,
) -> Option<HitTestResult> {
  hit_test_dom_with_scroll(prepared, scroll, viewport_point_css.translate(scroll.viewport))
}
