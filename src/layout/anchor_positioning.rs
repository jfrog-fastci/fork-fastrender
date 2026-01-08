//! CSS Anchor Positioning (css-anchor-position-1) helpers.
//!
//! This module provides the minimal plumbing needed to support `anchor()` inside inset
//! properties (`top/right/bottom/left`).
//!
//! Baseline semantics:
//! - Anchors are collected from the already-laid-out fragment subtree that is available when
//!   positioning out-of-flow children (absolute/fixed).
//! - The anchor rectangle starts as the fragment's border box (`FragmentNode::bounds`) and then
//!   applies any CSS transforms/motion paths on the fragment and its ancestors, producing an
//!   axis-aligned bounding box in the coordinate space of the containing block.
//! - If multiple fragments expose the same anchor name, the **last fragment visited** wins. This
//!   matches the spec's "last in tree order wins" rule for multiple anchors with the same name and
//!   provides a deterministic policy for fragmentation (we currently do not attempt to merge
//!   fragmented boxes).
//! - Scoping (`anchor-scope`) is honored by associating each anchor name with the nearest ancestor
//!   scope root that applies to it. Lookups for a positioned box are limited to the nearest matching
//!   scope root in its ancestor chain (when present).

use crate::geometry::Point;
use crate::geometry::Rect;
use crate::geometry::Size;
use crate::paint::display_list::Transform3D;
use crate::style::types::AnchorScope;
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
  by_name: HashMap<String, HashMap<Option<usize>, AnchorBox>>,
  scope_chain_by_box_id: HashMap<usize, Vec<ScopeEntry>>,
}

impl AnchorIndex {
  pub(crate) fn new() -> Self {
    Self {
      by_name: HashMap::new(),
      scope_chain_by_box_id: HashMap::new(),
    }
  }

  pub(crate) fn from_fragments(fragments: &[FragmentNode], viewport: Size) -> Self {
    let mut index = Self::new();
    let mut scope_stack = Vec::new();
    index.collect_from_fragments(
      fragments,
      Point::ZERO,
      Transform3D::identity(),
      viewport,
      &mut scope_stack,
    );
    index
  }

  pub(crate) fn from_fragments_with_root_scope(
    fragments: &[FragmentNode],
    root_box_id: usize,
    root_scope: &AnchorScope,
    viewport: Size,
  ) -> Self {
    let mut index = Self::new();
    let mut scope_stack = Vec::new();
    if !matches!(root_scope, AnchorScope::None) {
      scope_stack.push(ScopeEntry {
        box_id: root_box_id,
        scope: root_scope.clone(),
      });
    }
    index
      .scope_chain_by_box_id
      .insert(root_box_id, scope_stack.clone());
    index.collect_from_fragments(
      fragments,
      Point::ZERO,
      Transform3D::identity(),
      viewport,
      &mut scope_stack,
    );
    index
  }

  pub(crate) fn get(&self, name: &str) -> Option<Rect> {
    self
      .by_name
      .get(name)
      .and_then(|by_scope| by_scope.get(&None))
      .map(|anchor| anchor.rect)
  }

  pub(crate) fn get_anchor_for_query(
    &self,
    name: &str,
    query_parent_box_id: Option<usize>,
  ) -> Option<AnchorBox> {
    let scoped_root = self.scope_root_for_query(name, query_parent_box_id);
    self
      .by_name
      .get(name)
      .and_then(|by_scope| by_scope.get(&scoped_root))
      .copied()
  }

  pub(crate) fn insert_names_for_box(&mut self, box_id: usize, names: &[String], anchor: AnchorBox) {
    let scope_chain = self
      .scope_chain_by_box_id
      .get(&box_id)
      .cloned()
      .unwrap_or_default();
    self.insert_names_with_scopes(names, anchor, &scope_chain);
  }

