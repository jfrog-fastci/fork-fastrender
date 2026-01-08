use fastrender::css::types::{Declaration, PropertyValue};
use fastrender::style::properties::{apply_declaration_with_base, DEFAULT_VIEWPORT};
use fastrender::style::types::{TransitionBehavior, TransitionProperty, TransitionTimingFunction};
use fastrender::style::ComputedStyle;

#[test]
fn transition_shorthand_initial_resets_all_subproperties() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let decl = Declaration {
    property: "transition".into(),
    value: PropertyValue::Keyword("opacity 1s linear 2s allow-discrete".into()),
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
    vec![TransitionProperty::Name("opacity".to_string())].into()
  );
  assert_eq!(styles.transition_durations, vec![1000.0].into());
  assert_eq!(styles.transition_delays, vec![2000.0].into());
  assert_eq!(
    styles.transition_timing_functions,
    vec![TransitionTimingFunction::Linear].into()
  );
  assert_eq!(
    styles.transition_behaviors,
    vec![TransitionBehavior::AllowDiscrete].into()
  );

  let initial_decl = Declaration {
    property: "transition".into(),
    value: PropertyValue::Keyword("initial".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &initial_decl,
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
    vec![TransitionProperty::All].into()
  );
  assert_eq!(styles.transition_durations, vec![0.0].into());
  assert_eq!(styles.transition_delays, vec![0.0].into());
  assert_eq!(
    styles.transition_timing_functions,
    vec![TransitionTimingFunction::Ease].into()
  );
  assert_eq!(
    styles.transition_behaviors,
    vec![TransitionBehavior::Normal].into()
  );
}

#[test]
fn transition_shorthand_inherit_copies_all_subproperties() {
  let base_parent = ComputedStyle::default();
  let mut parent = ComputedStyle::default();
  let decl = Declaration {
    property: "transition".into(),
    value: PropertyValue::Keyword("opacity 1s linear 2s allow-discrete".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut parent,
    &decl,
    &base_parent,
    &ComputedStyle::default(),
    None,
    base_parent.font_size,
    base_parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  let mut child = ComputedStyle::default();
  let inherit_decl = Declaration {
    property: "transition".into(),
    value: PropertyValue::Keyword("inherit".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut child,
    &inherit_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(child.transition_properties, parent.transition_properties);
  assert_eq!(child.transition_durations, parent.transition_durations);
  assert_eq!(child.transition_delays, parent.transition_delays);
  assert_eq!(
    child.transition_timing_functions,
    parent.transition_timing_functions
  );
  assert_eq!(child.transition_behaviors, parent.transition_behaviors);
}

#[test]
fn transition_property_inherit_copies_from_parent() {
  let base_parent = ComputedStyle::default();
  let mut parent = ComputedStyle::default();
  let decl = Declaration {
    property: "transition-property".into(),
    value: PropertyValue::Keyword("opacity".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut parent,
    &decl,
    &base_parent,
    &ComputedStyle::default(),
    None,
    base_parent.font_size,
    base_parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  let mut child = ComputedStyle::default();
  let inherit_decl = Declaration {
    property: "transition-property".into(),
    value: PropertyValue::Keyword("inherit".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut child,
    &inherit_decl,
    &parent,
    &ComputedStyle::default(),
    None,
    parent.font_size,
    parent.root_font_size,
    DEFAULT_VIEWPORT,
    false,
  );

  assert_eq!(
    child.transition_properties,
    vec![TransitionProperty::Name("opacity".to_string())].into()
  );
}

#[test]
fn transition_behavior_initial_resets_to_normal() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  let allow_decl = Declaration {
    property: "transition-behavior".into(),
    value: PropertyValue::Keyword("allow-discrete".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &allow_decl,
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

  let initial_decl = Declaration {
    property: "transition-behavior".into(),
    value: PropertyValue::Keyword("initial".into()),
    contains_var: false,
    raw_value: String::new(),
    important: false,
  };
  apply_declaration_with_base(
    &mut styles,
    &initial_decl,
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
    vec![TransitionBehavior::Normal].into()
  );
}

