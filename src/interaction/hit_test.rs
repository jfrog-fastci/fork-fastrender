use crate::dom::DomNode;
use crate::geometry::Point;
use crate::style::computed::Visibility;
use crate::style::types::PointerEvents;
use crate::tree::box_tree::{BoxNode, BoxTree};
use crate::tree::fragment_tree::FragmentTree;
use std::collections::HashMap;
use std::ptr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitTestResult {
  pub box_id: usize,
  pub styled_node_id: usize,
  pub dom_node_id: usize,
  pub kind: HitTestKind,
  pub href: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitTestKind {
  Link,
  FormControl,
  Label,
  Other,
}

struct BoxIndex {
  id_to_ptr: Vec<*const BoxNode>,
  parent: Vec<usize>,
}

impl BoxIndex {
  fn new(box_tree: &BoxTree) -> Self {
    let mut id_to_ptr: Vec<*const BoxNode> = vec![ptr::null()];
    let mut parent: Vec<usize> = vec![0];

    // (node, parent_box_id)
    let mut stack: Vec<(&BoxNode, usize)> = vec![(&box_tree.root, 0)];
    while let Some((node, parent_id)) = stack.pop() {
      let id = node.id;
      if id == 0 {
        // `BoxTree::new` assigns ids starting from 1; ignore any uninitialized nodes.
        continue;
      }
      if id >= id_to_ptr.len() {
        id_to_ptr.resize(id + 1, ptr::null());
        parent.resize(id + 1, 0);
      }
      id_to_ptr[id] = node as *const BoxNode;
      parent[id] = parent_id;

      // Mirror `assign_box_ids` traversal ordering.
      if let Some(body) = node.footnote_body.as_deref() {
        stack.push((body, id));
      }
      for child in node.children.iter().rev() {
        stack.push((child, id));
      }
    }

    Self { id_to_ptr, parent }
  }

  fn node(&self, box_id: usize) -> Option<&BoxNode> {
    let ptr = *self.id_to_ptr.get(box_id)?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: `ptr` originates from a live `BoxTree` borrowed for the duration of `hit_test_dom`.
    Some(unsafe { &*ptr })
  }

  fn parent_id(&self, box_id: usize) -> Option<usize> {
    self.parent.get(box_id).copied()
  }
}

struct DomIndex {
  id_to_ptr: Vec<*const DomNode>,
  parent: Vec<usize>,
}

impl DomIndex {
  fn new(dom: &DomNode) -> Self {
    let mut id_to_ptr: Vec<*const DomNode> = vec![ptr::null()];
    let mut parent: Vec<usize> = vec![0];

    // Pre-order traversal, matching `dom::enumerate_dom_ids` / cascade node ids.
    // (node, parent_dom_id)
    let mut stack: Vec<(&DomNode, usize)> = vec![(dom, 0)];
    while let Some((node, parent_id)) = stack.pop() {
      let id = id_to_ptr.len();
      id_to_ptr.push(node as *const DomNode);
      parent.push(parent_id);
      for child in node.children.iter().rev() {
        stack.push((child, id));
      }
    }

    Self { id_to_ptr, parent }
  }

  fn node(&self, node_id: usize) -> Option<&DomNode> {
    let ptr = *self.id_to_ptr.get(node_id)?;
    if ptr.is_null() {
      return None;
    }
    // SAFETY: `ptr` originates from a live `DomNode` borrowed for the duration of `hit_test_dom`.
    Some(unsafe { &*ptr })
  }

  fn parent_id(&self, node_id: usize) -> Option<usize> {
    self.parent.get(node_id).copied()
  }

  fn node_ids(&self) -> impl Iterator<Item = usize> + '_ {
    (1..self.id_to_ptr.len()).filter(|&id| !self.id_to_ptr[id].is_null())
  }

  fn is_ancestor(&self, ancestor: usize, mut node_id: usize) -> bool {
    while node_id != 0 {
      if node_id == ancestor {
        return true;
      }
      node_id = self.parent.get(node_id).copied().unwrap_or(0);
    }
    false
  }
}

fn box_is_interactive(box_node: &BoxNode) -> bool {
  let style = &box_node.style;
  style.pointer_events != PointerEvents::None
    && style.visibility == Visibility::Visible
    && style.inert == false
}

