use crate::geometry::Rect;
use super::registry::{FrameId, RendererProcessId, SiteKey};
use std::collections::HashMap;

/// Stable identifier for an iframe element instance in the parent renderer output.
///
/// The embedding renderer is responsible for producing stable tokens across updates for the same
/// iframe element (e.g. derived from DOM node identity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FrameToken(u64);

impl FrameToken {
  pub const fn from_u64(token: u64) -> Self {
    Self(token)
  }

  pub const fn as_u64(self) -> u64 {
    self.0
  }
}

/// Geometry used by the browser compositor to place a child frame inside its embedding document.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EmbeddingGeometry {
  /// The embedding rectangle in the parent coordinate space.
  pub rect: Rect,
  /// Optional clip rect in the parent coordinate space.
  pub clip_rect: Option<Rect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameNodeStatus {
  Active,
  BlockedByDepthLimit {
    requested_depth: usize,
    max_depth: usize,
  },
}

impl FrameNodeStatus {
  pub fn blocked_by_depth_limit(&self) -> bool {
    matches!(self, Self::BlockedByDepthLimit { .. })
  }
}

#[derive(Debug, Clone)]
pub struct FrameNode {
  pub id: FrameId,
  pub parent: Option<FrameId>,
  pub token_in_parent: Option<FrameToken>,
  pub site: SiteKey,
  pub initial_url: Option<String>,
  pub embedding_geometry: Option<EmbeddingGeometry>,
  pub children: Vec<FrameId>,
  pub status: FrameNodeStatus,
  pub renderer_process: Option<RendererProcessId>,
}

impl FrameNode {
  pub fn depth(&self, tree: &FrameTree) -> Option<usize> {
    tree.depth(self.id)
  }
}

/// Browser-owned browsing-context tree.
///
/// This is the authoritative structure for the frame hierarchy used by the browser process to
/// coordinate renderer processes (OOPIF + site isolation). Each iframe element in a renderer output
/// is identified by a [`FrameToken`]; the browser maps `(parent, token)` pairs to stable [`FrameId`]
/// values.
#[derive(Debug, Clone)]
pub struct FrameTree {
  nodes: HashMap<FrameId, FrameNode>,
  by_parent_token: HashMap<(FrameId, FrameToken), FrameId>,
  root: Option<FrameId>,
  next_frame_id: u64,
  max_depth: usize,
  diagnostics: Vec<String>,
}

impl FrameTree {
  pub fn new(max_depth: usize) -> Self {
    Self {
      nodes: HashMap::new(),
      by_parent_token: HashMap::new(),
      root: None,
      next_frame_id: 1,
      max_depth: max_depth.max(1),
      diagnostics: Vec::new(),
    }
  }

  pub const fn max_depth(&self) -> usize {
    self.max_depth
  }

  pub const fn root_frame_id(&self) -> Option<FrameId> {
    self.root
  }

  fn alloc_frame_id(&mut self) -> FrameId {
    let id = self.next_frame_id;
    self.next_frame_id = self
      .next_frame_id
      .checked_add(1)
      .expect("FrameId counter overflow"); // fastrender-allow-unwrap
    FrameId::new(id)
  }

  pub fn get(&self, id: FrameId) -> Option<&FrameNode> {
    self.nodes.get(&id)
  }

  pub fn get_mut(&mut self, id: FrameId) -> Option<&mut FrameNode> {
    self.nodes.get_mut(&id)
  }

  pub fn lookup_child(&self, parent: FrameId, token: FrameToken) -> Option<FrameId> {
    self.by_parent_token.get(&(parent, token)).copied()
  }

