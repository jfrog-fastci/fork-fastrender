//! CSS Anchor Positioning (css-anchor-position-1) helpers.
//!
//! This module provides the minimal plumbing needed to support `anchor()` inside inset
//! properties (`top/right/bottom/left`).
//!
//! Baseline semantics:
//! - Anchors are collected from the already-laid-out fragment subtree that is available when
//!   positioning out-of-flow children (absolute/fixed).
//! - The anchor rectangle is the fragment's border box (`FragmentNode::bounds`).
//! - If multiple fragments expose the same anchor name, the **last fragment visited** wins. This
//!   matches the spec's "last in tree order wins" rule for multiple anchors with the same name and
//!   provides a deterministic policy for fragmentation (we currently do not attempt to merge
//!   fragmented boxes).
//! - Scoping (`anchor-scope`) is parsed/stored on styles but not enforced yet; callers decide which
//!   fragment subtree to index.

use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::style::types::Direction;
use crate::style::types::WritingMode;
use crate::tree::fragment_tree::FragmentNode;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AnchorBox {
  pub rect: Rect,
  pub writing_mode: WritingMode,
  pub direction: Direction,
}

/// Lookup table mapping `anchor-name` identifiers (e.g. `--tooltip`) to anchor rectangles.
#[derive(Debug, Default, Clone)]
pub(crate) struct AnchorIndex {
  by_name: HashMap<String, AnchorBox>,
}

impl AnchorIndex {
  pub(crate) fn new() -> Self {
    Self {
      by_name: HashMap::new(),
    }
  }

  pub(crate) fn from_fragments(fragments: &[FragmentNode], viewport: Size) -> Self {
    let mut index = Self::new();
    index.collect_from_fragments(fragments, Point::ZERO, viewport);
    index
  }

  pub(crate) fn get(&self, name: &str) -> Option<Rect> {
    self.by_name.get(name).map(|anchor| anchor.rect)
  }

  pub(crate) fn get_anchor(&self, name: &str) -> Option<AnchorBox> {
    self.by_name.get(name).copied()
  }

  pub(crate) fn insert_names(&mut self, names: &[String], anchor: AnchorBox) {
    for name in names {
      self.by_name.insert(name.clone(), anchor);
    }
  }

  fn collect_from_fragments(&mut self, fragments: &[FragmentNode], parent_origin: Point, viewport: Size) {
    for fragment in fragments {
      self.collect_from_fragment(fragment, parent_origin, viewport);
    }
  }

  fn collect_from_fragment(&mut self, fragment: &FragmentNode, parent_origin: Point, viewport: Size) {
    let abs_bounds = fragment.bounds.translate(parent_origin);

    if let Some(style) = fragment.style.as_ref() {
      let abs_bounds = if style.has_transform() {
        crate::paint::transform_resolver::resolve_transform3d(
          style,
          abs_bounds,
          Some((viewport.width, viewport.height)),
        )
        .map_or(abs_bounds, |transform| transform.transform_rect(abs_bounds))
      } else {
        abs_bounds
      };
      self.insert_names(
        &style.anchor_names,
        AnchorBox {
          rect: abs_bounds,
          writing_mode: style.writing_mode,
          direction: style.direction,
        },
      );
    }

    let child_origin = parent_origin.translate(fragment.bounds.origin);
    self.collect_from_fragments(fragment.children.as_ref(), child_origin, viewport);
  }
}
