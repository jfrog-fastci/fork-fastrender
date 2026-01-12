use crate::layout::axis::FragmentAxes;
use crate::style::content::{StringSetAssignment, StringSetValue};
use crate::style::position::Position;
use crate::style::ComputedStyle;
use crate::tree::box_tree::{BoxNode, BoxTree, BoxType, MarkerContent};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

fn box_is_in_style_containment(
  mut box_id: usize,
  styles_by_id: &HashMap<usize, Arc<crate::style::ComputedStyle>>,
  parent_by_id: &HashMap<usize, usize>,
) -> bool {
  loop {
    if styles_by_id
      .get(&box_id)
      .is_some_and(|style| style.containment.style)
    {
      return true;
    }
    let Some(parent) = parent_by_id.get(&box_id).copied() else {
      return false;
    };
    box_id = parent;
  }
}

/// A single string-set assignment event, positioned on the block axis.
#[derive(Debug, Clone)]
pub struct StringSetEvent {
  /// Absolute block-axis position of the assigning fragment.
  pub abs_block: f32,
  /// Stable monotonic sequence number for deterministic ordering.
  ///
  /// Pagination sorts string-set events by `(abs_block, sequence)` so that multiple assignments at
  /// the same block position (or within the same fragment slice) are applied in collection order.
  pub sequence: usize,
  /// Originating box id for the assignment, when known.
  ///
  /// This is used by pagination to avoid applying duplicate assignments when a box is fragmented
  /// across pages/columns (the same box_id can appear in multiple fragment slices).
  pub box_id: Option<usize>,
  /// Name of the string being assigned.
  pub name: String,
  /// Resolved value for the assignment.
  pub value: String,
}

/// Precomputed metadata for resolving `string-set` values.
///
/// This avoids rebuilding box_id→style and box_id→text maps when collecting string-set events from
/// multiple layouts during pagination.
#[derive(Debug, Clone)]
pub struct StringSetEventCollector {
  styles_by_id: HashMap<usize, Arc<ComputedStyle>>,
  parent_by_id: HashMap<usize, usize>,
  box_text: HashMap<usize, String>,
}

impl StringSetEventCollector {
  pub fn new(box_tree: &BoxTree) -> Self {
    let mut styles_by_id = HashMap::new();
    collect_box_styles(&box_tree.root, &mut styles_by_id);

    let mut parent_by_id = HashMap::new();
    collect_box_parents(&box_tree.root, None, &mut parent_by_id);

    let mut box_text = HashMap::new();
    collect_box_text(&box_tree.root, &mut box_text);

    Self {
      styles_by_id,
      parent_by_id,
      box_text,
    }
  }

  /// Collect string-set events from `root`, positioning them in the provided fragmentation axes.
  pub fn collect(&self, root: &FragmentNode, axes: FragmentAxes) -> Vec<StringSetEvent> {
    self.collect_with_abs_start(root, 0.0, axes)
  }

  /// Collect string-set events with a caller-provided absolute block-axis offset.
  pub fn collect_with_abs_start(
    &self,
    root: &FragmentNode,
    abs_start: f32,
    axes: FragmentAxes,
  ) -> Vec<StringSetEvent> {
    let mut events = Vec::new();
    let mut seen_boxes = HashSet::new();
    let mut sequence = 0usize;
    collect_string_set_events_inner(
      root,
      abs_start,
      axes.block_size(&root.logical_bounds()),
      axes,
      &mut events,
      &self.styles_by_id,
      &self.parent_by_id,
      &self.box_text,
      &mut seen_boxes,
      &mut sequence,
    );
    events
  }
}

/// Collect all string-set assignments from a laid-out fragment tree.
///
/// The traversal uses the original (unclipped) fragment tree and records the absolute
/// block-axis position of each fragment carrying a `string-set` declaration.
pub fn collect_string_set_events(
  root: &FragmentNode,
  box_tree: &BoxTree,
  axes: FragmentAxes,
) -> Vec<StringSetEvent> {
  StringSetEventCollector::new(box_tree).collect(root, axes)
}

fn collect_box_styles(node: &BoxNode, out: &mut HashMap<usize, Arc<ComputedStyle>>) {
  out.insert(node.id, node.style.clone());
  for child in node.children.iter() {
    collect_box_styles(child, out);
  }
}

fn collect_box_parents(node: &BoxNode, parent: Option<usize>, out: &mut HashMap<usize, usize>) {
  if let Some(parent_id) = parent {
    if node.id != 0 && parent_id != 0 {
      out.insert(node.id, parent_id);
    }
  }
  let next_parent = if node.id != 0 { Some(node.id) } else { parent };
  for child in node.children.iter() {
    collect_box_parents(child, next_parent, out);
  }
}

