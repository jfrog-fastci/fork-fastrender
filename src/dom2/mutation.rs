use crate::dom::HTML_NAMESPACE;

use super::live_mutation::utf16_len;
use super::DomError;
use super::{Document, Node, NodeId, NodeKind};

struct CloneNodeData {
  kind: NodeKind,
  inert_subtree: bool,
  is_html_script: bool,
  script_force_async: bool,
  script_already_started: bool,
  mathml_annotation_xml_integration_point: bool,
}

fn clone_node_data(doc: &Document, src: &Node, parent: Option<NodeId>) -> CloneNodeData {
  let is_html_script = doc.kind_is_html_script(&src.kind);

  let script_force_async = if is_html_script {
    let NodeKind::Element { attributes, .. } = &src.kind else {
      unreachable!(); // fastrender-allow-panic
    };
    !attributes
      .iter()
      .any(|(name, _)| name.eq_ignore_ascii_case("async"))
  } else {
    false
  };

  let kind = match &src.kind {
    NodeKind::Document { quirks_mode } => {
      // A `dom2::Document` stores its primary document node at `Document::root()`, but cloning a
      // document node is still useful (e.g. `Document.cloneNode()` in JS). The cloned `Document`
      // node must remain detached: `Document` nodes cannot be inserted into the tree.
      debug_assert!(
        parent.is_none(),
        "Document nodes cannot have a parent; clone_node() must only clone them as detached roots"
      );
      NodeKind::Document {
        quirks_mode: *quirks_mode,
      }
    }
    NodeKind::DocumentFragment => NodeKind::DocumentFragment,
    NodeKind::Comment { content } => NodeKind::Comment {
      content: content.clone(),
    },
    NodeKind::ProcessingInstruction { target, data } => NodeKind::ProcessingInstruction {
      target: target.clone(),
      data: data.clone(),
    },
    NodeKind::Doctype {
      name,
      public_id,
      system_id,
    } => NodeKind::Doctype {
      name: name.clone(),
      public_id: public_id.clone(),
      system_id: system_id.clone(),
    },
    NodeKind::ShadowRoot {
      mode,
      delegates_focus,
      slot_assignment,
      clonable,
      serializable,
      declarative,
    } => NodeKind::ShadowRoot {
      mode: *mode,
      delegates_focus: *delegates_focus,
      slot_assignment: *slot_assignment,
      clonable: *clonable,
      serializable: *serializable,
      declarative: *declarative,
    },
    NodeKind::Slot {
      namespace,
      attributes,
      ..
    } => {
      // `NodeKind::Slot` is an HTML-only fast-path. When cloning into an XML document (or into a
      // non-HTML namespace), treat it as a plain element.
      let is_html_slot = doc.is_html_case_insensitive_namespace(namespace);
      let namespace = if doc.is_html_document() && namespace == HTML_NAMESPACE {
        String::new()
      } else {
        namespace.clone()
      };
      if is_html_slot {
        NodeKind::Slot {
          namespace,
          attributes: attributes.clone(),
          // Slot assignment is derived state; cloned nodes start detached.
          assigned: false,
        }
      } else {
        NodeKind::Element {
          tag_name: "slot".to_string(),
          namespace,
          prefix: None,
          attributes: attributes.clone(),
        }
      }
    }
    NodeKind::Element {
      tag_name,
      namespace,
      prefix,
      attributes,
    } => NodeKind::Element {
      tag_name: tag_name.clone(),
      namespace: if doc.is_html_document() && namespace == HTML_NAMESPACE {
        String::new()
      } else {
        namespace.clone()
      },
      prefix: prefix.clone(),
      attributes: attributes.clone(),
    },
    NodeKind::Text { content } => NodeKind::Text {
      content: content.clone(),
    },
  };

  CloneNodeData {
    kind,
    // Template inertness is an HTML-only behaviour; don't carry it into XML documents.
    inert_subtree: src.inert_subtree && doc.is_html_document(),
    is_html_script,
    script_force_async,
    script_already_started: src.script_already_started,
    mathml_annotation_xml_integration_point: src.mathml_annotation_xml_integration_point,
  }
}

fn push_cloned_node(doc: &mut Document, parent: Option<NodeId>, data: CloneNodeData) -> NodeId {
  let dst = doc.push_node(data.kind, parent, data.inert_subtree);

  // Preserve HTML parser flags that affect future parsing behavior.
  doc.nodes[dst.index()].mathml_annotation_xml_integration_point =
    data.mathml_annotation_xml_integration_point;

  if data.is_html_script {
    // HTML: script element cloning steps copy the "already started" flag only.
    //
    // https://html.spec.whatwg.org/multipage/scripting.html#script-processing-model
    doc.nodes[dst.index()].script_force_async = data.script_force_async;
    doc.nodes[dst.index()].script_already_started = data.script_already_started;
  }

  dst
}

impl Document {
  fn clone_node_shallow(
    &mut self,
    src: NodeId,
    parent: Option<NodeId>,
  ) -> Result<NodeId, DomError> {
    let copy_form_state = self.is_html_document();
    let (data, input_state, textarea_state, option_state) = {
      let node = self.node_checked(src)?;
      (
        clone_node_data(self, node, parent),
        if copy_form_state {
          self.input_states[src.index()].clone()
        } else {
          None
        },
        if copy_form_state {
          self.textarea_states[src.index()].clone()
        } else {
          None
        },
        if copy_form_state {
          self.option_states[src.index()].clone()
        } else {
          None
        },
      )
    };
    let dst = push_cloned_node(self, parent, data);
    if copy_form_state {
      // Only overwrite the destination's freshly-initialized form control state when the source
      // node actually had state to preserve. This avoids accidentally clearing state when importing
      // from an XML document (which never has HTML form control internal state).
      if input_state.is_some() {
        self.input_states[dst.index()] = input_state;
      }
      if textarea_state.is_some() {
        self.textarea_states[dst.index()] = textarea_state;
      }
      if option_state.is_some() {
        self.option_states[dst.index()] = option_state;
      }
    }
    Ok(dst)
  }

