#![cfg(all(test, feature = "browser_ui"))]

use serde::Serialize;

/// A snapshot-friendly summary of a single AccessKit node emitted by FastRender.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccessKitNodeSnapshot {
  /// AccessKit's `NodeId` is a `NonZeroU128`. We keep it as a string to avoid any JSON number
  /// portability issues and to keep snapshots stable.
  pub id: String,
  pub role: String,
  #[serde(skip_serializing_if = "String::is_empty")]
  pub name: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub bounds: Option<[f64; 4]>,
}

/// Snapshot-friendly representation of an AccessKit tree update.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccessKitSnapshot {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub root_id: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub focus_id: Option<String>,
  pub nodes: Vec<AccessKitNodeSnapshot>,
}

/// Extract a deterministic, JSON-serializable snapshot of a FastRender-generated AccessKit tree update.
pub fn snapshot_from_tree_update(update: &accesskit::TreeUpdate) -> AccessKitSnapshot {
  let root_id = update.tree.as_ref().map(|t| t.root.0.get().to_string());
  let focus_id = update.focus.map(|id| id.0.get().to_string());

  let mut nodes: Vec<AccessKitNodeSnapshot> = update
    .nodes
    .iter()
    .map(|(id, node)| AccessKitNodeSnapshot {
      id: id.0.get().to_string(),
      role: format!("{:?}", node.role()),
      name: node.name().unwrap_or("").trim().to_string(),
      bounds: node.bounds().map(|rect| [rect.x0, rect.y0, rect.x1, rect.y1]),
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

/// Convenience helper for `assert_eq!` / snapshot-style tests.
pub fn pretty_json_from_tree_update(update: &accesskit::TreeUpdate) -> String {
  let snapshot = snapshot_from_tree_update(update);
  serde_json::to_string_pretty(&snapshot).expect("accesskit snapshot must serialize to JSON")
}

  #[cfg(test)]
  mod tests {
  use super::*;
  use accesskit::{NodeBuilder, NodeClassSet, NodeId, Rect, Role, Tree, TreeUpdate};
  use std::num::NonZeroU128;

  fn id(n: u128) -> NodeId {
    NodeId(NonZeroU128::new(n).expect("node id must be non-zero"))
  }

  fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Rect {
    Rect { x0, y0, x1, y1 }
  }

  fn build_update(nodes: Vec<(NodeId, accesskit::Node)>) -> TreeUpdate {
    TreeUpdate {
      nodes,
      tree: Some(Tree::new(id(1))),
      focus: Some(id(2)),
    }
  }

  #[test]
  fn snapshot_sorts_nodes_by_role_name_id() {
    let mut classes = NodeClassSet::default();

    let mut root = NodeBuilder::new(Role::Window);
    root.set_name("Root".to_string());
    root.set_bounds(rect(0.0, 0.0, 800.0, 600.0));
    root.push_child(id(2));
    root.push_child(id(3));

    let mut button_click = NodeBuilder::new(Role::Button);
    button_click.set_name("Click".to_string());

    let mut button_alpha = NodeBuilder::new(Role::Button);
    button_alpha.set_name("Alpha".to_string());
    button_alpha.set_bounds(rect(10.0, 20.0, 30.0, 40.0));

    let update = build_update(vec![
      (id(1), root.build(&mut classes)),
      (id(3), button_click.build(&mut classes)),
      (id(2), button_alpha.build(&mut classes)),
    ]);

    let snapshot = snapshot_from_tree_update(&update);
    assert_eq!(snapshot.root_id, Some("1".to_string()));
    assert_eq!(snapshot.focus_id, Some("2".to_string()));
    assert_eq!(
      snapshot.nodes,
      vec![
        AccessKitNodeSnapshot {
          id: "2".to_string(),
          role: "Button".to_string(),
          name: "Alpha".to_string(),
          bounds: Some([10.0, 20.0, 30.0, 40.0]),
        },
        AccessKitNodeSnapshot {
          id: "3".to_string(),
          role: "Button".to_string(),
          name: "Click".to_string(),
          bounds: None,
        },
        AccessKitNodeSnapshot {
          id: "1".to_string(),
          role: "Window".to_string(),
          name: "Root".to_string(),
          bounds: Some([0.0, 0.0, 800.0, 600.0]),
        },
      ]
    );
  }

  #[test]
  fn pretty_json_is_deterministic_across_input_order() {
    let update_a = {
      let mut classes = NodeClassSet::default();

      let mut root = NodeBuilder::new(Role::Window);
      root.set_name("Root".to_string());

      let mut button = NodeBuilder::new(Role::Button);
      button.set_name("Alpha".to_string());

      let mut text = NodeBuilder::new(Role::StaticText);
      text.set_name("Hello".to_string());

      build_update(vec![
        (id(1), root.build(&mut classes)),
        (id(2), button.build(&mut classes)),
        (id(3), text.build(&mut classes)),
      ])
    };

    let update_b = {
      let mut classes = NodeClassSet::default();

      let mut root = NodeBuilder::new(Role::Window);
      root.set_name("Root".to_string());

      let mut button = NodeBuilder::new(Role::Button);
      button.set_name("Alpha".to_string());

      let mut text = NodeBuilder::new(Role::StaticText);
      text.set_name("Hello".to_string());

      build_update(vec![
        (id(3), text.build(&mut classes)),
        (id(1), root.build(&mut classes)),
        (id(2), button.build(&mut classes)),
      ])
    };

    assert_eq!(
      pretty_json_from_tree_update(&update_a),
      pretty_json_from_tree_update(&update_b)
    );
  }
}
