use super::{Document, DomError, NodeId, NodeKind};

pub type NodeIdMapping = Vec<(NodeId, NodeId)>;

#[derive(Debug, Clone)]
pub struct AdoptedSubtree {
  pub new_root: NodeId,
  pub mapping: NodeIdMapping,
}

/// Clone a `dom2` subtree from `src` into the `dst` document, returning the new root plus a mapping
/// of every cloned source node id to its corresponding destination node id.
///
/// This helper is intended for embedding layers that need to transfer subtrees across multiple
/// `dom2::Document` instances (e.g. for DOM `Document.importNode()` and cross-document insertion).
///
/// The returned root is always detached in the destination document (`parent == None`).
pub fn clone_node_into_document(
  src: &Document,
  src_root: NodeId,
  dst: &mut Document,
  deep: bool,
) -> Result<(NodeId, NodeIdMapping), DomError> {
  let mut mapping: NodeIdMapping = Vec::new();
  let new_root = clone_subtree_from_other_document(
    dst,
    src,
    src_root,
    /* dst_parent */ None,
    deep,
    Some(&mut mapping),
    CrossDocumentCloneSemantics::Clone,
  )?;
  Ok((new_root, mapping))
}

/// Convenience wrapper around [`clone_node_into_document`] for deep cloning.
pub fn clone_node_into_document_deep(
  src: &Document,
  src_root: NodeId,
  dst: &mut Document,
) -> Result<(NodeId, NodeIdMapping), DomError> {
  clone_node_into_document(src, src_root, dst, /* deep */ true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CrossDocumentCloneSemantics {
  /// Clone semantics as used by DOM `Node.cloneNode()` / `Document.importNode()`.
  ///
  /// This follows HTML's per-element cloning steps, including resetting some script internal slots.
  Clone,
  /// Adoption semantics as used by DOM `Document.adoptNode()`.
  ///
  /// Real adoption moves the same node instance into a new document without running cloning steps.
  /// Since `dom2` represents each document as an independent node arena, we approximate adoption by
  /// copying the subtree and returning an old→new mapping. In that approximation, we must preserve
  /// node-internal state that would have stayed on the moved node.
  Adopt,
}

fn clone_node_shallow_from_other_document(
  dst: &mut Document,
  src: &Document,
  src_id: NodeId,
  parent: Option<NodeId>,
  semantics: CrossDocumentCloneSemantics,
) -> Result<NodeId, DomError> {
  src.node_checked(src_id)?;

  let dst_is_html = dst.is_html_document();

  // HTML cloning steps for form controls copy internal state (value/checkedness + dirty flags).
  // `push_node` initializes state from attributes in the destination document, so capture the live
  // source state and overwrite the freshly-allocated destination slots.
  let input_state = if dst_is_html {
    src.input_states[src_id.index()].clone()
  } else {
    None
  };
  let textarea_state = if dst_is_html {
    src.textarea_states[src_id.index()].clone()
  } else {
    None
  };
  let option_state = if dst_is_html {
    src.option_states[src_id.index()].clone()
  } else {
    None
  };

  let (
    kind,
    inert_subtree,
    is_html_script,
    script_parser_document,
    script_force_async,
    script_already_started,
    mathml_annotation_xml_integration_point,
  ) = {
    let node = &src.nodes[src_id.index()];
    let is_html_script = dst.kind_is_html_script(&node.kind);
    let script_parser_document = node.script_parser_document;
    let script_force_async = match semantics {
      CrossDocumentCloneSemantics::Clone => {
        if is_html_script {
          let NodeKind::Element { attributes, .. } = &node.kind else {
            unreachable!();
          };
          // HTML: script element cloning steps recompute the "force async" flag from the presence
          // of an `async` content attribute.
          !attributes
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("async"))
        } else {
          false
        }
      }
      CrossDocumentCloneSemantics::Adopt => node.script_force_async,
    };

    let kind = match &node.kind {
      NodeKind::Document { quirks_mode } => {
        // A `dom2::Document` stores its primary document node at `Document::root()`, but cloning a
        // document node is still useful for same-document `cloneNode()` and for internal adoption
        // work. The cloned `Document` node must remain detached: `Document` nodes cannot be inserted
        // into a tree.
        if parent.is_some() {
          return Err(DomError::InvalidNodeType);
        }
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
        ..
      } => NodeKind::ShadowRoot {
        mode: *mode,
        delegates_focus: *delegates_focus,
        slot_assignment: *slot_assignment,
      },
      NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => {
        if dst.is_html_case_insensitive_namespace(namespace) {
          NodeKind::Slot {
            namespace: namespace.clone(),
            attributes: attributes.clone(),
            // Slot assignment is derived state; imported clones start detached.
            assigned: false,
          }
        } else {
          NodeKind::Element {
            tag_name: "slot".to_string(),
            namespace: namespace.clone(),
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
        ..
      } => NodeKind::Element {
        tag_name: tag_name.clone(),
        namespace: namespace.clone(),
        prefix: prefix.clone(),
        attributes: attributes.clone(),
      },
      NodeKind::Text { content } => NodeKind::Text {
        content: content.clone(),
      },
    };

    (
      kind,
      node.inert_subtree,
      is_html_script,
      script_parser_document,
      script_force_async,
      node.script_already_started,
      node.mathml_annotation_xml_integration_point,
    )
  };

  let inert_subtree = inert_subtree && dst_is_html;
  let dst_id = dst.push_node(kind, parent, inert_subtree);

  if dst_is_html {
    // Only overwrite freshly-initialized form control state when the source node actually had state
    // to preserve. This avoids accidentally clearing state when cloning from an XML document.
    if input_state.is_some() {
      dst.input_states[dst_id.index()] = input_state;
    }
    if textarea_state.is_some() {
      dst.textarea_states[dst_id.index()] = textarea_state;
    }
    if option_state.is_some() {
      dst.option_states[dst_id.index()] = option_state;
    }
  }

  // Preserve HTML parser flags that affect future parsing behavior.
  dst.nodes[dst_id.index()].mathml_annotation_xml_integration_point =
    mathml_annotation_xml_integration_point;

  match semantics {
    CrossDocumentCloneSemantics::Clone => {
      if is_html_script {
        // HTML: script element cloning steps copy the "already started" flag only.
        //
        // https://html.spec.whatwg.org/multipage/scripting.html#script-processing-model
        dst.nodes[dst_id.index()].script_force_async = script_force_async;
        dst.nodes[dst_id.index()].script_already_started = script_already_started;
      }
    }
    CrossDocumentCloneSemantics::Adopt => {
      // Adoption moves the node without running per-element cloning steps; preserve node-internal
      // state exactly.
      dst.nodes[dst_id.index()].inert_subtree = inert_subtree;
      dst.nodes[dst_id.index()].script_parser_document = script_parser_document;
      dst.nodes[dst_id.index()].script_force_async = script_force_async;
      dst.nodes[dst_id.index()].script_already_started = script_already_started;
    }
  }

  Ok(dst_id)
}

fn clone_subtree_from_other_document(
  dst: &mut Document,
  src: &Document,
  src_root: NodeId,
  dst_parent: Option<NodeId>,
  deep: bool,
  mut mapping: Option<&mut NodeIdMapping>,
  semantics: CrossDocumentCloneSemantics,
) -> Result<NodeId, DomError> {
  src.node_checked(src_root)?;
  if let Some(parent) = dst_parent {
    dst.node_checked(parent)?;
  }

  let dst_root =
    clone_node_shallow_from_other_document(dst, src, src_root, dst_parent, semantics)?;
  if let Some(mapping) = mapping.as_mut() {
    mapping.push((src_root, dst_root));
  }
  if !deep {
    return Ok(dst_root);
  }

  struct Frame {
    src: NodeId,
    dst: NodeId,
    next_child: usize,
  }

  let mut stack: Vec<Frame> = vec![Frame {
    src: src_root,
    dst: dst_root,
    next_child: 0,
  }];

  while let Some(mut frame) = stack.pop() {
    let child_src = src.nodes[frame.src.index()]
      .children
      .get(frame.next_child)
      .copied();

    let Some(child_src) = child_src else {
      continue;
    };

    frame.next_child += 1;
    let parent_dst = frame.dst;
    stack.push(frame);

    let child_dst =
      clone_node_shallow_from_other_document(dst, src, child_src, Some(parent_dst), semantics)?;
    if let Some(mapping) = mapping.as_mut() {
      mapping.push((child_src, child_dst));
    }

    stack.push(Frame {
      src: child_src,
      dst: child_dst,
      next_child: 0,
    });
  }

  Ok(dst_root)
}

impl Document {
  /// Clone `node` from `src` into `self`, optionally cloning its descendant subtree.
  ///
  /// The returned node is always detached (`parent == None`) and belongs to `self`.
  ///
  /// Deep cloning is implemented iteratively to avoid recursion overflow on degenerate trees.
  pub fn clone_node_from(
    &mut self,
    src: &Document,
    node: NodeId,
    deep: bool,
  ) -> Result<NodeId, DomError> {
    clone_subtree_from_other_document(
      self,
      src,
      node,
      /* dst_parent */ None,
      deep,
      /* mapping */ None,
      CrossDocumentCloneSemantics::Clone,
    )
  }

  /// DOM `Document.importNode(node, deep)` equivalent for `dom2`.
  ///
  /// Imports (clones) `node` from a potentially different `src` `dom2::Document` into `self`.
  ///
  /// - The returned node is always detached (`parent == None`) and belongs to `self`.
  /// - If `deep` is `false`, only the node itself is cloned.
  /// - If `deep` is `true`, the full descendant subtree is cloned iteratively (no recursion).
  ///
  /// Note: importing a document node or a shadow root is not supported (mirrors the DOM Standard's
  /// `importNode`).
  pub fn import_node_from(
    &mut self,
    src: &Document,
    node: NodeId,
    deep: bool,
  ) -> Result<NodeId, DomError> {
    let src_node = src.node_checked(node)?;
    if node == src.root()
      || matches!(&src_node.kind, NodeKind::Document { .. })
      || matches!(&src_node.kind, NodeKind::ShadowRoot { .. })
    {
      return Err(DomError::NotSupportedError);
    }

    self.clone_node_from(src, node, deep)
  }

  /// Adopt (move) `node` from `src` into `self`.
  ///
  /// This clones the subtree into `self` and returns the new root plus a stable old→new mapping so
  /// embedding layers can preserve JS wrapper identity.
  ///
  /// The source subtree is detached in `src` by clearing parent pointers (dom2 has no deletion).
  pub fn adopt_node_from(
    &mut self,
    src: &mut Document,
    node: NodeId,
  ) -> Result<AdoptedSubtree, DomError> {
    let src_node = src.node_checked(node)?;
    if node == src.root() || matches!(&src_node.kind, NodeKind::Document { .. }) {
      return Err(DomError::NotSupportedError);
    }
    if matches!(&src_node.kind, NodeKind::ShadowRoot { .. }) {
      return Err(DomError::HierarchyRequestError);
    }

    // Detach the root using existing mutation APIs so mutation logs are recorded.
    if let Some(old_parent) = src.nodes[node.index()].parent {
      let _ = src.remove_child(old_parent, node)?;
    }

    let mut mapping: NodeIdMapping = Vec::new();
    let new_root = {
      let src_ref: &Document = &*src;
      clone_subtree_from_other_document(
        self,
        src_ref,
        node,
        /* dst_parent */ None,
        /* deep */ true,
        Some(&mut mapping),
        CrossDocumentCloneSemantics::Adopt,
      )?
    };

    // Leave the old nodes detached. We intentionally do not attempt to delete nodes from `src`
    // (dom2 has no deletion), but ensure the old subtree is no longer connected via parent pointers.
    for (old, _) in &mapping {
      src.nodes[old.index()].parent = None;
    }

    // `dom2::Document::adopt_node_from` approximates DOM adoption via subtree cloning. Since event
    // listeners are stored in a per-document registry keyed by `NodeId`, we must explicitly transfer
    // listeners from the old nodes into their cloned counterparts.
    src
      .events
      .transfer_node_listeners(&self.events, mapping.as_slice());

    Ok(AdoptedSubtree { new_root, mapping })
  }
}
