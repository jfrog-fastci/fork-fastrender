#![cfg(all(test, feature = "browser_ui"))]

use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// A snapshot-friendly summary of a single AccessKit node emitted by egui.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccessKitNodeSnapshot {
  /// AccessKit's `NodeId` is a `NonZeroU128`. We keep it as a string to avoid any JSON number
  /// portability issues and to keep snapshots stable.
  pub id: String,
  pub role: String,
  #[serde(skip_serializing_if = "String::is_empty")]
  pub name: String,
}

/// Snapshot-friendly representation of egui's latest AccessKit update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccessKitSnapshot {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub root_id: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub focus_id: Option<String>,
  pub nodes: Vec<AccessKitNodeSnapshot>,
}

/// A stable (ID-free) view of the named nodes emitted by egui/AccessKit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccessKitNamedRoleSnapshot {
  pub role: String,
  pub name: String,
}

/// Snapshot-friendly representation of an AccessKit node reachable from the tree root.
///
/// Unlike [`AccessKitSnapshot`], this snapshot preserves tree order (pre-order traversal) and
/// intentionally omits node IDs to keep snapshots stable across runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccessKitReachableNodeSnapshot {
  pub role: String,
  #[serde(skip_serializing_if = "String::is_empty")]
  pub name: String,
}

/// Snapshot-friendly representation of reachability for an AccessKit update.
///
/// This is intended for debugging and snapshot tests that need to ensure injected subtrees are
/// actually connected to the root (not just present in `update.nodes`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccessKitConnectivitySnapshot {
  pub root_id: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub focus_id: Option<String>,
  /// Nodes reachable from `root_id` in pre-order.
  pub reachable: Vec<AccessKitReachableNodeSnapshot>,
  /// Nodes present in `update.nodes` that are not reachable from `root_id`.
  #[serde(skip_serializing_if = "Vec::is_empty")]
  pub orphans: Vec<AccessKitNodeSnapshot>,
}

