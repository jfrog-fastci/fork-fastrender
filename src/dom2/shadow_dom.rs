use crate::dom::{ShadowRootMode, HTML_NAMESPACE};

use super::{Document, NodeId, NodeKind};

fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

fn node_is_element_like(kind: &NodeKind) -> bool {
  matches!(kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
}

fn node_is_template_element(kind: &NodeKind) -> bool {
  matches!(
    kind,
    NodeKind::Element { tag_name, .. } if tag_name.eq_ignore_ascii_case("template")
  )
}

fn node_is_shadow_root(kind: &NodeKind) -> bool {
  matches!(kind, NodeKind::ShadowRoot { .. })
}

fn get_attribute<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
  attrs
    .iter()
    .find(|(k, _)| k.eq_ignore_ascii_case(name))
    .map(|(_, v)| v.as_str())
}

fn has_attribute(attrs: &[(String, String)], name: &str) -> bool {
  attrs.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
}

fn parse_shadow_root_definition(kind: &NodeKind) -> Option<(ShadowRootMode, bool)> {
  let NodeKind::Element {
    tag_name,
    namespace,
    attributes,
  } = kind
  else {
    return None;
  };

  if !tag_name.eq_ignore_ascii_case("template") {
    return None;
  }
  // Declarative shadow DOM only applies to HTML templates, not e.g. SVG <template>.
  if !is_html_namespace(namespace) {
    return None;
  }

  let mode_attr =
    get_attribute(attributes, "shadowroot").or_else(|| get_attribute(attributes, "shadowrootmode"))?;
  let mode = if mode_attr.eq_ignore_ascii_case("open") {
    ShadowRootMode::Open
  } else if mode_attr.eq_ignore_ascii_case("closed") {
    ShadowRootMode::Closed
  } else {
    return None;
  };

  let delegates_focus = has_attribute(attributes, "shadowrootdelegatesfocus");

  Some((mode, delegates_focus))
}

fn promote_template_to_shadow_root(
  doc: &mut Document,
  host: NodeId,
  template: NodeId,
  template_idx: usize,
  mode: ShadowRootMode,
  delegates_focus: bool,
) {
  // Detach the template from the host.
  doc.nodes[host.index()].children.remove(template_idx);
  doc.nodes[template.index()].parent = None;

  // Create a new shadow root node (detached for now so we can insert at index 0).
  let shadow_root = doc.push_node(
    NodeKind::ShadowRoot {
      mode,
      delegates_focus,
    },
    None,
    /* inert_subtree */ false,
  );

  // Move template children to shadow root.
  let moved_children = std::mem::take(&mut doc.nodes[template.index()].children);
  for &child in &moved_children {
    doc.nodes[child.index()].parent = Some(shadow_root);
  }
  doc.nodes[shadow_root.index()].children = moved_children;

  // Attach shadow root to host at index 0.
  doc.nodes[shadow_root.index()].parent = Some(host);
  doc.nodes[host.index()].children.insert(0, shadow_root);
}

impl Document {
  /// Attach declarative shadow roots represented by `<template shadowroot=...>` elements.
  ///
  /// This mirrors `crate::dom::attach_shadow_roots` and must run as a post-processing step once
  /// HTML parsing is complete.
  pub(crate) fn attach_shadow_roots(&mut self) {
    // Run in post-order so nested declarative shadow roots are promoted before their ancestors.
    let mut stack: Vec<(NodeId, bool)> = Vec::new();
    stack.push((self.root, false));

    while let Some((id, after_children)) = stack.pop() {
      if !after_children {
        stack.push((id, true));
        let node = self.node(id);

        // Declarative shadow DOM only promotes the first shadow root template child of a shadow host
        // element. Additional `<template shadowroot=...>` siblings must remain inert, so we skip
        // traversing into them here.
        let first_declarative_shadow_template =
          if node_is_element_like(&node.kind)
            && !node_is_template_element(&node.kind)
            && !node.children.iter().any(|&child_id| {
              self.node(child_id).parent == Some(id) && node_is_shadow_root(&self.node(child_id).kind)
            })
          {
            node.children.iter().position(|&child_id| {
              self.node(child_id).parent == Some(id)
                && parse_shadow_root_definition(&self.node(child_id).kind).is_some()
            })
          } else {
            None
          };

        // Push children in reverse so we traverse in tree order.
        for idx in (0..node.children.len()).rev() {
          let child_id = node.children[idx];
          if self.node(child_id).parent != Some(id) {
            continue;
          }
          let child_kind = &self.node(child_id).kind;

          // Template contents are inert; only the first declarative shadow DOM template is walked so
          // nested declarative shadow roots inside it can be promoted.
          if node_is_template_element(child_kind) && first_declarative_shadow_template != Some(idx) {
            continue;
          }

          stack.push((child_id, false));
        }

        continue;
      }

      let shadow_template = {
        let node = self.node(id);
        if !node_is_element_like(&node.kind) || node_is_template_element(&node.kind) {
          None
        } else if node.children.iter().any(|&child_id| {
          self.node(child_id).parent == Some(id) && node_is_shadow_root(&self.node(child_id).kind)
        }) {
          None
        } else {
          node
            .children
            .iter()
            .enumerate()
            .find_map(|(idx, &child_id)| {
              if self.node(child_id).parent != Some(id) {
                return None;
              }
              let child_kind = &self.node(child_id).kind;
              parse_shadow_root_definition(child_kind)
                .map(|(mode, delegates_focus)| (idx, child_id, mode, delegates_focus))
            })
        }
      };

      let Some((template_idx, template_id, mode, delegates_focus)) = shadow_template else {
        continue;
      };

      promote_template_to_shadow_root(self, id, template_id, template_idx, mode, delegates_focus);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::snapshot::snapshot_dom;
  use selectors::context::QuirksMode;

  fn node_id_attribute(kind: &NodeKind) -> Option<&str> {
    match kind {
      NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("id"))
        .map(|(_, v)| v.as_str()),
      _ => None,
    }
  }

  fn find_node_by_id(doc: &Document, id: &str) -> Option<NodeId> {
    doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| (node_id_attribute(&node.kind) == Some(id)).then_some(NodeId(idx)))
  }

