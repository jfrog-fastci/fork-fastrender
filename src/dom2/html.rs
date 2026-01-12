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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdjacentPosition {
  BeforeBegin,
  AfterBegin,
  BeforeEnd,
  AfterEnd,
}

fn parse_adjacent_position(position: &str) -> Result<AdjacentPosition, DomError> {
  if position.eq_ignore_ascii_case("beforebegin") {
    Ok(AdjacentPosition::BeforeBegin)
  } else if position.eq_ignore_ascii_case("afterbegin") {
    Ok(AdjacentPosition::AfterBegin)
  } else if position.eq_ignore_ascii_case("beforeend") {
    Ok(AdjacentPosition::BeforeEnd)
  } else if position.eq_ignore_ascii_case("afterend") {
    Ok(AdjacentPosition::AfterEnd)
  } else {
    Err(DomError::SyntaxError)
  }
}

fn first_light_child(doc: &Document, parent: NodeId) -> Option<NodeId> {
  let node = doc.nodes.get(parent.index())?;
  node.children.iter().copied().find(|&child| {
    doc
      .nodes
      .get(child.index())
      .is_some_and(|n| n.parent == Some(parent) && !matches!(n.kind, NodeKind::ShadowRoot { .. }))
  })
}

fn next_light_sibling(doc: &Document, node: NodeId) -> Option<NodeId> {
  let parent = doc.nodes.get(node.index())?.parent?;
  let parent_node = doc.nodes.get(parent.index())?;
  let pos = parent_node.children.iter().position(|&c| c == node)?;
  parent_node
    .children
    .iter()
    .skip(pos + 1)
    .copied()
    .find(|&sib| {
      doc
        .nodes
        .get(sib.index())
        .is_some_and(|n| n.parent == Some(parent) && !matches!(n.kind, NodeKind::ShadowRoot { .. }))
    })
}