/// Assertion helper that fails the test if `update` contains orphan nodes.
///
/// This is intended for page-a11y subtree injection tests: if injected nodes are not connected to
/// the tree root, they will show up as *orphans* (present in `update.nodes` but unreachable).
#[track_caller]
pub fn assert_accesskit_update_has_no_orphans<'a, I>(
  update: &'a accesskit::TreeUpdate,
  root_id_fallback: Option<accesskit::NodeId>,
  additional_nodes: I,
) where
  I: IntoIterator<Item = (accesskit::NodeId, &'a accesskit::Node)>,
{
  let snapshot =
    accesskit_connectivity_snapshot_from_update(update, root_id_fallback, additional_nodes);
  if snapshot.orphans.is_empty() {
    return;
  }
  let pretty = serde_json::to_string_pretty(&snapshot)
    .expect("accesskit connectivity snapshot must serialize to JSON");
  panic!("AccessKit update contains orphan nodes (unreachable from root):\n{pretty}");
}

/// Assertion helper that fails the test if `target` is not reachable from the tree root.
///
/// When this fails, it prints a full [`AccessKitConnectivitySnapshot`] to help debug why the node
/// is unreachable.
#[track_caller]
pub fn assert_accesskit_node_is_reachable<'a, I>(
  update: &'a accesskit::TreeUpdate,
  target: accesskit::NodeId,
  root_id_fallback: Option<accesskit::NodeId>,
  additional_nodes: I,
) where
  I: IntoIterator<Item = (accesskit::NodeId, &'a accesskit::Node)>,
{
  let additional: Vec<(accesskit::NodeId, &'a accesskit::Node)> =
    additional_nodes.into_iter().collect();
  let reachable =
    accesskit_reachable_node_ids_from_update(update, root_id_fallback, additional.iter().copied());
  if reachable.contains(&target) {
    return;
  }
  let snapshot = accesskit_connectivity_snapshot_from_update(
    update,
    root_id_fallback,
    additional.iter().copied(),
  );
  let pretty = serde_json::to_string_pretty(&snapshot)
    .expect("accesskit connectivity snapshot must serialize to JSON");
  panic!(
    "AccessKit node id {} is not reachable from root {}.\n{pretty}",
    target.0.get(),
    snapshot.root_id
  );
}

/// Convenience helper for tests that need to reason about multiple incremental AccessKit updates.
///
/// `accesskit::TreeUpdate` objects can omit `tree` (and thus the root id) and may only contain the
/// nodes that changed in a given frame. To validate reachability across frames, tests typically need
/// to keep a store of previously seen nodes and the last-known root id.
#[derive(Debug, Default, Clone)]
pub struct AccessKitTestTree {
  pub root_id: Option<accesskit::NodeId>,
  pub nodes: HashMap<accesskit::NodeId, accesskit::Node>,
}

impl AccessKitTestTree {
  /// Merge a tree update into this store, updating the remembered root id when present.
  pub fn apply_update(&mut self, update: &accesskit::TreeUpdate) {
    if let Some(tree) = update.tree.as_ref() {
      self.root_id = Some(tree.root);
    }
    for (id, node) in update.nodes.iter() {
      self.nodes.insert(*id, node.clone());
    }
  }

  /// Merge the AccessKit update emitted by egui into this store.
  pub fn apply_platform_output(&mut self, output: &egui::PlatformOutput) {
    let update = accesskit_update_from_platform_output(output);
    self.apply_update(update);
  }

  pub fn apply_full_output(&mut self, output: &egui::FullOutput) {
    self.apply_platform_output(&output.platform_output);
  }

  pub fn nodes_iter(&self) -> impl Iterator<Item = (accesskit::NodeId, &accesskit::Node)> + '_ {
    self.nodes.iter().map(|(id, node)| (*id, node))
  }

  pub fn reachable_node_ids(&self, update: &accesskit::TreeUpdate) -> Vec<accesskit::NodeId> {
    accesskit_reachable_node_ids_from_update(update, self.root_id, self.nodes_iter())
  }

  pub fn reachable_node_ids_from_platform_output(
    &self,
    output: &egui::PlatformOutput,
  ) -> Vec<accesskit::NodeId> {
    let update = accesskit_update_from_platform_output(output);
    self.reachable_node_ids(update)
  }

  pub fn reachable_node_ids_from_full_output(
    &self,
    output: &egui::FullOutput,
  ) -> Vec<accesskit::NodeId> {
    self.reachable_node_ids_from_platform_output(&output.platform_output)
  }

  pub fn orphan_node_ids(&self, update: &accesskit::TreeUpdate) -> Vec<accesskit::NodeId> {
    accesskit_orphan_node_ids_from_update(update, self.root_id, self.nodes_iter())
  }

  pub fn orphan_nodes_snapshot(
    &self,
    update: &accesskit::TreeUpdate,
  ) -> Vec<AccessKitNodeSnapshot> {
    accesskit_orphan_nodes_snapshot_from_update(update, self.root_id, self.nodes_iter())
  }

  pub fn orphan_node_ids_from_platform_output(
    &self,
    output: &egui::PlatformOutput,
  ) -> Vec<accesskit::NodeId> {
    let update = accesskit_update_from_platform_output(output);
    self.orphan_node_ids(update)
  }

  pub fn orphan_node_ids_from_full_output(
    &self,
    output: &egui::FullOutput,
  ) -> Vec<accesskit::NodeId> {
    self.orphan_node_ids_from_platform_output(&output.platform_output)
  }

  pub fn orphan_nodes_snapshot_from_platform_output(
    &self,
    output: &egui::PlatformOutput,
  ) -> Vec<AccessKitNodeSnapshot> {
    let update = accesskit_update_from_platform_output(output);
    self.orphan_nodes_snapshot(update)
  }

  pub fn orphan_nodes_snapshot_from_full_output(
    &self,
    output: &egui::FullOutput,
  ) -> Vec<AccessKitNodeSnapshot> {
    self.orphan_nodes_snapshot_from_platform_output(&output.platform_output)
  }

  pub fn reachable_nodes_snapshot(
    &self,
    update: &accesskit::TreeUpdate,
  ) -> Vec<AccessKitReachableNodeSnapshot> {
    accesskit_reachable_nodes_snapshot_from_update(update, self.root_id, self.nodes_iter())
  }

  pub fn reachable_nodes_snapshot_from_platform_output(
    &self,
    output: &egui::PlatformOutput,
  ) -> Vec<AccessKitReachableNodeSnapshot> {
    let update = accesskit_update_from_platform_output(output);
    self.reachable_nodes_snapshot(update)
  }

  pub fn reachable_nodes_snapshot_from_full_output(
    &self,
    output: &egui::FullOutput,
  ) -> Vec<AccessKitReachableNodeSnapshot> {
    self.reachable_nodes_snapshot_from_platform_output(&output.platform_output)
  }

  pub fn connectivity_snapshot(
    &self,
    update: &accesskit::TreeUpdate,
  ) -> AccessKitConnectivitySnapshot {
    accesskit_connectivity_snapshot_from_update(update, self.root_id, self.nodes_iter())
  }

  pub fn connectivity_snapshot_from_platform_output(
    &self,
    output: &egui::PlatformOutput,
  ) -> AccessKitConnectivitySnapshot {
    let update = accesskit_update_from_platform_output(output);
    self.connectivity_snapshot(update)
  }

  pub fn connectivity_snapshot_from_full_output(
    &self,
    output: &egui::FullOutput,
  ) -> AccessKitConnectivitySnapshot {
    self.connectivity_snapshot_from_platform_output(&output.platform_output)
  }

  #[track_caller]
  pub fn assert_update_has_no_orphans(&self, update: &accesskit::TreeUpdate) {
    assert_accesskit_update_has_no_orphans(update, self.root_id, self.nodes_iter());
  }

  #[track_caller]
  pub fn assert_node_is_reachable(
    &self,
    update: &accesskit::TreeUpdate,
    target: accesskit::NodeId,
  ) {
    assert_accesskit_node_is_reachable(update, target, self.root_id, self.nodes_iter());
  }
}

fn accesskit_update_from_platform_output(output: &egui::PlatformOutput) -> &accesskit::TreeUpdate {
  output.accesskit_update.as_ref().expect(
    "egui did not emit an AccessKit update. \
    Ensure `ctx.enable_accesskit()` was called for the frame under test, \
    and that `egui-winit` is built with its `accesskit` feature.",
  )
}

/// Extract a deterministic, JSON-serializable snapshot of the AccessKit tree update emitted by egui.
///
/// The output is intentionally lossy: it records only `id`, `role`, and the accessible `name`.
pub fn accesskit_snapshot_from_platform_output(output: &egui::PlatformOutput) -> AccessKitSnapshot {
  let update = accesskit_update_from_platform_output(output);

  let root_id = update.tree.as_ref().map(|t| t.root.0.get().to_string());
  let focus_id = update.focus.map(|id| id.0.get().to_string());

  let mut nodes: Vec<AccessKitNodeSnapshot> = update
    .nodes
    .iter()
    .map(|(id, node)| AccessKitNodeSnapshot {
      id: id.0.get().to_string(),
      role: format!("{:?}", node.role()),
      name: node.name().unwrap_or("").trim().to_string(),
    })
    .collect();

  // Sort for deterministic snapshots: role → name → id.
  nodes.sort_by(|a, b| (&a.role, &a.name, &a.id).cmp(&(&b.role, &b.name, &b.id)));

  AccessKitSnapshot {
    root_id,
    focus_id,
    nodes,
  }
}

pub fn accesskit_snapshot_from_full_output(output: &egui::FullOutput) -> AccessKitSnapshot {
  accesskit_snapshot_from_platform_output(&output.platform_output)
}

/// Returns a sorted list of all non-empty accessible names emitted by egui/AccessKit for the frame.
pub fn accesskit_names_from_platform_output(output: &egui::PlatformOutput) -> Vec<String> {
  let snapshot = accesskit_snapshot_from_platform_output(output);

  let mut names: Vec<String> = snapshot
    .nodes
    .into_iter()
    .map(|n| n.name)
    .filter(|name| !name.is_empty())
    .collect();
  names.sort();
  names.dedup();
  names
}

/// Returns a sorted list of all non-empty accessible names emitted by egui/AccessKit for the frame.
pub fn accesskit_names_from_full_output(output: &egui::FullOutput) -> Vec<String> {
  accesskit_names_from_platform_output(&output.platform_output)
}

/// Convenience helper for `assert_eq!` / snapshot-style tests.
pub fn accesskit_pretty_json_from_platform_output(output: &egui::PlatformOutput) -> String {
  let snapshot = accesskit_snapshot_from_platform_output(output);
  serde_json::to_string_pretty(&snapshot).expect("accesskit snapshot must serialize to JSON")
}

pub fn accesskit_pretty_json_from_full_output(output: &egui::FullOutput) -> String {
  accesskit_pretty_json_from_platform_output(&output.platform_output)
}

/// Returns a stable (ID-free) snapshot of all non-empty accessible `(role, name)` pairs in the frame.
pub fn accesskit_named_roles_from_platform_output(
  output: &egui::PlatformOutput,
) -> Vec<AccessKitNamedRoleSnapshot> {
  let update = accesskit_update_from_platform_output(output);

  let mut out: Vec<AccessKitNamedRoleSnapshot> = update
    .nodes
    .iter()
    .filter_map(|(_id, node)| {
      let name = node.name().unwrap_or("").trim().to_string();
      if name.is_empty() {
        return None;
      }
      Some(AccessKitNamedRoleSnapshot {
        role: format!("{:?}", node.role()),
        name,
      })
    })
    .collect();
  out.sort_by(|a, b| (&a.role, &a.name).cmp(&(&b.role, &b.name)));
  out.dedup();
  out
}

pub fn accesskit_named_roles_from_full_output(
  output: &egui::FullOutput,
) -> Vec<AccessKitNamedRoleSnapshot> {
  accesskit_named_roles_from_platform_output(&output.platform_output)
}

pub fn accesskit_named_roles_pretty_json_from_full_output(output: &egui::FullOutput) -> String {
  let snapshot = accesskit_named_roles_from_full_output(output);
  serde_json::to_string_pretty(&snapshot)
    .expect("accesskit named role snapshot must serialize to JSON")
}

/// Determine the root node for a tree update.
///
/// AccessKit updates may omit `update.tree` for incremental updates, so tests can pass the previous
/// root via `root_id_fallback`.
fn accesskit_root_id_from_update(
  update: &accesskit::TreeUpdate,
  root_id_fallback: Option<accesskit::NodeId>,
) -> accesskit::NodeId {
  update
    .tree
    .as_ref()
    .map(|tree| tree.root)
    .or(root_id_fallback)
    .expect(
      "AccessKit TreeUpdate did not include a tree/root. \
       Pass `root_id_fallback` to compute reachability for incremental updates.",
    )
}

/// Compute the list of node ids reachable from `root_id` in pre-order.
///
/// This helper is primarily intended for tests: it detects orphan nodes (present in an update but
/// not connected to the root) by traversing `Node::children()`.
///
/// The `nodes_by_id` map must contain *all* nodes referenced by the reachable subtree. For
/// incremental updates, callers can build such a map by merging the current update's nodes with a
/// previously known node store (see [`accesskit_reachable_node_ids_from_update`]).
pub fn accesskit_reachable_node_ids(
  root_id: accesskit::NodeId,
  nodes_by_id: &HashMap<accesskit::NodeId, &accesskit::Node>,
) -> Vec<accesskit::NodeId> {
  let mut out = Vec::new();
  let mut visited: HashSet<accesskit::NodeId> = HashSet::new();

  // Manual stack for a pre-order traversal (push children in reverse order).
  let mut stack = vec![root_id];
  while let Some(id) = stack.pop() {
    if !visited.insert(id) {
      continue;
    }

    let node = nodes_by_id.get(&id).unwrap_or_else(|| {
      panic!(
        "AccessKit reachability traversal referenced node id {} but it was not present in the \
         provided node map",
        id.0.get()
      )
    });

    out.push(id);

    // Ensure pre-order by pushing children in reverse.
    for child in node.children().iter().rev() {
      stack.push(*child);
    }
  }

  out
}

/// Compute the list of reachable node ids for a [`accesskit::TreeUpdate`].
///
/// This function builds a temporary `NodeId → Node` map from the update, optionally merging in
/// `additional_nodes` (e.g. previously emitted nodes from earlier updates).
///
/// If `update.tree` is `Some`, its root is used. Otherwise, `root_id_fallback` must be provided.
pub fn accesskit_reachable_node_ids_from_update<'a, I>(
  update: &'a accesskit::TreeUpdate,
  root_id_fallback: Option<accesskit::NodeId>,
  additional_nodes: I,
) -> Vec<accesskit::NodeId>
where
  I: IntoIterator<Item = (accesskit::NodeId, &'a accesskit::Node)>,
{
  let root_id = accesskit_root_id_from_update(update, root_id_fallback);

  let mut nodes_by_id: HashMap<accesskit::NodeId, &accesskit::Node> = HashMap::new();
  for (id, node) in additional_nodes {
    nodes_by_id.insert(id, node);
  }
  // The latest update should win if `additional_nodes` contains older versions of the same id.
  for (id, node) in update.nodes.iter() {
    nodes_by_id.insert(*id, node);
  }

  accesskit_reachable_node_ids(root_id, &nodes_by_id)
}

/// Return a stable list of node ids that are present in `update.nodes` but not reachable from the
/// update's root.
///
/// This is useful for detecting *orphan* nodes that were emitted by egui/AccessKit but never
/// connected to the tree.
pub fn accesskit_orphan_node_ids_from_update<'a, I>(
  update: &'a accesskit::TreeUpdate,
  root_id_fallback: Option<accesskit::NodeId>,
  additional_nodes: I,
) -> Vec<accesskit::NodeId>
where
  I: IntoIterator<Item = (accesskit::NodeId, &'a accesskit::Node)>,
{
  let reachable =
    accesskit_reachable_node_ids_from_update(update, root_id_fallback, additional_nodes);
  let reachable_set: HashSet<accesskit::NodeId> = reachable.into_iter().collect();
  update
    .nodes
    .iter()
    .map(|(id, _node)| *id)
    .filter(|id| !reachable_set.contains(id))
    .collect()
}

/// Snapshot-friendly list of nodes that are present in `update.nodes` but not reachable from the
/// update's root.
///
/// The output is sorted by `role → name → id` for stable diffs.
pub fn accesskit_orphan_nodes_snapshot_from_update<'a, I>(
  update: &'a accesskit::TreeUpdate,
  root_id_fallback: Option<accesskit::NodeId>,
  additional_nodes: I,
) -> Vec<AccessKitNodeSnapshot>
where
  I: IntoIterator<Item = (accesskit::NodeId, &'a accesskit::Node)>,
{
  let root_id = accesskit_root_id_from_update(update, root_id_fallback);

  let mut nodes_by_id: HashMap<accesskit::NodeId, &accesskit::Node> = HashMap::new();
  for (id, node) in additional_nodes {
    nodes_by_id.insert(id, node);
  }
  for (id, node) in update.nodes.iter() {
    nodes_by_id.insert(*id, node);
  }

  let reachable_ids = accesskit_reachable_node_ids(root_id, &nodes_by_id);
  let reachable_set: HashSet<accesskit::NodeId> = reachable_ids.into_iter().collect();

  let mut out: Vec<AccessKitNodeSnapshot> = update
    .nodes
    .iter()
    .filter(|(id, _node)| !reachable_set.contains(id))
    .map(|(id, node)| AccessKitNodeSnapshot {
      id: id.0.get().to_string(),
      role: format!("{:?}", node.role()),
      name: node.name().unwrap_or("").trim().to_string(),
    })
    .collect();
  out.sort_by(|a, b| (&a.role, &a.name, &a.id).cmp(&(&b.role, &b.name, &b.id)));
  out
}

pub fn accesskit_connectivity_snapshot_from_update<'a, I>(
  update: &'a accesskit::TreeUpdate,
  root_id_fallback: Option<accesskit::NodeId>,
  additional_nodes: I,
) -> AccessKitConnectivitySnapshot
where
  I: IntoIterator<Item = (accesskit::NodeId, &'a accesskit::Node)>,
{
  let additional: Vec<(accesskit::NodeId, &'a accesskit::Node)> =
    additional_nodes.into_iter().collect();
  let root_id = accesskit_root_id_from_update(update, root_id_fallback);
  AccessKitConnectivitySnapshot {
    root_id: root_id.0.get().to_string(),
    focus_id: update.focus.map(|id| id.0.get().to_string()),
    reachable: accesskit_reachable_nodes_snapshot_from_update(
      update,
      Some(root_id),
      additional.iter().copied(),
    ),
    orphans: accesskit_orphan_nodes_snapshot_from_update(
      update,
      Some(root_id),
      additional.iter().copied(),
    ),
  }
}

/// Snapshot-friendly pre-order list of all nodes reachable from the update's root.
pub fn accesskit_reachable_nodes_snapshot_from_update<'a, I>(
  update: &'a accesskit::TreeUpdate,
  root_id_fallback: Option<accesskit::NodeId>,
  additional_nodes: I,
) -> Vec<AccessKitReachableNodeSnapshot>
where
  I: IntoIterator<Item = (accesskit::NodeId, &'a accesskit::Node)>,
{
  let root_id = accesskit_root_id_from_update(update, root_id_fallback);

  let mut nodes_by_id: HashMap<accesskit::NodeId, &accesskit::Node> = HashMap::new();
  for (id, node) in additional_nodes {
    nodes_by_id.insert(id, node);
  }
  for (id, node) in update.nodes.iter() {
    nodes_by_id.insert(*id, node);
  }

  accesskit_reachable_node_ids(root_id, &nodes_by_id)
    .into_iter()
    .map(|id| {
      let node = nodes_by_id
        .get(&id)
        .expect("reachable node ids must exist in map");
      AccessKitReachableNodeSnapshot {
        role: format!("{:?}", node.role()),
        name: node.name().unwrap_or("").trim().to_string(),
      }
    })
    .collect()
}

/// Snapshot-friendly pre-order list of all nodes reachable from the AccessKit update emitted by egui.
///
/// This is a convenience wrapper around [`accesskit_reachable_nodes_snapshot_from_update`] for tests
/// that already have an `egui::PlatformOutput`.
pub fn accesskit_reachable_nodes_snapshot_from_platform_output(
  output: &egui::PlatformOutput,
) -> Vec<AccessKitReachableNodeSnapshot> {
  let update = accesskit_update_from_platform_output(output);
  accesskit_reachable_nodes_snapshot_from_update(update, None, std::iter::empty())
}

pub fn accesskit_reachable_nodes_snapshot_from_full_output(
  output: &egui::FullOutput,
) -> Vec<AccessKitReachableNodeSnapshot> {
  accesskit_reachable_nodes_snapshot_from_platform_output(&output.platform_output)
}

pub fn accesskit_reachable_nodes_pretty_json_from_platform_output(
  output: &egui::PlatformOutput,
) -> String {
  let snapshot = accesskit_reachable_nodes_snapshot_from_platform_output(output);
  serde_json::to_string_pretty(&snapshot)
    .expect("accesskit reachable node snapshot must serialize to JSON")
}

pub fn accesskit_reachable_nodes_pretty_json_from_full_output(
  output: &egui::FullOutput,
) -> String {
  accesskit_reachable_nodes_pretty_json_from_platform_output(&output.platform_output)
}

pub fn accesskit_orphan_nodes_snapshot_from_platform_output(
  output: &egui::PlatformOutput,
) -> Vec<AccessKitNodeSnapshot> {
  let update = accesskit_update_from_platform_output(output);
  accesskit_orphan_nodes_snapshot_from_update(update, None, std::iter::empty())
}

pub fn accesskit_orphan_nodes_snapshot_from_full_output(
  output: &egui::FullOutput,
) -> Vec<AccessKitNodeSnapshot> {
  accesskit_orphan_nodes_snapshot_from_platform_output(&output.platform_output)
}

pub fn accesskit_orphan_nodes_pretty_json_from_platform_output(
  output: &egui::PlatformOutput,
) -> String {
  let snapshot = accesskit_orphan_nodes_snapshot_from_platform_output(output);
  serde_json::to_string_pretty(&snapshot)
    .expect("accesskit orphan node snapshot must serialize to JSON")
}

pub fn accesskit_orphan_nodes_pretty_json_from_full_output(output: &egui::FullOutput) -> String {
  accesskit_orphan_nodes_pretty_json_from_platform_output(&output.platform_output)
}

pub fn accesskit_connectivity_snapshot_from_platform_output(
  output: &egui::PlatformOutput,
) -> AccessKitConnectivitySnapshot {
  let update = accesskit_update_from_platform_output(output);
  accesskit_connectivity_snapshot_from_update(update, None, std::iter::empty())
}

pub fn accesskit_connectivity_snapshot_from_full_output(
  output: &egui::FullOutput,
) -> AccessKitConnectivitySnapshot {
  accesskit_connectivity_snapshot_from_platform_output(&output.platform_output)
}

pub fn accesskit_connectivity_pretty_json_from_platform_output(
  output: &egui::PlatformOutput,
) -> String {
  let snapshot = accesskit_connectivity_snapshot_from_platform_output(output);
  serde_json::to_string_pretty(&snapshot)
    .expect("accesskit connectivity snapshot must serialize to JSON")
}

pub fn accesskit_connectivity_pretty_json_from_full_output(output: &egui::FullOutput) -> String {
  accesskit_connectivity_pretty_json_from_platform_output(&output.platform_output)
}

#[track_caller]
pub fn assert_accesskit_platform_output_has_no_orphans(output: &egui::PlatformOutput) {
  let update = accesskit_update_from_platform_output(output);
  assert_accesskit_update_has_no_orphans(update, None, std::iter::empty());
}

#[track_caller]
pub fn assert_accesskit_full_output_has_no_orphans(output: &egui::FullOutput) {
  assert_accesskit_platform_output_has_no_orphans(&output.platform_output);
}

#[track_caller]
pub fn assert_accesskit_platform_output_node_is_reachable(
  output: &egui::PlatformOutput,
  target: accesskit::NodeId,
) {
  let update = accesskit_update_from_platform_output(output);
  assert_accesskit_node_is_reachable(update, target, None, std::iter::empty());
}

#[track_caller]
pub fn assert_accesskit_full_output_node_is_reachable(
  output: &egui::FullOutput,
  target: accesskit::NodeId,
) {
  assert_accesskit_platform_output_node_is_reachable(&output.platform_output, target);
}

/// Find the first AccessKit node with the given accessible name.
///
/// This is intended for writing stateful accessibility regression tests without asserting on
/// unstable AccessKit `NodeId`s.
///
/// Returns `None` when no matching node is present. Panics if more than one node matches the
/// provided name (tests should disambiguate by using a more specific name).
pub fn accesskit_node_by_name<'a>(
  update: &'a accesskit::TreeUpdate,
  name: &str,
) -> Option<(accesskit::NodeId, &'a accesskit::Node)> {
  let needle = name.trim();
  let mut found: Option<(accesskit::NodeId, &'a accesskit::Node)> = None;
  let mut duplicates: Vec<(String, String)> = Vec::new();

  for (id, node) in update.nodes.iter() {
    let node_name = node.name().unwrap_or("").trim();
    if node_name != needle {
      continue;
    }

    if found.is_some() {
      duplicates.push((id.0.get().to_string(), format!("{:?}", node.role())));
      continue;
    }
    found = Some((*id, node));
  }

  if let Some((id, node)) = found {
    if !duplicates.is_empty() {
      duplicates.insert(0, (id.0.get().to_string(), format!("{:?}", node.role())));
      panic!(
        "multiple AccessKit nodes matched name {needle:?}: {duplicates:?}. \
        Use a more specific accessible name to disambiguate."
      );
    }
    return Some((id, node));
  }

  None
}

pub fn accesskit_node_by_name_from_platform_output<'a>(
  output: &'a egui::PlatformOutput,
  name: &str,
) -> Option<(accesskit::NodeId, &'a accesskit::Node)> {
  let update = accesskit_update_from_platform_output(output);
  accesskit_node_by_name(update, name)
}

pub fn accesskit_node_by_name_from_full_output<'a>(
  output: &'a egui::FullOutput,
  name: &str,
) -> Option<(accesskit::NodeId, &'a accesskit::Node)> {
  accesskit_node_by_name_from_platform_output(&output.platform_output, name)
}