  #[test]
  fn attach_shadow_roots_promotes_only_first_template_and_supports_nested_dsd() {
    let html = concat!(
      "<!doctype html>",
      "<div id=host>",
      "<template id=outer1 shadowroot=open>",
      "<div id=inner>",
      "<template id=inner_tpl shadowroot=closed><span>nested</span></template>",
      "<b>light</b>",
      "</div>",
      "</template>",
      "<template id=outer2 shadowroot=open>",
      "<div id=inert_inner>",
      "<template id=inert_inner_tpl shadowroot=open><i>ignore</i></template>",
      "</div>",
      "</template>",
      "<p>light</p>",
      "</div>"
    );

    let expected = crate::dom::parse_html(html).unwrap();
    let doc = crate::dom2::parse_html(html).unwrap();

    let host = find_node_by_id(&doc, "host").expect("host element not found");
    let host_children = doc.node(host).children.clone();
    assert!(
      matches!(
        doc.node(host_children[0]).kind,
        NodeKind::ShadowRoot {
          mode: ShadowRootMode::Open,
          delegates_focus: false
        }
      ),
      "host should have an attached open shadow root at index 0"
    );

    let outer1 = find_node_by_id(&doc, "outer1").expect("outer1 template not found");
    assert_eq!(
      doc.node(outer1).parent,
      None,
      "promoted shadowroot template should be detached"
    );
    assert!(
      doc.node(outer1).children.is_empty(),
      "promoted template should have its children moved out"
    );

    let outer2 = find_node_by_id(&doc, "outer2").expect("outer2 template not found");
    assert_eq!(
      doc.node(outer2).parent,
      Some(host),
      "subsequent shadowroot templates must remain in the light DOM"
    );

    let inner = find_node_by_id(&doc, "inner").expect("inner host not found");
    let inner_children = doc.node(inner).children.clone();
    assert!(
      matches!(
        doc.node(inner_children[0]).kind,
        NodeKind::ShadowRoot {
          mode: ShadowRootMode::Closed,
          delegates_focus: false
        }
      ),
      "nested shadow root should be promoted within the first template contents"
    );

    let inner_tpl = find_node_by_id(&doc, "inner_tpl").expect("inner_tpl template not found");
    assert_eq!(
      doc.node(inner_tpl).parent,
      None,
      "nested promoted template should be detached"
    );
    assert!(
      doc.node(inner_tpl).children.is_empty(),
      "nested promoted template should have its children moved out"
    );

    // The second template subtree remains inert; nested declarative shadow roots inside it must not
    // be promoted.
    let inert_inner = find_node_by_id(&doc, "inert_inner").expect("inert_inner host not found");
    let inert_inner_children = doc.node(inert_inner).children.clone();
    assert!(
      inert_inner_children
        .iter()
        .all(|&child| !matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. })),
      "inert template subtree should not have shadow roots attached"
    );
    assert!(
      inert_inner_children
        .iter()
        .any(|&child| node_is_template_element(&doc.node(child).kind)),
      "inert template subtree should retain the declarative shadow root template element"
    );

    let roundtrip = doc.to_renderer_dom();
    assert_eq!(
      snapshot_dom(&expected),
      snapshot_dom(&roundtrip),
      "dom2 shadow attachment should match crate::dom::parse_html snapshot"
    );
  }

  #[test]
  fn attach_shadow_roots_ignores_detached_shadow_templates() {
    // Declarative shadow DOM should only consider children that are actually connected to their
    // parent via the `parent` pointer. If a tree is partially detached (stale entry in `children`
    // list), promotion must not treat it as a live shadow root template.
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let root = doc.root();

    let host = doc.push_node(
      NodeKind::Element {
        tag_name: "div".to_string(),
        namespace: "".to_string(),
        attributes: vec![("id".to_string(), "host".to_string())],
      },
      Some(root),
      /* inert_subtree */ false,
    );
    let template = doc.push_node(
      NodeKind::Element {
        tag_name: "template".to_string(),
        namespace: "".to_string(),
        attributes: vec![("shadowroot".to_string(), "open".to_string())],
      },
      Some(host),
      /* inert_subtree */ false,
    );
    let span = doc.push_node(
      NodeKind::Element {
        tag_name: "span".to_string(),
        namespace: "".to_string(),
        attributes: Vec::new(),
      },
      Some(template),
      /* inert_subtree */ false,
    );
    doc.push_node(
      NodeKind::Text {
        content: "shadow".to_string(),
      },
      Some(span),
      /* inert_subtree */ false,
    );

    // Detach the template by severing the parent pointer, but leave it in the host's children list.
    doc.node_mut(template).parent = None;

    doc.attach_shadow_roots();

    assert!(
      doc
        .node(host)
        .children
        .iter()
        .all(|&child| !matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. })),
      "detached shadowroot template must not be promoted"
    );
    assert!(
      doc.node(host).children.contains(&template),
      "detached template should remain untouched"
    );
  }
}