  /// Clone a single node, optionally cloning its descendant subtree.
  ///
  /// This is a subset of the WHATWG DOM `Node.cloneNode(deep)` algorithm for the node kinds modeled
  /// by `dom2`.
  ///
  /// The cloned root is always detached (`parent == None`) and belongs to the same `dom2::Document`.
  ///
  /// Deep cloning is implemented iteratively to avoid recursion overflow on degenerate trees.
  pub fn clone_node(&mut self, node: NodeId, deep: bool) -> Result<NodeId, DomError> {
    let src_node = self.node_checked(node)?;

    // WHATWG DOM: `Node.cloneNode()` throws for ShadowRoot.
    // https://dom.spec.whatwg.org/#dom-node-clonenode
    if matches!(src_node.kind, NodeKind::ShadowRoot { .. }) {
      return Err(DomError::NotSupportedError);
    }

    let dst_root = self.clone_node_shallow(node, None)?;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Phase {
      TreeChildren,
      ShadowRootChildren,
    }

    struct Frame {
      src: NodeId,
      dst: NodeId,
      phase: Phase,
      next_child: usize,
      shadow_root_src: Option<NodeId>,
      shadow_root_dst: Option<NodeId>,
      next_shadow_child: usize,
    }

    fn should_skip_tree_child(parent_kind: &NodeKind, child_kind: &NodeKind) -> bool {
      // Shadow roots are not part of an element's tree children (light DOM).
      matches!(
        parent_kind,
        NodeKind::Element { .. } | NodeKind::Slot { .. }
      ) && matches!(child_kind, NodeKind::ShadowRoot { .. })
    }

    fn initial_phase(deep: bool) -> Phase {
      if deep {
        Phase::TreeChildren
      } else {
        Phase::ShadowRootChildren
      }
    }

    let mut stack: Vec<Frame> = vec![Frame {
      src: node,
      dst: dst_root,
      phase: initial_phase(deep),
      next_child: 0,
      shadow_root_src: None,
      shadow_root_dst: None,
      next_shadow_child: 0,
    }];

    'clone: while let Some(mut frame) = stack.pop() {
      match frame.phase {
        Phase::TreeChildren => {
          // Clone tree children (light DOM), in tree order.
          //
          // For element nodes, `dom2` stores the attached shadow root as a child, but it is not a
          // tree child per WHATWG DOM's "clone a node" algorithm. Those are handled separately in
          // `Phase::ShadowRootChildren`.
          let src_children_len = self.nodes[frame.src.index()].children.len();
          while frame.next_child < src_children_len {
            let child_src = self.nodes[frame.src.index()].children[frame.next_child];
            frame.next_child += 1;

            let child_node = self.node_checked(child_src)?;
            if child_node.parent != Some(frame.src) {
              continue;
            }
            if should_skip_tree_child(&self.nodes[frame.src.index()].kind, &child_node.kind) {
              continue;
            }

            let parent_dst = frame.dst;
            stack.push(frame);
            let child_dst = self.clone_node_shallow(child_src, Some(parent_dst))?;
            stack.push(Frame {
              src: child_src,
              dst: child_dst,
              phase: initial_phase(deep),
              next_child: 0,
              shadow_root_src: None,
              shadow_root_dst: None,
              next_shadow_child: 0,
            });
            // Continue DFS with the cloned child.
            continue 'clone;
          }

          // Tree children complete; proceed to cloning the element's shadow root (if any).
          frame.phase = Phase::ShadowRootChildren;
          stack.push(frame);
        }

        Phase::ShadowRootChildren => {
          // Initialize shadow root cloning state if we have not yet done so.
          if frame.shadow_root_src.is_none() {
            if matches!(
              &self.nodes[frame.src.index()].kind,
              NodeKind::Element { .. } | NodeKind::Slot { .. }
            ) {
              if let Some(src_shadow_root) = self.shadow_root_for_host(frame.src) {
                let (mode, delegates_focus, slot_assignment, clonable, serializable, declarative) =
                  match &self.nodes[src_shadow_root.index()].kind {
                    NodeKind::ShadowRoot {
                      mode,
                      delegates_focus,
                      slot_assignment,
                      clonable,
                      serializable,
                      declarative,
                    } => (
                      *mode,
                      *delegates_focus,
                      *slot_assignment,
                      *clonable,
                      *serializable,
                      *declarative,
                    ),
                    _ => unreachable!("shadow_root_for_host must return a ShadowRoot node"), // fastrender-allow-panic
                  };

                if clonable {
                  // Attach a new shadow root to the cloned host.
                  //
                  // We cannot use `Document::attach_shadow_root()` here: cloneNode must not emit
                  // live mutation hooks or mark the document as mutated, and `attach_shadow_root`
                  // performs full insertion steps.
                  let shadow_root_dst = self.push_node(
                    NodeKind::ShadowRoot {
                      mode,
                      delegates_focus,
                      slot_assignment,
                      clonable: true,
                      serializable,
                      declarative,
                    },
                    None,
                    /* inert_subtree */ false,
                  );
                  self.nodes[shadow_root_dst.index()].parent = Some(frame.dst);
                  self.nodes[frame.dst.index()]
                    .children
                    .insert(0, shadow_root_dst);

                  frame.shadow_root_src = Some(src_shadow_root);
                  frame.shadow_root_dst = Some(shadow_root_dst);
                }
              }
            }
          }

          let (Some(src_shadow_root), Some(dst_shadow_root)) =
            (frame.shadow_root_src, frame.shadow_root_dst)
          else {
            // No clonable shadow root: nothing left to do for this frame.
            continue;
          };

          let src_children_len = self.nodes[src_shadow_root.index()].children.len();
          while frame.next_shadow_child < src_children_len {
            let child_src = self.nodes[src_shadow_root.index()].children[frame.next_shadow_child];
            frame.next_shadow_child += 1;

            let child_node = self.node_checked(child_src)?;
            if child_node.parent != Some(src_shadow_root) {
              continue;
            }

            stack.push(frame);
            let child_dst = self.clone_node_shallow(child_src, Some(dst_shadow_root))?;
            stack.push(Frame {
              src: child_src,
              dst: child_dst,
              phase: initial_phase(deep),
              next_child: 0,
              shadow_root_src: None,
              shadow_root_dst: None,
              next_shadow_child: 0,
            });
            continue 'clone;
          }
        }
      }
    }