fn collect_box_text(node: &BoxNode, out: &mut HashMap<usize, String>) -> String {
  let mut text = String::new();
  match &node.box_type {
    BoxType::Text(t) => text.push_str(&t.text),
    BoxType::Marker(marker) => {
      if let MarkerContent::Text(t) = &marker.content {
        text.push_str(t);
      }
    }
    _ => {}
  }

  for child in node.children.iter() {
    text.push_str(&collect_box_text(child, out));
  }

  out.insert(node.id, text.clone());
  text
}

fn collect_string_set_events_inner(
  node: &FragmentNode,
  abs_start: f32,
  parent_block_size: f32,
  axes: FragmentAxes,
  out: &mut Vec<StringSetEvent>,
  styles_by_id: &HashMap<usize, Arc<ComputedStyle>>,
  parent_by_id: &HashMap<usize, usize>,
  box_text: &HashMap<usize, String>,
  seen_boxes: &mut HashSet<usize>,
  sequence: &mut usize,
) {
  let logical_bounds = node.logical_bounds();
  let abs_start = if abs_start.is_finite() { abs_start } else { 0.0 };
  let parent_block_size = if parent_block_size.is_finite() {
    parent_block_size
  } else {
    0.0
  };
  let start_raw = axes.abs_block_start(&logical_bounds, abs_start, parent_block_size);
  let start = if start_raw.is_finite() { start_raw } else { abs_start };
  let node_block_size_raw = axes.block_size(&logical_bounds);
  let node_block_size = if node_block_size_raw.is_finite() {
    node_block_size_raw
  } else {
    parent_block_size
  };

  if fragment_is_out_of_flow(node, styles_by_id, parent_by_id) {
    return;
  }

  let mut assignments: Option<&[StringSetAssignment]> = None;
  let mut assignments_in_flow = true;
  let fragment_box_id: Option<usize> = fragment_box_id(node);
  let mut assignments_box_id: Option<usize> = None;
  if let Some(mut probe) = fragment_box_id {
    loop {
      if let Some(style) = styles_by_id.get(&probe) {
        if !style.string_set.is_empty() {
          assignments = Some(style.string_set.as_slice());
          assignments_in_flow = style.position.is_in_flow();
          assignments_box_id = Some(probe);
          break;
        }
      }
      let Some(parent) = parent_by_id.get(&probe).copied() else {
        break;
      };
      probe = parent;
    }
  }

  if assignments.is_none() {
    if let Some(style) = node.style.as_deref() {
      if !style.string_set.is_empty() {
        assignments = Some(style.string_set.as_slice());
        assignments_in_flow = style.position.is_in_flow();
        assignments_box_id = fragment_box_id;
      }
    }
  }

  if let Some(assignments) = assignments {
    let in_style_containment = assignments_box_id
      .map(|box_id| box_is_in_style_containment(box_id, styles_by_id, parent_by_id))
      .unwrap_or_else(|| {
        node
          .style
          .as_deref()
          .is_some_and(|style| style.containment.style)
      });

    let should_emit = assignments_box_id
      .map(|box_id| seen_boxes.insert(box_id))
      .unwrap_or(true);

    if should_emit && !in_style_containment && assignments_in_flow {
      for StringSetAssignment { name, value } in assignments {
        let resolved = resolve_string_set_value(node, value, assignments_box_id, box_text);
        out.push(StringSetEvent {
          abs_block: start,
          sequence: *sequence,
          box_id: assignments_box_id,
          name: name.clone(),
          value: resolved,
        });
        *sequence += 1;
      }
    }
  }

  for child in node.children() {
    collect_string_set_events_inner(
      child,
      start,
      node_block_size,
      axes,
      out,
      styles_by_id,
      parent_by_id,
      box_text,
      seen_boxes,
      sequence,
    );
  }
}

fn fragment_is_out_of_flow(
  node: &FragmentNode,
  styles_by_id: &HashMap<usize, Arc<ComputedStyle>>,
  parent_by_id: &HashMap<usize, usize>,
) -> bool {
  if node
    .style
    .as_deref()
    .is_some_and(|style| matches!(style.position, Position::Fixed | Position::Absolute))
  {
    return true;
  }

  let mut probe = fragment_box_id(node);
  while let Some(box_id) = probe {
    if let Some(style) = styles_by_id.get(&box_id) {
      if matches!(style.position, Position::Fixed | Position::Absolute) {
        return true;
      }
    }
    probe = parent_by_id.get(&box_id).copied();
  }

  false
}

