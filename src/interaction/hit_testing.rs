use crate::api::PreparedDocument;
use crate::geometry::Point;
use crate::scroll::ScrollState;
use crate::tree::fragment_tree::FragmentNode;

use super::hit_test::{hit_test_dom, HitTestResult};

pub fn hit_test_with_scroll(
  prepared: &PreparedDocument,
  scroll: &ScrollState,
  page_point_css: Point,
) -> Vec<FragmentNode> {
  let mut tree = prepared.fragment_tree().clone();
  crate::scroll::apply_scroll_offsets(&mut tree, scroll);
  tree.hit_test(page_point_css).into_iter().cloned().collect()
}

pub fn hit_test_dom_with_scroll(
  prepared: &PreparedDocument,
  scroll: &ScrollState,
  page_point_css: Point,
) -> Option<HitTestResult> {
  let mut tree = prepared.fragment_tree().clone();
  crate::scroll::apply_scroll_offsets(&mut tree, scroll);
  hit_test_dom(prepared.dom(), prepared.box_tree(), &tree, page_point_css)
}

pub fn hit_test_dom_viewport_point(
  prepared: &PreparedDocument,
  scroll: &ScrollState,
  viewport_point_css: Point,
) -> Option<HitTestResult> {
  hit_test_dom_with_scroll(prepared, scroll, viewport_point_css.translate(scroll.viewport))
}

