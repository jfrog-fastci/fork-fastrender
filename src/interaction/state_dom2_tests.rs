#![cfg(test)]

use crate::interaction::selection_serialize::{
  DocumentSelectionPoint, DocumentSelectionPointDom2, DocumentSelectionRange,
  DocumentSelectionRangeDom2,
};
use crate::interaction::state::InteractionStateDom2;
use crate::interaction::state::{
  DocumentSelectionRanges, DocumentSelectionRangesDom2, DocumentSelectionState,
  DocumentSelectionStateDom2, FileSelection, FormStateDom2, ImePreeditStateDom2,
  TextEditPaintStateDom2,
};
use crate::{dom::ShadowRootMode, dom2::SlotAssignmentMode};
use crate::text::caret::CaretAffinity;
use rustc_hash::FxHashSet;
use std::path::PathBuf;

#[test]
fn document_selection_dom2_from_preorder_tracks_dom_mutations() {
  let html = "<!doctype html><html><body><div id=a>hello</div></body></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();

  let div = doc.get_element_by_id("a").expect("div");
  let text = doc.node(div).children[0];

  let snapshot1 = doc.to_renderer_dom_with_mapping();
  let preorder_1 = snapshot1.mapping.preorder_for_node_id(text).unwrap();

  let start_pre = DocumentSelectionPoint {
    node_id: preorder_1,
    char_offset: 1,
  };
  let end_pre = DocumentSelectionPoint {
    node_id: preorder_1,
    char_offset: 4,
  };
  let selection_preorder = DocumentSelectionState::Ranges(DocumentSelectionRanges {
    ranges: vec![DocumentSelectionRange {
      start: start_pre,
      end: end_pre,
    }],
    primary: 0,
    anchor: start_pre,
    focus: end_pre,
  });
 
  let selection_dom2 =
    DocumentSelectionStateDom2::from_preorder(&selection_preorder, &doc, &snapshot1.mapping)
      .expect("convert selection to dom2");
  let DocumentSelectionStateDom2::Ranges(ranges_dom2) = &selection_dom2 else {
    panic!("expected dom2 selection to be a range");
  };
  assert_eq!(ranges_dom2.ranges.len(), 1);
  assert_eq!(ranges_dom2.ranges[0].start.node_id, text);
  assert_eq!(ranges_dom2.ranges[0].end.node_id, text);

  // Insert a new earlier sibling before the selected text node.
  let new_text = doc.create_text("X");
  assert!(doc.insert_before(div, new_text, Some(text)).unwrap());

  let snapshot2 = doc.to_renderer_dom_with_mapping();
  let preorder_2 = snapshot2.mapping.preorder_for_node_id(text).unwrap();
  assert_ne!(preorder_1, preorder_2);

  let projected = selection_dom2.project_to_preorder(&snapshot2.mapping);
  let DocumentSelectionState::Ranges(projected) = projected else {
    panic!("expected projected selection to be a range");
  };
  assert_eq!(projected.ranges.len(), 1);
  assert_eq!(projected.ranges[0].start.node_id, preorder_2);
  assert_eq!(projected.ranges[0].end.node_id, preorder_2);
  assert_eq!(projected.ranges[0].start.char_offset, 1);
  assert_eq!(projected.ranges[0].end.char_offset, 4);
}