fn fragment_box_id(node: &FragmentNode) -> Option<usize> {
  match &node.content {
    FragmentContent::Block { box_id } => *box_id,
    FragmentContent::Inline { box_id, .. } => *box_id,
    FragmentContent::Text { box_id, .. } => *box_id,
    FragmentContent::Replaced { box_id, .. } => *box_id,
    _ => None,
  }
}

fn resolve_string_set_value(
  node: &FragmentNode,
  value: &StringSetValue,
  box_id: Option<usize>,
  box_text: &HashMap<usize, String>,
) -> String {
  match value {
    StringSetValue::Content => {
      if let Some(box_id) = box_id {
        if let Some(text) = box_text.get(&box_id) {
          return text.clone();
        }
      }
      let mut fallback = String::new();
      collect_text(node, &mut fallback);
      fallback
    }
    StringSetValue::Literal(s) => s.clone(),
  }
}

fn collect_text(node: &FragmentNode, out: &mut String) {
  if let FragmentContent::Text { text, .. } = &node.content {
    out.push_str(text);
  }
  for child in node.children() {
    collect_text(child, out);
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::geometry::Rect;
  use crate::layout::axis::FragmentAxes;
  use crate::style::content::{StringSetAssignment, StringSetValue};
  use crate::style::display::FormattingContextType;
  use crate::style::position::Position;
  use crate::style::ComputedStyle;
  use crate::tree::box_tree::{BoxNode, BoxTree};
  use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
  use std::sync::Arc;

  #[test]
  fn collect_string_set_events_traverses_descendants() {
    let mut string_set_style = ComputedStyle::default();
    string_set_style.string_set = vec![StringSetAssignment {
      name: "chapter".into(),
      value: StringSetValue::Content,
    }];

    let text_box = BoxNode::new_text(Arc::new(ComputedStyle::default()), "Box Value".into());
    let box_with_string_set = BoxNode::new_block(
      Arc::new(string_set_style),
      FormattingContextType::Block,
      vec![text_box],
    );
    let root_box = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![box_with_string_set],
    );
    let box_tree = BoxTree::new(root_box);

    let string_set_box = &box_tree.root.children[0];
    let string_set_text_box = &string_set_box.children[0];

    let text_fragment = FragmentNode::new(
      Rect::from_xywh(0.0, 0.0, 10.0, 5.0),
      FragmentContent::Text {
        text: "Box Value".into(),
        box_id: Some(string_set_text_box.id),
        source_range: None,
        baseline_offset: 0.0,
        shaped: None,
        is_marker: false,
        emphasis_offset: Default::default(),
      },
      vec![],
    );
    let child_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 5.0, 10.0, 10.0),
      string_set_box.id,
      vec![text_fragment],
    );

    let mut inline_style = ComputedStyle::default();
    inline_style.string_set = vec![StringSetAssignment {
      name: "note".into(),
      value: StringSetValue::Content,
    }];
    let inline_text = FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 10.0, 5.0),
      Arc::<str>::from("Inline fallback"),
      0.0,
    );
    let mut inline_fragment =
      FragmentNode::new_inline(Rect::from_xywh(0.0, 20.0, 10.0, 5.0), 0, vec![inline_text]);
    inline_fragment.style = Some(Arc::new(inline_style));

    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 10.0, 40.0),
      box_tree.root.id,
      vec![child_fragment, inline_fragment],
    );

    let events = collect_string_set_events(&root_fragment, &box_tree, FragmentAxes::default());

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].name, "chapter");
    assert_eq!(events[0].value, "Box Value");
    assert_eq!(events[0].box_id, Some(string_set_box.id));
    assert!((events[0].abs_block - 5.0).abs() < 0.001);
    assert_eq!(events[0].sequence, 0);

    assert_eq!(events[1].name, "note");
    assert_eq!(events[1].value, "Inline fallback");
    assert_eq!(events[1].box_id, None);
    assert!((events[1].abs_block - 20.0).abs() < 0.001);
    assert_eq!(events[1].sequence, 1);
  }

  #[test]
  fn collect_string_set_events_skips_fixed_position_sources() {
    let mut fixed_style = ComputedStyle::default();
    fixed_style.position = Position::Fixed;
    fixed_style.string_set = vec![StringSetAssignment {
      name: "chapter".into(),
      value: StringSetValue::Literal("Fixed".into()),
    }];

    let fixed_text_box =
      BoxNode::new_text(Arc::new(ComputedStyle::default()), "Ignored".into());
    let fixed_box = BoxNode::new_block(
      Arc::new(fixed_style),
      FormattingContextType::Block,
      vec![fixed_text_box],
    );
    let root_box = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![fixed_box],
    );
    let box_tree = BoxTree::new(root_box);

    let fixed_box = &box_tree.root.children[0];
    let fixed_text_box = &fixed_box.children[0];

    let text_fragment = FragmentNode::new(
      Rect::from_xywh(0.0, 0.0, 10.0, 5.0),
      FragmentContent::Text {
        text: "Ignored".into(),
        box_id: Some(fixed_text_box.id),
        source_range: None,
        baseline_offset: 0.0,
        shaped: None,
        is_marker: false,
        emphasis_offset: Default::default(),
      },
      vec![],
    );
    let fixed_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 5.0, 10.0, 10.0),
      fixed_box.id,
      vec![],
    );
    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 10.0, 20.0),
      box_tree.root.id,
      vec![text_fragment, fixed_fragment],
    );

    let collector = StringSetEventCollector::new(&box_tree);
    let events = collector.collect(&root_fragment, FragmentAxes::default());
    assert!(events.is_empty());
  }

  #[test]
  fn string_set_events_have_stable_sequence_ordering() {
    let mut style = ComputedStyle::default();
    style.string_set = vec![
      StringSetAssignment {
        name: "a".into(),
        value: StringSetValue::Literal("1".into()),
      },
      StringSetAssignment {
        name: "b".into(),
        value: StringSetValue::Literal("2".into()),
      },
    ];

    let box_with_string_set = BoxNode::new_block(
      Arc::new(style),
      FormattingContextType::Block,
      vec![],
    );
    let root_box = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![box_with_string_set],
    );
    let box_tree = BoxTree::new(root_box);

    let string_set_box = &box_tree.root.children[0];
    let root_fragment = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 10.0, 20.0),
      box_tree.root.id,
      vec![FragmentNode::new_block_with_id(
        Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
        string_set_box.id,
        vec![],
      )],
    );

    let collector = StringSetEventCollector::new(&box_tree);
    let events = collector.collect(&root_fragment, FragmentAxes::default());
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].name, "a");
    assert_eq!(events[0].sequence, 0);
    assert_eq!(events[1].name, "b");
    assert_eq!(events[1].sequence, 1);

    // When multiple events share the same block position, ordering must be derived from the
    // monotonic sequence number so pagination can deterministically apply them.
    let mut reversed = vec![events[1].clone(), events[0].clone()];
    reversed.sort_by(|a, b| {
      a.abs_block
        .total_cmp(&b.abs_block)
        .then_with(|| a.sequence.cmp(&b.sequence))
    });
    assert_eq!(reversed[0].name, "a");
    assert_eq!(reversed[1].name, "b");
  }

  #[test]
  fn collect_string_set_events_ignores_out_of_flow_subtrees() {
    let mut fixed_style = ComputedStyle::default();
    fixed_style.position = Position::Fixed;
    fixed_style.string_set = vec![StringSetAssignment {
      name: "fixed".into(),
      value: StringSetValue::Literal("bad".into()),
    }];

    let mut inner_style = ComputedStyle::default();
    inner_style.string_set = vec![StringSetAssignment {
      name: "inner".into(),
      value: StringSetValue::Literal("also bad".into()),
    }];

    let fixed_inner_box = BoxNode::new_block(
      Arc::new(inner_style),
      FormattingContextType::Block,
      vec![],
    );
    let fixed_box = BoxNode::new_block(
      Arc::new(fixed_style),
      FormattingContextType::Block,
      vec![fixed_inner_box],
    );

    let mut normal_style = ComputedStyle::default();
    normal_style.string_set = vec![StringSetAssignment {
      name: "normal".into(),
      value: StringSetValue::Literal("ok".into()),
    }];
    let normal_box = BoxNode::new_block(
      Arc::new(normal_style),
      FormattingContextType::Block,
      vec![],
    );

    let root_box = BoxNode::new_block(
      Arc::new(ComputedStyle::default()),
      FormattingContextType::Block,
      vec![fixed_box, normal_box],
    );
    let box_tree = BoxTree::new(root_box);

    let fixed_box = &box_tree.root.children[0];
    let fixed_inner_box = &fixed_box.children[0];
    let normal_box = &box_tree.root.children[1];

    let fixed_inner_frag = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 10.0, 0.0),
      fixed_inner_box.id,
      vec![],
    );
    let fixed_frag = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 10.0, 0.0),
      fixed_box.id,
      vec![fixed_inner_frag],
    );
    let normal_frag = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 10.0, 0.0),
      normal_box.id,
      vec![],
    );
    let root_frag = FragmentNode::new_block_with_id(
      Rect::from_xywh(0.0, 0.0, 10.0, 10.0),
      box_tree.root.id,
      vec![fixed_frag, normal_frag],
    );

    let events = collect_string_set_events(&root_frag, &box_tree, FragmentAxes::default());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "normal");
    assert_eq!(events[0].value, "ok");
  }
}