fn insert_adjacent_node(
  doc: &mut Document,
  element: NodeId,
  position: AdjacentPosition,
  node: NodeId,
) -> DomResult<Option<NodeId>> {
  match position {
    AdjacentPosition::BeforeBegin => {
      let Some(parent) = doc.nodes.get(element.index()).and_then(|n| n.parent) else {
        return Ok(None);
      };
      doc.insert_before(parent, node, Some(element))?;
      Ok(Some(node))
    }
    AdjacentPosition::AfterBegin => {
      let reference = first_light_child(doc, element);
      doc.insert_before(element, node, reference)?;
      Ok(Some(node))
    }
    AdjacentPosition::BeforeEnd => {
      doc.insert_before(element, node, None)?;
      Ok(Some(node))
    }
    AdjacentPosition::AfterEnd => {
      let Some(parent) = doc.nodes.get(element.index()).and_then(|n| n.parent) else {
        return Ok(None);
      };
      let reference = next_light_sibling(doc, element);
      doc.insert_before(parent, node, reference)?;
      Ok(Some(node))
    }
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

    // Detach existing light DOM children, preserving attached shadow roots. This must go through
    // the structured `remove_child` mutation API so live traversal hooks (e.g. Range/NodeIterator)
    // and MutationObserver childList records observe the removals.
    let old_children = self.nodes[element.index()].children.clone();
    for child in old_children {
      let should_remove = self
        .nodes
        .get(child.index())
        .is_some_and(|node| node.parent == Some(element) && !matches!(node.kind, NodeKind::ShadowRoot { .. }));
      if should_remove {
        self.remove_child(element, child)?;
      }
    }

    // Append the fragment; `DocumentFragment` insertion semantics splice its children into the
    // element (in order) and empty the fragment.
    let _ = self.append_child(element, fragment)?;
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
  /// Spec: <https://html.spec.whatwg.org/multipage/dynamic-markup-insertion.html#dom-element-insertadjacenthtml>
  pub fn insert_adjacent_html(
    &mut self,
    element: NodeId,
    position: &str,
    html: &str,
  ) -> DomResult<()> {
    validate_element_like(self, element)?;

    let pos = parse_adjacent_position(position)?;

    let parent = self.nodes.get(element.index()).and_then(|n| n.parent);

    // Step 3: choose parsing context or throw for detached / document parents.
    let (context, parse_context) = match pos {
      AdjacentPosition::BeforeBegin | AdjacentPosition::AfterEnd => {
        let parent = parent.ok_or(DomError::NoModificationAllowedError)?;
        match self.nodes.get(parent.index()).map(|n| &n.kind) {
          Some(NodeKind::Document { .. }) => return Err(DomError::NoModificationAllowedError),
          Some(_) => {}
          None => return Err(DomError::NotFoundError),
        }

        // Step 4: if context is not an Element, parse in a synthetic <body> element.
        let parse_context = match self.nodes.get(parent.index()).map(|n| &n.kind) {
          Some(NodeKind::Element {
            tag_name,
            namespace,
            ..
          }) if tag_name.eq_ignore_ascii_case("html")
            && (namespace.is_empty() || namespace == HTML_NAMESPACE) =>
          {
            self.create_element("body", HTML_NAMESPACE)
          }
          Some(NodeKind::Element { .. } | NodeKind::Slot { .. }) => parent,
          Some(_) => self.create_element("body", HTML_NAMESPACE),
          None => return Err(DomError::NotFoundError),
        };
        (parent, parse_context)
      }

      AdjacentPosition::AfterBegin | AdjacentPosition::BeforeEnd => {
        // Step 4: if context is the HTML <html> element, parse in a synthetic <body> element.
        let parse_context = match self.nodes.get(element.index()).map(|n| &n.kind) {
          Some(NodeKind::Element {
            tag_name,
            namespace,
            ..
          }) if tag_name.eq_ignore_ascii_case("html")
            && (namespace.is_empty() || namespace == HTML_NAMESPACE) =>
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

    match pos {
      AdjacentPosition::BeforeBegin => {
        self.insert_before(context, fragment, Some(element))?;
      }
      AdjacentPosition::AfterBegin => {
        let reference = first_light_child(self, element);
        self.insert_before(element, fragment, reference)?;
      }
      AdjacentPosition::BeforeEnd => {
        self.append_child(element, fragment)?;
      }
      AdjacentPosition::AfterEnd => {
        let reference = next_light_sibling(self, element);
        self.insert_before(context, fragment, reference)?;
      }
    }

    Ok(())
  }

  /// DOM `Element.insertAdjacentElement(where, element)` for a `dom2` element.
  ///
  /// Spec: <https://dom.spec.whatwg.org/#dom-element-insertadjacentelement>
  pub fn insert_adjacent_element(
    &mut self,
    element: NodeId,
    where_: &str,
    new_element: NodeId,
  ) -> DomResult<Option<NodeId>> {
    validate_element_like(self, element)?;
    validate_element_like(self, new_element)?;
    let pos = parse_adjacent_position(where_)?;
    insert_adjacent_node(self, element, pos, new_element)
  }

  /// DOM `Element.insertAdjacentText(where, data)` for a `dom2` element.
  ///
  /// Spec: <https://dom.spec.whatwg.org/#dom-element-insertadjacenttext>
  pub fn insert_adjacent_text(
    &mut self,
    element: NodeId,
    where_: &str,
    data: &str,
  ) -> DomResult<()> {
    validate_element_like(self, element)?;
    let pos = parse_adjacent_position(where_)?;

    // The DOM spec creates the Text node before attempting insertion. Since dom2 nodes are stored
    // in a grow-only arena (no GC), avoid allocating an unreachable Text node when insertion is
    // guaranteed to do nothing.
    if matches!(
      pos,
      AdjacentPosition::BeforeBegin | AdjacentPosition::AfterEnd
    ) && self
      .nodes
      .get(element.index())
      .and_then(|n| n.parent)
      .is_none()
    {
      return Ok(());
    }

    let text = self.create_text(data);
    let _ = insert_adjacent_node(self, element, pos, text)?;
    Ok(())
  }

  /// HTML `Range.createContextualFragment(string)` adapted for `dom2`.
  ///
  /// This returns a detached `DocumentFragment` whose children are parsed in a context derived from
  /// `context_node` (per the HTML spec). Unlike `innerHTML`/`outerHTML`/`insertAdjacentHTML`,
  /// `<script>` elements inside the returned fragment are *not* marked "already started".
  ///
  /// Spec: <https://html.spec.whatwg.org/multipage/dynamic-markup-insertion.html#dom-range-createcontextualfragment>
  pub fn create_contextual_fragment(
    &mut self,
    context_node: NodeId,
    html: &str,
  ) -> DomResult<NodeId> {
    let Some(node) = self.nodes.get(context_node.index()) else {
      return Err(DomError::NotFoundError);
    };

    let (element, element_is_html) = match &node.kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => (
        Some(context_node),
        tag_name.eq_ignore_ascii_case("html")
          && (namespace.is_empty() || namespace == HTML_NAMESPACE),
      ),
      NodeKind::Slot { .. } => (Some(context_node), false),
      NodeKind::Text { .. } | NodeKind::Comment { .. } => {
        if let Some(parent) = node.parent {
          if let Some(parent_node) = self.nodes.get(parent.index()) {
            match &parent_node.kind {
              NodeKind::Element {
                tag_name,
                namespace,
                ..
              } => (
                Some(parent),
                tag_name.eq_ignore_ascii_case("html")
                  && (namespace.is_empty() || namespace == HTML_NAMESPACE),
              ),
              NodeKind::Slot { .. } => (Some(parent), false),
              _ => (None, false),
            }
          } else {
            (None, false)
          }
        } else {
          (None, false)
        }
      }
      _ => (None, false),
    };

    let parse_context = match (element, element_is_html) {
      (Some(element), false) => element,
      _ => self.create_element("body", HTML_NAMESPACE),
    };

    let fragment = super::dom_parsing::parse_html_fragment_as_fragment(self, parse_context, html)?;

    // Range.createContextualFragment must *not* mark scripts as already started; reset the flag on
    // any script element descendants.
    let to_check: Vec<NodeId> = self.subtree_preorder(fragment).collect();
    for node_id in to_check {
      let is_html_script = match &self.nodes[node_id.index()].kind {
        NodeKind::Element {
          tag_name,
          namespace,
          ..
        } => {
          tag_name.eq_ignore_ascii_case("script")
            && (namespace.is_empty() || namespace == HTML_NAMESPACE)
        }
        _ => false,
      };
      if is_html_script {
        self.nodes[node_id.index()].script_already_started = false;
        // HTML Range.createContextualFragment also clears the "parser document" internal slot so
        // scripts in the returned fragment are not treated as parser-inserted.
        self.nodes[node_id.index()].script_parser_document = false;
      }
    }

    Ok(fragment)
  }
}