#[test]
fn interaction_state_dom2_projection_projects_document_selection_and_updates_on_dom_mutation() {
  let html = "<!doctype html><html><body><div id=a>hello</div></body></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();

  let div = doc.get_element_by_id("a").expect("div");
  let text = doc.node(div).children[0];
  let selection = DocumentSelectionStateDom2::Ranges(DocumentSelectionRangesDom2 {
    ranges: vec![DocumentSelectionRangeDom2 {
      start: DocumentSelectionPointDom2 {
        node_id: text,
        char_offset: 1,
      },
      end: DocumentSelectionPointDom2 {
        node_id: text,
        char_offset: 4,
      },
    }],
    primary: 0,
    anchor: DocumentSelectionPointDom2 {
      node_id: text,
      char_offset: 1,
    },
    focus: DocumentSelectionPointDom2 {
      node_id: text,
      char_offset: 4,
    },
  });

  let mut state_dom2 = InteractionStateDom2 {
    document_selection: Some(selection),
    ..Default::default()
  };

  let snapshot1 = doc.to_renderer_dom_with_mapping();
  let preorder_1 = snapshot1.mapping.preorder_for_node_id(text).unwrap();
  let projected_1 = state_dom2.project_to_preorder(&snapshot1.mapping);
  let DocumentSelectionState::Ranges(ranges_1) = projected_1
    .document_selection
    .as_ref()
    .expect("projected selection")
  else {
    panic!("expected projected selection to be a range");
  };
  assert_eq!(ranges_1.ranges.len(), 1);
  assert_eq!(ranges_1.ranges[0].start.node_id, preorder_1);
  assert_eq!(ranges_1.ranges[0].end.node_id, preorder_1);
  assert_eq!(ranges_1.ranges[0].start.char_offset, 1);
  assert_eq!(ranges_1.ranges[0].end.char_offset, 4);

  // Insert a new earlier sibling (before the selected text node) so preorder ids shift.
  let new_text = doc.create_text("X");
  assert!(doc.insert_before(div, new_text, Some(text)).unwrap());

  let snapshot2 = doc.to_renderer_dom_with_mapping();
  let preorder_2 = snapshot2.mapping.preorder_for_node_id(text).unwrap();
  assert_ne!(preorder_1, preorder_2);

  let projected_2 = state_dom2.project_to_preorder(&snapshot2.mapping);
  let DocumentSelectionState::Ranges(ranges_2) = projected_2
    .document_selection
    .as_ref()
    .expect("projected selection")
  else {
    panic!("expected projected selection to be a range");
  };
  assert_eq!(ranges_2.ranges.len(), 1);
  assert_eq!(ranges_2.ranges[0].start.node_id, preorder_2);
  assert_eq!(ranges_2.ranges[0].end.node_id, preorder_2);
  assert_eq!(ranges_2.ranges[0].start.char_offset, 1);
  assert_eq!(ranges_2.ranges[0].end.char_offset, 4);

  assert_eq!(
    snapshot2.mapping.node_id_for_preorder(preorder_2),
    Some(text)
  );
}

#[test]
fn interaction_state_dom2_projection_prunes_detached_document_selection() {
  let html = "<!doctype html><html><body><div id=a>hello</div></body></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();

  let div = doc.get_element_by_id("a").expect("div");
  let text = doc.node(div).children[0];

  let selection = DocumentSelectionStateDom2::Ranges(DocumentSelectionRangesDom2 {
    ranges: vec![DocumentSelectionRangeDom2 {
      start: DocumentSelectionPointDom2 {
        node_id: text,
        char_offset: 1,
      },
      end: DocumentSelectionPointDom2 {
        node_id: text,
        char_offset: 4,
      },
    }],
    primary: 0,
    anchor: DocumentSelectionPointDom2 {
      node_id: text,
      char_offset: 1,
    },
    focus: DocumentSelectionPointDom2 {
      node_id: text,
      char_offset: 4,
    },
  });

  let mut state_dom2 = InteractionStateDom2 {
    document_selection: Some(selection),
    ..Default::default()
  };

  let snapshot1 = doc.to_renderer_dom_with_mapping();
  let projected_1 = state_dom2.project_to_preorder(&snapshot1.mapping);
  assert!(
    projected_1.document_selection.is_some(),
    "expected selection to project while node is connected"
  );

  // Detach the selected text node.
  assert!(doc.remove_child(div, text).unwrap());

  let snapshot2 = doc.to_renderer_dom_with_mapping();
  let projected_2 = state_dom2.project_to_preorder(&snapshot2.mapping);
  assert!(
    projected_2.document_selection.is_none(),
    "expected selection to be cleared when node becomes detached"
  );
  assert!(
    state_dom2.document_selection.is_none(),
    "expected stable dom2 selection to be pruned when node becomes detached"
  );
}

