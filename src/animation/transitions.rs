use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::geometry::Size;
use crate::style::types::{OutlineColor, TransitionBehavior, TransitionTimingFunction};
use crate::style::values::{CustomPropertySyntax, CustomPropertyValue};
use crate::style::ComputedStyle;
use crate::tree::box_tree::{BoxNode, BoxTree, GeneratedPseudoElement};
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};

use super::{AnimatedValue, AnimationResolveContext};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ElementKey {
  pub(crate) styled_node_id: usize,
  pub(crate) pseudo: Option<GeneratedPseudoElement>,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum TransitionValue {
  Builtin(AnimatedValue),
  Custom(CustomPropertyValue),
}

#[derive(Debug, Clone)]
pub(super) struct SampledTransition {
  pub(super) value: TransitionValue,
  pub(super) progress: f32,
  pub(super) delay_ms: f32,
  pub(super) duration_ms: f32,
}

#[derive(Debug, Clone)]
pub(super) struct TransitionRecord {
  pub(super) property: Arc<str>,
  pub(super) start_time_ms: f32,
  pub(super) delay_ms: f32,
  pub(super) duration_ms: f32,
  pub(super) timing_function: TransitionTimingFunction,
  pub(super) transition_behavior: TransitionBehavior,
  pub(super) allow_discrete: bool,
  pub(super) from_style: Arc<ComputedStyle>,
  pub(super) to_style: Arc<ComputedStyle>,
  pub(super) reversing_adjusted_start_value: TransitionValue,
  pub(super) reversing_shortening_factor: f32,
}

impl TransitionRecord {
  fn raw_progress(&self, now_ms: f32) -> f32 {
    if self.duration_ms <= 0.0 {
      return 1.0;
    }
    let elapsed = now_ms - self.start_time_ms - self.delay_ms;
    if elapsed <= 0.0 {
      return 0.0;
    }
    if elapsed >= self.duration_ms {
      return 1.0;
    }
    (elapsed / self.duration_ms).clamp(0.0, 1.0)
  }

  fn is_finished(&self, now_ms: f32) -> bool {
    if self.duration_ms <= 0.0 {
      return true;
    }
    now_ms - self.start_time_ms - self.delay_ms >= self.duration_ms
  }

  fn extract_value(style: &ComputedStyle, name: &str, ctx: &AnimationResolveContext) -> Option<TransitionValue> {
    if name.starts_with("--") {
      return style
        .custom_properties
        .get(name)
        .cloned()
        .map(TransitionValue::Custom);
    }
    let interpolator = super::interpolator_for(name)?;
    let value = (interpolator.extract)(style, ctx)?;
    Some(TransitionValue::Builtin(value))
  }

  fn sample_builtin(
    &self,
    _now_ms: f32,
    ctx: &AnimationResolveContext,
    progress: f32,
  ) -> Option<AnimatedValue> {
    let name = self.property.as_ref();

    if !self.allow_discrete
      && matches!(
        name,
        "visibility"
          | "border-style"
          | "border-top-style"
          | "border-right-style"
          | "border-bottom-style"
          | "border-left-style"
          | "outline-style"
      )
    {
      // CSS Transitions Level 2: discrete transitions only run when explicitly enabled via
      // `transition-behavior: allow-discrete`.
      return None;
    }

    let interpolator = super::interpolator_for(name)?;
    let from_val = (interpolator.extract)(&self.from_style, ctx)?;
    let to_val = (interpolator.extract)(&self.to_style, ctx)?;

    if self.allow_discrete {
      return (interpolator.interpolate)(&from_val, &to_val, progress).or_else(|| {
        if progress >= 0.5 {
          Some(to_val.clone())
        } else {
          Some(from_val.clone())
        }
      });
    }

    let mut value = (interpolator.interpolate)(&from_val, &to_val, progress)?;

    // Suppress discrete sub-components for shorthands that include both interpolable and discrete
    // parts (e.g. `border` includes `border-*-style`).
    match (&mut value, &to_val) {
      (AnimatedValue::Border(_, styles, _), AnimatedValue::Border(_, to_styles, _))
        if matches!(
          name,
          "border" | "border-top" | "border-right" | "border-bottom" | "border-left"
        ) =>
      {
        *styles = *to_styles;
      }
      (
        AnimatedValue::Outline(color, outline_style, _),
        AnimatedValue::Outline(to_color, to_style, _),
      ) if name == "outline" => {
        *outline_style = *to_style;
        // Outline color interpolation is only continuous when both endpoints are explicit colors;
        // otherwise it is discrete and follows `transition-behavior`.
        if !matches!(
          (&from_val, &to_val),
          (
            AnimatedValue::Outline(OutlineColor::Color(_), _, _),
            AnimatedValue::Outline(OutlineColor::Color(_), _, _)
          )
        ) {
          *color = *to_color;
        }
      }
      _ => {}
    }

    Some(value)
  }

  fn sample_custom(
    &self,
    ctx: &AnimationResolveContext,
    progress: f32,
  ) -> Option<CustomPropertyValue> {
    let name = self.property.as_ref();
    let from_val = self.from_style.custom_properties.get(name)?.clone();
    let to_val = self.to_style.custom_properties.get(name)?.clone();

    let can_interpolate = match (
      self.from_style.custom_property_registry.get(name),
      self.to_style.custom_property_registry.get(name),
    ) {
      (Some(from_rule), Some(to_rule))
        if from_rule.syntax == to_rule.syntax
          && !matches!(from_rule.syntax, CustomPropertySyntax::Universal) =>
      {
        true
      }
      _ => false,
    };

    let sampled = (can_interpolate
      .then(|| {
        super::interpolate_custom_property(
          &from_val,
          &to_val,
          progress,
          &self.from_style,
          &self.to_style,
          ctx,
        )
      })
      .flatten())
    .or_else(|| {
      if self.allow_discrete {
        if progress >= 0.5 {
          Some(to_val.clone())
        } else {
          Some(from_val.clone())
        }
      } else {
        None
      }
    })?;

    Some(sampled)
  }

  pub(super) fn sample(&self, now_ms: f32, ctx: &AnimationResolveContext) -> Option<SampledTransition> {
    if self.duration_ms <= 0.0 {
      return None;
    }

    let elapsed = now_ms - self.start_time_ms - self.delay_ms;
    if elapsed >= self.duration_ms {
      return None;
    }

    let raw_progress = if elapsed <= 0.0 {
      0.0
    } else {
      (elapsed / self.duration_ms).clamp(0.0, 1.0)
    };
    let progress = self.timing_function.value_at(raw_progress);

    let value = if self.property.starts_with("--") {
      let value = self.sample_custom(ctx, progress)?;
      TransitionValue::Custom(value)
    } else {
      let value = self.sample_builtin(now_ms, ctx, progress)?;
      TransitionValue::Builtin(value)
    };

    Some(SampledTransition {
      value,
      progress,
      delay_ms: self.delay_ms,
      duration_ms: self.duration_ms,
    })
  }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ElementTransitionState {
  pub(super) running: HashMap<Arc<str>, TransitionRecord>,
  pub(super) completed: HashMap<Arc<str>, TransitionRecord>,
}

#[derive(Debug, Clone)]
pub struct TransitionState {
  pub(super) elements: HashMap<ElementKey, ElementTransitionState>,
  pub(super) box_to_element: HashMap<usize, ElementKey>,
  viewport_size: Size,
  element_sizes: HashMap<ElementKey, Size>,
}

impl Default for TransitionState {
  fn default() -> Self {
    Self {
      elements: HashMap::new(),
      box_to_element: HashMap::new(),
      viewport_size: Size::ZERO,
      element_sizes: HashMap::new(),
    }
  }
}

fn collect_element_data(box_tree: &BoxTree) -> (HashMap<ElementKey, Arc<ComputedStyle>>, HashMap<usize, ElementKey>) {
  let mut styles: HashMap<ElementKey, Arc<ComputedStyle>> = HashMap::new();
  let mut map: HashMap<usize, ElementKey> = HashMap::new();

  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if let Some(styled_node_id) = node.styled_node_id {
      let key = ElementKey {
        styled_node_id,
        pseudo: node.generated_pseudo,
      };
      map.insert(node.id, key);
      styles.entry(key).or_insert_with(|| node.style.clone());
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }

  (styles, map)
}

fn collect_starting_styles(box_tree: &BoxTree) -> HashMap<ElementKey, Arc<ComputedStyle>> {
  let mut styles: HashMap<ElementKey, Arc<ComputedStyle>> = HashMap::new();
  let mut stack: Vec<&BoxNode> = vec![&box_tree.root];
  while let Some(node) = stack.pop() {
    if let (Some(styled_node_id), Some(starting_style)) =
      (node.styled_node_id, node.starting_style.as_ref())
    {
      let key = ElementKey {
        styled_node_id,
        pseudo: node.generated_pseudo,
      };
      styles.entry(key).or_insert_with(|| starting_style.clone());
    }
    if let Some(body) = node.footnote_body.as_deref() {
      stack.push(body);
    }
    for child in node.children.iter().rev() {
      stack.push(child);
    }
  }
  styles
}

fn default_update_context() -> AnimationResolveContext {
  // `TransitionState::update_for_style_change` only sees computed styles, not layout results. Use a
  // non-zero fallback size so percentage-based values don't collapse to 0 and incorrectly suppress
  // transitions. Paint-time sampling uses the real fragment bounds + viewport sizes.
  const FALLBACK: Size = Size::new(1000.0, 1000.0);
  AnimationResolveContext::new(FALLBACK, FALLBACK)
}

fn is_discrete_property(name: &str) -> bool {
  matches!(
    name,
    "visibility"
      | "border-style"
      | "border-top-style"
      | "border-right-style"
      | "border-bottom-style"
      | "border-left-style"
      | "outline-style"
  )
}

fn transition_value_distance(a: &TransitionValue, b: &TransitionValue) -> Option<f32> {
  match (a, b) {
    (
      TransitionValue::Builtin(AnimatedValue::Opacity(x)),
      TransitionValue::Builtin(AnimatedValue::Opacity(y)),
    ) => Some((*y - *x).abs()),
    _ => None,
  }
}

fn can_interpolate_custom_property(from: &ComputedStyle, to: &ComputedStyle, name: &str) -> bool {
  match (
    from.custom_property_registry.get(name),
    to.custom_property_registry.get(name),
  ) {
    (Some(from_rule), Some(to_rule))
      if from_rule.syntax == to_rule.syntax && !matches!(from_rule.syntax, CustomPropertySyntax::Universal) =>
    {
      true
    }
    _ => false,
  }
}

impl TransitionState {
  /// Captures viewport + per-element sizes from the supplied fragment tree.
  ///
  /// This is used by [`TransitionState::update_for_style_change`] to sample in-flight transitions
  /// for interruption without collapsing percentage-based values to a dummy fallback.
  pub fn capture_layout_from_fragment_tree(&mut self, tree: &FragmentTree) {
    self.viewport_size = tree.viewport_size();
    self.element_sizes.clear();

    fn record(map: &mut HashMap<ElementKey, Size>, key: ElementKey, size: Size) {
      match map.entry(key) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
          let existing = entry.get_mut();
          existing.width = existing.width.max(size.width);
          existing.height = existing.height.max(size.height);
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
          entry.insert(size);
        }
      }
    }

    let mut stack: Vec<&FragmentNode> = Vec::new();
    stack.push(&tree.root);
    for root in &tree.additional_fragments {
      stack.push(root);
    }
    while let Some(node) = stack.pop() {
      if let Some(box_id) = node.box_id() {
        if let Some(key) = self.box_to_element.get(&box_id).copied() {
          record(
            &mut self.element_sizes,
            key,
            Size::new(node.bounds.width(), node.bounds.height()),
          );
        }
      }
      for child in node.children.iter() {
        stack.push(child);
      }
      match &node.content {
        FragmentContent::RunningAnchor { snapshot, .. }
        | FragmentContent::FootnoteAnchor { snapshot } => {
          stack.push(snapshot.as_ref());
        }
        _ => {}
      }
    }
  }

  fn update_context_for_element(&self, key: &ElementKey) -> AnimationResolveContext {
    const FALLBACK: Size = Size::new(1000.0, 1000.0);
    let viewport = if self.viewport_size.width > 0.0 && self.viewport_size.height > 0.0 {
      self.viewport_size
    } else {
      FALLBACK
    };
    let element_size = self
      .element_sizes
      .get(key)
      .copied()
      .filter(|size| size.width > 0.0 && size.height > 0.0)
      .unwrap_or(FALLBACK);
    AnimationResolveContext::new(viewport, element_size)
  }

  pub fn update_for_style_change(
    prev: Option<&TransitionState>,
    prev_box_tree: Option<&BoxTree>,
    new_box_tree: &BoxTree,
    now_ms: f32,
  ) -> TransitionState {
    let (new_styles, box_to_element) = collect_element_data(new_box_tree);
    let starting_styles =
      prev_box_tree.is_none().then(|| collect_starting_styles(new_box_tree));

    let mut next = TransitionState::default();
    next.box_to_element = box_to_element;

    let cmp_ctx = default_update_context();
    let prev_styles = prev_box_tree.map(|tree| collect_element_data(tree).0);
    let event_time_ms = if prev_styles.is_some() { now_ms } else { 0.0 };

    for (key, after_style_arc) in new_styles {
      let before_style_arc = prev_styles
        .as_ref()
        .and_then(|styles| styles.get(&key))
        .or_else(|| starting_styles.as_ref().and_then(|styles| styles.get(&key)));
      let Some(before_style_arc) = before_style_arc else {
        continue;
      };

      let mut element = ElementTransitionState::default();

      if let Some(prev_state) = prev {
        if let Some(prev_element) = prev_state.elements.get(&key) {
          element.completed = prev_element.completed.clone();
          for (name, record) in &prev_element.running {
            if record.is_finished(now_ms) {
              element.completed.insert(name.clone(), record.clone());
            } else {
              element.running.insert(name.clone(), record.clone());
            }
          }
        }
      }

      let after_style = after_style_arc.as_ref();
      let before_style = before_style_arc.as_ref();

      let eligible_pairs =
        super::transition_pairs(&after_style.transition_properties, before_style, after_style)
          .unwrap_or_default();
      let mut eligible: HashMap<Arc<str>, usize> = HashMap::new();
      for (name, idx) in eligible_pairs {
        eligible.insert(Arc::from(name), idx);
      }

      let mut names: HashSet<Arc<str>> = HashSet::new();
      names.extend(eligible.keys().cloned());
      names.extend(element.running.keys().cloned());
      names.extend(element.completed.keys().cloned());

      for name_arc in names {
        let name = name_arc.as_ref();

        // CSS Transitions 1 step 3: if `transition-property` changes such that it no longer matches
        // a running or completed transition, the transition must be cancelled/removed.
        let eligible_idx = eligible.get(&name_arc).copied();
        if eligible_idx.is_none()
          && (element.running.contains_key(&name_arc) || element.completed.contains_key(&name_arc))
        {
          element.running.remove(&name_arc);
          element.completed.remove(&name_arc);
          continue;
        }

        let Some(after_value) = TransitionRecord::extract_value(after_style, name, &cmp_ctx) else {
          element.running.remove(&name_arc);
          element.completed.remove(&name_arc);
          continue;
        };

        // CSS Transitions 1 step 2: if a completed transition's end value no longer matches the
        // after-change style value, drop it.
        let completed_matches_after = element
          .completed
          .get(&name_arc)
          .and_then(|record| TransitionRecord::extract_value(&record.to_style, name, &cmp_ctx))
          .is_some_and(|end| end == after_value);
        if !completed_matches_after {
          element.completed.remove(&name_arc);
        }

        let existing = element.running.get(&name_arc).cloned();

        if let Some(existing) = existing {
          // CSS Transitions 1: If `transition-property` changes such that the transition would not
          // have started, the running transition must be cancelled and the property value snaps to
          // its final value immediately. Timing/duration/delay changes do *not* affect a running
          // transition, but `transition-property` removal is special-cased.
          if !eligible.contains_key(&name_arc) {
            element.running.remove(&name_arc);
            element.completed.remove(&name_arc);
            continue;
          }

          if let Some(existing_target) =
            TransitionRecord::extract_value(&existing.to_style, name, &cmp_ctx)
          {
            if existing_target == after_value {
              // CSS Transitions 1: transition parameters are captured at start; changes to
              // `transition-*` properties do not affect running transitions. As long as the target
              // value is unchanged, keep the existing transition running.
              continue;
            }
          }

          // Target changed: cancel the existing transition and decide whether to start a new one.
          element.running.remove(&name_arc);

          let sample_ctx = prev
            .map(|state| state.update_context_for_element(&key))
            .unwrap_or(cmp_ctx);
          let before_value = existing
            .sample(now_ms, &sample_ctx)
            .map(|sample| sample.value)
            .or_else(|| TransitionRecord::extract_value(before_style, name, &sample_ctx));

          let Some(before_value) = before_value else {
            element.completed.remove(&name_arc);
            continue;
          };

          if before_value == after_value {
            element.completed.remove(&name_arc);
            continue;
          }

          let Some(idx) = eligible.get(&name_arc).copied() else {
            element.completed.remove(&name_arc);
            continue;
          };

          let duration = super::pick(&after_style.transition_durations, idx, 0.0);
          if duration <= 0.0 {
            element.completed.remove(&name_arc);
            continue;
          }
          let delay = super::pick(&after_style.transition_delays, idx, 0.0);
          let timing = super::pick(
            &after_style.transition_timing_functions,
            idx,
            TransitionTimingFunction::Ease,
          );
          let behavior = super::pick(
            &after_style.transition_behaviors,
            idx,
            TransitionBehavior::Normal,
          );
          let allow_discrete = matches!(behavior, TransitionBehavior::AllowDiscrete);
          let reversing = existing.reversing_adjusted_start_value == after_value;

          if !allow_discrete && is_discrete_property(name) {
            element.completed.remove(&name_arc);
            continue;
          }

          let adjusted_duration = if reversing {
            duration
          } else {
            let old_distance = TransitionRecord::extract_value(&existing.from_style, name, &cmp_ctx)
              .and_then(|from| {
                TransitionRecord::extract_value(&existing.to_style, name, &cmp_ctx)
                  .and_then(|to| transition_value_distance(&from, &to))
              });
            let new_distance = transition_value_distance(&before_value, &after_value);
            match (old_distance, new_distance) {
              (Some(old), Some(new)) if old > 0.0 => duration * (new / old),
              _ => duration,
            }
          };

          let record = if reversing {
            let Some(old_end_value) =
              TransitionRecord::extract_value(&existing.to_style, name, &cmp_ctx)
            else {
              element.completed.remove(&name_arc);
              continue;
            };

            let old_raw_progress = existing.raw_progress(now_ms);
            let timing_output = existing.timing_function.value_at(old_raw_progress);
            let old_factor = existing.reversing_shortening_factor;
            let mut new_factor = (timing_output * old_factor + (1.0 - old_factor)).abs();
            if !new_factor.is_finite() {
              new_factor = 1.0;
            }
            let new_factor = new_factor.clamp(0.0, 1.0);

            let scaled_delay = if delay >= 0.0 { delay } else { delay * new_factor };
            let scaled_duration = duration * new_factor;
            if scaled_duration <= 0.0 {
              element.completed.remove(&name_arc);
              continue;
            }

            let record = start_transition_record(
              name_arc.clone(),
              &before_style_arc,
              &after_style_arc,
              &before_value,
              now_ms,
              scaled_delay,
              scaled_duration,
              timing,
              behavior,
              allow_discrete,
              old_end_value,
              new_factor,
            );
            let Some(record) = record else {
              element.completed.remove(&name_arc);
              continue;
            };
            record
          } else {
            let record = start_transition_record(
              name_arc.clone(),
              &before_style_arc,
              &after_style_arc,
              &before_value,
              now_ms,
              delay,
              adjusted_duration,
              timing,
              behavior,
              allow_discrete,
              before_value.clone(),
              1.0,
            );
            let Some(record) = record else {
              element.completed.remove(&name_arc);
              continue;
            };
            record
          };

          if is_transitionable(
            name,
            &before_value,
            &after_value,
            allow_discrete,
            before_style,
            after_style,
            &cmp_ctx,
          ) {
            element.running.insert(name_arc.clone(), record);
            element.completed.remove(&name_arc);
          } else {
            element.completed.remove(&name_arc);
          }

          continue;
        }

        // No existing transition: only start one if the property is listed in transition-property.
        let Some(idx) = eligible.get(&name_arc).copied() else {
          continue;
        };

        // CSS Transitions 1 step 1: if a completed transition exists and already ended at the
        // after-change style value, don't start a new transition.
        if completed_matches_after {
          continue;
        }

        let Some(before_value) = TransitionRecord::extract_value(before_style, name, &cmp_ctx) else {
          continue;
        };

        if before_value == after_value {
          continue;
        }

        let duration = super::pick(&after_style.transition_durations, idx, 0.0);
        if duration <= 0.0 {
          continue;
        }
        let delay = super::pick(&after_style.transition_delays, idx, 0.0);
        let timing = super::pick(
          &after_style.transition_timing_functions,
          idx,
          TransitionTimingFunction::Ease,
        );
        let behavior = super::pick(
          &after_style.transition_behaviors,
          idx,
          TransitionBehavior::Normal,
        );
        let allow_discrete = matches!(behavior, TransitionBehavior::AllowDiscrete);

        if !allow_discrete && is_discrete_property(name) {
          continue;
        }

        let record = TransitionRecord {
          property: name_arc.clone(),
          start_time_ms: event_time_ms,
          delay_ms: delay,
          duration_ms: duration,
          timing_function: timing,
          transition_behavior: behavior,
          allow_discrete,
          // Use the raw computed styles so percentage-based values resolve correctly at paint-time.
          from_style: before_style_arc.clone(),
          to_style: after_style_arc.clone(),
          reversing_adjusted_start_value: before_value.clone(),
          reversing_shortening_factor: 1.0,
        };

        if is_transitionable(
          name,
          &before_value,
          &after_value,
          allow_discrete,
          before_style,
          after_style,
          &cmp_ctx,
        ) {
          element.running.insert(name_arc.clone(), record);
          element.completed.remove(&name_arc);
        }
      }

      if !element.running.is_empty() || !element.completed.is_empty() {
        next.elements.insert(key, element);
      }
    }

    next
  }
}

