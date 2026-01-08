use fastrender::css::types::{Declaration, PropertyValue};
use fastrender::style::properties::{apply_declaration_with_base, DEFAULT_VIEWPORT};
use fastrender::style::types::{TransitionBehavior, TransitionProperty};
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