/// Resolve the AccessKit focus id into the focused node's accessible name.
///
/// Returns `None` when the update has no focus, the focused node is missing from the update, or the
/// focused node has an empty name.
pub fn accesskit_focus_name(update: &accesskit::TreeUpdate) -> Option<String> {
  let focus_id = update.focus?;
  let node = update
    .nodes
    .iter()
    .find_map(|(id, node)| (*id == focus_id).then_some(node))?;
  let name = node.name().unwrap_or("").trim();
  (!name.is_empty()).then_some(name.to_string())
}

pub fn accesskit_focus_name_from_platform_output(output: &egui::PlatformOutput) -> Option<String> {
  let update = accesskit_update_from_platform_output(output);
  accesskit_focus_name(update)
}

pub fn accesskit_focus_name_from_full_output(output: &egui::FullOutput) -> Option<String> {
  accesskit_focus_name_from_platform_output(&output.platform_output)
}

/// Common state helpers for AccessKit nodes.
///
/// AccessKit's API exposes some states as optional values (e.g. expanded/collapsed) because not all
/// nodes support the concept. These helpers are thin wrappers intended for test readability.
pub fn accesskit_node_expanded(node: &accesskit::Node) -> Option<bool> {
  node.is_expanded()
}

pub fn accesskit_node_selected(node: &accesskit::Node) -> Option<bool> {
  node.is_selected()
}