  fn insert_names_with_scopes(
    &mut self,
    names: &[String],
    anchor: AnchorBox,
    scope_stack: &[ScopeEntry],
  ) {
    for name in names {
      let scope_root = scope_root_for_anchor(scope_stack, name);
      self
        .by_name
        .entry(name.clone())
        .or_default()
        .insert(scope_root, anchor);
    }
  }

  fn scope_root_for_query(&self, name: &str, query_parent_box_id: Option<usize>) -> Option<usize> {
    let Some(query_parent_box_id) = query_parent_box_id else {
      return None;
    };
    let Some(chain) = self.scope_chain_by_box_id.get(&query_parent_box_id) else {
      return None;
    };
    for entry in chain.iter().rev() {
      match &entry.scope {
        AnchorScope::None => {}
        AnchorScope::Names(names) => {
          if names.iter().any(|n| n == name) {
            return Some(entry.box_id);
          }
        }
        AnchorScope::All => {
          let Some(by_scope) = self.by_name.get(name) else {
            continue;
          };
          if by_scope.contains_key(&Some(entry.box_id)) {
            return Some(entry.box_id);
          }
        }
      }
    }
    None
  }

  fn collect_from_fragments(
    &mut self,
    fragments: &[FragmentNode],
    parent_origin: Point,
    parent_transform: Transform3D,
    viewport: Size,
    scope_stack: &mut Vec<ScopeEntry>,
  ) {
    for fragment in fragments {
      self.collect_from_fragment(fragment, parent_origin, parent_transform, viewport, scope_stack);
    }
  }

  fn collect_from_fragment(
    &mut self,
    fragment: &FragmentNode,
    parent_origin: Point,
    parent_transform: Transform3D,
    viewport: Size,
    scope_stack: &mut Vec<ScopeEntry>,
  ) {
    let abs_bounds = fragment.bounds.translate(parent_origin);

    let mut current_transform = parent_transform;
    if let Some(style) = fragment.style.as_ref() {
      let self_transform = (style.has_transform() || style.has_motion_path()).then(|| {
        crate::paint::transform_resolver::resolve_transform3d(
          style,
          abs_bounds,
          Some((viewport.width, viewport.height)),
        )
      });
      if let Some(Some(transform)) = self_transform {
        current_transform = current_transform.multiply(&transform);
      }
    }
    let transformed_bounds = if current_transform.is_identity() {
      abs_bounds
    } else {
      current_transform.transform_rect(abs_bounds)
    };

    let box_id = fragment.box_id();
    let mut pushed_scope = false;
    if let (Some(box_id), Some(style)) = (box_id, fragment.style.as_deref()) {
      if !matches!(style.anchor_scope, AnchorScope::None) {
        scope_stack.push(ScopeEntry {
          box_id,
          scope: style.anchor_scope.clone(),
        });
        pushed_scope = true;
      }
    }
    if let Some(box_id) = box_id {
      self
        .scope_chain_by_box_id
        .insert(box_id, scope_stack.clone());
    }
    if let Some(style) = fragment.style.as_deref() {
      self.insert_names_with_scopes(
        &style.anchor_names,
        AnchorBox {
          rect: transformed_bounds,
          writing_mode: style.writing_mode,
          direction: style.direction,
        },
        scope_stack,
      );
    }

    let child_origin = parent_origin.translate(fragment.bounds.origin);
    self.collect_from_fragments(
      fragment.children.as_ref(),
      child_origin,
      current_transform,
      viewport,
      scope_stack,
    );

    if pushed_scope {
      scope_stack.pop();
    }
  }
}

#[derive(Debug, Clone)]
struct ScopeEntry {
  box_id: usize,
  scope: AnchorScope,
}

fn scope_root_for_anchor(scope_stack: &[ScopeEntry], anchor_name: &str) -> Option<usize> {
  for entry in scope_stack.iter().rev() {
    match &entry.scope {
      AnchorScope::None => {}
      AnchorScope::All => return Some(entry.box_id),
      AnchorScope::Names(names) => {
        if names.iter().any(|n| n == anchor_name) {
          return Some(entry.box_id);
        }
      }
    }
  }
  None
}
