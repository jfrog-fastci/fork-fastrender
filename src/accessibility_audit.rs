use crate::accessibility::AccessibilityNode;

/// A single issue reported by `audit_accessibility_tree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessibilityAuditIssue {
  /// Pre-order index in the provided accessibility tree (root is `0`).
  pub node_id: usize,
  pub role: String,
  pub message: String,
}

fn has_non_empty_text(value: Option<&str>) -> bool {
  value.is_some_and(|value| !value.trim().is_empty())
}

fn role_requires_accessible_name(role: &str) -> bool {
  // FastRender emits lowercased role tokens, mirroring the ARIA role model.
  matches!(
    role,
    "button"
      | "link"
      | "checkbox"
      | "radio"
      | "menuitem"
      | "option"
      | "combobox"
      | "listbox"
      | "textbox"
      | "searchbox"
      | "slider"
      | "spinbutton"
  )
}

fn textbox_like_allows_alternative_label(node: &AccessibilityNode) -> bool {
  if has_non_empty_text(node.description.as_deref()) {
    return true;
  }

  node
    .relations
    .as_ref()
    .is_some_and(|relations| !relations.labelled_by.is_empty())
}

/// Perform a basic accessibility audit on the exported accessibility tree.
///
/// This intentionally checks only a small set of rules that are valuable for FastRender's
/// chrome/UI HTML, where missing labels can easily happen during migrations.
pub fn audit_accessibility_tree(root: &AccessibilityNode) -> Vec<AccessibilityAuditIssue> {
  struct Frame<'a> {
    node: &'a AccessibilityNode,
    next_child: usize,
    node_id: usize,
  }

  let mut issues: Vec<AccessibilityAuditIssue> = Vec::new();
  let mut next_node_id = 0usize;

  let mut stack: Vec<Frame<'_>> = Vec::new();
  stack.push(Frame {
    node: root,
    next_child: 0,
    node_id: next_node_id,
  });
  next_node_id += 1;

  while let Some(frame) = stack.last_mut() {
    if frame.next_child == 0 {
      let node = frame.node;
      let node_id = frame.node_id;
      let role = node.role.as_str();

      if node.states.focusable {
        if role == "generic" {
          issues.push(AccessibilityAuditIssue {
            node_id,
            role: node.role.clone(),
            message: "Focusable node has suspicious role=\"generic\"".to_string(),
          });
        }

        if role_requires_accessible_name(role) {
          let mut has_name = has_non_empty_text(node.name.as_deref());

          // Optional exception: allow textboxes/searchboxes to omit a name if they have other
          // accessible labelling signal we can detect in the exported tree.
          if !has_name && matches!(role, "textbox" | "searchbox") {
            has_name = textbox_like_allows_alternative_label(node);
          }

          if !has_name {
            issues.push(AccessibilityAuditIssue {
              node_id,
              role: node.role.clone(),
              message: "Focusable control is missing a non-empty accessible name".to_string(),
            });
          }
        }
      }
    }

    if frame.next_child < frame.node.children.len() {
      let child = &frame.node.children[frame.next_child];
      frame.next_child += 1;

      let child_id = next_node_id;
      next_node_id += 1;
      stack.push(Frame {
        node: child,
        next_child: 0,
        node_id: child_id,
      });
    } else {
      stack.pop();
    }
  }

  issues
}