/// AccessKit 0.11 exposes "checked"/"toggled"/"pressed" semantics via a single `CheckedState` value.
///
/// These helpers fan out the state based on role so tests can assert on ARIA-style terminology:
/// - `Role::ToggleButton` → "pressed"
/// - `Role::Switch` → "toggled"
/// - everything else → "checked"
pub fn accesskit_node_checked(node: &accesskit::Node) -> Option<accesskit::CheckedState> {
  if matches!(node.role(), accesskit::Role::ToggleButton | accesskit::Role::Switch) {
    return None;
  }
  node.checked_state()
}

pub fn accesskit_node_toggled(node: &accesskit::Node) -> Option<accesskit::CheckedState> {
  if !matches!(node.role(), accesskit::Role::Switch) {
    return None;
  }
  node.checked_state()
}

pub fn accesskit_node_pressed(node: &accesskit::Node) -> Option<accesskit::CheckedState> {
  if !matches!(node.role(), accesskit::Role::ToggleButton) {
    return None;
  }
  node.checked_state()
}

pub fn accesskit_node_supports_action(node: &accesskit::Node, action: accesskit::Action) -> bool {
  node.supports_action(action)
}

#[cfg(test)]
mod tests {
  use super::*;
  use accesskit::{Action, NodeBuilder, NodeClassSet, NodeId, Role, TreeUpdate};
  use std::num::NonZeroU128;

