use fastrender::css::properties::parse_property_value;
use fastrender::css::types::Declaration;
use fastrender::style::properties::apply_declaration;
use fastrender::style::types::MotionPathCommand;
use fastrender::style::types::MotionPosition;
use fastrender::style::types::OffsetPath;
use fastrender::style::types::Ray;
use fastrender::style::values::Length;
use fastrender::ComputedStyle;

fn decl(name: &'static str, value: &str) -> Declaration {
  let contains_var = fastrender::style::var_resolution::contains_var(value);
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

