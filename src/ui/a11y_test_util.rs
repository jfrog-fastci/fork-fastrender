#![cfg(all(test, feature = "browser_ui"))]

use serde::Serialize;

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

fn accesskit_update_from_output(output: &egui::FullOutput) -> &accesskit::TreeUpdate {
  output
    .platform_output
    .accesskit_update
    .as_ref()
    .expect(
      "egui did not emit an AccessKit update. \
      Ensure `ctx.enable_accesskit()` was called for the frame under test, \
      and that `egui-winit` is built with its `accesskit` feature.",
    )
}

/// Extract a deterministic, JSON-serializable snapshot of the AccessKit tree update emitted by egui.
///
/// The output is intentionally lossy: it records only `id`, `role`, and the accessible `name`.
pub fn accesskit_snapshot_from_full_output(output: &egui::FullOutput) -> AccessKitSnapshot {
  let update = accesskit_update_from_output(output);

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

/// Returns a sorted list of all non-empty accessible names emitted by egui/AccessKit for the frame.
pub fn accesskit_names_from_full_output(output: &egui::FullOutput) -> Vec<String> {
  let snapshot = accesskit_snapshot_from_full_output(output);

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

/// Convenience helper for `assert_eq!` / snapshot-style tests.
pub fn accesskit_pretty_json_from_full_output(output: &egui::FullOutput) -> String {
  let snapshot = accesskit_snapshot_from_full_output(output);
  serde_json::to_string_pretty(&snapshot).expect("accesskit snapshot must serialize to JSON")
}
