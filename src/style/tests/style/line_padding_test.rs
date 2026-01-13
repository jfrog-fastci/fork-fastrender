use crate::css::properties::parse_property_value;
use crate::css::types::Declaration;
use crate::style::properties::apply_declaration;
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
fn line_padding_parses_and_resolves_to_absolute_length() {
  let parent = ComputedStyle::default();

  let mut styles = ComputedStyle::default();
  apply_declaration(
    &mut styles,
    &decl("line-padding", "0"),
    &parent,
    parent.font_size,
    parent.root_font_size,
  );
  assert!((styles.line_padding - 0.0).abs() < 1e-6);

  apply_declaration(
    &mut styles,
    &decl("line-padding", "2px"),
    &parent,
    parent.font_size,
    parent.root_font_size,
  );
  assert!((styles.line_padding - 2.0).abs() < 1e-6);

  // em units resolve against the element's computed font size.
  apply_declaration(
    &mut styles,
    &decl("font-size", "20px"),
    &parent,
    parent.font_size,
    parent.root_font_size,
  );
  apply_declaration(
    &mut styles,
    &decl("line-padding", "0.5em"),
    &parent,
    parent.font_size,
    parent.root_font_size,
  );
  assert!((styles.line_padding - 10.0).abs() < 1e-6);
}

#[test]
fn line_padding_is_inherited() {
  let parent_base = ComputedStyle::default();
  let mut parent = ComputedStyle::default();
  apply_declaration(
    &mut parent,
    &decl("line-padding", "4px"),
    &parent_base,
    parent_base.font_size,
    parent_base.root_font_size,
  );

  let mut child = ComputedStyle::default();
  apply_declaration(
    &mut child,
    &decl("line-padding", "inherit"),
    &parent,
    parent.font_size,
    parent.root_font_size,
  );
  assert!((child.line_padding - 4.0).abs() < 1e-6);
}
