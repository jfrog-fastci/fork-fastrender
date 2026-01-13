use crate::dom::{is_valid_shadow_host_name, ShadowRootMode};

use super::{Attribute, Document, DomError, NodeId, NodeKind, SlotAssignmentMode, NULL_NAMESPACE};

fn node_is_element_like(kind: &NodeKind) -> bool {
  matches!(kind, NodeKind::Element { .. } | NodeKind::Slot { .. })
}

fn node_is_template_element(doc: &Document, kind: &NodeKind) -> bool {
  matches!(
    kind,
    NodeKind::Element {
      tag_name,
      namespace,
      ..
    } if tag_name.eq_ignore_ascii_case("template") && doc.is_html_case_insensitive_namespace(namespace)
  )
}

fn node_is_shadow_root(kind: &NodeKind) -> bool {
  matches!(kind, NodeKind::ShadowRoot { .. })
}

fn node_is_valid_shadow_host(doc: &Document, kind: &NodeKind) -> bool {
  match kind {
    NodeKind::Element {
      tag_name,
      namespace,
      ..
    } => doc.is_html_case_insensitive_namespace(namespace) && is_valid_shadow_host_name(tag_name),
    NodeKind::Slot { namespace, .. } => {
      doc.is_html_case_insensitive_namespace(namespace) && is_valid_shadow_host_name("slot")
    }
    _ => false,
  }
}

fn get_attribute<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a str> {
  attrs
    .iter()
    .find(|attr| attr.namespace == NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case(name))
    .map(|attr| attr.value.as_str())
}

fn has_attribute(attrs: &[Attribute], name: &str) -> bool {
  attrs
    .iter()
    .any(|attr| attr.namespace == NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case(name))
}

