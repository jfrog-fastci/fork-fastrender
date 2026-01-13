use crate::BrowserDocument;
use crate::style::types::{AnimationTimeline, TransitionProperty};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

/// Returns `true` when the document contains time-based primitives (CSS animations or transitions)
/// and should be periodically repainted with an advancing animation timestamp.
///
/// This is a shared helper used by both:
/// - the main browser render worker (to decide whether to schedule periodic ticks), and
/// - chrome/runtime integrations that render trusted HTML/CSS (to drive `animation_time_ms`).
pub(crate) fn browser_document_wants_ticks(doc: &BrowserDocument) -> bool {
  doc
    .prepared()
    .is_some_and(|prepared| fragment_tree_wants_ticks(prepared.fragment_tree()))
}

fn fragment_tree_wants_ticks(tree: &FragmentTree) -> bool {
  // If the transition engine is tracking running transitions, we need time to advance.
  if tree
    .transition_state
    .as_deref()
    .is_some_and(crate::animation::TransitionState::has_running_transitions)
  {
    return true;
  }

  // Walk fragments looking for:
  // - time-based animations (`animation-name` + `animation-timeline: auto`), or
  // - @starting-style transitions (indicated by `starting_style` snapshots).
  let mut stack: Vec<&FragmentNode> = Vec::new();
  stack.push(&tree.root);
  for root in &tree.additional_fragments {
    stack.push(root);
  }

  while let Some(node) = stack.pop() {
    if fragment_node_wants_ticks(node) {
      return true;
    }

    for child in node.children.iter() {
      stack.push(child);
    }

    match &node.content {
      FragmentContent::RunningAnchor { snapshot, .. }
      | FragmentContent::FootnoteAnchor { snapshot, .. } => {
        stack.push(snapshot.as_ref());
      }
      _ => {}
    }
  }

  false
}

fn fragment_node_wants_ticks(node: &FragmentNode) -> bool {
  let Some(style) = node.style.as_deref() else {
    return false;
  };

  if style_has_time_based_animation(style) {
    return true;
  }

  // `starting_style` snapshots indicate a transition that begins at document time 0 (CSS
  // `@starting-style`). Those transitions need time progression even before we have an explicit
  // `TransitionState`.
  if node.starting_style.is_some() && style_has_time_based_transition(style) {
    return true;
  }

  false
}

fn style_has_time_based_animation(style: &crate::style::ComputedStyle) -> bool {
  for (idx, name) in style.animation_names.iter().enumerate() {
    if name.is_none() {
      continue;
    }

    // The animation engine treats an empty `animation_timelines` list as `auto`.
    if timeline_is_time_based(&style.animation_timelines, idx) {
      return true;
    }
  }
  false
}

fn timeline_is_time_based(list: &[AnimationTimeline], idx: usize) -> bool {
  if list.is_empty() {
    return true;
  }
  let timeline = list.get(idx).or_else(|| list.last());
  matches!(timeline, Some(AnimationTimeline::Auto))
}

fn style_has_time_based_transition(style: &crate::style::ComputedStyle) -> bool {
  // `transition-property: none` disables transitions.
  if style.transition_properties.len() == 1
    && matches!(&style.transition_properties[0], TransitionProperty::None)
  {
    return false;
  }

  // No time progression when both duration and delay are 0.
  if style
    .transition_durations
    .iter()
    .any(|ms| ms.is_finite() && *ms > 0.0)
  {
    return true;
  }
  // A positive delay still implies a time-based update (hold start value until delay elapses).
  if style
    .transition_delays
    .iter()
    .any(|ms| ms.is_finite() && *ms > 0.0)
  {
    return true;
  }

  false
}
