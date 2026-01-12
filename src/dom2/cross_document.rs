use crate::dom::HTML_NAMESPACE;

use super::{Document, DomError, NodeId, NodeKind};

#[derive(Debug, Clone)]
pub struct AdoptedSubtree {
  pub new_root: NodeId,
  pub mapping: Vec<(NodeId, NodeId)>,
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
    let is_html_script = matches!(
      &node.kind,
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } if tag_name.eq_ignore_ascii_case("script")
        && (namespace.is_empty() || namespace == HTML_NAMESPACE)
    );
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
        debug_assert!(
          parent.is_none(),
          "Document nodes cannot have a parent; cloning must only produce detached roots"
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
      } => NodeKind::ShadowRoot {
        mode: *mode,
        delegates_focus: *delegates_focus,
      },
      NodeKind::Slot {
        namespace,
        attributes,
        ..
      } => NodeKind::Slot {
        namespace: namespace.clone(),
        attributes: attributes.clone(),
        // Slot assignment is derived state; imported clones start detached.
        assigned: false,
      },
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

  let dst_id = dst.push_node(kind, parent, inert_subtree);

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
      dst.input_states[dst_id.index()] = src.input_states[src_id.index()].clone();
      dst.textarea_states[dst_id.index()] = src.textarea_states[src_id.index()].clone();
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
  mut mapping: Option<&mut Vec<(NodeId, NodeId)>>,
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

    let mut mapping: Vec<(NodeId, NodeId)> = Vec::new();
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

    Ok(AdoptedSubtree { new_root, mapping })
  }
}