fn node_is_inert_like(node: &DomNode) -> bool {
  if node.get_attribute_ref("inert").is_some() {
    return true;
  }
  node
    .get_attribute_ref("data-fastr-inert")
    .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn node_is_link(node: &DomNode) -> Option<String> {
  let tag = node.tag_name()?;
  if !(tag.eq_ignore_ascii_case("a") || tag.eq_ignore_ascii_case("area")) {
    return None;
  }
  node.get_attribute_ref("href").map(|href| href.to_string())
}

fn node_is_form_control(node: &DomNode) -> bool {
  let Some(tag) = node.tag_name() else {
    return false;
  };

  if tag.eq_ignore_ascii_case("input") {
    let ty = node.get_attribute_ref("type").unwrap_or("").trim();
    return !ty.eq_ignore_ascii_case("hidden");
  }

  tag.eq_ignore_ascii_case("textarea")
    || tag.eq_ignore_ascii_case("select")
    || tag.eq_ignore_ascii_case("button")
}

fn resolve_styled_node_id_from_box_ancestors(
  box_index: &BoxIndex,
  mut box_id: usize,
) -> Option<usize> {
  while box_id != 0 {
    let node = box_index.node(box_id)?;
    if let Some(styled_node_id) = node.styled_node_id {
      return Some(styled_node_id);
    }
    box_id = box_index.parent_id(box_id).unwrap_or(0);
  }
  None
}

enum SemanticResolveResult {
  Hit {
    node_id: usize,
    kind: HitTestKind,
    href: Option<String>,
  },
  InertSubtree,
  Invalid,
}

fn resolve_semantic_target(dom_index: &DomIndex, start_node_id: usize) -> SemanticResolveResult {
  if dom_index.node(start_node_id).is_none() {
    return SemanticResolveResult::Invalid;
  }

  let mut current = start_node_id;
  let mut first_element: Option<usize> = None;

  while current != 0 {
    let Some(node) = dom_index.node(current) else {
      return SemanticResolveResult::Invalid;
    };

    if node.is_element() {
      if node_is_inert_like(node) {
        // Inert subtrees block interaction target resolution entirely.
        return SemanticResolveResult::InertSubtree;
      }

      first_element.get_or_insert(current);

      if let Some(href) = node_is_link(node) {
        return SemanticResolveResult::Hit {
          node_id: current,
          kind: HitTestKind::Link,
          href: Some(href),
        };
      }
      if node_is_form_control(node) {
        return SemanticResolveResult::Hit {
          node_id: current,
          kind: HitTestKind::FormControl,
          href: None,
        };
      }
      if matches!(node.tag_name(), Some(tag) if tag.eq_ignore_ascii_case("label")) {
        return SemanticResolveResult::Hit {
          node_id: current,
          kind: HitTestKind::Label,
          href: None,
        };
      }
    }

    current = dom_index.parent_id(current).unwrap_or(0);
  }

  match first_element {
    Some(node_id) => SemanticResolveResult::Hit {
      node_id,
      kind: HitTestKind::Other,
      href: None,
    },
    None => SemanticResolveResult::Invalid,
  }
}

pub fn hit_test_dom(
  dom: &DomNode,
  box_tree: &BoxTree,
  fragment_tree: &FragmentTree,
  point: Point,
) -> Option<HitTestResult> {
  let box_index = BoxIndex::new(box_tree);
  let dom_index = DomIndex::new(dom);

  for fragment in fragment_tree.hit_test(point) {
    let Some(box_id) = fragment.box_id() else {
      continue;
    };
    let Some(box_node) = box_index.node(box_id) else {
      continue;
    };
    if !box_is_interactive(box_node) {
      continue;
    }

    let Some(styled_node_id) = resolve_styled_node_id_from_box_ancestors(&box_index, box_id) else {
      continue;
    };

    // MVP: styled node ids are the cascade DOM pre-order ids.
    let dom_node_id = styled_node_id;

    let (semantic_dom_node_id, kind, href) = match resolve_semantic_target(&dom_index, dom_node_id)
    {
      SemanticResolveResult::Hit {
        node_id,
        kind,
        href,
      } => (node_id, kind, href),
      SemanticResolveResult::InertSubtree => {
        // Stop and return None if the target falls within an inert subtree.
        return None;
      }
      SemanticResolveResult::Invalid => continue,
    };

    return Some(HitTestResult {
      box_id,
      styled_node_id,
      dom_node_id: semantic_dom_node_id,
      kind,
      href,
    });
  }

  None
}

pub fn resolve_label_associated_control(dom: &DomNode, label_node_id: usize) -> Option<usize> {
  let dom_index = DomIndex::new(dom);
  let label = dom_index.node(label_node_id)?;

  if !matches!(label.tag_name(), Some(tag) if tag.eq_ignore_ascii_case("label")) {
    return None;
  }

  if let Some(for_value) = label
    .get_attribute_ref("for")
    .map(str::trim)
    .filter(|v| !v.is_empty())
  {
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for node_id in dom_index.node_ids() {
      let node = dom_index
        .node(node_id)
        .expect("node_ids only yields valid ids");
      let Some(id_attr) = node.get_attribute_ref("id") else {
        continue;
      };
      by_id.entry(id_attr.to_string()).or_insert(node_id);
    }
    return by_id
      .get(for_value)
      .copied()
      .filter(|&node_id| dom_index.node(node_id).is_some_and(node_is_form_control));
  }

  // No explicit `for` => first descendant control element inside the label.
  for candidate_id in dom_index.node_ids() {
    if candidate_id == label_node_id {
      continue;
    }
    if !dom_index.is_ancestor(label_node_id, candidate_id) {
      continue;
    }
    let node = dom_index
      .node(candidate_id)
      .expect("node_ids only yields valid ids");
    if node_is_form_control(node) {
      return Some(candidate_id);
    }
  }

  None
}