    Ok(dst_root)
  }

  /// Clone a node from a different `dom2::Document` into this document.
  ///
  /// This mirrors the behaviour of `Document.importNode()` in the DOM Standard for the node kinds
  /// represented by `dom2`. The returned subtree is always detached (`parent == None`) and belongs
  /// to the destination document (`self`).
  ///
  /// Returns the new root `NodeId` in this document plus a mapping of all cloned nodes in pre-order
  /// traversal order (including the root).
  ///
  /// Deep cloning is implemented iteratively to avoid recursion overflow on degenerate trees.
  pub fn import_node_from_document(
    &mut self,
    src: &Document,
    src_root: NodeId,
    deep: bool,
  ) -> Result<(NodeId, Vec<(NodeId, NodeId)>), DomError> {
    let src_node = src.node_checked(src_root)?;
    // WHATWG DOM: `Document.importNode()` does not support importing a document node or shadow root.
    if src_root == src.root()
      || matches!(&src_node.kind, NodeKind::Document { .. })
      || matches!(&src_node.kind, NodeKind::ShadowRoot { .. })
    {
      return Err(DomError::NotSupportedError);
    }

    // Delegate to the shared cross-document clone logic (WHATWG DOM "clone a node" semantics),
    // then reorder the returned mapping into source subtree pre-order for backwards compatibility
    // with this helper's original API.
    let (dst_root, mapping) = crate::dom2::clone_node_into_document(src, src_root, self, deep)?;

    let mut map = std::collections::HashMap::<NodeId, NodeId>::with_capacity(mapping.len());
    for (old, new) in mapping {
      map.insert(old, new);
    }

    let ordered_mapping: Vec<(NodeId, NodeId)> = src
      .subtree_preorder(src_root)
      .filter_map(|old| map.get(&old).copied().map(|new| (old, new)))
      .collect();

    Ok((dst_root, ordered_mapping))
  }

  fn validate_insert_hierarchy(&self, parent: NodeId, child: NodeId) -> Result<(), DomError> {
    // NodeId validation is performed by callers, but keep this self-contained for internal use.
    let parent_kind = &self.node_checked(parent)?.kind;
    let child_kind = &self.node_checked(child)?.kind;

    // The document root cannot be inserted anywhere.
    if child == self.root() {
      return Err(DomError::HierarchyRequestError);
    }

    // `Document` nodes cannot be inserted into the tree.
    if matches!(child_kind, NodeKind::Document { .. }) {
      return Err(DomError::InvalidNodeTypeError);
    }

    // Leaf nodes cannot accept children.
    if matches!(
      parent_kind,
      NodeKind::Text { .. }
        | NodeKind::Comment { .. }
        | NodeKind::ProcessingInstruction { .. }
        | NodeKind::Doctype { .. }
    ) {
      return Err(DomError::HierarchyRequestError);
    }

    if matches!(parent_kind, NodeKind::Document { .. })
      && matches!(child_kind, NodeKind::Text { .. })
    {
      return Err(DomError::HierarchyRequestError);
    }

    match child_kind {
      NodeKind::Doctype { .. } => match parent_kind {
        NodeKind::Document { .. } => {}
        _ => return Err(DomError::HierarchyRequestError),
      },
      NodeKind::ShadowRoot { .. } => match parent_kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {}
        _ => return Err(DomError::HierarchyRequestError),
      },
      NodeKind::Slot { .. } => match parent_kind {
        NodeKind::Element { .. } | NodeKind::ShadowRoot { .. } | NodeKind::DocumentFragment => {}
        _ => return Err(DomError::HierarchyRequestError),
      },
      _ => {}
    }

    Ok(())
  }

  fn validate_document_insertion(
    &self,
    parent: NodeId,
    new_child: NodeId,
    reference: Option<NodeId>,
    insertion_idx: usize,
  ) -> Result<(), DomError> {
    let parent_kind = &self.node_checked(parent)?.kind;
    if !matches!(parent_kind, NodeKind::Document { .. }) {
      return Ok(());
    }

    fn is_element_child(kind: &NodeKind) -> bool {
      matches!(kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
    }

    let children = self.node_checked(parent)?.children.as_slice();
    let moving_existing_child = self.nodes[new_child.index()].parent == Some(parent);
    let has_element_child = children.iter().any(|&id| {
      if moving_existing_child && id == new_child {
        return false;
      }
      self
        .nodes
        .get(id.index())
        .is_some_and(|node| is_element_child(&node.kind))
    });
    let has_doctype_child = children.iter().any(|&id| {
      if moving_existing_child && id == new_child {
        return false;
      }
      self
        .nodes
        .get(id.index())
        .is_some_and(|node| matches!(node.kind, NodeKind::Doctype { .. }))
    });

    let new_kind = &self.node_checked(new_child)?.kind;
    match new_kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => {
        if has_element_child {
          return Err(DomError::HierarchyRequestError);
        }

        if reference.is_some()
          && children[insertion_idx..].iter().any(|&id| {
            self
              .nodes
              .get(id.index())
              .is_some_and(|node| matches!(node.kind, NodeKind::Doctype { .. }))
          })
        {
          return Err(DomError::HierarchyRequestError);
        }
      }
      NodeKind::Doctype { .. } => {
        if has_doctype_child {
          return Err(DomError::HierarchyRequestError);
        }

        if reference.is_some() {
          if children[..insertion_idx].iter().any(|&id| {
            self
              .nodes
              .get(id.index())
              .is_some_and(|node| is_element_child(&node.kind))
          }) {
            return Err(DomError::HierarchyRequestError);
          }
        } else if has_element_child {
          return Err(DomError::HierarchyRequestError);
        }
      }
      _ => {}
    }

    Ok(())
  }

  fn validate_document_replacement(
    &self,
    parent: NodeId,
    new_child: NodeId,
    old_child: NodeId,
    old_child_idx: usize,
  ) -> Result<(), DomError> {
    let parent_kind = &self.node_checked(parent)?.kind;
    if !matches!(parent_kind, NodeKind::Document { .. }) {
      return Ok(());
    }

    fn is_element_child(kind: &NodeKind) -> bool {
      matches!(kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
    }

    let children = self.node_checked(parent)?.children.as_slice();
    let new_kind = &self.node_checked(new_child)?.kind;

    match new_kind {
      NodeKind::Element { .. } | NodeKind::Slot { .. } => {
        if children.iter().any(|&id| {
          id != old_child
            && self
              .node_checked(id)
              .is_ok_and(|node| is_element_child(&node.kind))
        }) {
          return Err(DomError::HierarchyRequestError);
        }

        if old_child_idx + 1 < children.len()
          && children[old_child_idx + 1..].iter().any(|&id| {
            self
              .node_checked(id)
              .is_ok_and(|node| matches!(node.kind, NodeKind::Doctype { .. }))
          })
        {
          return Err(DomError::HierarchyRequestError);
        }
      }
      NodeKind::Doctype { .. } => {
        if children.iter().any(|&id| {
          id != old_child
            && self
              .node_checked(id)
              .is_ok_and(|node| matches!(node.kind, NodeKind::Doctype { .. }))
        }) {
          return Err(DomError::HierarchyRequestError);
        }

        if children[..old_child_idx].iter().any(|&id| {
          self
            .node_checked(id)
            .is_ok_and(|node| is_element_child(&node.kind))
        }) {
          return Err(DomError::HierarchyRequestError);
        }
      }
      _ => {}
    }

    Ok(())
  }

  fn validate_document_fragment_splice(
    &self,
    parent: NodeId,
    prefix: &[NodeId],
    inserted: &[NodeId],
    suffix: &[NodeId],
  ) -> Result<(), DomError> {
    let parent_kind = &self.node_checked(parent)?.kind;
    if !matches!(parent_kind, NodeKind::Document { .. }) {
      return Ok(());
    }

    let mut element_count = 0usize;
    let mut doctype_count = 0usize;
    let mut first_element_pos: Option<usize> = None;
    let mut first_doctype_pos: Option<usize> = None;

    let mut pos = 0usize;
    for &id in prefix.iter().chain(inserted.iter()).chain(suffix.iter()) {
      let kind = &self.node_checked(id)?.kind;
      match kind {
        NodeKind::Element { .. } | NodeKind::Slot { .. } => {
          element_count += 1;
          if element_count > 1 {
            return Err(DomError::HierarchyRequestError);
          }
          if first_element_pos.is_none() {
            first_element_pos = Some(pos);
          }
        }
        NodeKind::Doctype { .. } => {
          doctype_count += 1;
          if doctype_count > 1 {
            return Err(DomError::HierarchyRequestError);
          }
          if first_doctype_pos.is_none() {
            first_doctype_pos = Some(pos);
          }
        }
        _ => {}
      }
      pos += 1;
    }

    if let (Some(doctype_pos), Some(element_pos)) = (first_doctype_pos, first_element_pos) {
      if doctype_pos > element_pos {
        return Err(DomError::HierarchyRequestError);
      }
    }

    Ok(())
  }

  fn validate_document_fragment_insertion(
    &self,
    parent: NodeId,
    insertion_idx: usize,
    fragment_children: &[NodeId],
  ) -> Result<(), DomError> {
    let children = self.node_checked(parent)?.children.as_slice();
    let (prefix, suffix) = children.split_at(insertion_idx);
    self.validate_document_fragment_splice(parent, prefix, fragment_children, suffix)
  }

  fn validate_document_fragment_replacement(
    &self,
    parent: NodeId,
    old_child_idx: usize,
    fragment_children: &[NodeId],
  ) -> Result<(), DomError> {
    let children = self.node_checked(parent)?.children.as_slice();
    let prefix = &children[..old_child_idx];
    let suffix = &children[old_child_idx + 1..];
    self.validate_document_fragment_splice(parent, prefix, fragment_children, suffix)
  }

  fn validate_no_cycles(&self, parent: NodeId, child: NodeId) -> Result<(), DomError> {
    if parent == child {
      return Err(DomError::HierarchyRequestError);
    }

    // A leaf node (no children) cannot be an ancestor of `parent` unless `parent == child` which is
    // handled above. This fast path keeps common insertions O(1) on deep trees.
    if self.node_checked(child)?.children.is_empty() {
      return Ok(());
    }

    let mut current = Some(parent);
    while let Some(id) = current {
      if id == child {
        return Err(DomError::HierarchyRequestError);
      }
      current = self.node_checked(id)?.parent;
    }

    Ok(())
  }

  fn index_of_child_internal(
    &self,
    parent: NodeId,
    child: NodeId,
  ) -> Result<Option<usize>, DomError> {
    self.node_checked(parent)?;
    self.node_checked(child)?;
    Ok(
      self.nodes[parent.index()]
        .children
        .iter()
        .position(|&c| c == child),
    )
  }

  fn detach_from_parent(&mut self, child: NodeId) -> Result<Option<NodeId>, DomError> {
    self.node_checked(child)?;
    let Some(old_parent) = self.nodes[child.index()].parent else {
      return Ok(None);
    };

    self.node_checked(old_parent)?;
    let pos = self.nodes[old_parent.index()]
      .children
      .iter()
      .position(|&c| c == child)
      .ok_or(DomError::NotFoundError)?;

    let (previous_sibling, next_sibling) = {
      let siblings = self.nodes[old_parent.index()].children.as_slice();
      let prev = pos
        .checked_sub(1)
        .and_then(|idx| siblings.get(idx))
        .copied();
      let next = siblings.get(pos + 1).copied();
      (prev, next)
    };

    self.live_mutation.pre_remove(child, old_parent, pos);
    if let Some(tree_index) = self.tree_child_index_for_range(old_parent, child) {
      self.live_range_pre_remove_steps(child, old_parent, tree_index);
    }
    self.node_iterator_pre_remove_steps(child);
    self.nodes[old_parent.index()].children.remove(pos);
    let _ = self.mutation_observer_add_transient_observers_on_remove(child, old_parent);
    self.nodes[child.index()].parent = None;
    self.record_child_list_mutation(old_parent);
    let _ = self.queue_mutation_record_child_list(
      old_parent,
      Vec::new(),
      vec![child],
      previous_sibling,
      next_sibling,
    );
    Ok(Some(old_parent))
  }

  pub fn create_element(&mut self, tag_name: &str, namespace: &str) -> NodeId {
    self.create_element_ns(tag_name, namespace, None)
  }

  pub fn create_element_ns(
    &mut self,
    tag_name: &str,
    namespace: &str,
    prefix: Option<&str>,
  ) -> NodeId {
    let is_html_ns = self.is_html_case_insensitive_namespace(namespace);
    // Normalise HTML namespace to the empty string for HTML documents, matching the renderer DOM
    // representation.
    let namespace = if is_html_ns && namespace == HTML_NAMESPACE {
      ""
    } else {
      namespace
    };

    let inert_subtree = is_html_ns && tag_name.eq_ignore_ascii_case("template");
    let kind = if is_html_ns && tag_name.eq_ignore_ascii_case("slot") && prefix.is_none() {
      NodeKind::Slot {
        namespace: namespace.to_string(),
        attributes: Vec::new(),
        assigned: false,
      }
    } else {
      NodeKind::Element {
        tag_name: tag_name.to_string(),
        namespace: namespace.to_string(),
        prefix: prefix.map(|p| p.to_string()),
        attributes: Vec::new(),
      }
    };

    let id = self.push_node(kind, None, inert_subtree);
    if is_html_ns && tag_name.eq_ignore_ascii_case("script") {
      // HTML: Scripts created via DOM APIs have their "force async" flag set by default.
      self.nodes[id.index()].script_force_async = true;
    }
    id
  }

  pub fn create_text(&mut self, data: &str) -> NodeId {
    self.push_node(
      NodeKind::Text {
        content: data.to_string(),
      },
      None,
      /* inert_subtree */ false,
    )
  }

  pub fn create_comment(&mut self, data: &str) -> NodeId {
    self.push_node(
      NodeKind::Comment {
        content: data.to_string(),
      },
      None,
      /* inert_subtree */ false,
    )
  }

  pub fn create_processing_instruction(&mut self, target: &str, data: &str) -> NodeId {
    self.push_node(
      NodeKind::ProcessingInstruction {
        target: target.to_string(),
        data: data.to_string(),
      },
      None,
      /* inert_subtree */ false,
    )
  }

  pub fn create_document_fragment(&mut self) -> NodeId {
    self.push_node(
      NodeKind::DocumentFragment,
      None,
      /* inert_subtree */ false,
    )
  }

  pub fn create_doctype(&mut self, name: &str, public_id: &str, system_id: &str) -> NodeId {
    self.push_node(
      NodeKind::Doctype {
        name: name.to_string(),
        public_id: public_id.to_string(),
        system_id: system_id.to_string(),
      },
      None,
      /* inert_subtree */ false,
    )
  }

  pub fn create_document_type(&mut self, name: &str, public_id: &str, system_id: &str) -> NodeId {
    self.create_doctype(name, public_id, system_id)
  }

  pub fn text_data(&self, node: NodeId) -> Result<&str, DomError> {
    let node = self.node_checked(node)?;
    match &node.kind {
      NodeKind::Text { content } => Ok(content.as_str()),
      _ => Err(DomError::InvalidNodeTypeError),
    }
  }

  /// Split a `Text` node at a UTF-16 code unit offset, returning the newly created trailing node.
  ///
  /// Spec: https://dom.spec.whatwg.org/#concept-text-split
  pub fn split_text(&mut self, node: NodeId, offset: usize) -> Result<NodeId, DomError> {
    let node_id = node;
    self.node_checked(node_id)?;

    let old_value = match &self.node(node_id).kind {
      NodeKind::Text { content } => content.clone(),
      _ => return Err(DomError::InvalidNodeType),
    };

    // NOTE: DOM `Text.splitText(offset)` offsets are defined in UTF-16 code units.
    let units: Vec<u16> = old_value.encode_utf16().collect();
    if offset > units.len() {
      return Err(DomError::IndexSizeError);
    }

    let new_data = String::from_utf16_lossy(&units[offset..]);
    let new_node = self.create_text(&new_data);

    // Step 6: Let parent be the node's parent.
    let parent = self.nodes[node_id.index()].parent;
    if let Some(parent_id) = parent {
      // Step 7.1: Insert newNode into parent before node's next sibling.
      let index = self
        .index_of_child_internal(parent_id, node_id)?
        .ok_or(DomError::NotFoundError)?;
      let reference = self.nodes[parent_id.index()].children.get(index + 1).copied();
      let _ = self.insert_before(parent_id, new_node, reference)?;

      // Step 7.2–7.5: Live range updates.
      self.live_range_split_text_steps(node_id, offset, new_node, parent_id, index);
    }

    // Step 8: Replace the data from `offset` to the end with the empty string.
    let _ = self.replace_data(node_id, offset, usize::MAX, "")?;

    Ok(new_node)
  }

  pub fn comment_data(&self, node: NodeId) -> Result<&str, DomError> {
    let node = self.node_checked(node)?;
    match &node.kind {
      NodeKind::Comment { content } => Ok(content.as_str()),
      _ => Err(DomError::InvalidNodeTypeError),
    }
  }

  pub fn processing_instruction_data(&self, node: NodeId) -> Result<&str, DomError> {
    let node = self.node_checked(node)?;
    match &node.kind {
      NodeKind::ProcessingInstruction { data, .. } => Ok(data.as_str()),
      _ => Err(DomError::InvalidNodeTypeError),
    }
  }

  pub fn replace_data(
    &mut self,
    node: NodeId,
    offset: usize,
    count: usize,
    data: &str,
  ) -> Result<bool, DomError> {
    let node_id = node;
    self.node_checked(node_id)?;

    let has_live_subscribers = self.live_mutation.has_subscribers();
    let has_live_ranges = !self.ranges.is_empty();
    #[derive(Clone, Copy)]
    enum ReplaceTarget {
      Text,
      Comment,
      ProcessingInstruction,
    }
    let (target_kind, old_value) = match &self.node(node_id).kind {
      NodeKind::Text { content } => (ReplaceTarget::Text, content.clone()),
      NodeKind::Comment { content } => (ReplaceTarget::Comment, content.clone()),
      NodeKind::ProcessingInstruction { data, .. } => {
        (ReplaceTarget::ProcessingInstruction, data.clone())
      }
      _ => return Err(DomError::InvalidNodeTypeError),
    };

    let is_text_node = matches!(target_kind, ReplaceTarget::Text);

    // NOTE: DOM `CharacterData.replaceData` offsets/counts are defined in UTF-16 code units.
    let mut units: Vec<u16> = old_value.encode_utf16().collect();
    if offset > units.len() {
      return Err(DomError::IndexSizeError);
    }
    let end = offset.saturating_add(count).min(units.len());
    let removed_len = end.saturating_sub(offset);
    let inserted_len = (has_live_subscribers || has_live_ranges)
      .then(|| utf16_len(data))
      .unwrap_or(0);

    // `Vec::splice` applies its mutation when the returned iterator is dropped; we discard it.
    let _ = units.splice(offset..end, data.encode_utf16());
    let new_value = String::from_utf16_lossy(&units);
    if new_value == old_value {
      return Ok(false);
    }

    if has_live_subscribers {
      self
        .live_mutation
        .replace_data(node_id, offset, removed_len, inserted_len);
    }
    if has_live_ranges {
      self.live_range_replace_data_steps(node_id, offset, removed_len, inserted_len);
    }

    {
      let node = self.node_checked_mut(node_id)?;
      match (&mut node.kind, target_kind) {
        (NodeKind::Text { content }, ReplaceTarget::Text) => *content = new_value,
        (NodeKind::Comment { content }, ReplaceTarget::Comment) => *content = new_value,
        (NodeKind::ProcessingInstruction { data, .. }, ReplaceTarget::ProcessingInstruction) => {
          *data = new_value
        }
        _ => unreachable!("replace_data target kind changed unexpectedly"), // fastrender-allow-panic
      }
    }

    // Only text node mutations are render-affecting today; comments and processing instructions are
    // ignored by renderer snapshots.
    if is_text_node {
      self.record_text_mutation(node_id);
      self.bump_mutation_generation_classified();
    }
    let _ = self.queue_mutation_record_character_data(node_id, Some(old_value));
    Ok(true)
  }

  /// Set the `data` string for any CharacterData node (Text, Comment, ProcessingInstruction).
  ///
  /// This is primarily used by binding layers implementing `Node.nodeValue` / `CharacterData.data`.
  ///
  /// Per DOM, setting `nodeValue` on non-CharacterData nodes is a no-op.
  pub fn set_character_data(&mut self, node: NodeId, data: &str) -> Result<bool, DomError> {
    match self.replace_data(node, 0, usize::MAX, data) {
      Ok(changed) => Ok(changed),
      Err(DomError::InvalidNodeTypeError) => Ok(false),
      Err(err) => Err(err),
    }
  }

  pub fn set_text_data(&mut self, node: NodeId, data: &str) -> Result<bool, DomError> {
    let node_id = node;
    self.node_checked(node_id)?;
    // Implement `setTextData` in terms of the DOM "replace data" primitive so live Range updates can
    // be driven by `replace_data(offset, removed_len, inserted_len)`.
    let old_value = match &self.node_checked(node_id)?.kind {
      NodeKind::Text { content } => {
        if content == data {
          return Ok(false);
        }
        content.clone()
      }
      _ => return Err(DomError::InvalidNodeTypeError),
    };

    let has_live_subscribers = self.live_mutation.has_subscribers();
    let has_live_ranges = !self.ranges.is_empty();
    if has_live_subscribers || has_live_ranges {
      let removed_len = utf16_len(&old_value);
      let inserted_len = utf16_len(data);
      if has_live_subscribers {
        self
          .live_mutation
          .replace_data(node_id, /* offset */ 0, removed_len, inserted_len);
      }
      if has_live_ranges {
        self.live_range_replace_data_steps(node_id, /* offset */ 0, removed_len, inserted_len);
      }
    }

    {
      let node = self.node_checked_mut(node_id)?;
      let NodeKind::Text { content } = &mut node.kind else {
        return Err(DomError::InvalidNodeTypeError);
      };
      content.clear();
      content.push_str(data);
    }

    self.record_text_mutation(node_id);
    self.bump_mutation_generation_classified();
    let _ = self.queue_mutation_record_character_data(node_id, Some(old_value));
    Ok(true)
  }

  /// Split a Text node at a UTF-16 code unit offset, returning the newly-created trailing Text node.
  ///
  /// Mirrors the WHATWG DOM `Text.splitText(offset)` algorithm for `dom2`'s supported node kinds.
  ///
  /// `offset_utf16` is measured in UTF-16 code units (not bytes and not Unicode scalar values),
  /// matching JavaScript string indexing.
  pub fn split_text(&mut self, node: NodeId, offset_utf16: usize) -> Result<NodeId, DomError> {
    let node_id = node;
    self.node_checked(node_id)?;

    let old_value = match &self.node_checked(node_id)?.kind {
      NodeKind::Text { content } => content.clone(),
      _ => return Err(DomError::InvalidNodeTypeError),
    };

    // Offsets are defined in UTF-16 code units.
    let units: Vec<u16> = old_value.encode_utf16().collect();
    if offset_utf16 > units.len() {
      return Err(DomError::IndexSizeError);
    }

    let new_data = String::from_utf16_lossy(&units[offset_utf16..]);
    let new_node = self.create_text(&new_data);

    let parent = self.nodes[node_id.index()].parent;
    if let Some(parent) = parent {
      // Find the Text node's index among its parent's child list so we can insert immediately after
      // it and perform the spec's splitText-specific live Range updates.
      let raw_index = self
        .index_of_child_internal(parent, node_id)?
        .ok_or(DomError::NotFoundError)?;
      let tree_index = self
        .tree_child_index_for_range(parent, node_id)
        .unwrap_or_else(|| self.tree_child_index_from_raw_index_for_range(parent, raw_index));

      // Insert the new Text node immediately after the original node.
      let reference = self
        .nodes[parent.index()]
        .children
        .get(raw_index + 1)
        .copied();
      let _ = self.insert_before(parent, new_node, reference)?;

      // Live range updates for splitText (the extra steps beyond generic insert/replace-data).
      self.live_range_split_text_steps(node_id, offset_utf16, new_node, parent, tree_index);
    }

    // Truncate the original node's data to the prefix [0, offset).
    let count = units.len().saturating_sub(offset_utf16);
    let _ = self.replace_data(node_id, offset_utf16, count, "")?;

    Ok(new_node)
  }

  pub fn set_comment_data(&mut self, node: NodeId, data: &str) -> Result<bool, DomError> {
    let node_id = node;
    // Drive live Range/NodeIterator updates via the DOM "replace data" primitive.
    let old_value = match &self.node_checked(node_id)?.kind {
      NodeKind::Comment { content } => {
        if content == data {
          return Ok(false);
        }
        content.clone()
      }
      _ => return Err(DomError::InvalidNodeTypeError),
    };

    let has_live_subscribers = self.live_mutation.has_subscribers();
    let has_live_ranges = !self.ranges.is_empty();
    if has_live_subscribers || has_live_ranges {
      let removed_len = utf16_len(&old_value);
      let inserted_len = utf16_len(data);
      if has_live_subscribers {
        self
          .live_mutation
          .replace_data(node_id, /* offset */ 0, removed_len, inserted_len);
      }
      if has_live_ranges {
        self.live_range_replace_data_steps(node_id, /* offset */ 0, removed_len, inserted_len);
      }
    }

    {
      let node = self.node_checked_mut(node_id)?;
      let NodeKind::Comment { content } = &mut node.kind else {
        return Err(DomError::InvalidNodeTypeError);
      };
      content.clear();
      content.push_str(data);
    }

    // Comments are currently ignored by renderer DOM snapshots, so treat comment data changes as
    // non-render-affecting. Still queue MutationObserver CharacterData records so observers and
    // future live-mutation hooks observe the update.
    let _ = self.queue_mutation_record_character_data(node_id, Some(old_value));
    Ok(true)
  }

  pub fn set_processing_instruction_data(
    &mut self,
    node: NodeId,
    data: &str,
  ) -> Result<bool, DomError> {
    let node_id = node;
    // Drive live Range/NodeIterator updates via the DOM "replace data" primitive.
    let old_value = match &self.node_checked(node_id)?.kind {
      NodeKind::ProcessingInstruction { data: value, .. } => {
        if value == data {
          return Ok(false);
        }
        value.clone()
      }
      _ => return Err(DomError::InvalidNodeTypeError),
    };

    let has_live_subscribers = self.live_mutation.has_subscribers();
    let has_live_ranges = !self.ranges.is_empty();
    if has_live_subscribers || has_live_ranges {
      let removed_len = utf16_len(&old_value);
      let inserted_len = utf16_len(data);
      if has_live_subscribers {
        self
          .live_mutation
          .replace_data(node_id, /* offset */ 0, removed_len, inserted_len);
      }
      if has_live_ranges {
        self.live_range_replace_data_steps(node_id, /* offset */ 0, removed_len, inserted_len);
      }
    }

    {
      let node = self.node_checked_mut(node_id)?;
      let NodeKind::ProcessingInstruction { data: value, .. } = &mut node.kind else {
        return Err(DomError::InvalidNodeTypeError);
      };
      value.clear();
      value.push_str(data);
    }

    // Processing instructions are currently ignored by renderer DOM snapshots, so treat data
    // changes as non-render-affecting. Still queue MutationObserver CharacterData records so
    // observers and future live-mutation hooks observe the update.
    let _ = self.queue_mutation_record_character_data(node_id, Some(old_value));
    Ok(true)
  }
  pub fn parent(&self, node: NodeId) -> Result<Option<NodeId>, DomError> {
    Ok(self.node_checked(node)?.parent)
  }

  pub fn children(&self, node: NodeId) -> Result<&[NodeId], DomError> {
    Ok(self.node_checked(node)?.children.as_slice())
  }

  pub fn index_of_child(&self, parent: NodeId, child: NodeId) -> Result<Option<usize>, DomError> {
    self.index_of_child_internal(parent, child)
  }

  pub(crate) fn with_shadow_root_as_document_fragment<T>(
    &mut self,
    shadow_root: NodeId,
    f: impl FnOnce(&mut Self) -> Result<T, DomError>,
  ) -> Result<T, DomError> {
    self.node_checked(shadow_root)?;
    if !matches!(
      self.nodes[shadow_root.index()].kind,
      NodeKind::ShadowRoot { .. }
    ) {
      return Err(DomError::InvalidNodeTypeError);
    }

    struct RestoreGuard {
      doc: *mut Document,
      node: NodeId,
      kind: Option<NodeKind>,
      parent: Option<NodeId>,
    }

    impl Drop for RestoreGuard {
      fn drop(&mut self) {
        // SAFETY: `doc` is valid for the extent of the mutation call that created this guard.
        let doc = unsafe { &mut *self.doc };
        if self.node.index() >= doc.nodes.len() {
          return;
        }
        if let Some(kind) = self.kind.take() {
          doc.nodes[self.node.index()].kind = kind;
        }
        doc.nodes[self.node.index()].parent = self.parent;
      }
    }

    let old_kind = std::mem::replace(
      &mut self.nodes[shadow_root.index()].kind,
      NodeKind::DocumentFragment,
    );
    let old_parent = self.nodes[shadow_root.index()].parent;
    let _guard = RestoreGuard {
      doc: self as *mut Document,
      node: shadow_root,
      kind: Some(old_kind),
      parent: old_parent,
    };

    f(self)
  }

  pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<bool, DomError> {
    self.insert_before(parent, child, None)
  }

  pub fn insert_before(
    &mut self,
    parent: NodeId,
    new_child: NodeId,
    reference: Option<NodeId>,
  ) -> Result<bool, DomError> {
    self.node_checked(parent)?;
    self.node_checked(new_child)?;
    if let Some(reference) = reference {
      self.node_checked(reference)?;
    }

    self.validate_insert_hierarchy(parent, new_child)?;
    self.validate_no_cycles(parent, new_child)?;

    let mut insertion_idx = match reference {
      Some(reference) => self
        .index_of_child_internal(parent, reference)?
        .ok_or(DomError::NotFoundError)?,
      None => self.nodes[parent.index()].children.len(),
    };

    // Sibling pointers for the insertion-side mutation record. These are computed before any
    // potential removal of `new_child` (mirroring the DOM Standard's insertion algorithm, which
    // determines the insertion siblings before adopting/removing the node from its old parent).
    let record_next_sibling = reference;
    let record_previous_sibling = {
      let siblings = self.nodes[parent.index()].children.as_slice();
      if record_next_sibling.is_some() {
        insertion_idx
          .checked_sub(1)
          .and_then(|idx| siblings.get(idx))
          .copied()
      } else {
        siblings.last().copied()
      }
    };

    if matches!(
      self.nodes[new_child.index()].kind,
      NodeKind::DocumentFragment
    ) {
      // DocumentFragment insertion is transparent: insert its children in order, then empty it.
      // Pre-validate all children before mutating to ensure atomicity.
      let frag_children_len = self.nodes[new_child.index()].children.len();
      for idx in 0..frag_children_len {
        let child = self.nodes[new_child.index()].children[idx];
        self.validate_insert_hierarchy(parent, child)?;
        self.validate_no_cycles(parent, child)?;
      }

      if frag_children_len == 0 {
        return Ok(false);
      }

      self.validate_document_fragment_insertion(
        parent,
        insertion_idx,
        self.nodes[new_child.index()].children.as_slice(),
      )?;

      let moved_children = self.nodes[new_child.index()].children.clone();
      for (idx, &child) in moved_children.iter().enumerate() {
        self.live_mutation.pre_remove(child, new_child, idx);
      }
      self
        .live_mutation
        .pre_insert(parent, insertion_idx, moved_children.len());
      self.live_range_pre_insert_steps(
        parent,
        self.tree_child_index_from_raw_index_for_range(parent, insertion_idx),
        self.inserted_tree_children_count_for_range(parent, &moved_children),
      );

      let mut children_to_move: Vec<NodeId> = Vec::with_capacity(moved_children.len());
      while let Some(child) = self.nodes[new_child.index()].children.first().copied() {
        self.live_range_pre_remove_steps(child, new_child, 0);
        self.node_iterator_pre_remove_steps(child);
        self.nodes[new_child.index()].children.remove(0);
        let _ = self.mutation_observer_add_transient_observers_on_remove(child, new_child);
        self.nodes[child.index()].parent = None;
        children_to_move.push(child);
      }
      // Fragments are always detached.
      self.nodes[new_child.index()].parent = None;

      // Per DOM: inserting a DocumentFragment queues a childList record on the fragment itself for
      // the removal of its children.
      let _ = self.queue_mutation_record_child_list(
        new_child,
        Vec::new(),
        moved_children.clone(),
        None,
        None,
      );

      for &child in &children_to_move {
        self.nodes[child.index()].parent = Some(parent);
      }

      self.nodes[parent.index()]
        .children
        .splice(insertion_idx..insertion_idx, children_to_move);
      self.record_child_list_mutation(parent);
      self.bump_mutation_generation_classified();

      let inserted_len = moved_children.len();
      let (previous_sibling, next_sibling) = {
        let siblings = self.nodes[parent.index()].children.as_slice();
        let prev = insertion_idx
          .checked_sub(1)
          .and_then(|idx| siblings.get(idx))
          .copied();
        let next = siblings.get(insertion_idx + inserted_len).copied();
        (prev, next)
      };

      let _ = self.queue_mutation_record_child_list(
        parent,
        moved_children,
        Vec::new(),
        previous_sibling,
        next_sibling,
      );
      return Ok(true);
    }

    self.validate_document_insertion(parent, new_child, reference, insertion_idx)?;

    let current_parent = self.nodes[new_child.index()].parent;

    if current_parent == Some(parent) {
      // Move within the same parent.
      let current_idx = self
        .index_of_child_internal(parent, new_child)?
        .ok_or(DomError::NotFoundError)?;

      // If the node is being removed from a position before the insertion point, the insertion
      // index shifts left by one.
      if current_idx < insertion_idx {
        insertion_idx -= 1;
      }

      if current_idx == insertion_idx {
        return Ok(false);
      }
    }

    if current_parent.is_some() {
      self.detach_from_parent(new_child)?;
    }

    self.live_mutation.pre_insert(parent, insertion_idx, 1);
    self.live_range_pre_insert_steps(
      parent,
      self.tree_child_index_from_raw_index_for_range(parent, insertion_idx),
      self.inserted_tree_children_count_for_range(parent, &[new_child]),
    );

    self.nodes[parent.index()]
      .children
      .insert(insertion_idx, new_child);
    self.nodes[new_child.index()].parent = Some(parent);
    self.record_child_list_mutation(parent);
    self.bump_mutation_generation_classified();
    let _ = self.queue_mutation_record_child_list(
      parent,
      vec![new_child],
      Vec::new(),
      record_previous_sibling,
      record_next_sibling,
    );
    Ok(true)
  }

  pub fn remove_child(&mut self, parent: NodeId, child: NodeId) -> Result<bool, DomError> {
    self.node_checked(parent)?;
    self.node_checked(child)?;

    if self.nodes[child.index()].parent != Some(parent) {
      return Err(DomError::NotFoundError);
    }
    let idx = self
      .index_of_child_internal(parent, child)?
      .ok_or(DomError::NotFoundError)?;

    let (previous_sibling, next_sibling) = {
      let siblings = self.nodes[parent.index()].children.as_slice();
      let prev = idx.checked_sub(1).and_then(|i| siblings.get(i)).copied();
      let next = siblings.get(idx + 1).copied();
      (prev, next)
    };

    self.live_mutation.pre_remove(child, parent, idx);
    if let Some(tree_index) = self.tree_child_index_for_range(parent, child) {
      self.live_range_pre_remove_steps(child, parent, tree_index);
    }
    self.node_iterator_pre_remove_steps(child);
    self.nodes[parent.index()].children.remove(idx);
    let _ = self.mutation_observer_add_transient_observers_on_remove(child, parent);
    self.nodes[child.index()].parent = None;
    self.record_child_list_mutation(parent);
    self.bump_mutation_generation_classified();
    let _ = self.queue_mutation_record_child_list(
      parent,
      Vec::new(),
      vec![child],
      previous_sibling,
      next_sibling,
    );
    Ok(true)
  }

  pub fn replace_child(
    &mut self,
    parent: NodeId,
    new_child: NodeId,
    old_child: NodeId,
  ) -> Result<bool, DomError> {
    self.node_checked(parent)?;
    self.node_checked(new_child)?;
    self.node_checked(old_child)?;

    if new_child == old_child {
      return Ok(false);
    }

    self.validate_insert_hierarchy(parent, new_child)?;
    self.validate_no_cycles(parent, new_child)?;

    // Ensure `old_child` is actually a child of `parent`.
    if self.nodes[old_child.index()].parent != Some(parent) {
      return Err(DomError::NotFoundError);
    }
    let mut old_child_idx = self
      .index_of_child_internal(parent, old_child)?
      .ok_or(DomError::NotFoundError)?;

    // Sibling pointers for the replacement-side mutation record, computed before any removals or
    // insertions (mirrors the DOM Standard's `replace` algorithm).
    let (record_previous_sibling, record_next_sibling) = {
      let siblings = self.nodes[parent.index()].children.as_slice();
      let prev = old_child_idx
        .checked_sub(1)
        .and_then(|idx| siblings.get(idx))
        .copied();
      let mut next = siblings.get(old_child_idx + 1).copied();
      if next == Some(new_child) {
        next = siblings.get(old_child_idx + 2).copied();
      }
      (prev, next)
    };

    if matches!(
      self.nodes[new_child.index()].kind,
      NodeKind::DocumentFragment
    ) {
      // DocumentFragment replacement is transparent: remove `old_child`, then insert the
      // fragment's children in its place, and finally empty the fragment.
      //
      // Pre-validate all children before mutating to ensure atomicity.
      let frag_children_len = self.nodes[new_child.index()].children.len();
      for idx in 0..frag_children_len {
        let child = self.nodes[new_child.index()].children[idx];
        self.validate_insert_hierarchy(parent, child)?;
        self.validate_no_cycles(parent, child)?;
      }

      self.validate_document_fragment_replacement(
        parent,
        old_child_idx,
        self.nodes[new_child.index()].children.as_slice(),
      )?;

      self
        .live_mutation
        .pre_remove(old_child, parent, old_child_idx);
      if let Some(tree_index) = self.tree_child_index_for_range(parent, old_child) {
        self.live_range_pre_remove_steps(old_child, parent, tree_index);
      }
      self.node_iterator_pre_remove_steps(old_child);
      self.nodes[parent.index()].children.remove(old_child_idx);
      let _ = self.mutation_observer_add_transient_observers_on_remove(old_child, parent);
      self.nodes[old_child.index()].parent = None;

      let moved_children = self.nodes[new_child.index()].children.clone();
      for (idx, &child) in moved_children.iter().enumerate() {
        self.live_mutation.pre_remove(child, new_child, idx);
      }
      if !moved_children.is_empty() {
        self
          .live_mutation
          .pre_insert(parent, old_child_idx, moved_children.len());
        self.live_range_pre_insert_steps(
          parent,
          self.tree_child_index_from_raw_index_for_range(parent, old_child_idx),
          self.inserted_tree_children_count_for_range(parent, &moved_children),
        );
      }

      let mut children_to_move: Vec<NodeId> = Vec::with_capacity(moved_children.len());
      while let Some(child) = self.nodes[new_child.index()].children.first().copied() {
        self.live_range_pre_remove_steps(child, new_child, 0);
        self.node_iterator_pre_remove_steps(child);
        self.nodes[new_child.index()].children.remove(0);
        let _ = self.mutation_observer_add_transient_observers_on_remove(child, new_child);
        self.nodes[child.index()].parent = None;
        children_to_move.push(child);
      }
      self.nodes[new_child.index()].parent = None;

      // Per DOM: inserting a DocumentFragment queues a childList record on the fragment itself for
      // the removal of its children.
      if !moved_children.is_empty() {
        let _ = self.queue_mutation_record_child_list(
          new_child,
          Vec::new(),
          moved_children.clone(),
          None,
          None,
        );
      }

      for &child in &children_to_move {
        self.nodes[child.index()].parent = Some(parent);
      }

      self.nodes[parent.index()]
        .children
        .splice(old_child_idx..old_child_idx, children_to_move);
      self.record_child_list_mutation(parent);
      self.bump_mutation_generation_classified();
      let _ = self.queue_mutation_record_child_list(
        parent,
        moved_children,
        vec![old_child],
        record_previous_sibling,
        record_next_sibling,
      );
      return Ok(true);
    }

    self.validate_document_replacement(parent, new_child, old_child, old_child_idx)?;

    let current_parent = self.nodes[new_child.index()].parent;
    if current_parent == Some(parent) {
      // Remove the existing instance so we can insert at the replacement index.
      let idx = self
        .index_of_child_internal(parent, new_child)?
        .ok_or(DomError::NotFoundError)?;
      if idx < old_child_idx {
        old_child_idx -= 1;
      }
      self.detach_from_parent(new_child)?;
    } else if current_parent.is_some() {
      self.detach_from_parent(new_child)?;
    }

    self
      .live_mutation
      .pre_remove(old_child, parent, old_child_idx);
    if let Some(tree_index) = self.tree_child_index_for_range(parent, old_child) {
      self.live_range_pre_remove_steps(old_child, parent, tree_index);
    }
    self.node_iterator_pre_remove_steps(old_child);
    self.nodes[parent.index()].children.remove(old_child_idx);
    let _ = self.mutation_observer_add_transient_observers_on_remove(old_child, parent);
    self.nodes[old_child.index()].parent = None;

    self.live_mutation.pre_insert(parent, old_child_idx, 1);
    self.live_range_pre_insert_steps(
      parent,
      self.tree_child_index_from_raw_index_for_range(parent, old_child_idx),
      self.inserted_tree_children_count_for_range(parent, &[new_child]),
    );
    self.nodes[parent.index()]
      .children
      .insert(old_child_idx, new_child);
    self.nodes[new_child.index()].parent = Some(parent);
    self.record_child_list_mutation(parent);
    self.bump_mutation_generation_classified();
    let _ = self.queue_mutation_record_child_list(
      parent,
      vec![new_child],
      vec![old_child],
      record_previous_sibling,
      record_next_sibling,
    );
    Ok(true)
  }
}