  fn id(n: u128) -> NodeId {
    NodeId(NonZeroU128::new(n).expect("node id must be non-zero"))
  }

  fn node_with_classes(
    classes: &mut NodeClassSet,
    role: Role,
    name: &str,
    children: &[NodeId],
  ) -> accesskit::Node {
    let mut builder = NodeBuilder::new(role);
    if !name.is_empty() {
      builder.set_name(name);
    }
    for child in children {
      builder.push_child(*child);
    }
    builder.build(classes)
  }

  fn output_with_update(update: TreeUpdate) -> egui::PlatformOutput {
    let mut out = egui::PlatformOutput::default();
    out.accesskit_update = Some(update);
    out
  }

  #[test]
  fn reachable_node_ids_are_preorder_and_exclude_orphans() {
    let root_id = id(1);
    let a_id = id(2);
    let b_id = id(3);
    let a1_id = id(4);
    let orphan_id = id(999);

    let mut classes = NodeClassSet::new();
    let root = node_with_classes(&mut classes, Role::Window, "root", &[a_id, b_id]);
    let a = node_with_classes(&mut classes, Role::Group, "A", &[a1_id]);
    let a1 = node_with_classes(&mut classes, Role::Button, "A1", &[]);
    let b = node_with_classes(&mut classes, Role::Button, "B", &[]);
    let orphan = node_with_classes(&mut classes, Role::Button, "orphan", &[]);

    let update = TreeUpdate {
      nodes: vec![
        (root_id, root),
        (a_id, a),
        (a1_id, a1),
        (b_id, b),
        (orphan_id, orphan),
      ],
      tree: Some(accesskit::Tree {
        root: root_id,
        root_scroller: None,
      }),
      focus: Some(root_id),
    };

    let reachable = accesskit_reachable_node_ids_from_update(&update, None, std::iter::empty());
    assert_eq!(reachable, vec![root_id, a_id, a1_id, b_id]);
    let orphans = accesskit_orphan_node_ids_from_update(&update, None, std::iter::empty());
    assert_eq!(orphans, vec![orphan_id]);
    let orphan_snap =
      accesskit_orphan_nodes_snapshot_from_update(&update, None, std::iter::empty());
    assert_eq!(
      orphan_snap,
      vec![AccessKitNodeSnapshot {
        id: orphan_id.0.get().to_string(),
        role: "Button".to_string(),
        name: "orphan".to_string(),
      }]
    );

    let snapshot = accesskit_reachable_nodes_snapshot_from_update(&update, None, std::iter::empty());
    assert_eq!(
      snapshot,
      vec![
        AccessKitReachableNodeSnapshot {
          role: "Window".to_string(),
          name: "root".to_string(),
        },
        AccessKitReachableNodeSnapshot {
          role: "Group".to_string(),
          name: "A".to_string(),
        },
        AccessKitReachableNodeSnapshot {
          role: "Button".to_string(),
          name: "A1".to_string(),
        },
        AccessKitReachableNodeSnapshot {
          role: "Button".to_string(),
          name: "B".to_string(),
        },
      ]
    );
  }

