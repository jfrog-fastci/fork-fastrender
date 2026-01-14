use crate::api::PreparedDocument;
use crate::geometry::Point;
use crate::scroll::ScrollState;
use crate::tree::fragment_tree::FragmentNode;
use crate::tree::fragment_tree::FragmentTree;

use super::hit_test::{hit_test_dom, hit_test_dom_all, HitTestResult};

/// Clone a fragment tree and apply paint-time scroll-dependent geometry adjustments.
///
/// Layout produces fragment trees in an unscrolled coordinate space; to hit-test pointer events in
/// page coordinates, callers must:
/// - translate scroll container contents by their scroll offsets, and
/// - cancel viewport scroll for viewport-fixed (`position: fixed`) elements so hit testing mirrors
///   what the painter renders after scrolling.
///
/// Note: viewport scroll is not applied here; callers should translate the input point by
/// `scroll.viewport` (or use [`hit_test_dom_viewport_point`]).
pub fn fragment_tree_with_scroll(
  fragment_tree: &FragmentTree,
  scroll: &ScrollState,
) -> FragmentTree {
  let mut tree = fragment_tree.clone();
  crate::scroll::apply_scroll_offsets(&mut tree, scroll);
  crate::scroll::apply_viewport_scroll_cancel(&mut tree, scroll);
  tree
}

/// Clone the prepared document's fragment tree and apply paint-time geometry adjustments
/// (scroll + sticky + viewport-fixed scroll cancel).
///
/// This is a convenience wrapper around [`PreparedDocument::fragment_tree_for_geometry`]. Like
/// [`fragment_tree_with_scroll`], viewport scroll is not applied; callers should translate points by
/// `scroll.viewport` (or use [`hit_test_dom_viewport_point`]).
pub fn fragment_tree_with_scroll_and_sticky(
  prepared: &PreparedDocument,
  scroll: &ScrollState,
) -> FragmentTree {
  prepared.fragment_tree_for_geometry_fast(scroll)
}

pub fn hit_test_with_scroll(
  prepared: &PreparedDocument,
  scroll: &ScrollState,
  page_point_css: Point,
) -> Vec<FragmentNode> {
  let tree = fragment_tree_with_scroll_and_sticky(prepared, scroll);
  tree.hit_test(page_point_css).into_iter().cloned().collect()
}

pub fn hit_test_dom_with_scroll(
  prepared: &PreparedDocument,
  scroll: &ScrollState,
  page_point_css: Point,
) -> Option<HitTestResult> {
  let tree = fragment_tree_with_scroll_and_sticky(prepared, scroll);
  hit_test_dom(prepared.dom(), prepared.box_tree(), &tree, page_point_css)
}

pub fn hit_test_dom_with_scroll_all(
  prepared: &PreparedDocument,
  scroll: &ScrollState,
  page_point_css: Point,
) -> Vec<HitTestResult> {
  let tree = fragment_tree_with_scroll_and_sticky(prepared, scroll);
  hit_test_dom_all(prepared.dom(), prepared.box_tree(), &tree, page_point_css)
}

pub fn hit_test_dom_viewport_point(
  prepared: &PreparedDocument,
  scroll: &ScrollState,
  viewport_point_css: Point,
) -> Option<HitTestResult> {
  hit_test_dom_with_scroll(
    prepared,
    scroll,
    viewport_point_css.translate(scroll.viewport),
  )
}

pub fn hit_test_dom_viewport_point_all(
  prepared: &PreparedDocument,
  scroll: &ScrollState,
  viewport_point_css: Point,
) -> Vec<HitTestResult> {
  hit_test_dom_with_scroll_all(prepared, scroll, viewport_point_css.translate(scroll.viewport))
}