  pub fn create_root_frame(&mut self, site: SiteKey, initial_url: Option<String>) -> FrameId {
    if let Some(root_id) = self.root {
      if let Some(node) = self.nodes.get_mut(&root_id) {
        node.site = site;
        node.initial_url = initial_url;
      }
      return root_id;
    }

    let id = self.alloc_frame_id();
    let node = FrameNode {
      id,
      parent: None,
      token_in_parent: None,
      site,
      initial_url,
      embedding_geometry: None,
      children: Vec::new(),
      status: FrameNodeStatus::Active,
      renderer_process: None,
    };
    self.nodes.insert(id, node);
    self.root = Some(id);
    id
  }

  /// Creates a child frame under `parent`, keyed by `token`.
  ///
  /// If the `(parent, token)` mapping already exists, returns the existing [`FrameId`] without
  /// mutating the tree topology.
  pub fn create_child_frame(
    &mut self,
    parent: FrameId,
    token: FrameToken,
    site: SiteKey,
    initial_url: Option<String>,
  ) -> FrameId {
    if let Some(existing) = self.by_parent_token.get(&(parent, token)).copied() {
      if let Some(node) = self.nodes.get_mut(&existing) {
        node.site = site;
        node.initial_url = initial_url;
      }
      return existing;
    }

    let parent_depth = match self.depth(parent) {
      Some(depth) => depth,
      None => {
        let message = format!(
          "attempted to create child frame under unknown parent frame id={} token={}",
          parent.raw(),
          token.as_u64()
        );
        self.diagnostics.push(message);
        debug_assert!(false, "unknown parent frame id {parent:?}");

        let id = self.alloc_frame_id();
        let node = FrameNode {
          id,
          parent: Some(parent),
          token_in_parent: Some(token),
          site,
          initial_url,
          embedding_geometry: None,
          children: Vec::new(),
          status: FrameNodeStatus::Active,
          renderer_process: None,
        };
        self.nodes.insert(id, node);
        self.by_parent_token.insert((parent, token), id);
        return id;
      }
    };
    let requested_depth = parent_depth + 1;

    let status = if requested_depth > self.max_depth {
      let message = format!(
        "blocked iframe creation by depth limit: parent_frame={} token={} requested_depth={} max_depth={}",
        parent.raw(),
        token.as_u64(),
        requested_depth,
        self.max_depth
      );
      self.diagnostics.push(message);
      FrameNodeStatus::BlockedByDepthLimit {
        requested_depth,
        max_depth: self.max_depth,
      }
    } else {
      FrameNodeStatus::Active
    };

    let id = self.alloc_frame_id();
    let node = FrameNode {
      id,
      parent: Some(parent),
      token_in_parent: Some(token),
      site,
      initial_url,
      embedding_geometry: None,
      children: Vec::new(),
      status,
      renderer_process: None,
    };
    self.nodes.insert(id, node);
    self.by_parent_token.insert((parent, token), id);
    if let Some(parent_node) = self.nodes.get_mut(&parent) {
      parent_node.children.push(id);
    } else {
      let message = format!(
        "failed to attach child frame id={} to missing parent frame id={} token={}",
        id.raw(),
        parent.raw(),
        token.as_u64()
      );
      self.diagnostics.push(message);
      debug_assert!(false, "unknown parent frame id {parent:?}");
    }
    id
  }

  /// Updates the embedding geometry for a child identified by `(parent, token)`.
  pub fn update_child_embedding_geometry(
    &mut self,
    parent: FrameId,
    token: FrameToken,
    rect: Rect,
    clip_rect: Option<Rect>,
  ) -> Option<FrameId> {
    let child = self.lookup_child(parent, token)?;
    let node = self.nodes.get_mut(&child)?;
    node.embedding_geometry = Some(EmbeddingGeometry { rect, clip_rect });
    Some(child)
  }

  /// Returns the depth of `frame` in the tree, counting the root as depth 1.
  pub fn depth(&self, frame: FrameId) -> Option<usize> {
    let mut depth = 0usize;
    let mut current = Some(frame);
    while let Some(id) = current {
      let node = self.nodes.get(&id)?;
      depth = depth.saturating_add(1);
      current = node.parent;
    }
    Some(depth)
  }