  #[test]
  fn reachable_node_ids_can_use_root_fallback_and_additional_nodes() {
    // Simulate an incremental update where AccessKit omits `update.tree` and does not resend the
    // unchanged root node.
    let root_id = id(1);
    let child_id = id(2);
    let grandchild_id = id(3);

    let mut classes = NodeClassSet::new();
    let root = node_with_classes(&mut classes, Role::Window, "root", &[child_id]);
    let child = node_with_classes(&mut classes, Role::Group, "child", &[grandchild_id]);
    let grandchild = node_with_classes(&mut classes, Role::Button, "grandchild", &[]);

    let update = TreeUpdate {
      nodes: vec![(child_id, child), (grandchild_id, grandchild)],
      tree: None,
      focus: Some(child_id),
    };

    let additional_nodes = vec![(root_id, &root)];
    let reachable =
      accesskit_reachable_node_ids_from_update(&update, Some(root_id), additional_nodes);
    assert_eq!(reachable, vec![root_id, child_id, grandchild_id]);

    let additional_nodes = vec![(root_id, &root)];
    let orphans = accesskit_orphan_node_ids_from_update(&update, Some(root_id), additional_nodes);
    assert!(orphans.is_empty());
  }

  #[test]
  fn accesskit_test_tree_tracks_incremental_updates_and_detects_orphans() {
    let root_id = id(1);
    let child_id = id(2);
    let orphan_id = id(3);

    let mut classes = accesskit::NodeClassSet::new();
    let root = node_with_classes(&mut classes, accesskit::Role::Window, "root", &[child_id]);
    let child = node_with_classes(&mut classes, accesskit::Role::Button, "child", &[]);

    let initial = accesskit::TreeUpdate {
      nodes: vec![(root_id, root), (child_id, child)],
      tree: Some(accesskit::Tree {
        root: root_id,
        root_scroller: None,
      }),
      focus: Some(child_id),
    };

    let mut store = AccessKitTestTree::default();
    store.apply_update(&initial);

    // Incremental update introduces a brand-new node but does not attach it anywhere.
    let orphan = node_with_classes(&mut classes, accesskit::Role::Button, "orphan", &[]);
    let incremental = accesskit::TreeUpdate {
      nodes: vec![(orphan_id, orphan)],
      tree: None,
      focus: None,
    };

    assert_eq!(store.reachable_node_ids(&incremental), vec![root_id, child_id]);
    assert_eq!(store.orphan_node_ids(&incremental), vec![orphan_id]);

    let snapshot = store.connectivity_snapshot(&incremental);
    assert_eq!(snapshot.root_id, root_id.0.get().to_string());
    assert_eq!(
      snapshot.reachable,
      vec![
        AccessKitReachableNodeSnapshot {
          role: "Window".to_string(),
          name: "root".to_string(),
        },
        AccessKitReachableNodeSnapshot {
          role: "Button".to_string(),
          name: "child".to_string(),
        },
      ]
    );
    assert_eq!(
      snapshot.orphans,
      vec![AccessKitNodeSnapshot {
        id: orphan_id.0.get().to_string(),
        role: "Button".to_string(),
        name: "orphan".to_string(),
      }]
    );
  }

