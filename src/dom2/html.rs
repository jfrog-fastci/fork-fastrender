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

  /// DOM `Element.insertAdjacentHTML(position, string)` for a `dom2` element.
  ///
  /// Spec: https://html.spec.whatwg.org/multipage/dynamic-markup-insertion.html#dom-element-insertadjacenthtml
  pub fn insert_adjacent_html(&mut self, element: NodeId, position: &str, html: &str) -> DomResult<()> {
    validate_element_like(self, element)?;

    #[derive(Clone, Copy)]
    enum Position {
      BeforeBegin,
      AfterBegin,
      BeforeEnd,
      AfterEnd,
    }

    let pos = if position.eq_ignore_ascii_case("beforebegin") {
      Position::BeforeBegin
    } else if position.eq_ignore_ascii_case("afterbegin") {
      Position::AfterBegin
    } else if position.eq_ignore_ascii_case("beforeend") {
      Position::BeforeEnd
    } else if position.eq_ignore_ascii_case("afterend") {
      Position::AfterEnd
    } else {
      return Err(DomError::SyntaxError);
    };

    let parent = self.nodes.get(element.index()).and_then(|n| n.parent);

    // Step 3: choose parsing context or throw for detached / document parents.
    let (context, parse_context) = match pos {
      Position::BeforeBegin | Position::AfterEnd => {
        let parent = parent.ok_or(DomError::NoModificationAllowedError)?;
        match self.nodes.get(parent.index()).map(|n| &n.kind) {
          Some(NodeKind::Document { .. }) => return Err(DomError::NoModificationAllowedError),
          Some(_) => {}
          None => return Err(DomError::NotFoundError),
        }

        // Step 4: if context is not an Element, parse in a synthetic <body> element.
        let parse_context = match self.nodes.get(parent.index()).map(|n| &n.kind) {
          Some(NodeKind::Element { tag_name, namespace, .. })
            if tag_name.eq_ignore_ascii_case("html") && (namespace.is_empty() || namespace == HTML_NAMESPACE) =>
          {
            self.create_element("body", HTML_NAMESPACE)
          }
          Some(NodeKind::Element { .. } | NodeKind::Slot { .. }) => parent,
          Some(_) => self.create_element("body", HTML_NAMESPACE),
          None => return Err(DomError::NotFoundError),
        };
        (parent, parse_context)
      }

      Position::AfterBegin | Position::BeforeEnd => {
        // Step 4: if context is the HTML <html> element, parse in a synthetic <body> element.
        let parse_context = match self.nodes.get(element.index()).map(|n| &n.kind) {
          Some(NodeKind::Element { tag_name, namespace, .. })
            if tag_name.eq_ignore_ascii_case("html") && (namespace.is_empty() || namespace == HTML_NAMESPACE) =>
          {
            self.create_element("body", HTML_NAMESPACE)
          }
          Some(_) => element,
          None => return Err(DomError::NotFoundError),
        };
        (element, parse_context)
      }
    };

    let fragment = super::dom_parsing::parse_html_fragment_as_fragment(self, parse_context, html)?;

    // Helper: return the first light DOM child (skipping stored ShadowRoot nodes).
    let first_light_child = |doc: &Document, parent: NodeId| -> Option<NodeId> {
      let node = doc.nodes.get(parent.index())?;
      node.children.iter().copied().find(|&child| {
        doc.nodes.get(child.index()).is_some_and(|n| {
          n.parent == Some(parent) && !matches!(n.kind, NodeKind::ShadowRoot { .. })
        })
      })
    };

    // Helper: return next light sibling (skipping stored ShadowRoot nodes).
    let next_light_sibling = |doc: &Document, node: NodeId| -> Option<NodeId> {
      let parent = doc.nodes.get(node.index())?.parent?;
      let parent_node = doc.nodes.get(parent.index())?;
      let pos = parent_node.children.iter().position(|&c| c == node)?;
      parent_node.children.iter().skip(pos + 1).copied().find(|&sib| {
        doc.nodes.get(sib.index()).is_some_and(|n| {
          n.parent == Some(parent) && !matches!(n.kind, NodeKind::ShadowRoot { .. })
        })
      })
    };

    match pos {
      Position::BeforeBegin => {
        self.insert_before(context, fragment, Some(element))?;
      }
      Position::AfterBegin => {
        let reference = first_light_child(self, element);
        self.insert_before(element, fragment, reference)?;
      }
      Position::BeforeEnd => {
        self.append_child(element, fragment)?;
      }
      Position::AfterEnd => {
        let reference = next_light_sibling(self, element);
        self.insert_before(context, fragment, reference)?;
      }
    }

    Ok(())
  }
}