#[test]
fn document_selection_contains_point_dom2_returns_false_for_unmappable_points() {
  use crate::interaction::state::document_selection_contains_point_dom2;

  let html = "<!doctype html><html><body><div id=a>hello</div></body></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();

  // Create a detached text node and use it for a bogus selection.
  let detached = doc.create_text("detached");
  assert!(
    !doc.is_connected(detached),
    "newly-created node should be disconnected until inserted"
  );

  let selection = DocumentSelectionStateDom2::Ranges(DocumentSelectionRangesDom2 {
    ranges: vec![DocumentSelectionRangeDom2 {
      start: DocumentSelectionPointDom2 {
        node_id: detached,
        char_offset: 0,
      },
      end: DocumentSelectionPointDom2 {
        node_id: detached,
        char_offset: 1,
      },
    }],
    primary: 0,
    anchor: DocumentSelectionPointDom2 {
      node_id: detached,
      char_offset: 0,
    },
    focus: DocumentSelectionPointDom2 {
      node_id: detached,
      char_offset: 1,
    },
  });

  let snapshot = doc.to_renderer_dom_with_mapping();
  assert!(
    snapshot.mapping.preorder_for_node_id(detached).is_none(),
    "detached nodes should not map to renderer preorder ids"
  );

  assert!(
    !document_selection_contains_point_dom2(
      &selection,
      DocumentSelectionPointDom2 {
        node_id: detached,
        char_offset: 0,
      },
      &snapshot.mapping
    ),
    "unmappable points must not be treated as inside selection highlights"
  );
}

#[test]
fn interaction_state_dom2_projection_updates_preorder_ids_after_dom_mutation() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=a></div>",
    "<div id=b></div>",
    "</body></html>",
  );

  let mut doc = crate::dom2::parse_html(html).unwrap();
  let focused = doc.get_element_by_id("b").expect("missing #b element");

  let snapshot1 = doc.to_renderer_dom_with_mapping();
  let focused_preorder_1 = snapshot1
    .mapping
    .preorder_for_node_id(focused)
    .expect("focused node should be connected");

  let mut state_dom2 = InteractionStateDom2 {
    focused: Some(focused),
    focus_visible: true,
    focus_chain: vec![focused],
    ..Default::default()
  };

  let projected_1 = state_dom2.project_to_preorder(&snapshot1.mapping);
  assert_eq!(projected_1.focused, Some(focused_preorder_1));
  assert_eq!(projected_1.focus_chain(), &[focused_preorder_1]);
  assert!(projected_1.is_focus_within(focused_preorder_1));

  // Mutate the DOM by inserting a new sibling immediately before the focused element; this should
  // shift renderer preorder ids while leaving the focused NodeId stable.
  let parent = doc.parent(focused).unwrap().unwrap();
  let inserted = doc.create_element("div", "");
  assert!(doc.insert_before(parent, inserted, Some(focused)).unwrap());

  let snapshot2 = doc.to_renderer_dom_with_mapping();
  let focused_preorder_2 = snapshot2
    .mapping
    .preorder_for_node_id(focused)
    .expect("focused node should still be connected");
  assert_ne!(
    focused_preorder_1, focused_preorder_2,
    "expected DOM insertion to shift preorder ids"
  );

  assert_eq!(state_dom2.focused, Some(focused));
  let projected_2 = state_dom2.project_to_preorder(&snapshot2.mapping);
  assert_eq!(projected_2.focused, Some(focused_preorder_2));
  assert_eq!(projected_2.focus_chain(), &[focused_preorder_2]);
  assert!(projected_2.is_focus_within(focused_preorder_2));
}

#[test]
fn interaction_state_dom2_projection_survives_preorder_shifts_and_drops_detached_targets() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<div id=before></div>",
    "<div id=target></div>",
    "</body></html>",
  );
  let mut doc = crate::dom2::parse_html(html).unwrap();
  let target = doc.get_element_by_id("target").expect("missing #target");

  let mut state_dom2 = InteractionStateDom2::default();
  state_dom2.focused = Some(target);
  state_dom2.focus_visible = true;
  state_dom2.focus_chain = vec![target];
  state_dom2.hover_chain = vec![target];
  state_dom2.visited_links.insert(target);

  let snapshot1 = doc.to_renderer_dom_with_mapping();
  let projected1 = state_dom2.project_to_preorder(&snapshot1.mapping);
  let preorder_1 = projected1.focused.expect("focused preorder id");
  assert!(preorder_1 > 0, "preorder ids should be 1-based");
  assert_eq!(
    snapshot1.mapping.node_id_for_preorder(preorder_1),
    Some(target),
    "projected focus preorder id should map back to the original NodeId"
  );
  assert!(projected1.is_hovered(preorder_1));
  assert!(projected1.is_visited_link(preorder_1));

  // Insert a new sibling before the target so renderer preorder ids shift.
  let parent = doc.parent(target).unwrap().unwrap();
  let inserted = doc.create_element("div", "");
  assert!(doc.insert_before(parent, inserted, Some(target)).unwrap());

  let snapshot2 = doc.to_renderer_dom_with_mapping();
  let projected2 = state_dom2.project_to_preorder(&snapshot2.mapping);
  let preorder_2 = projected2.focused.expect("focused preorder id after insertion");
  assert_ne!(
    preorder_1, preorder_2,
    "expected the focused preorder id to change after inserting a prior sibling"
  );
  assert_eq!(
    snapshot2.mapping.node_id_for_preorder(preorder_2),
    Some(target),
    "new focused preorder id should still refer to the same stable NodeId"
  );
  assert!(projected2.is_hovered(preorder_2));
  assert!(projected2.is_visited_link(preorder_2));

  // Detach the target; projection should drop focus/hover/visited state for it.
  assert!(doc.remove_child(parent, target).unwrap());
  let snapshot3 = doc.to_renderer_dom_with_mapping();
  let projected3 = state_dom2.project_to_preorder(&snapshot3.mapping);
  assert_eq!(projected3.focused, None);
  assert!(projected3.hover_chain().is_empty());
  assert!(projected3.visited_links().is_empty());
}

