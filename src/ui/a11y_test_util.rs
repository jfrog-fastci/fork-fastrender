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

/// A stable (ID-free) view of the named nodes emitted by egui/AccessKit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccessKitNamedRoleSnapshot {
  pub role: String,
  pub name: String,
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
