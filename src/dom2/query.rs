use crate::dom::DomNode;
use crate::web::dom::selectors::{node_matches_selector_list, parse_selector_list};
use crate::web::dom::DomException;
use selectors::matching::SelectorCaches;
use selectors::OpaqueElement;

use super::{Document, NodeId, NodeKind};

impl Document {
  pub fn document_element(&self) -> Option<NodeId> {
    let root = self.root();
    let node = self.nodes.get(root.index())?;
    node.children.iter().copied().find(|&child| {
      self
        .nodes
        .get(child.index())
        .is_some_and(|node| matches!(node.kind, NodeKind::Element { .. } | NodeKind::Slot { .. }))
    })
  }

  pub fn get_element_by_id(&self, id: &str) -> Option<NodeId> {
    if id.is_empty() {
      return None;
    }

    for node_id in self.subtree_preorder(self.root()) {
      let Some(node) = self.nodes.get(node_id.index()) else {
        continue;
      };
      let attributes = match &node.kind {
        NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes,
        _ => continue,
      };
      if attributes
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case("id") && value == id)
      {
        return Some(node_id);
      }
    }

    None
  }

  pub fn query_selector(
    &mut self,
    selectors: &str,
    scope: Option<NodeId>,
  ) -> Result<Option<NodeId>, DomException> {
    let selector_list = parse_selector_list(selectors)?;
    let snapshot = self.to_renderer_dom_with_mapping();
    let renderer_ids = crate::dom::enumerate_dom_ids(&snapshot.dom);
    let quirks_mode = snapshot.dom.document_quirks_mode();

    let mut selector_caches = SelectorCaches::default();
    selector_caches.set_epoch(crate::dom::next_selector_cache_epoch());

    if let Some(scope_id) = scope {
      if snapshot.preorder_id_from_node_id(scope_id).is_none() {
        return Ok(None);
      }
    }

    struct StackItem<'a> {
      node: &'a DomNode,
      exiting: bool,
      node_id: Option<NodeId>,
    }

    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut stack: Vec<StackItem<'_>> = vec![StackItem {
      node: &snapshot.dom,
      exiting: false,
      node_id: None,
    }];

    let mut scope_active = scope.is_none();
    let mut scope_anchor: Option<OpaqueElement> = None;

    while let Some(item) = stack.pop() {
      if item.exiting {
        ancestors.pop();
        if scope.is_some() && item.node_id == scope {
          break;
        }
        continue;
      }

      let preorder_id = renderer_ids
        .get(&(item.node as *const DomNode))
        .copied()
        .unwrap_or(0);
      let dom2_id = (preorder_id != 0)
        .then(|| snapshot.node_id_from_preorder(preorder_id))
        .flatten();

      if dom2_id.is_none() {
        // Shouldn't happen for nodes in the snapshot tree, but avoid panics if the mapping is out of
        // sync for some reason.
        stack.push(StackItem {
          node: item.node,
          exiting: true,
          node_id: None,
        });
        ancestors.push(item.node);
        for child in item.node.children.iter().rev() {
          stack.push(StackItem {
            node: child,
            exiting: false,
            node_id: None,
          });
        }
        continue;
      }

      let dom2_id = dom2_id.unwrap();
      if scope == Some(dom2_id) {
        scope_active = true;
        if item.node.is_element() {
          scope_anchor = Some(OpaqueElement::new(item.node));
        }
      }

      if scope_active && item.node.is_element() {
        if node_matches_selector_list(
          item.node,
          &ancestors,
          &selector_list,
          &mut selector_caches,
          quirks_mode,
          scope_anchor,
        ) {
          return Ok(Some(dom2_id));
        }
      }

      stack.push(StackItem {
        node: item.node,
        exiting: true,
        node_id: Some(dom2_id),
      });
      ancestors.push(item.node);

      // Respect template inert semantics: do not traverse into inert children.
      if self.node(dom2_id).inert_subtree {
        continue;
      }

      for child in item.node.children.iter().rev() {
        stack.push(StackItem {
          node: child,
          exiting: false,
          node_id: None,
        });
      }
    }

    Ok(None)
  }

  pub fn query_selector_all(
    &mut self,
    selectors: &str,
    scope: Option<NodeId>,
  ) -> Result<Vec<NodeId>, DomException> {
    let selector_list = parse_selector_list(selectors)?;
    let snapshot = self.to_renderer_dom_with_mapping();
    let renderer_ids = crate::dom::enumerate_dom_ids(&snapshot.dom);
    let quirks_mode = snapshot.dom.document_quirks_mode();

    let mut selector_caches = SelectorCaches::default();
    selector_caches.set_epoch(crate::dom::next_selector_cache_epoch());

    if let Some(scope_id) = scope {
      if snapshot.preorder_id_from_node_id(scope_id).is_none() {
        return Ok(Vec::new());
      }
    }

    struct StackItem<'a> {
      node: &'a DomNode,
      exiting: bool,
      node_id: Option<NodeId>,
    }

    let mut results: Vec<NodeId> = Vec::new();
    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut stack: Vec<StackItem<'_>> = vec![StackItem {
      node: &snapshot.dom,
      exiting: false,
      node_id: None,
    }];

    let mut scope_active = scope.is_none();
    let mut scope_anchor: Option<OpaqueElement> = None;

    while let Some(item) = stack.pop() {
      if item.exiting {
        ancestors.pop();
        if scope.is_some() && item.node_id == scope {
          break;
        }
        continue;
      }

      let preorder_id = renderer_ids
        .get(&(item.node as *const DomNode))
        .copied()
        .unwrap_or(0);
      let dom2_id = (preorder_id != 0)
        .then(|| snapshot.node_id_from_preorder(preorder_id))
        .flatten();

      if dom2_id.is_none() {
        stack.push(StackItem {
          node: item.node,
          exiting: true,
          node_id: None,
        });
        ancestors.push(item.node);
        for child in item.node.children.iter().rev() {
          stack.push(StackItem {
            node: child,
            exiting: false,
            node_id: None,
          });
        }
        continue;
      }

      let dom2_id = dom2_id.unwrap();
      if scope == Some(dom2_id) {
        scope_active = true;
        if item.node.is_element() {
          scope_anchor = Some(OpaqueElement::new(item.node));
        }
      }

      if scope_active && item.node.is_element() {
        if node_matches_selector_list(
          item.node,
          &ancestors,
          &selector_list,
          &mut selector_caches,
          quirks_mode,
          scope_anchor,
        ) {
          results.push(dom2_id);
        }
      }

      stack.push(StackItem {
        node: item.node,
        exiting: true,
        node_id: Some(dom2_id),
      });
      ancestors.push(item.node);

      if self.node(dom2_id).inert_subtree {
        continue;
      }

      for child in item.node.children.iter().rev() {
        stack.push(StackItem {
          node: child,
          exiting: false,
          node_id: None,
        });
      }
    }

    Ok(results)
  }

  pub fn matches_selector(
    &mut self,
    element: NodeId,
    selectors: &str,
  ) -> Result<bool, DomException> {
    let selector_list = parse_selector_list(selectors)?;
    if element.index() >= self.nodes.len() {
      return Ok(false);
    }
    match &self.node(element).kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
      _ => return Ok(false),
    }

    let snapshot = self.to_renderer_dom_with_mapping();
    let renderer_ids = crate::dom::enumerate_dom_ids(&snapshot.dom);
    let quirks_mode = snapshot.dom.document_quirks_mode();

    let mut selector_caches = SelectorCaches::default();
    selector_caches.set_epoch(crate::dom::next_selector_cache_epoch());

    let mut ancestors: Vec<&DomNode> = Vec::new();
    let mut stack: Vec<(&DomNode, bool)> = vec![(&snapshot.dom, false)];

    while let Some((node, exiting)) = stack.pop() {
      if exiting {
        ancestors.pop();
        continue;
      }

      let preorder_id = *renderer_ids.get(&(node as *const DomNode)).unwrap_or(&0);
      let dom2_id = (preorder_id != 0)
        .then(|| snapshot.node_id_from_preorder(preorder_id))
        .flatten();

      if dom2_id == Some(element) {
        let anchor = node.is_element().then(|| OpaqueElement::new(node));
        return Ok(node_matches_selector_list(
          node,
          &ancestors,
          &selector_list,
          &mut selector_caches,
          quirks_mode,
          anchor,
        ));
      }

      stack.push((node, true));
      ancestors.push(node);
      for child in node.children.iter().rev() {
        stack.push((child, false));
      }
    }

    Ok(false)
  }
}
