use crate::dom::HTML_NAMESPACE;

use super::{Document, DomError, DomResult, NodeId, NodeKind};

fn validate_element_like(doc: &Document, element: NodeId) -> DomResult<()> {
  let Some(node) = doc.nodes.get(element.index()) else {
    return Err(DomError::NotFoundError);
  };

  match node.kind {
    NodeKind::Element { .. } | NodeKind::Slot { .. } => Ok(()),
    _ => Err(DomError::InvalidNodeType),
  }
}

impl Document {
  /// Serialize the element's light DOM children as HTML.
  ///
  /// Shadow roots stored under the element are excluded from serialization (matching the platform
  /// `Element.innerHTML` semantics).
  pub fn inner_html(&self, element: NodeId) -> DomResult<String> {
    validate_element_like(self, element)?;
    Ok(super::serialization::serialize_children(self, element))
  }

  /// Backwards-compatible alias for callers that still use `get_inner_html`.
  pub fn get_inner_html(&self, element: NodeId) -> DomResult<String> {
    self.inner_html(element)
  }

  pub fn set_inner_html(&mut self, element: NodeId, html: &str) -> DomResult<()> {
    validate_element_like(self, element)?;

    let fragment = super::dom_parsing::parse_html_fragment_as_fragment(self, element, html)?;

    // Detach existing children (but preserve attached shadow roots, which are not part of the light
    // DOM and should not be affected by `innerHTML`).
    let old_children = std::mem::take(&mut self.nodes[element.index()].children);
    let mut preserved_shadow_roots: Vec<NodeId> = Vec::new();
    for child in old_children {
      if child.index() >= self.nodes.len() {
        continue;
      }
      if matches!(self.nodes[child.index()].kind, NodeKind::ShadowRoot { .. }) {
        preserved_shadow_roots.push(child);
      } else if let Some(node) = self.nodes.get_mut(child.index()) {
        node.parent = None;
      }
    }
    self.nodes[element.index()].children = preserved_shadow_roots;

    // Append the fragment; `DocumentFragment` insertion semantics splice its children into the
    // element (in order) and empty the fragment.
    self.append_child(element, fragment)?;
    Ok(())
  }

  /// Serialize an element as HTML (the equivalent of platform `Element.outerHTML`).
  pub fn outer_html(&self, node: NodeId) -> DomResult<String> {
    validate_element_like(self, node)?;
    Ok(super::serialization::serialize_outer(self, node))
  }

  /// Backwards-compatible alias for callers that still use `get_outer_html`.
  pub fn get_outer_html(&self, node: NodeId) -> DomResult<String> {
    self.outer_html(node)
  }

  pub fn set_outer_html(&mut self, node: NodeId, html: &str) -> DomResult<()> {
    validate_element_like(self, node)?;

    let Some(parent) = self.nodes[node.index()].parent else {
      // Detached nodes are a no-op, matching browsers.
      return Ok(());
    };

    let parse_context = match self.nodes.get(parent.index()).map(|n| &n.kind) {
      Some(NodeKind::Document { .. }) => return Err(DomError::NoModificationAllowedError),
      // ShadowRoot inherits from DocumentFragment; use the same fragment parsing context.
      Some(NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. }) => {
        // Spec: when the parent is a DocumentFragment, fragment parsing uses a synthetic `<body>`
        // element as the context.
        self.create_element("body", HTML_NAMESPACE)
      }
      Some(NodeKind::Element { .. } | NodeKind::Slot { .. }) => parent,
      Some(_) => return Err(DomError::InvalidNodeType),
      None => return Err(DomError::NotFoundError),
    };

    let fragment = super::dom_parsing::parse_html_fragment_as_fragment(self, parse_context, html)?;
    self.replace_child(parent, fragment, node)?;
    Ok(())
  }
}