#[test]
fn interaction_state_dom2_projection_clears_unmappable_focus() {
  let html = "<!doctype html><html><body><div id=a></div></body></html>";
  let mut doc = crate::dom2::parse_html(html).unwrap();

  // Create a detached element and treat it as focused.
  let detached = doc.create_element("div", "");
  let mut state_dom2 = InteractionStateDom2 {
    focused: Some(detached),
    focus_visible: true,
    focus_chain: vec![detached],
    ..Default::default()
  };

  let snapshot = doc.to_renderer_dom_with_mapping();
  assert_eq!(
    snapshot.mapping.preorder_for_node_id(detached),
    None,
    "detached nodes should not map to renderer preorder ids"
  );

  let projected = state_dom2.project_to_preorder(&snapshot.mapping);
  assert_eq!(projected.focused, None);
  assert!(
    projected.focus_chain().is_empty(),
    "focus_chain should be cleared when focused node is unmappable"
  );
  assert!(
    !projected.focus_visible,
    "focus_visible should be cleared when no focused element is projected"
  );

  assert_eq!(
    state_dom2.focused, None,
    "pruning should clear stable focus state when the target node is detached"
  );
  assert!(state_dom2.focus_chain.is_empty());
  assert!(!state_dom2.focus_visible);
}

#[test]
fn interaction_state_dom2_prunes_detached_focus_hover_active_and_text_state() {
  let mut doc = crate::dom2::parse_html("<!doctype html><html><body><input id=a></body></html>")
    .expect("parse html");
  let input = doc.get_element_by_id("a").expect("missing input");

  let mut state_dom2 = InteractionStateDom2::default();
  state_dom2.focused = Some(input);
  state_dom2.focus_visible = true;
  state_dom2.focus_chain = vec![input];
  state_dom2.hover_chain = vec![input];
  state_dom2.active_chain = vec![input];
  state_dom2.visited_links.insert(input);
  state_dom2.user_validity.insert(input);
  state_dom2.ime_preedit = Some(ImePreeditStateDom2 {
    node_id: input,
    text: "あ".to_string(),
    cursor: Some((0, 1)),
  });
  state_dom2.text_edit = Some(TextEditPaintStateDom2 {
    node_id: input,
    caret: 0,
    caret_affinity: CaretAffinity::Downstream,
    selection: None,
  });

  let body = doc.body().expect("missing body");
  assert!(doc.remove_child(body, input).unwrap());

  let snapshot = doc.to_renderer_dom_with_mapping();
  let projected = state_dom2.project_to_preorder(&snapshot.mapping);
  assert_eq!(projected.focused, None);
  assert!(!projected.focus_visible);
  assert!(projected.focus_chain().is_empty());
  assert!(projected.hover_chain().is_empty());
  assert!(projected.active_chain().is_empty());
  assert!(projected.ime_preedit.is_none());
  assert!(projected.text_edit.is_none());

  // The stable dom2 state should also be repaired.
  assert_eq!(state_dom2.focused, None);
  assert!(!state_dom2.focus_visible);
  assert!(state_dom2.focus_chain.is_empty());
  assert!(state_dom2.hover_chain.is_empty());
  assert!(state_dom2.active_chain.is_empty());
  assert!(state_dom2.ime_preedit.is_none());
  assert!(state_dom2.text_edit.is_none());
  assert!(!state_dom2.visited_links.contains(&input));
  assert!(!state_dom2.user_validity.contains(&input));
}