  #[test]
  #[should_panic(expected = "contains orphan nodes")]
  fn assert_accesskit_update_has_no_orphans_panics_on_orphans() {
    let root_id = id(1);
    let orphan_id = id(2);
    let mut classes = accesskit::NodeClassSet::new();
    let root = node_with_classes(&mut classes, accesskit::Role::Window, "root", &[]);
    let orphan = node_with_classes(&mut classes, accesskit::Role::Button, "orphan", &[]);
    let update = accesskit::TreeUpdate {
      nodes: vec![(root_id, root), (orphan_id, orphan)],
      tree: Some(accesskit::Tree {
        root: root_id,
        root_scroller: None,
      }),
      focus: None,
    };
    assert_accesskit_update_has_no_orphans(&update, None, std::iter::empty());
  }

  #[test]
  #[should_panic(expected = "not reachable")]
  fn assert_accesskit_node_is_reachable_panics_when_unreachable() {
    let root_id = id(1);
    let orphan_id = id(2);
    let mut classes = accesskit::NodeClassSet::new();
    let root = node_with_classes(&mut classes, accesskit::Role::Window, "root", &[]);
    let orphan = node_with_classes(&mut classes, accesskit::Role::Button, "orphan", &[]);
    let update = accesskit::TreeUpdate {
      nodes: vec![(root_id, root), (orphan_id, orphan)],
      tree: Some(accesskit::Tree {
        root: root_id,
        root_scroller: None,
      }),
      focus: None,
    };
    assert_accesskit_node_is_reachable(&update, orphan_id, None, std::iter::empty());
  }