  /// Removes `frame` and all of its descendants from the tree.
  ///
  /// Returns `true` if the frame existed.
  pub fn remove_subtree(&mut self, frame: FrameId) -> bool {
    let Some(root_node) = self.nodes.get(&frame).cloned() else {
      return false;
    };

    if let Some(parent) = root_node.parent {
      if let Some(parent_node) = self.nodes.get_mut(&parent) {
        parent_node.children.retain(|&child| child != frame);
      }
    } else {
      self.root = None;
    }

    let mut stack = vec![frame];
    let mut to_remove = Vec::new();
    while let Some(id) = stack.pop() {
      if let Some(node) = self.nodes.get(&id) {
        stack.extend(node.children.iter().copied());
      }
      to_remove.push(id);
    }

    for id in to_remove {
      if let Some(node) = self.nodes.remove(&id) {
        if let (Some(parent), Some(token)) = (node.parent, node.token_in_parent) {
          self.by_parent_token.remove(&(parent, token));
        }
      }
    }

    true
  }

  /// Drain any diagnostics produced by structural operations (e.g. depth-limit blocks).
  pub fn take_diagnostics(&mut self) -> Vec<String> {
    std::mem::take(&mut self.diagnostics)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn site(id: u64) -> SiteKey {
    SiteKey::Opaque(id)
  }

  #[test]
  fn token_mapping_stability() {
    let mut tree = FrameTree::new(8);
    let root = tree.create_root_frame(site(1), Some("https://root/".into()));
    let token = FrameToken::from_u64(42);

    let first = tree.create_child_frame(
      root,
      token,
      site(2),
      Some("https://child/".into()),
    );
    let second = tree.create_child_frame(root, token, site(2), None);
    assert_eq!(first, second);

    let node = tree.get(first).expect("child node");
    assert_eq!(node.parent, Some(root));
    assert_eq!(node.token_in_parent, Some(token));
  }

  #[test]
  fn subtree_removal_removes_descendants() {
    let mut tree = FrameTree::new(8);
    let root = tree.create_root_frame(site(1), None);

    let child_token = FrameToken::from_u64(1);
    let child = tree.create_child_frame(root, child_token, site(2), None);

    let grandchild_token = FrameToken::from_u64(2);
    let grandchild = tree.create_child_frame(child, grandchild_token, site(3), None);

    let sibling = tree.create_child_frame(root, FrameToken::from_u64(3), site(4), None);

    assert!(tree.get(child).is_some());
    assert!(tree.get(grandchild).is_some());
    assert!(tree.get(sibling).is_some());

    assert!(tree.remove_subtree(child));

    assert!(tree.get(child).is_none());
    assert!(tree.get(grandchild).is_none());
    assert!(tree.get(sibling).is_some());

    assert!(tree.lookup_child(root, child_token).is_none());
    assert!(tree.lookup_child(child, grandchild_token).is_none());

    let root_node = tree.get(root).expect("root node");
    assert_eq!(root_node.children, vec![sibling]);
  }

  #[test]
  fn depth_limit_blocks_third_level_when_max_depth_is_two() {
    let mut tree = FrameTree::new(2);
    let root = tree.create_root_frame(site(1), None);
    let child = tree.create_child_frame(root, FrameToken::from_u64(1), site(2), None);

    assert_eq!(tree.depth(root), Some(1));
    assert_eq!(tree.depth(child), Some(2));

    let grandchild =
      tree.create_child_frame(child, FrameToken::from_u64(2), site(3), None);
    assert_eq!(tree.depth(grandchild), Some(3));
    let grandchild_node = tree.get(grandchild).expect("grandchild node");
    assert!(grandchild_node.status.blocked_by_depth_limit());

    let diagnostics = tree.take_diagnostics();
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].contains("blocked iframe creation by depth limit"));
  }
}