#[test]
fn form_state_dom2_projection_maps_file_inputs_and_select_selected() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<input id=f type=file>",
    "<select id=s><option id=o1>one</option><option id=o2>two</option></select>",
    "</body></html>",
  );

  let doc = crate::dom2::parse_html(html).unwrap();
  let file_input = doc.get_element_by_id("f").expect("missing #f");
  let select = doc.get_element_by_id("s").expect("missing #s");
  let option1 = doc.get_element_by_id("o1").expect("missing #o1");

  let mut form_state = FormStateDom2::default();
  form_state.file_inputs.insert(
    file_input,
    vec![FileSelection {
      path: PathBuf::from("/tmp/example.txt"),
      filename: "example.txt".to_string(),
      content_type: "text/plain".to_string(),
      bytes: vec![1, 2, 3],
    }],
  );
  form_state
    .select_selected
    .insert(select, FxHashSet::from_iter([option1]));

  let snapshot = doc.to_renderer_dom_with_mapping();
  let projected = form_state.project_to_preorder(&snapshot.mapping);

  let file_input_preorder = snapshot
    .mapping
    .preorder_for_node_id(file_input)
    .expect("file input should be connected");
  let select_preorder = snapshot
    .mapping
    .preorder_for_node_id(select)
    .expect("select should be connected");
  let option1_preorder = snapshot
    .mapping
    .preorder_for_node_id(option1)
    .expect("option should be connected");

  assert!(
    projected.file_inputs.contains_key(&file_input_preorder),
    "expected projected file input entry"
  );
  assert_eq!(
    projected.file_inputs.get(&file_input_preorder).unwrap()[0].filename,
    "example.txt"
  );

  assert!(
    projected.select_selected.contains_key(&select_preorder),
    "expected projected select selection entry"
  );
  assert!(
    projected
      .select_selected
      .get(&select_preorder)
      .unwrap()
      .contains(&option1_preorder),
    "expected projected select selection to contain option preorder id"
  );
}

#[test]
fn interaction_state_dom2_prune_disconnected_clears_state_for_inert_template_contents() {
  let html = concat!(
    "<!doctype html>",
    "<html><body>",
    "<template id=t><input id=a></template>",
    "</body></html>",
  );
  let doc = crate::dom2::parse_html(html).unwrap();

  let template = doc.get_element_by_id("t").expect("missing template");
  let input = doc
    .subtree_preorder(template)
    .find(|&id| doc.get_attribute(id, "id").unwrap() == Some("a"))
    .expect("missing input inside template");

  assert!(
    !doc.is_connected_for_scripting(input),
    "nodes inside inert <template> contents must be treated as disconnected for scripting"
  );

  // The renderer snapshot includes inert template contents, so mapping-based pruning alone would not
  // consider this node detached.
  let snapshot = doc.to_renderer_dom_with_mapping();
  assert!(
    snapshot.mapping.preorder_for_node_id(input).is_some(),
    "renderer mapping should include inert template contents"
  );

  let mut state_dom2 = InteractionStateDom2 {
    focused: Some(input),
    focus_visible: true,
    focus_chain: vec![input],
    hover_chain: vec![input],
    active_chain: vec![input],
    ..Default::default()
  };

  state_dom2.prune_disconnected(&doc);
  assert_eq!(state_dom2.focused, None);
  assert!(!state_dom2.focus_visible);
  assert!(state_dom2.focus_chain.is_empty());
  assert!(state_dom2.hover_chain.is_empty());
  assert!(state_dom2.active_chain.is_empty());
}

#[test]
fn document_selection_ranges_dom2_normalize_uses_dom_tree_order_not_node_id_index() {
  let mut doc = crate::dom2::parse_html("<!doctype html><html><body></body></html>").unwrap();
  let body = doc.body().expect("body");

  let container = doc.create_element("div", "");
  doc.append_child(body, container).unwrap();

  // Create nodes in one order (t1 then t2)...
  let t1 = doc.create_text("A");
  let t2 = doc.create_text("B");
  // ...but insert them in the opposite DOM order (t2 before t1).
  doc.append_child(container, t1).unwrap();
  doc.insert_before(container, t2, Some(t1)).unwrap();

  assert!(
    t1.index() < t2.index(),
    "test invariant: node ids are creation-order (t1 < t2)"
  );

  let mut selection = DocumentSelectionRangesDom2 {
    ranges: vec![DocumentSelectionRangeDom2 {
      start: DocumentSelectionPointDom2 {
        node_id: t1,
        char_offset: 0,
      },
      end: DocumentSelectionPointDom2 {
        node_id: t2,
        char_offset: 0,
      },
    }],
    primary: 0,
    anchor: DocumentSelectionPointDom2 {
      node_id: t1,
      char_offset: 0,
    },
    focus: DocumentSelectionPointDom2 {
      node_id: t2,
      char_offset: 0,
    },
  };

  selection.normalize(&doc);

  assert_eq!(selection.ranges.len(), 1);
  assert_eq!(selection.ranges[0].start.node_id, t2);
  assert_eq!(selection.ranges[0].end.node_id, t1);
}

