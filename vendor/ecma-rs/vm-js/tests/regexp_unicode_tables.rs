#[path = "../src/regexp_unicode_tables.rs"]
mod regexp_unicode_tables;

use regexp_unicode_tables::{
  contains_code_point, resolve_property_name, resolve_property_value, BinaryProp, GeneralCategory,
  NonBinaryProp, NonBinaryValue, ResolvedCodePointProperty, Script, UnicodePropertyName,
};

#[test]
fn resolver_is_strict_and_case_sensitive() {
  assert!(matches!(
    resolve_property_name("Assigned", false),
    Some(UnicodePropertyName::Binary(BinaryProp::Assigned))
  ));
  assert!(resolve_property_name("ASSIGNED", false).is_none());

  assert!(matches!(
    resolve_property_name("scx", false),
    Some(UnicodePropertyName::NonBinary(NonBinaryProp::Script_Extensions))
  ));
  assert!(resolve_property_name("Scx", false).is_none());

  assert!(resolve_property_name("script_extensions", false).is_none());
  assert!(resolve_property_name("Block", false).is_none());
}

#[test]
fn surrogates_are_included_as_code_points() {
  assert!(contains_code_point(
    ResolvedCodePointProperty::Binary(BinaryProp::Any),
    0xD800
  ));
  assert!(contains_code_point(
    ResolvedCodePointProperty::Binary(BinaryProp::Any),
    0xDFFF
  ));

  let gc = resolve_property_value(NonBinaryProp::General_Category, "Surrogate")
    .and_then(|v| match v {
      NonBinaryValue::GeneralCategory(gc) => Some(gc),
      _ => None,
    })
    .unwrap();
  assert_eq!(gc, GeneralCategory::Surrogate);
  assert!(contains_code_point(
    ResolvedCodePointProperty::GeneralCategory(gc),
    0xD800
  ));

  let sc = resolve_property_value(NonBinaryProp::Script, "Unknown")
    .and_then(|v| match v {
      NonBinaryValue::Script(sc) => Some(sc),
      _ => None,
    })
    .unwrap();
  assert_eq!(sc, Script::Unknown);
  assert!(contains_code_point(
    ResolvedCodePointProperty::Script(sc),
    0xD800
  ));
}

#[test]
fn known_positives() {
  assert!(contains_code_point(
    ResolvedCodePointProperty::Binary(BinaryProp::ASCII),
    0x41
  ));

  let greek = resolve_property_value(NonBinaryProp::Script, "Greek")
    .and_then(|v| match v {
      NonBinaryValue::Script(sc) => Some(sc),
      _ => None,
    })
    .unwrap();
  assert_eq!(greek, Script::Greek);
  assert!(contains_code_point(
    ResolvedCodePointProperty::Script(greek),
    0x0370
  ));
}