fn is_transitionable(
  name: &str,
  before_value: &TransitionValue,
  after_value: &TransitionValue,
  allow_discrete: bool,
  before_style: &ComputedStyle,
  after_style: &ComputedStyle,
  ctx: &AnimationResolveContext,
) -> bool {
  if name.starts_with("--") {
    let TransitionValue::Custom(_before_custom) = before_value else {
      return false;
    };
    let TransitionValue::Custom(_after_custom) = after_value else {
      return false;
    };

    let can_interpolate = can_interpolate_custom_property(before_style, after_style, name);
    return can_interpolate || allow_discrete;
  }

  let TransitionValue::Builtin(before_animated) = before_value else {
    return false;
  };
  let TransitionValue::Builtin(after_animated) = after_value else {
    return false;
  };

  let Some(interpolator) = super::interpolator_for(name) else {
    return false;
  };

  if allow_discrete {
    // Discrete fallback is always available.
    true
  } else {
    // Continuous transitions require interpolation to succeed.
    (interpolator.interpolate)(before_animated, after_animated, 0.5).is_some()
      && (interpolator.extract)(before_style, ctx).is_some()
      && (interpolator.extract)(after_style, ctx).is_some()
  }
}

fn start_transition_record(
  name_arc: Arc<str>,
  before_style_arc: &Arc<ComputedStyle>,
  after_style_arc: &Arc<ComputedStyle>,
  before_value: &TransitionValue,
  now_ms: f32,
  delay_ms: f32,
  duration_ms: f32,
  timing: TransitionTimingFunction,
  behavior: TransitionBehavior,
  allow_discrete: bool,
  reversing_adjusted_start_value: TransitionValue,
  reversing_shortening_factor: f32,
) -> Option<TransitionRecord> {
  let mut start_style = (**before_style_arc).clone();
  if name_arc.starts_with("--") {
    let TransitionValue::Custom(value) = before_value else {
      return None;
    };
    start_style
      .custom_properties
      .insert(name_arc.clone(), value.clone());
  } else {
    let TransitionValue::Builtin(value) = before_value else {
      return None;
    };
    let interpolator = super::interpolator_for(name_arc.as_ref())?;
    (interpolator.apply)(&mut start_style, value);
  }

  Some(TransitionRecord {
    property: name_arc,
    start_time_ms: now_ms,
    delay_ms,
    duration_ms,
    timing_function: timing,
    transition_behavior: behavior,
    allow_discrete,
    from_style: Arc::new(start_style),
    to_style: after_style_arc.clone(),
    reversing_adjusted_start_value,
    reversing_shortening_factor,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::geometry::Rect;
  use crate::style::computed::Visibility;
  use crate::style::display::FormattingContextType;
  use crate::style::types::{TransitionBehavior, TransitionProperty, TransitionTimingFunction};
  use crate::tree::fragment_tree::{FragmentNode, FragmentTree};

  fn make_opacity_style_with_transition(
    opacity: f32,
    transition_properties: Arc<[TransitionProperty]>,
    duration_ms: f32,
    delay_ms: f32,
    timing: TransitionTimingFunction,
  ) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.opacity = opacity;
    style.transition_properties = transition_properties;
    style.transition_durations = Arc::from([duration_ms]);
    style.transition_delays = Arc::from([delay_ms]);
    style.transition_timing_functions = Arc::from([timing]);
    style.transition_behaviors = Arc::from([TransitionBehavior::Normal]);
    Arc::new(style)
  }

  fn make_opacity_style(opacity: f32) -> Arc<ComputedStyle> {
    make_opacity_style_with_transition(
      opacity,
      Arc::from([TransitionProperty::Name("opacity".to_string())]),
      1000.0,
      0.0,
      TransitionTimingFunction::Linear,
    )
  }

  fn make_visibility_style(visibility: Visibility, behavior: TransitionBehavior) -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.visibility = visibility;
    style.transition_properties = Arc::from([TransitionProperty::Name("visibility".to_string())]);
    style.transition_durations = Arc::from([1000.0]);
    style.transition_delays = Arc::from([0.0]);
    style.transition_timing_functions = Arc::from([TransitionTimingFunction::Linear]);
    style.transition_behaviors = Arc::from([behavior]);
    Arc::new(style)
  }

  fn make_box_tree(style: Arc<ComputedStyle>) -> BoxTree {
    let mut node = BoxNode::new_block(style, FormattingContextType::Block, vec![]);
    node.styled_node_id = Some(1);
    BoxTree::new(node)
  }

  fn make_fragment_tree(style: Arc<ComputedStyle>, state: TransitionState) -> FragmentTree {
    let mut root = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 100.0), 1, vec![]);
    root.style = Some(style);
    let mut tree = FragmentTree::with_viewport(root, Size::new(100.0, 100.0));
    tree.transition_state = Some(Arc::new(state));
    tree
  }

  #[test]
  fn transition_state_samples_stably_across_frames() {
    let before_tree = make_box_tree(make_opacity_style(0.0));
    let after_tree = make_box_tree(make_opacity_style(1.0));
    let state = TransitionState::update_for_style_change(None, Some(&before_tree), &after_tree, 0.0);
    let base = make_fragment_tree(after_tree.root.style.clone(), state);

    let viewport = Size::new(100.0, 100.0);

    let mut t100 = base.clone();
    super::super::apply_transitions(&mut t100, 100.0, viewport);
    let style = t100.root.style.as_deref().expect("style");
    assert!((style.opacity - 0.1).abs() < 1e-6, "opacity={}", style.opacity);

    let mut t200 = base.clone();
    super::super::apply_transitions(&mut t200, 200.0, viewport);
    let style = t200.root.style.as_deref().expect("style");
    assert!((style.opacity - 0.2).abs() < 1e-6, "opacity={}", style.opacity);
  }

  #[test]
  fn transition_state_reversing_shortens_duration_with_linear_timing() {
    let tree_a = make_box_tree(make_opacity_style(0.0));
    let tree_b = make_box_tree(make_opacity_style(1.0));

    let state_ab = TransitionState::update_for_style_change(None, Some(&tree_a), &tree_b, 0.0);
    let state_ba =
      TransitionState::update_for_style_change(Some(&state_ab), Some(&tree_b), &tree_a, 200.0);

    let key = ElementKey {
      styled_node_id: 1,
      pseudo: None,
    };
    let record = state_ba
      .elements
      .get(&key)
      .and_then(|el| el.running.get("opacity"))
      .expect("reverse transition record");
    assert!(
      (record.start_time_ms - 200.0).abs() < 1e-6,
      "expected start time at reversal event, got {}",
      record.start_time_ms,
    );
    assert!(
      (record.duration_ms - 200.0).abs() < 1e-6,
      "expected shortened duration, got {}",
      record.duration_ms,
    );
    assert!(
      (record.reversing_shortening_factor - 0.2).abs() < 1e-6,
      "expected reversing shortening factor, got {}",
      record.reversing_shortening_factor,
    );
    assert_eq!(
      record.reversing_adjusted_start_value,
      TransitionValue::Builtin(AnimatedValue::Opacity(1.0))
    );

    let mut t300 = make_fragment_tree(tree_a.root.style.clone(), state_ba.clone());
    super::super::apply_transitions(&mut t300, 300.0, Size::new(100.0, 100.0));
    let style = t300.root.style.as_deref().expect("style");
    assert!((style.opacity - 0.1).abs() < 1e-6, "opacity={}", style.opacity);

    let mut t450 = make_fragment_tree(tree_a.root.style.clone(), state_ba);
    super::super::apply_transitions(&mut t450, 450.0, Size::new(100.0, 100.0));
    let style = t450.root.style.as_deref().expect("style");
    assert!((style.opacity - 0.0).abs() < 1e-6, "opacity={}", style.opacity);
  }

  #[test]
  fn transition_state_reversing_shortens_duration_using_timing_function_output() {
    let tree_a = make_box_tree(make_opacity_style_with_transition(
      0.0,
      Arc::from([TransitionProperty::Name("opacity".to_string())]),
      1000.0,
      0.0,
      TransitionTimingFunction::EaseIn,
    ));
    let tree_b = make_box_tree(make_opacity_style_with_transition(
      1.0,
      Arc::from([TransitionProperty::Name("opacity".to_string())]),
      1000.0,
      0.0,
      TransitionTimingFunction::EaseIn,
    ));

    // Start A -> B at t=0ms, then reverse at 500ms. With ease-in timing, the timing function
    // output at t=0.5 is ~0.315, so the reversed transition should be shortened to ~315ms.
    let state_ab = TransitionState::update_for_style_change(None, Some(&tree_a), &tree_b, 0.0);
    let state_ba =
      TransitionState::update_for_style_change(Some(&state_ab), Some(&tree_b), &tree_a, 500.0);

    let key = ElementKey {
      styled_node_id: 1,
      pseudo: None,
    };
    let old_record = state_ab
      .elements
      .get(&key)
      .and_then(|el| el.running.get("opacity"))
      .expect("original transition record");
    let new_record = state_ba
      .elements
      .get(&key)
      .and_then(|el| el.running.get("opacity"))
      .expect("reverse transition record");

    let expected_factor = TransitionTimingFunction::EaseIn.value_at(0.5);
    let expected_duration = 1000.0 * expected_factor;
    let eps = 1e-4;

    assert!(
      (new_record.start_time_ms - 500.0).abs() < eps,
      "start_time={}",
      new_record.start_time_ms
    );
    assert!(
      (new_record.duration_ms - expected_duration).abs() < eps,
      "expected duration {expected_duration}, got {}",
      new_record.duration_ms
    );
    assert!(
      (new_record.reversing_shortening_factor - expected_factor).abs() < eps,
      "expected factor {expected_factor}, got {}",
      new_record.reversing_shortening_factor
    );
    assert_eq!(
      new_record.reversing_adjusted_start_value,
      TransitionValue::Builtin(AnimatedValue::Opacity(1.0))
    );

    let ctx = default_update_context();
    let old_value = old_record.sample(500.0, &ctx).expect("old sample").value;
    let new_value = new_record.sample(500.0, &ctx).expect("new sample").value;
    assert_eq!(old_value, new_value, "expected reversal to be continuous at t=500ms");
  }

  #[test]
  fn transition_state_repeated_reversals_accumulate_reversing_shortening_factor() {
    let tree_a = make_box_tree(make_opacity_style(0.0));
    let tree_b = make_box_tree(make_opacity_style(1.0));

    // A -> B at t=0, reverse to A at t=200, then reverse back to B at t=250.
    let state_ab = TransitionState::update_for_style_change(None, Some(&tree_a), &tree_b, 0.0);
    let state_ba =
      TransitionState::update_for_style_change(Some(&state_ab), Some(&tree_b), &tree_a, 200.0);
    let state_ab2 =
      TransitionState::update_for_style_change(Some(&state_ba), Some(&tree_a), &tree_b, 250.0);

    let key = ElementKey {
      styled_node_id: 1,
      pseudo: None,
    };
    let old_record = state_ba
      .elements
      .get(&key)
      .and_then(|el| el.running.get("opacity"))
      .expect("first reverse transition record");
    let new_record = state_ab2
      .elements
      .get(&key)
      .and_then(|el| el.running.get("opacity"))
      .expect("second reverse transition record");

    // Derived from the spec formula with linear timing:
    // - first reversal: shortening factor = 0.2 (at t=0.2)
    // - second reversal happens 50ms into a 200ms transition => raw progress 0.25
    //   new factor = 0.25 * 0.2 + (1 - 0.2) = 0.85
    let eps = 1e-6;
    assert!(
      (new_record.start_time_ms - 250.0).abs() < eps,
      "start_time={}",
      new_record.start_time_ms
    );
    assert!(
      (new_record.reversing_shortening_factor - 0.85).abs() < eps,
      "factor={}",
      new_record.reversing_shortening_factor
    );
    assert!(
      (new_record.duration_ms - 850.0).abs() < eps,
      "duration={}",
      new_record.duration_ms
    );
    assert_eq!(
      new_record.reversing_adjusted_start_value,
      TransitionValue::Builtin(AnimatedValue::Opacity(0.0))
    );

    let ctx = default_update_context();
    let old_value = old_record.sample(250.0, &ctx).expect("old sample").value;
    let new_value = new_record.sample(250.0, &ctx).expect("new sample").value;
    assert_eq!(
      old_value, new_value,
      "expected repeated reversal to be continuous at t=250ms"
    );
  }

  #[test]
  fn transition_state_reversing_scales_negative_delay_by_shortening_factor() {
    let tree_a = make_box_tree(make_opacity_style_with_transition(
      0.0,
      Arc::from([TransitionProperty::Name("opacity".to_string())]),
      1000.0,
      -500.0,
      TransitionTimingFunction::Linear,
    ));
    let tree_b = make_box_tree(make_opacity_style_with_transition(
      1.0,
      Arc::from([TransitionProperty::Name("opacity".to_string())]),
      1000.0,
      -500.0,
      TransitionTimingFunction::Linear,
    ));

    let state_ab = TransitionState::update_for_style_change(None, Some(&tree_a), &tree_b, 0.0);
    let state_ba =
      TransitionState::update_for_style_change(Some(&state_ab), Some(&tree_b), &tree_a, 200.0);

    let key = ElementKey {
      styled_node_id: 1,
      pseudo: None,
    };
    let record = state_ba
      .elements
      .get(&key)
      .and_then(|el| el.running.get("opacity"))
      .expect("reverse transition record");

    // With delay=-500ms, at t=200ms the raw progress is (200 - (-500))/1000 = 0.7, so the
    // reversing shortening factor is 0.7. The negative delay is scaled by that factor.
    let eps = 1e-6;
    assert!((record.reversing_shortening_factor - 0.7).abs() < eps, "factor={}", record.reversing_shortening_factor);
    assert!((record.delay_ms - -350.0).abs() < eps, "delay={}", record.delay_ms);
    assert!((record.duration_ms - 700.0).abs() < eps, "duration={}", record.duration_ms);
  }

  #[test]
  fn transition_state_discrete_gating_blocks_visibility_without_allow_discrete() {
    let before_tree = make_box_tree(make_visibility_style(Visibility::Hidden, TransitionBehavior::Normal));
    let after_tree = make_box_tree(make_visibility_style(Visibility::Visible, TransitionBehavior::Normal));
    let state =
      TransitionState::update_for_style_change(None, Some(&before_tree), &after_tree, 0.0);
    assert!(
      state.elements.is_empty(),
      "expected no running transitions for visibility without allow-discrete"
    );

    let before_tree =
      make_box_tree(make_visibility_style(Visibility::Hidden, TransitionBehavior::AllowDiscrete));
    let after_tree =
      make_box_tree(make_visibility_style(Visibility::Visible, TransitionBehavior::AllowDiscrete));
    let state =
      TransitionState::update_for_style_change(None, Some(&before_tree), &after_tree, 0.0);
    let key = ElementKey {
      styled_node_id: 1,
      pseudo: None,
    };
    let has_visibility = state
      .elements
      .get(&key)
      .map(|el| el.running.contains_key("visibility"))
      .unwrap_or(false);
    assert!(has_visibility, "expected visibility transition when allow-discrete is enabled");
  }

  #[test]
  fn transition_state_moves_finished_transitions_to_completed() {
    let before_tree = make_box_tree(make_opacity_style(0.0));
    let after_tree = make_box_tree(make_opacity_style(1.0));
    let state0 = TransitionState::update_for_style_change(None, Some(&before_tree), &after_tree, 0.0);

    let key = ElementKey {
      styled_node_id: 1,
      pseudo: None,
    };
    let element0 = state0.elements.get(&key).expect("element state");
    assert!(element0.running.contains_key("opacity"), "expected running transition");
    assert!(
      !element0.completed.contains_key("opacity"),
      "expected no completed transition initially"
    );

    let state1 =
      TransitionState::update_for_style_change(Some(&state0), Some(&after_tree), &after_tree, 1200.0);
    let element1 = state1.elements.get(&key).expect("element state");
    assert!(
      !element1.running.contains_key("opacity"),
      "expected running transition to be cleared after completion"
    );
    assert!(
      element1.completed.contains_key("opacity"),
      "expected completed transition entry after completion"
    );
  }

  #[test]
  fn transition_state_removes_completed_when_transition_property_no_longer_matches() {
    let before_tree = make_box_tree(make_opacity_style(0.0));
    let after_tree = make_box_tree(make_opacity_style(1.0));
    let state0 = TransitionState::update_for_style_change(None, Some(&before_tree), &after_tree, 0.0);
    let state1 =
      TransitionState::update_for_style_change(Some(&state0), Some(&after_tree), &after_tree, 1200.0);

    let none_style = make_opacity_style_with_transition(
      1.0,
      Arc::from([TransitionProperty::None]),
      1000.0,
      0.0,
      TransitionTimingFunction::Linear,
    );
    let none_tree = make_box_tree(none_style);
    let state2 =
      TransitionState::update_for_style_change(Some(&state1), Some(&after_tree), &none_tree, 1300.0);
    assert!(
      state2.elements.is_empty(),
      "expected completed transition to be removed when transition-property is none"
    );
  }

  #[test]
  fn transition_state_starting_new_transition_clears_completed() {
    let before_tree = make_box_tree(make_opacity_style(0.0));
    let after_tree = make_box_tree(make_opacity_style(1.0));
    let state0 = TransitionState::update_for_style_change(None, Some(&before_tree), &after_tree, 0.0);
    let state1 =
      TransitionState::update_for_style_change(Some(&state0), Some(&after_tree), &after_tree, 1200.0);

    let next_tree = make_box_tree(make_opacity_style(0.0));
    let state2 =
      TransitionState::update_for_style_change(Some(&state1), Some(&after_tree), &next_tree, 2000.0);

    let key = ElementKey {
      styled_node_id: 1,
      pseudo: None,
    };
    let element = state2.elements.get(&key).expect("element state");
    assert!(
      element.running.contains_key("opacity"),
      "expected new running transition to start"
    );
    assert!(
      !element.completed.contains_key("opacity"),
      "expected completed entry to be removed when starting a new transition"
    );
  }

  #[test]
  fn transition_state_removes_completed_when_value_changes_but_no_transition_starts() {
    let before_tree = make_box_tree(make_opacity_style(0.0));
    let after_tree = make_box_tree(make_opacity_style(1.0));
    let state0 = TransitionState::update_for_style_change(None, Some(&before_tree), &after_tree, 0.0);
    let state1 =
      TransitionState::update_for_style_change(Some(&state0), Some(&after_tree), &after_tree, 1200.0);

    let duration_zero_style = make_opacity_style_with_transition(
      0.0,
      Arc::from([TransitionProperty::Name("opacity".to_string())]),
      0.0,
      0.0,
      TransitionTimingFunction::Linear,
    );
    let duration_zero_tree = make_box_tree(duration_zero_style);
    let state2 = TransitionState::update_for_style_change(
      Some(&state1),
      Some(&after_tree),
      &duration_zero_tree,
      2000.0,
    );
    assert!(
      state2.elements.is_empty(),
      "expected completed transition to be removed when after-change value differs and duration is zero"
    );
  }

  #[test]
  fn transition_state_completed_gate_prevents_restart_when_end_matches_after_value() {
    let before_style = make_opacity_style(0.0);
    let after_style = make_opacity_style(1.0);
    let completed_record = TransitionRecord {
      property: Arc::from("opacity"),
      start_time_ms: 0.0,
      delay_ms: 0.0,
      duration_ms: 1000.0,
      timing_function: TransitionTimingFunction::Linear,
      transition_behavior: TransitionBehavior::Normal,
      allow_discrete: false,
      from_style: before_style.clone(),
      to_style: after_style.clone(),
      reversing_adjusted_start_value: TransitionValue::Builtin(AnimatedValue::Opacity(0.0)),
      reversing_shortening_factor: 1.0,
    };

    let key = ElementKey {
      styled_node_id: 1,
      pseudo: None,
    };
    let mut prev_element = ElementTransitionState::default();
    prev_element
      .completed
      .insert(Arc::from("opacity"), completed_record);
    let mut prev_state = TransitionState::default();
    prev_state.elements.insert(key, prev_element);

    let before_tree = make_box_tree(before_style);
    let after_tree = make_box_tree(after_style);
    let next_state =
      TransitionState::update_for_style_change(Some(&prev_state), Some(&before_tree), &after_tree, 0.0);
    let element = next_state.elements.get(&key).expect("element state");
    assert!(
      element.running.get("opacity").is_none(),
      "expected no new transition to start when a matching completed transition exists"
    );
    assert!(
      element.completed.get("opacity").is_some(),
      "expected completed transition to remain present"
    );
  }
}
