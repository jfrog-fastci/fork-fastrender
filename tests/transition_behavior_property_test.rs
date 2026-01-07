use fastrender::css::types::{Declaration, PropertyValue};
use fastrender::style::properties::{apply_declaration_with_base, DEFAULT_VIEWPORT};
use fastrender::style::types::TransitionBehavior;
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