  #[test]
  fn node_by_name_finds_unique_node_and_focus_name_resolves() {
    let mut classes = NodeClassSet::default();

    let mut root = NodeBuilder::new(Role::Window);
    root.set_name("Root");
    root.push_child(id(2));

    let mut focused = NodeBuilder::new(Role::Button);
    focused.set_name("  Focus target  ");
    focused.add_action(Action::Default);

    let update = TreeUpdate {
      nodes: vec![
        (id(1), root.build(&mut classes)),
        (id(2), focused.build(&mut classes)),
      ],
      tree: Some(accesskit::Tree {
        root: id(1),
        root_scroller: None,
      }),
      focus: Some(id(2)),
    };

    let (found_id, found_node) =
      accesskit_node_by_name(&update, "Focus target").expect("expected node by name");
    assert_eq!(found_id, id(2));
    assert_eq!(found_node.name().unwrap_or("").trim(), "Focus target");
    assert_eq!(accesskit_focus_name(&update), Some("Focus target".to_string()));

    let platform_output = output_with_update(update);

    let (found_id, found_node) =
      accesskit_node_by_name_from_platform_output(&platform_output, "Focus target")
        .expect("expected node by name");
    assert_eq!(found_id, id(2));
    assert_eq!(found_node.name().unwrap_or("").trim(), "Focus target");

    assert_eq!(
      accesskit_focus_name_from_platform_output(&platform_output),
      Some("Focus target".to_string())
    );
    assert!(accesskit_node_by_name_from_platform_output(&platform_output, "Missing").is_none());
  }

  #[test]
  #[should_panic(expected = "multiple AccessKit nodes matched name")]
  fn node_by_name_panics_on_duplicate_names() {
    let mut classes = NodeClassSet::new();

    let mut root = NodeBuilder::new(Role::Window);
    root.set_name("Root");
    root.push_child(id(2));
    root.push_child(id(3));

    let mut a = NodeBuilder::new(Role::Button);
    a.set_name("Dup");
    let mut b = NodeBuilder::new(Role::Button);
    b.set_name("Dup");

    let update = TreeUpdate {
      nodes: vec![
        (id(1), root.build(&mut classes)),
        (id(2), a.build(&mut classes)),
        (id(3), b.build(&mut classes)),
      ],
      tree: Some(accesskit::Tree {
        root: id(1),
        root_scroller: None,
      }),
      focus: None,
    };

    let _ = accesskit_node_by_name(&update, "Dup");
  }

  #[test]
  fn node_state_helpers_reflect_accesskit_properties() {
    let mut classes = NodeClassSet::new();

    let mut group = NodeBuilder::new(Role::Group);
    group.set_name("Group");
    group.set_expanded(true);
    group.add_action(Action::Expand);

    let mut tab = NodeBuilder::new(Role::Tab);
    tab.set_name("Tab");
    tab.set_selected(true);

    let mut checkbox = NodeBuilder::new(Role::CheckBox);
    checkbox.set_name("Check");
    checkbox.set_checked_state(accesskit::CheckedState::True);

    let mut toggle = NodeBuilder::new(Role::ToggleButton);
    toggle.set_name("Toggle");
    toggle.set_checked_state(accesskit::CheckedState::True);

    let mut switch = NodeBuilder::new(Role::Switch);
    switch.set_name("Switch");
    switch.set_checked_state(accesskit::CheckedState::False);

    let group = group.build(&mut classes);
    let tab = tab.build(&mut classes);
    let checkbox = checkbox.build(&mut classes);
    let toggle = toggle.build(&mut classes);
    let switch = switch.build(&mut classes);

    assert_eq!(accesskit_node_expanded(&group), Some(true));
    assert!(accesskit_node_supports_action(&group, Action::Expand));

    assert_eq!(accesskit_node_selected(&tab), Some(true));

    assert_eq!(
      accesskit_node_checked(&checkbox),
      Some(accesskit::CheckedState::True)
    );

    assert_eq!(
      accesskit_node_pressed(&toggle),
      Some(accesskit::CheckedState::True)
    );
    assert_eq!(
      accesskit_node_toggled(&switch),
      Some(accesskit::CheckedState::False)
    );
  }
}