#[test]
fn document_selection_ranges_dom2_normalize_drops_cross_shadow_root_ranges() {
  let mut doc = crate::dom2::parse_html("<!doctype html><div id=host></div>").unwrap();
  let host = doc.get_element_by_id("host").expect("host element");
  let shadow = doc
    .attach_shadow_root(
      host,
      ShadowRootMode::Open,
      /* clonable */ false,
      /* serializable */ false,
      /* delegates_focus */ false,
      SlotAssignmentMode::Named,
    )
    .unwrap();

  let light_text = doc.create_text("light");
  doc.append_child(host, light_text).unwrap();
  let shadow_text = doc.create_text("shadow");
  doc.append_child(shadow, shadow_text).unwrap();

  let mut selection = DocumentSelectionRangesDom2 {
    ranges: vec![DocumentSelectionRangeDom2 {
      start: DocumentSelectionPointDom2 {
        node_id: light_text,
        char_offset: 0,
      },
      end: DocumentSelectionPointDom2 {
        node_id: shadow_text,
        char_offset: 0,
      },
    }],
    primary: 0,
    anchor: DocumentSelectionPointDom2 {
      node_id: light_text,
      char_offset: 0,
    },
    focus: DocumentSelectionPointDom2 {
      node_id: shadow_text,
      char_offset: 0,
    },
  };

  selection.normalize(&doc);
  assert!(
    selection.ranges.is_empty(),
    "cross-shadow-root selection ranges should be pruned"
  );
}

#[test]
fn document_selection_ranges_dom2_normalize_prefers_anchor_root_when_multiple_roots_present() {
  // While cross-shadow-root *ranges* are pruned, it's still possible for callers to construct a
  // multi-range selection that contains ranges from different Range tree roots (Document vs
  // ShadowRoot). Normalization should deterministically pick a root; prefer the anchor/focus span
  // root (primary range semantics) so behavior does not depend on `ranges` vector order.
  let mut doc = crate::dom2::parse_html("<!doctype html><div id=host></div>").unwrap();
  let host = doc.get_element_by_id("host").expect("host element");
  let shadow = doc
    .attach_shadow_root(
      host,
      ShadowRootMode::Open,
      /* clonable */ false,
      /* serializable */ false,
      /* delegates_focus */ false,
      SlotAssignmentMode::Named,
    )
    .unwrap();

  let light_text = doc.create_text("light");
  doc.append_child(host, light_text).unwrap();
  let shadow_text = doc.create_text("shadow");
  doc.append_child(shadow, shadow_text).unwrap();

  let mut selection = DocumentSelectionRangesDom2 {
    // Put the light-dom range first to ensure we don't accidentally pick it via vec order.
    ranges: vec![
      DocumentSelectionRangeDom2 {
        start: DocumentSelectionPointDom2 {
          node_id: light_text,
          char_offset: 0,
        },
        end: DocumentSelectionPointDom2 {
          node_id: light_text,
          char_offset: 0,
        },
      },
      DocumentSelectionRangeDom2 {
        start: DocumentSelectionPointDom2 {
          node_id: shadow_text,
          char_offset: 0,
        },
        end: DocumentSelectionPointDom2 {
          node_id: shadow_text,
          char_offset: 0,
        },
      },
    ],
    primary: 0,
    anchor: DocumentSelectionPointDom2 {
      node_id: shadow_text,
      char_offset: 0,
    },
    focus: DocumentSelectionPointDom2 {
      node_id: shadow_text,
      char_offset: 0,
    },
  };

  selection.normalize(&doc);
  assert_eq!(selection.ranges.len(), 1);
  assert_eq!(selection.ranges[0].start.node_id, shadow_text);
  assert_eq!(selection.anchor.node_id, shadow_text);
  assert_eq!(selection.focus.node_id, shadow_text);
}
