use crate::web::dom::DomException;
use crate::dom::HTML_NAMESPACE;

use super::{Document, DomError, DomResult, NodeId, NodeKind};

fn validate_element_like(doc: &Document, element: NodeId) -> Result<(), DomException> {
  let Some(node) = doc.nodes.get(element.index()) else {
    return Err(DomException::syntax_error("Invalid node id"));
  };

  match node.kind {
    NodeKind::Element { .. } | NodeKind::Slot { .. } => Ok(()),
    _ => Err(DomException::syntax_error("Node is not an element")),
  }
}

impl Document {
  /// Serialize the element's light DOM children as HTML.
  ///
  /// Unlike [`Document::get_inner_html`], this variant uses `dom2`'s deterministic [`DomError`]
  /// codes. Shadow roots stored under the element are excluded from serialization (matching the
  /// platform `Element.innerHTML` semantics).
  pub fn inner_html(&self, element: NodeId) -> DomResult<String> {
    let Some(node) = self.nodes.get(element.index()) else {
      return Err(DomError::NotFoundError);
    };
    match node.kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
      _ => return Err(DomError::InvalidNodeType),
    }
    Ok(super::serialization::serialize_children(self, element))
  }

  /// Serialize an element as HTML (the equivalent of platform `Element.outerHTML`).
  ///
  /// Shadow roots are not elements and have no `outerHTML` in the web platform; attempting to
  /// serialize a `NodeKind::ShadowRoot` yields `Err(DomError::InvalidNodeType)`.
  pub fn outer_html(&self, node: NodeId) -> DomResult<String> {
    let Some(n) = self.nodes.get(node.index()) else {
      return Err(DomError::NotFoundError);
    };
    match n.kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => Ok(super::serialization::serialize_outer(self, node)),
      NodeKind::ShadowRoot { .. } => Err(DomError::InvalidNodeType),
      _ => Err(DomError::InvalidNodeType),
    }
  }

  pub fn get_inner_html(&self, element: NodeId) -> Result<String, DomException> {
    validate_element_like(self, element)?;
    Ok(super::serialization::serialize_children(self, element))
  }

  pub fn set_inner_html(&mut self, element: NodeId, html: &str) -> Result<(), DomException> {
    validate_element_like(self, element)?;

    let new_children = super::dom_parsing::parse_html_fragment(self, element, html)?;

    let old_children = std::mem::take(&mut self.nodes[element.index()].children);
    let mut preserved_shadow_roots: Vec<NodeId> = Vec::new();
    for child in old_children {
      if matches!(self.node(child).kind, NodeKind::ShadowRoot { .. }) {
        // Preserve any attached shadow roots (and their subtree) when setting innerHTML; the setter
        // replaces only the host element's light DOM children.
        preserved_shadow_roots.push(child);
      } else if let Some(node) = self.nodes.get_mut(child.index()) {
        node.parent = None;
      }
    }

    for &child in &new_children {
      if let Some(node) = self.nodes.get_mut(child.index()) {
        node.parent = Some(element);
      }
    }
    preserved_shadow_roots.extend(new_children);
    self.nodes[element.index()].children = preserved_shadow_roots;

    Ok(())
  }

  pub fn get_outer_html(&self, element: NodeId) -> Result<String, DomException> {
    validate_element_like(self, element)?;
    Ok(super::serialization::serialize_outer(self, element))
  }

  pub fn set_outer_html(&mut self, element: NodeId, html: &str) -> Result<(), DomException> {
    validate_element_like(self, element)?;

    let Some(parent) = self.nodes[element.index()].parent else {
      // Spec: if the element has no parent, there is nowhere to insert the parsed nodes, so the
      // setter is a no-op.
      //
      // https://html.spec.whatwg.org/multipage/dynamic-markup-insertion.html#dom-element-outerhtml
      return Ok(());
    };

    let parse_context = match &self.nodes[parent.index()].kind {
      NodeKind::Document { .. } => {
        return Err(DomException::no_modification_allowed_error(
          "Cannot set outerHTML when the parent is a Document",
        ));
      }
      // ShadowRoot inherits from DocumentFragment; use the same fragment parsing context.
      NodeKind::DocumentFragment | NodeKind::ShadowRoot { .. } => {
        // Spec: when the parent is a DocumentFragment, fragment parsing uses a synthetic `<body>`
        // element as the context.
        self.create_element("body", HTML_NAMESPACE)
      }
      _ => {
        validate_element_like(self, parent)?;
        parent
      }
    };

    let idx = self.nodes[parent.index()]
      .children
      .iter()
      .position(|&child| child == element)
      .ok_or_else(|| DomException::syntax_error("Node is not a child of its parent"))?;

    let new_nodes = super::dom_parsing::parse_html_fragment(self, parse_context, html)?;

    // Detach the replaced element.
    self.nodes[element.index()].parent = None;

    self.nodes[parent.index()]
      .children
      .splice(idx..idx + 1, new_nodes.iter().copied());

    for node_id in new_nodes {
      if let Some(node) = self.nodes.get_mut(node_id.index()) {
        node.parent = Some(parent);
      }
    }

    Ok(())
  }
}
