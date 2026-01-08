use fastrender::css::types::{Declaration, PropertyValue};
use fastrender::style::properties::{apply_declaration_with_base, DEFAULT_VIEWPORT};
use fastrender::style::types::{
  AnimationComposition, AnimationDirection, AnimationFillMode, AnimationIterationCount,
  AnimationPlayState, TransitionBehavior, TransitionProperty, TransitionTimingFunction,
};
use fastrender::style::ComputedStyle;

#[test]
fn transition_behavior_property_parses_single_value() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();
  let declaration = Declaration {
    property: "transition-behavior".into(),
    value: PropertyValue::Keyword("allow-discrete".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };

  apply_declaration_with_base(
    &mut styles,
    &declaration,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.transition_behaviors,
    vec![TransitionBehavior::AllowDiscrete].into()
  );
}

#[test]
fn transition_behavior_property_parses_comma_separated_list() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();
  let declaration = Declaration {
    property: "transition-behavior".into(),
    value: PropertyValue::Keyword("normal, allow-discrete".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };

  apply_declaration_with_base(
    &mut styles,
    &declaration,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.transition_behaviors,
    vec![TransitionBehavior::Normal, TransitionBehavior::AllowDiscrete].into()
  );
}

#[test]
fn transition_behavior_longhand_overrides_transition_shorthand() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let transition_decl = Declaration {
    property: "transition".into(),
    value: PropertyValue::Keyword("opacity 1s linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &transition_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(styles.transition_behaviors, vec![TransitionBehavior::Normal].into());

  let behavior_decl = Declaration {
    property: "transition-behavior".into(),
    value: PropertyValue::Keyword("allow-discrete".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &behavior_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.transition_behaviors,
    vec![TransitionBehavior::AllowDiscrete].into()
  );
}

#[test]
fn transition_shorthand_resets_transition_behavior_longhand() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let behavior_decl = Declaration {
    property: "transition-behavior".into(),
    value: PropertyValue::Keyword("allow-discrete".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &behavior_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(
    styles.transition_behaviors,
    vec![TransitionBehavior::AllowDiscrete].into()
  );

  let transition_decl = Declaration {
    property: "transition".into(),
    value: PropertyValue::Keyword("opacity 1s linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &transition_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.transition_behaviors, vec![TransitionBehavior::Normal].into());
}

#[test]
fn transition_property_rejects_none_in_comma_list() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "transition-property".into(),
    value: PropertyValue::Keyword("opacity".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(
    styles.transition_properties,
    vec![TransitionProperty::Name("opacity".to_string())].into()
  );

  // `transition-property` is either the keyword `none` or a list that excludes it. `none, opacity`
  // must be invalid and should be ignored.
  let invalid_decl = Declaration {
    property: "transition-property".into(),
    value: PropertyValue::Keyword("none, opacity".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.transition_properties,
    vec![TransitionProperty::Name("opacity".to_string())].into()
  );
}

#[test]
fn transition_property_accepts_none_as_single_value() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let decl = Declaration {
    property: "transition-property".into(),
    value: PropertyValue::Keyword("none".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.transition_properties,
    vec![TransitionProperty::None].into()
  );
}

#[test]
fn transition_shorthand_rejects_none_in_comma_list() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "transition".into(),
    value: PropertyValue::Keyword("opacity 1s linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(
    styles.transition_properties,
    vec![TransitionProperty::Name("opacity".to_string())].into()
  );

  // `transition-property/none` is only valid when it is the sole shorthand entry; mixed lists like
  // `none, opacity 1s` should be invalid and ignored.
  let invalid_decl = Declaration {
    property: "transition".into(),
    value: PropertyValue::Keyword("none, opacity 1s".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.transition_properties,
    vec![TransitionProperty::Name("opacity".to_string())].into()
  );
}

#[test]
fn transition_duration_rejects_negative_values() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "transition-duration".into(),
    value: PropertyValue::Keyword("1s".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(styles.transition_durations, vec![1000.0].into());

  let invalid_decl = Declaration {
    property: "transition-duration".into(),
    value: PropertyValue::Keyword("-1s".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.transition_durations, vec![1000.0].into());
}

#[test]
fn transition_duration_rejects_invalid_tokens() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "transition-duration".into(),
    value: PropertyValue::Keyword("1s".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(styles.transition_durations, vec![1000.0].into());

  let invalid_decl = Declaration {
    property: "transition-duration".into(),
    value: PropertyValue::Keyword("wat".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.transition_durations, vec![1000.0].into());
}

#[test]
fn transition_delay_accepts_negative_values() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let decl = Declaration {
    property: "transition-delay".into(),
    value: PropertyValue::Keyword("-1s".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.transition_delays, vec![-1000.0].into());
}

#[test]
fn transition_shorthand_rejects_negative_duration() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "transition".into(),
    value: PropertyValue::Keyword("opacity 1s linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(
    styles.transition_properties,
    vec![TransitionProperty::Name("opacity".to_string())].into()
  );

  let invalid_decl = Declaration {
    property: "transition".into(),
    value: PropertyValue::Keyword("opacity -1s linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.transition_properties,
    vec![TransitionProperty::Name("opacity".to_string())].into()
  );
}

#[test]
fn transition_timing_function_ignores_invalid_value() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "transition-timing-function".into(),
    value: PropertyValue::Keyword("linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(
    styles.transition_timing_functions,
    vec![TransitionTimingFunction::Linear].into()
  );

  let invalid_decl = Declaration {
    property: "transition-timing-function".into(),
    value: PropertyValue::Keyword("wat".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.transition_timing_functions,
    vec![TransitionTimingFunction::Linear].into()
  );
}

#[test]
fn transition_timing_function_ignores_invalid_comma_list() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "transition-timing-function".into(),
    value: PropertyValue::Keyword("linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  let invalid_decl = Declaration {
    property: "transition-timing-function".into(),
    value: PropertyValue::Keyword("ease-in, wat".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.transition_timing_functions,
    vec![TransitionTimingFunction::Linear].into()
  );
}

#[test]
fn transition_timing_function_parses_valid_comma_list() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let decl = Declaration {
    property: "transition-timing-function".into(),
    value: PropertyValue::Keyword("ease-in, linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.transition_timing_functions,
    vec![TransitionTimingFunction::EaseIn, TransitionTimingFunction::Linear].into()
  );
}

#[test]
fn animation_timing_function_ignores_invalid_value() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "animation-timing-function".into(),
    value: PropertyValue::Keyword("linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(
    styles.animation_timing_functions,
    vec![TransitionTimingFunction::Linear].into()
  );

  let invalid_decl = Declaration {
    property: "animation-timing-function".into(),
    value: PropertyValue::Keyword("wat".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.animation_timing_functions,
    vec![TransitionTimingFunction::Linear].into()
  );
}

#[test]
fn animation_duration_ignores_invalid_value() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "animation-duration".into(),
    value: PropertyValue::Keyword("2s".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(styles.animation_durations, vec![2000.0].into());

  let invalid_decl = Declaration {
    property: "animation-duration".into(),
    value: PropertyValue::Keyword("wat".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.animation_durations, vec![2000.0].into());
}

#[test]
fn animation_duration_rejects_negative_values() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "animation-duration".into(),
    value: PropertyValue::Keyword("2s".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  let invalid_decl = Declaration {
    property: "animation-duration".into(),
    value: PropertyValue::Keyword("-1s".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.animation_durations, vec![2000.0].into());
}

#[test]
fn animation_delay_ignores_invalid_value() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "animation-delay".into(),
    value: PropertyValue::Keyword("1s".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(styles.animation_delays, vec![1000.0].into());

  let invalid_decl = Declaration {
    property: "animation-delay".into(),
    value: PropertyValue::Keyword("wat".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.animation_delays, vec![1000.0].into());
}

#[test]
fn animation_shorthand_rejects_negative_duration() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "animation".into(),
    value: PropertyValue::Keyword("fade 1s linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  let invalid_decl = Declaration {
    property: "animation".into(),
    value: PropertyValue::Keyword("fade -1s linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.animation_durations, vec![1000.0].into());
  assert_eq!(styles.animation_names, vec!["fade".to_string()]);
}

#[test]
fn animation_name_ignores_invalid_value() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "animation-name".into(),
    value: PropertyValue::Keyword("fade".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(styles.animation_names, vec!["fade".to_string()]);

  let invalid_decl = Declaration {
    property: "animation-name".into(),
    value: PropertyValue::Keyword("1s".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.animation_names, vec!["fade".to_string()]);
}

#[test]
fn animation_name_quoted_none_is_not_the_none_keyword() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let decl = Declaration {
    property: "animation-name".into(),
    value: PropertyValue::Keyword("\"none\"".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.animation_names, vec!["none".to_string()]);
}

#[test]
fn animation_iteration_count_ignores_invalid_value() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "animation-iteration-count".into(),
    value: PropertyValue::Keyword("2".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(
    styles.animation_iteration_counts,
    vec![AnimationIterationCount::Count(2.0)].into()
  );

  let invalid_decl = Declaration {
    property: "animation-iteration-count".into(),
    value: PropertyValue::Keyword("wat".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.animation_iteration_counts,
    vec![AnimationIterationCount::Count(2.0)].into()
  );
}

#[test]
fn animation_direction_ignores_invalid_value() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "animation-direction".into(),
    value: PropertyValue::Keyword("reverse".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(styles.animation_directions, vec![AnimationDirection::Reverse].into());

  let invalid_decl = Declaration {
    property: "animation-direction".into(),
    value: PropertyValue::Keyword("wat".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.animation_directions, vec![AnimationDirection::Reverse].into());
}

#[test]
fn animation_fill_mode_ignores_invalid_value() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "animation-fill-mode".into(),
    value: PropertyValue::Keyword("forwards".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(styles.animation_fill_modes, vec![AnimationFillMode::Forwards].into());

  let invalid_decl = Declaration {
    property: "animation-fill-mode".into(),
    value: PropertyValue::Keyword("wat".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.animation_fill_modes, vec![AnimationFillMode::Forwards].into());
}

#[test]
fn animation_play_state_ignores_invalid_value() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "animation-play-state".into(),
    value: PropertyValue::Keyword("paused".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(styles.animation_play_states, vec![AnimationPlayState::Paused].into());

  let invalid_decl = Declaration {
    property: "animation-play-state".into(),
    value: PropertyValue::Keyword("wat".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(styles.animation_play_states, vec![AnimationPlayState::Paused].into());
}

#[test]
fn animation_composition_ignores_invalid_value() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "animation-composition".into(),
    value: PropertyValue::Keyword("accumulate".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );
  assert_eq!(
    styles.animation_compositions,
    vec![AnimationComposition::Accumulate].into()
  );

  let invalid_decl = Declaration {
    property: "animation-composition".into(),
    value: PropertyValue::Keyword("wat".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &invalid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    styles.animation_compositions,
    vec![AnimationComposition::Accumulate].into()
  );
}

#[test]
fn transition_timing_function_rejects_invalid_steps_function() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "transition-timing-function".into(),
    value: PropertyValue::Keyword("linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  for value in ["steps(0)", "steps(1, jump-none)", "steps(5, wat)", "steps(5, start, end)"] {
    let invalid_decl = Declaration {
      property: "transition-timing-function".into(),
      value: PropertyValue::Keyword(value.into()),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    };
    apply_declaration_with_base(
      &mut styles,
      &invalid_decl,
      &parent,
      &ComputedStyle::default(),
      None,
      parent.font_size,
      parent.root_font_size,
      DEFAULT_VIEWPORT,
      false,
    );
    assert_eq!(
      styles.transition_timing_functions,
      vec![TransitionTimingFunction::Linear].into()
    );
  }
}

#[test]
fn transition_timing_function_rejects_invalid_cubic_bezier_function() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let valid_decl = Declaration {
    property: "transition-timing-function".into(),
    value: PropertyValue::Keyword("linear".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &valid_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  // x1/x2 out of range and extra parameters are invalid per CSS Easing.
  for value in [
    "cubic-bezier(-0.1, 0, 0.5, 1)",
    "cubic-bezier(0, 0, 1.1, 1)",
    "cubic-bezier(0, 0, 0, 0, wat)",
  ] {
    let invalid_decl = Declaration {
      property: "transition-timing-function".into(),
      value: PropertyValue::Keyword(value.into()),
      contains_var: false,
      raw_value: String::new(),
      important: false,
    };
    apply_declaration_with_base(
      &mut styles,
      &invalid_decl,
      &parent,
      &ComputedStyle::default(),
      None,
      parent.font_size,
      parent.root_font_size,
      DEFAULT_VIEWPORT,
      false,
    );

    assert_eq!(
      styles.transition_timing_functions,
      vec![TransitionTimingFunction::Linear].into()
    );
  }
}