fn parse_shadow_root_definition(
  doc: &Document,
  kind: &NodeKind,
) -> Option<(ShadowRootMode, bool, bool, bool)> {
  let NodeKind::Element {
    tag_name,
    namespace,
    prefix: _,
    attributes,
  } = kind
  else {
    return None;
  };

  if !tag_name.eq_ignore_ascii_case("template") {
    return None;
  }
  // Declarative shadow DOM only applies to HTML templates, not e.g. SVG <template>.
  if !doc.is_html_case_insensitive_namespace(namespace) {
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
  let clonable = has_attribute(attributes, "shadowrootclonable");
  let serializable = has_attribute(attributes, "shadowrootserializable");

  Some((mode, delegates_focus, clonable, serializable))
}

fn promote_template_to_shadow_root(
  doc: &mut Document,
  host: NodeId,
  template: NodeId,
  template_idx: usize,
  mode: ShadowRootMode,
  delegates_focus: bool,
  clonable: bool,
  serializable: bool,
) {
  // Detach the template from the host.
  doc.node_iterator_pre_remove_steps(template);
  doc.live_mutation.pre_remove(template, host, template_idx);
  doc.live_range_pre_remove_steps(template, host, template_idx);
  doc.nodes[host.index()].children.remove(template_idx);
  // Record the removed node id before it becomes disconnected so hosts can map it back to the
  // previous renderer snapshot for damage tracking.
  doc.record_node_removed(template);
  doc.nodes[template.index()].parent = None;
  doc.record_child_list_mutation(host);

  // Create a new shadow root node (detached for now so we can insert at index 0).
  let shadow_root = doc.push_node(
    NodeKind::ShadowRoot {
      mode,
      delegates_focus,
      slot_assignment: SlotAssignmentMode::Named,
      clonable,
      serializable,
      declarative: true,
    },
    None,
    /* inert_subtree */ false,
  );

  // Move template children to shadow root.
  let template_children = doc.nodes[template.index()].children.clone();
  for (idx, &child) in template_children.iter().enumerate().rev() {
    doc.live_range_pre_remove_steps(child, template, idx);
  }
  for (idx, &child) in template_children.iter().enumerate() {
    if doc.nodes[child.index()].parent == Some(template) {
      doc.node_iterator_pre_remove_steps(child);
    }
    doc.live_mutation.pre_remove(child, template, idx);
  }
  let moved_children = std::mem::take(&mut doc.nodes[template.index()].children);
  for &child in &moved_children {
    doc.nodes[child.index()].parent = None;
  }
  if !moved_children.is_empty() {
    doc
      .live_mutation
      .pre_insert(shadow_root, 0, moved_children.len());
    doc.live_range_pre_insert_steps(
      shadow_root,
      doc.tree_child_index_from_raw_index_for_range(shadow_root, 0),
      doc.inserted_tree_children_count_for_range(shadow_root, &moved_children),
    );
  }
  for &child in &moved_children {
    doc.nodes[child.index()].parent = Some(shadow_root);
  }
  doc.nodes[shadow_root.index()].children = moved_children;

  // Attach shadow root to host at index 0.
  doc.live_mutation.pre_insert(host, 0, 1);
  doc.live_range_pre_insert_steps(
    host,
    doc.tree_child_index_from_raw_index_for_range(host, 0),
    doc.inserted_tree_children_count_for_range(host, &[shadow_root]),
  );
  doc.nodes[host.index()].children.insert(0, shadow_root);
  doc.nodes[shadow_root.index()].parent = Some(host);
  doc.record_node_inserted(shadow_root);
  doc.record_child_list_mutation(host);
  doc.bump_mutation_generation_classified();
}

impl Document {
  /// Imperative Shadow DOM: `Element.attachShadow(init)`.
  ///
  /// Creates and attaches a new `ShadowRoot` node as the first child of `host`.
  pub fn attach_shadow_root(
    &mut self,
    host: NodeId,
    mode: ShadowRootMode,
    clonable: bool,
    serializable: bool,
    delegates_focus: bool,
    slot_assignment: SlotAssignmentMode,
  ) -> Result<NodeId, DomError> {
    self.node_checked(host)?;

    let (tag_name, namespace) = match &self.node(host).kind {
      NodeKind::Element {
        tag_name,
        namespace,
        ..
      } => (tag_name.as_str(), namespace.as_str()),
      // `attachShadow()` is not permitted on `<slot>` elements.
      NodeKind::Slot { .. } => return Err(DomError::NotSupportedError),
      _ => return Err(DomError::InvalidNodeTypeError),
    };

    if !self.is_html_case_insensitive_namespace(namespace) || !is_valid_shadow_host_name(tag_name) {
      return Err(DomError::NotSupportedError);
    }

    if self.node(host).children.iter().any(|&child| {
      self.node(child).parent == Some(host)
        && matches!(self.node(child).kind, NodeKind::ShadowRoot { .. })
    }) {
      return Err(DomError::NotSupportedError);
    }

    let shadow_root = self.push_node(
      NodeKind::ShadowRoot {
        mode,
        delegates_focus,
        slot_assignment,
        clonable,
        serializable,
        declarative: false,
      },
      None,
      /* inert_subtree */ false,
    );

    let reference = self.node(host).children.first().copied();
    let _ = self.insert_before(host, shadow_root, reference)?;
    Ok(shadow_root)
  }

  /// Attach declarative shadow roots represented by `<template shadowroot=...>` elements.
  ///
  /// This mirrors `crate::dom::attach_shadow_roots` and must run as a post-processing step once
  /// HTML parsing is complete.
  pub(crate) fn attach_shadow_roots(&mut self) {
    if !self.is_html_document() {
      return;
    }
    // Run in post-order so nested declarative shadow roots are promoted before their ancestors.
    let mut stack: Vec<(NodeId, bool)> = Vec::new();
    stack.push((self.root, false));

    while let Some((id, after_children)) = stack.pop() {
      if !after_children {
        stack.push((id, true));
        let node = self.node(id);

        // Declarative shadow DOM only promotes the first shadow root template child of a shadow host
        // element. Additional `<template shadowroot=...>` siblings must remain inert, so we skip
        // traversing into them here.
        let first_declarative_shadow_template = if node_is_element_like(&node.kind)
          && !node_is_template_element(self, &node.kind)
          && node_is_valid_shadow_host(self, &node.kind)
          && !node.children.iter().any(|&child_id| {
            self.node(child_id).parent == Some(id) && node_is_shadow_root(&self.node(child_id).kind)
          }) {
          node.children.iter().position(|&child_id| {
            self.node(child_id).parent == Some(id)
              && parse_shadow_root_definition(self, &self.node(child_id).kind).is_some()
          })
        } else {
          None
        };

        // Push children in reverse so we traverse in tree order.
        for idx in (0..node.children.len()).rev() {
          let child_id = node.children[idx];
          if self.node(child_id).parent != Some(id) {
            continue;
          }
          let child_kind = &self.node(child_id).kind;

          // Template contents are inert; only the first declarative shadow DOM template is walked so
          // nested declarative shadow roots inside it can be promoted.
          if node_is_template_element(self, child_kind)
            && first_declarative_shadow_template != Some(idx)
          {
            continue;
          }

          stack.push((child_id, false));
        }

        continue;
      }

      let shadow_template = {
        let node = self.node(id);
        if !node_is_element_like(&node.kind)
          || node_is_template_element(self, &node.kind)
          || !node_is_valid_shadow_host(self, &node.kind)
        {
          None
        } else if node.children.iter().any(|&child_id| {
          self.node(child_id).parent == Some(id) && node_is_shadow_root(&self.node(child_id).kind)
        }) {
          None
        } else {
          node
            .children
            .iter()
            .enumerate()
            .find_map(|(idx, &child_id)| {
              if self.node(child_id).parent != Some(id) {
                return None;
              }
              let child_kind = &self.node(child_id).kind;
              parse_shadow_root_definition(self, child_kind).map(
                |(mode, delegates_focus, clonable, serializable)| {
                  (idx, child_id, mode, delegates_focus, clonable, serializable)
                },
              )
            })
        }
      };

      let Some((template_idx, template_id, mode, delegates_focus, clonable, serializable)) =
        shadow_template
      else {
        continue;
      };

      promote_template_to_shadow_root(
        self,
        id,
        template_id,
        template_idx,
        mode,
        delegates_focus,
        clonable,
        serializable,
      );
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::debug::snapshot::snapshot_dom;
  use crate::dom2::live_mutation::{LiveMutationEvent, LiveMutationTestRecorder};
  use selectors::context::QuirksMode;

  fn node_id_attribute(kind: &NodeKind) -> Option<&str> {
    match kind {
      NodeKind::Element { attributes, .. } | NodeKind::Slot { attributes, .. } => attributes
        .iter()
        .find(|attr| attr.namespace == NULL_NAMESPACE && attr.local_name.eq_ignore_ascii_case("id"))
        .map(|attr| attr.value.as_str()),
      _ => None,
    }
  }

  fn find_node_by_id(doc: &Document, id: &str) -> Option<NodeId> {
    doc
      .nodes()
      .iter()
      .enumerate()
      .find_map(|(idx, node)| (node_id_attribute(&node.kind) == Some(id)).then_some(NodeId(idx)))
  }

  #[test]
  fn attach_shadow_roots_promotes_only_first_template_and_supports_nested_dsd() {
    let html = concat!(
      "<!doctype html>",
      "<div id=host>",
      "<template id=outer1 shadowroot=open>",
      "<div id=inner>",
      "<template id=inner_tpl shadowroot=closed><span>nested</span></template>",
      "<b>light</b>",
      "</div>",
      "</template>",
      "<template id=outer2 shadowroot=open>",
      "<div id=inert_inner>",
      "<template id=inert_inner_tpl shadowroot=open><i>ignore</i></template>",
      "</div>",
      "</template>",
      "<p>light</p>",
      "</div>"
    );

    let expected = crate::dom::parse_html(html).unwrap();
    let doc = crate::dom2::parse_html(html).unwrap();

    let host = find_node_by_id(&doc, "host").expect("host element not found");
    let host_children = doc.node(host).children.clone();
    assert!(
      matches!(
        doc.node(host_children[0]).kind,
        NodeKind::ShadowRoot {
          mode: ShadowRootMode::Open,
          delegates_focus: false,
          slot_assignment: SlotAssignmentMode::Named,
          ..
        }
      ),
      "host should have an attached open shadow root at index 0"
    );

    let outer1 = find_node_by_id(&doc, "outer1").expect("outer1 template not found");
    assert_eq!(
      doc.node(outer1).parent,
      None,
      "promoted shadowroot template should be detached"
    );
    assert!(
      doc.node(outer1).children.is_empty(),
      "promoted template should have its children moved out"
    );

    let outer2 = find_node_by_id(&doc, "outer2").expect("outer2 template not found");
    assert_eq!(
      doc.node(outer2).parent,
      Some(host),
      "subsequent shadowroot templates must remain in the light DOM"
    );

    let inner = find_node_by_id(&doc, "inner").expect("inner host not found");
    let inner_children = doc.node(inner).children.clone();
    assert!(
      matches!(
        doc.node(inner_children[0]).kind,
        NodeKind::ShadowRoot {
          mode: ShadowRootMode::Closed,
          delegates_focus: false,
          slot_assignment: SlotAssignmentMode::Named,
          ..
        }
      ),
      "nested shadow root should be promoted within the first template contents"
    );

    let inner_tpl = find_node_by_id(&doc, "inner_tpl").expect("inner_tpl template not found");
    assert_eq!(
      doc.node(inner_tpl).parent,
      None,
      "nested promoted template should be detached"
    );
    assert!(
      doc.node(inner_tpl).children.is_empty(),
      "nested promoted template should have its children moved out"
    );

    // The second template subtree remains inert; nested declarative shadow roots inside it must not
    // be promoted.
    let inert_inner = find_node_by_id(&doc, "inert_inner").expect("inert_inner host not found");
    let inert_inner_children = doc.node(inert_inner).children.clone();
    assert!(
      inert_inner_children
        .iter()
        .all(|&child| !matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. })),
      "inert template subtree should not have shadow roots attached"
    );
    assert!(
      inert_inner_children
        .iter()
        .any(|&child| node_is_template_element(&doc, &doc.node(child).kind)),
      "inert template subtree should retain the declarative shadow root template element"
    );

    let roundtrip = doc.to_renderer_dom();
    assert_eq!(
      snapshot_dom(&expected),
      snapshot_dom(&roundtrip),
      "dom2 shadow attachment should match crate::dom::parse_html snapshot"
    );
  }

  #[test]
  fn attach_shadow_roots_emits_live_mutation_hooks() {
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let recorder = LiveMutationTestRecorder::default();
    doc.set_live_mutation_hook(Some(Box::new(recorder.clone())));

    let root = doc.root();
    let host = doc.push_node(
      NodeKind::Element {
        tag_name: "div".to_string(),
        namespace: "".to_string(),
        prefix: None,
        attributes: vec![Attribute::new_no_namespace("id", "host")],
      },
      Some(root),
      /* inert_subtree */ false,
    );
    let template = doc.push_node(
      NodeKind::Element {
        tag_name: "template".to_string(),
        namespace: "".to_string(),
        prefix: None,
        attributes: vec![Attribute::new_no_namespace("shadowroot", "open")],
      },
      Some(host),
      /* inert_subtree */ false,
    );
    let span = doc.push_node(
      NodeKind::Element {
        tag_name: "span".to_string(),
        namespace: "".to_string(),
        prefix: None,
        attributes: vec![Attribute::new_no_namespace("id", "shadow")],
      },
      Some(template),
      /* inert_subtree */ false,
    );

    let _ = recorder.take();
    doc.attach_shadow_roots();

    let shadow_root = doc.node(host).children[0];
    assert!(matches!(
      doc.node(shadow_root).kind,
      NodeKind::ShadowRoot { .. }
    ));
    assert_eq!(doc.node(span).parent, Some(shadow_root));

    assert_eq!(
      recorder.take(),
      vec![
        LiveMutationEvent::PreRemove {
          node: template,
          old_parent: host,
          old_index: 0
        },
        LiveMutationEvent::PreRemove {
          node: span,
          old_parent: template,
          old_index: 0
        },
        LiveMutationEvent::PreInsert {
          parent: shadow_root,
          index: 0,
          count: 1
        },
        LiveMutationEvent::PreInsert {
          parent: host,
          index: 0,
          count: 1
        },
      ]
    );
  }

  #[test]
  fn attach_shadow_roots_records_structural_mutation_log() {
    let mut doc = Document::new(QuirksMode::NoQuirks);

    let root = doc.root();
    let host = doc.push_node(
      NodeKind::Element {
        tag_name: "div".to_string(),
        namespace: "".to_string(),
        prefix: None,
        attributes: vec![Attribute::new_no_namespace("id", "host")],
      },
      Some(root),
      /* inert_subtree */ false,
    );
    let template = doc.push_node(
      NodeKind::Element {
        tag_name: "template".to_string(),
        namespace: "".to_string(),
        prefix: None,
        attributes: vec![Attribute::new_no_namespace("shadowroot", "open")],
      },
      Some(host),
      /* inert_subtree */ false,
    );
    let _span = doc.push_node(
      NodeKind::Element {
        tag_name: "span".to_string(),
        namespace: "".to_string(),
        prefix: None,
        attributes: vec![Attribute::new_no_namespace("id", "shadow")],
      },
      Some(template),
      /* inert_subtree */ false,
    );

    let _ = doc.take_mutations();
    doc.attach_shadow_roots();

    let shadow_root = doc.node(host).children[0];
    let mutations = doc.take_mutations();
    assert!(
      mutations.nodes_removed.contains(&template),
      "expected promoted template to be recorded as removed"
    );
    assert!(
      mutations.nodes_inserted.contains(&shadow_root),
      "expected new shadow root to be recorded as inserted"
    );
    assert!(
      mutations.child_list_changed.contains(&host),
      "expected host child list to be recorded as changed"
    );
    assert!(
      mutations.nodes_moved.is_empty(),
      "expected declarative shadow root promotion to be tracked as remove+insert, not move"
    );
  }

  #[test]
  fn attach_shadow_roots_requires_valid_shadow_host_name() {
    let invalid =
      "<!doctype html><a id=host><template shadowroot=open><span>shadow</span></template></a>";
    let expected = crate::dom::parse_html(invalid).unwrap();
    let doc = crate::dom2::parse_html(invalid).unwrap();
    let host = find_node_by_id(&doc, "host").expect("host element not found");
    let host_children = doc.node(host).children.clone();
    assert!(
      host_children
        .iter()
        .all(|&child| !matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. })),
      "invalid shadow host names must not have declarative shadow roots attached"
    );
    assert!(
      host_children
        .iter()
        .any(|&child| node_is_template_element(&doc, &doc.node(child).kind)),
      "declarative shadow templates on invalid hosts should remain in the light DOM"
    );
    let roundtrip = doc.to_renderer_dom();
    assert_eq!(
      snapshot_dom(&expected),
      snapshot_dom(&roundtrip),
      "dom2 shadow attachment should match crate::dom::parse_html snapshot"
    );

    let custom_element_host = "<!doctype html><x-host id=host><template shadowroot=open><span>shadow</span></template></x-host>";
    let expected = crate::dom::parse_html(custom_element_host).unwrap();
    let doc = crate::dom2::parse_html(custom_element_host).unwrap();
    let host = find_node_by_id(&doc, "host").expect("custom element host not found");
    let host_children = doc.node(host).children.clone();
    assert!(
      matches!(doc.node(host_children[0]).kind, NodeKind::ShadowRoot { .. }),
      "valid custom element names should be treated as valid shadow hosts"
    );
    assert!(
      host_children
        .iter()
        .all(|&child| !node_is_template_element(&doc, &doc.node(child).kind)),
      "shadowroot template should be promoted to a shadow root on valid hosts"
    );
    let roundtrip = doc.to_renderer_dom();
    assert_eq!(
      snapshot_dom(&expected),
      snapshot_dom(&roundtrip),
      "dom2 shadow attachment should match crate::dom::parse_html snapshot"
    );
  }

  #[test]
  fn attach_shadow_roots_ignores_invalid_shadow_hosts_and_matches_legacy_snapshot() {
    let html = concat!(
      "<!doctype html>",
      "<select id=host>",
      "<template shadowroot=open><div id=shadow></div></template>",
      "<option id=light>Light</option>",
      "</select>",
    );

    let expected = crate::dom::parse_html(html).unwrap();
    let doc = crate::dom2::parse_html(html).unwrap();

    let host = find_node_by_id(&doc, "host").expect("host element not found");
    assert!(
      doc
        .node(host)
        .children
        .iter()
        .all(|&child| !matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. })),
      "invalid shadow hosts must not have shadow roots attached"
    );

    let roundtrip = doc.to_renderer_dom();
    assert_eq!(
      snapshot_dom(&expected),
      snapshot_dom(&roundtrip),
      "dom2 shadow attachment should match crate::dom::parse_html snapshot"
    );
  }

  #[test]
  fn attach_shadow_roots_ignores_invalid_shadow_hosts_with_shadowrootmode_and_matches_legacy_snapshot(
  ) {
    let html = concat!(
      "<!doctype html>",
      "<select id=host>",
      "<template shadowrootmode=open><div id=shadow></div></template>",
      "<option id=light>Light</option>",
      "</select>",
    );

    let expected = crate::dom::parse_html(html).unwrap();
    let doc = crate::dom2::parse_html(html).unwrap();

    let host = find_node_by_id(&doc, "host").expect("host element not found");
    assert!(
      doc
        .node(host)
        .children
        .iter()
        .all(|&child| !matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. })),
      "invalid shadow hosts must not have shadow roots attached"
    );

    let roundtrip = doc.to_renderer_dom();
    assert_eq!(
      snapshot_dom(&expected),
      snapshot_dom(&roundtrip),
      "dom2 shadow attachment should match crate::dom::parse_html snapshot"
    );
  }

  #[test]
  fn attach_shadow_roots_ignores_detached_shadow_templates() {
    // Declarative shadow DOM should only consider children that are actually connected to their
    // parent via the `parent` pointer. If a tree is partially detached (stale entry in `children`
    // list), promotion must not treat it as a live shadow root template.
    let mut doc = Document::new(QuirksMode::NoQuirks);
    let root = doc.root();

    let host = doc.push_node(
      NodeKind::Element {
        tag_name: "div".to_string(),
        namespace: "".to_string(),
        prefix: None,
        attributes: vec![Attribute::new_no_namespace("id", "host")],
      },
      Some(root),
      /* inert_subtree */ false,
    );
    let template = doc.push_node(
      NodeKind::Element {
        tag_name: "template".to_string(),
        namespace: "".to_string(),
        prefix: None,
        attributes: vec![Attribute::new_no_namespace("shadowroot", "open")],
      },
      Some(host),
      /* inert_subtree */ false,
    );
    let span = doc.push_node(
      NodeKind::Element {
        tag_name: "span".to_string(),
        namespace: "".to_string(),
        prefix: None,
        attributes: Vec::new(),
      },
      Some(template),
      /* inert_subtree */ false,
    );
    doc.push_node(
      NodeKind::Text {
        content: "shadow".to_string(),
      },
      Some(span),
      /* inert_subtree */ false,
    );

    // Detach the template by severing the parent pointer, but leave it in the host's children list.
    doc.node_mut(template).parent = None;

    doc.attach_shadow_roots();

    assert!(
      doc
        .node(host)
        .children
        .iter()
        .all(|&child| !matches!(doc.node(child).kind, NodeKind::ShadowRoot { .. })),
      "detached shadowroot template must not be promoted"
    );
    assert!(
      doc.node(host).children.contains(&template),
      "detached template should remain untouched"
    );
  }
}
