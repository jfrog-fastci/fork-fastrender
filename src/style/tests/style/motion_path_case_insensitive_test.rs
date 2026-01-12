use crate::css::properties::parse_property_value;
use crate::css::types::Declaration;
use crate::style::properties::apply_declaration;
use crate::style::types::MotionPathCommand;
use crate::style::types::MotionPosition;
use crate::style::types::OffsetPath;
use crate::style::types::Ray;
use crate::style::values::Length;
use crate::ComputedStyle;

fn decl(name: &'static str, value: &str) -> Declaration {
  let contains_var = crate::style::var_resolution::contains_var(value);
  Declaration {
    property: name.into(),
    value: parse_property_value(name, value).expect("parse property value"),
    contains_var,
    raw_value: value.to_string(),
    important: false,
  }
}

#[test]
fn motion_path_function_names_are_ascii_case_insensitive() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("offset-path", r#"PATH("M0 0 L10 20")"#),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(
    styles.offset_path,
    OffsetPath::Path(vec![
      MotionPathCommand::MoveTo(MotionPosition {
        x: Length::px(0.0),
        y: Length::px(0.0),
      }),
      MotionPathCommand::LineTo(MotionPosition {
        x: Length::px(10.0),
        y: Length::px(20.0),
      }),
    ])
  );

  apply_declaration(
    &mut styles,
    &decl("offset-path", "RAY(45deg)"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(
    styles.offset_path,
    OffsetPath::Ray(Ray {
      angle: 45.0,
      length: None,
      contain: false,
    })
  );
}
