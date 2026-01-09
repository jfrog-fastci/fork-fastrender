use crate::dom::{ShadowRootMode, HTML_NAMESPACE};

use super::{Document, NodeId, NodeKind};

fn is_html_namespace(namespace: &str) -> bool {
  namespace.is_empty() || namespace == HTML_NAMESPACE
}

fn element_tag_name(kind: &NodeKind) -> Option<&str> {
  match kind {
    NodeKind::Element { tag_name, .. } => Some(tag_name.as_str()),
    _ => None,
  }
}

fn is_template_element(kind: &NodeKind) -> bool {
  element_tag_name(kind)
    .is_some_and(|tag| tag.eq_ignore_ascii_case("template"))
}

fn is_element(kind: &NodeKind) -> bool {
  matches!(kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
}

fn is_shadow_root(kind: &NodeKind) -> bool {
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

fn parse_shadow_root_definition(doc: &Document, template: NodeId) -> Option<(ShadowRootMode, bool)> {
  let template_node = doc.node(template);
  let NodeKind::Element {
    tag_name,
    namespace,
    attributes,
  } = &template_node.kind
  else {
    return None;
  };

  if !tag_name.eq_ignore_ascii_case("template") {
    return None;
  }
  if !is_html_namespace(namespace) {
    return None;
  }

  let mode_attr = get_attribute(attributes, "shadowroot")
    .or_else(|| get_attribute(attributes, "shadowrootmode"))?;
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

impl Document {
  pub fn attach_shadow_roots(&mut self) {
    // Post-order traversal so nested declarative shadow roots inside template contents are promoted
    // before their shadow host is processed.
    let mut stack: Vec<(NodeId, bool)> = Vec::new();
    stack.push((self.root, false));

    while let Some((id, after_children)) = stack.pop() {
      if !after_children {
        stack.push((id, true));

        let first_declarative_shadow_template = {
          let node = self.node(id);
          let eligible_host = is_element(&node.kind) && !is_template_element(&node.kind);
          if eligible_host && !node.children.iter().any(|&child| is_shadow_root(&self.node(child).kind)) {
            node
              .children
              .iter()
              .position(|&child| parse_shadow_root_definition(self, child).is_some())
          } else {
            None
          }
        };

        let children_len = self.node(id).children.len();
        for idx in (0..children_len).rev() {
          let child_id = self.node(id).children[idx];
          let child_kind = &self.node(child_id).kind;
          if is_template_element(child_kind) && first_declarative_shadow_template != Some(idx) {
            continue;
          }
          stack.push((child_id, false));
        }
        continue;
      }

      let is_shadow_host = {
        let host_kind = &self.node(id).kind;
        is_element(host_kind) && !is_template_element(host_kind)
      };
      if !is_shadow_host {
        continue;
      }

      if self
        .node(id)
        .children
        .iter()
        .any(|&child| is_shadow_root(&self.node(child).kind))
      {
        continue;
      }

      let mut shadow_template = None;
      for (idx, &child) in self.node(id).children.iter().enumerate() {
        if let Some((mode, delegates_focus)) = parse_shadow_root_definition(self, child) {
          shadow_template = Some((idx, child, mode, delegates_focus));
          break;
        }
      }

      let Some((template_idx, template_id, mode, delegates_focus)) = shadow_template else {
        continue;
      };

      // Detach the template from the host, then promote its contents into a new ShadowRoot node.
      self.node_mut(id).children.remove(template_idx);
      self.node_mut(template_id).parent = None;
      let template_children = std::mem::take(&mut self.node_mut(template_id).children);

      let shadow_root_id = self.push_node(
        NodeKind::ShadowRoot {
          mode,
          delegates_focus,
        },
        None,
        /* inert_subtree */ false,
      );
      self.node_mut(shadow_root_id).parent = Some(id);
      self.node_mut(shadow_root_id).children = template_children;
      let moved_children = self.node(shadow_root_id).children.clone();
      for child in moved_children {
        self.node_mut(child).parent = Some(shadow_root_id);
      }

      let light_children = std::mem::take(&mut self.node_mut(id).children);
      let mut combined = Vec::with_capacity(light_children.len() + 1);
      combined.push(shadow_root_id);
      combined.extend(light_children);
      self.node_mut(id).children = combined;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::snapshot::snapshot_dom;
  use selectors::context::QuirksMode;

  fn push_element(
    doc: &mut Document,
    parent: NodeId,
    tag_name: &str,
    attributes: Vec<(String, String)>,
    inert_subtree: bool,
  ) -> NodeId {
    doc.push_node(
      NodeKind::Element {
        tag_name: tag_name.to_string(),
        namespace: "".to_string(),
        attributes,
      },
      Some(parent),
      inert_subtree,
    )
  }

  fn push_template(doc: &mut Document, parent: NodeId, attributes: Vec<(String, String)>) -> NodeId {
    push_element(doc, parent, "template", attributes, /* inert_subtree */ true)
  }

  fn push_text(doc: &mut Document, parent: NodeId, content: &str) -> NodeId {
    doc.push_node(
      NodeKind::Text {
        content: content.to_string(),
      },
      Some(parent),
      /* inert_subtree */ false,
    )
  }

  fn push_slot(doc: &mut Document, parent: NodeId, attributes: Vec<(String, String)>) -> NodeId {
    doc.push_node(
      NodeKind::Slot {
        namespace: "".to_string(),
        attributes,
        assigned: false,
      },
      Some(parent),
      /* inert_subtree */ false,
    )
  }

  #[test]
  fn attach_shadow_roots_matches_legacy_snapshot() {
    let html = concat!(
      "<!doctype html>",
      "<html><head></head><body>",
      "<div id=host>",
      "<template shadowroot=open><slot></slot><span>shadow</span></template>",
      "<p>light</p>",
      "</div>",
      "</body></html>"
    );
    let legacy = crate::dom::parse_html(html).unwrap();

    let mut doc = Document::new(QuirksMode::NoQuirks);
    let root = doc.root();
    let html_id = push_element(&mut doc, root, "html", Vec::new(), false);
    push_element(&mut doc, html_id, "head", Vec::new(), false);
    let body_id = push_element(&mut doc, html_id, "body", Vec::new(), false);

    let host_id = push_element(
      &mut doc,
      body_id,
      "div",
      vec![("id".to_string(), "host".to_string())],
      false,
    );
    let template_id = push_template(
      &mut doc,
      host_id,
      vec![("shadowroot".to_string(), "open".to_string())],
    );
    push_slot(&mut doc, template_id, Vec::new());
    let span_id = push_element(&mut doc, template_id, "span", Vec::new(), false);
    push_text(&mut doc, span_id, "shadow");
    let p_id = push_element(&mut doc, host_id, "p", Vec::new(), false);
    push_text(&mut doc, p_id, "light");

    doc.attach_shadow_roots();

    let roundtrip = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&legacy), snapshot_dom(&roundtrip));
  }

  #[test]
  fn attach_shadow_roots_handles_nested_and_ignores_later_templates() {
    let html = concat!(
      "<!doctype html>",
      "<html><head></head><body>",
      "<div id=host>",
      "<template shadowroot=open>",
      "<div id=innerhost>",
      "<template shadowrootmode=closed shadowrootdelegatesfocus>",
      "<span>inner shadow</span>",
      "</template>",
      "<p>inner light</p>",
      "</div>",
      "</template>",
      "<p>outer light</p>",
      "<template shadowroot=open>",
      "<div id=should_not_promote>",
      "<template shadowroot=open><span>ignored</span></template>",
      "</div>",
      "</template>",
      "</div>",
      "</body></html>"
    );
    let legacy = crate::dom::parse_html(html).unwrap();

    let mut doc = Document::new(QuirksMode::NoQuirks);
    let root = doc.root();
    let html_id = push_element(&mut doc, root, "html", Vec::new(), false);
    push_element(&mut doc, html_id, "head", Vec::new(), false);
    let body_id = push_element(&mut doc, html_id, "body", Vec::new(), false);

    let host_id = push_element(
      &mut doc,
      body_id,
      "div",
      vec![("id".to_string(), "host".to_string())],
      false,
    );

    // First declarative shadow root template (promoted).
    let outer_template = push_template(
      &mut doc,
      host_id,
      vec![("shadowroot".to_string(), "open".to_string())],
    );
    let inner_host = push_element(
      &mut doc,
      outer_template,
      "div",
      vec![("id".to_string(), "innerhost".to_string())],
      false,
    );
    let inner_template = push_template(
      &mut doc,
      inner_host,
      vec![
        ("shadowrootmode".to_string(), "closed".to_string()),
        ("shadowrootdelegatesfocus".to_string(), String::new()),
      ],
    );
    let inner_span = push_element(&mut doc, inner_template, "span", Vec::new(), false);
    push_text(&mut doc, inner_span, "inner shadow");
    let inner_p = push_element(&mut doc, inner_host, "p", Vec::new(), false);
    push_text(&mut doc, inner_p, "inner light");

    let outer_p = push_element(&mut doc, host_id, "p", Vec::new(), false);
    push_text(&mut doc, outer_p, "outer light");

    // Second declarative shadow template is not promoted and its contents remain inert.
    let inert_template = push_template(
      &mut doc,
      host_id,
      vec![("shadowroot".to_string(), "open".to_string())],
    );
    let inert_inner = push_element(
      &mut doc,
      inert_template,
      "div",
      vec![("id".to_string(), "should_not_promote".to_string())],
      false,
    );
    let inert_nested_template = push_template(
      &mut doc,
      inert_inner,
      vec![("shadowroot".to_string(), "open".to_string())],
    );
    let inert_span = push_element(&mut doc, inert_nested_template, "span", Vec::new(), false);
    push_text(&mut doc, inert_span, "ignored");

    doc.attach_shadow_roots();
    doc.attach_shadow_roots();

    let roundtrip = doc.to_renderer_dom();
    assert_eq!(snapshot_dom(&legacy), snapshot_dom(&roundtrip));
  }
}
