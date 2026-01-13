use crate::regexp_unicode_resolver::{
  resolve_unicode_property_value_expression, GeneralCategory, ResolvedCodePointProperty,
  ResolvedUnicodeProperty, UnicodeStringProperty,
};

#[test]
fn lone_general_category_value_precedence_lu_alias() {
  let resolved = resolve_unicode_property_value_expression("Lu", false).unwrap();
  assert_eq!(
    resolved,
    ResolvedUnicodeProperty::CodePoint(ResolvedCodePointProperty::GeneralCategory(
      GeneralCategory::UppercaseLetter
    ))
  );
}

#[test]
fn lone_general_category_value_precedence_long_name() {
  let resolved = resolve_unicode_property_value_expression("Uppercase_Letter", false).unwrap();
  assert_eq!(
    resolved,
    ResolvedUnicodeProperty::CodePoint(ResolvedCodePointProperty::GeneralCategory(
      GeneralCategory::UppercaseLetter
    ))
  );
}

#[test]
fn non_binary_property_name_without_value_is_error() {
  assert!(resolve_unicode_property_value_expression("Script", false).is_err());
}

#[test]
fn binary_property_with_explicit_value_is_error() {
  assert!(resolve_unicode_property_value_expression("ASCII=Yes", false).is_err());
}

#[test]
fn strict_case_sensitivity_rejects_unknown_aliases() {
  assert!(resolve_unicode_property_value_expression("ASSIGNED", false).is_err());
}

#[test]
fn scx_without_value_is_error() {
  // `scx` is a valid alias for `Script_Extensions`, but non-binary properties require `name=value`.
  assert!(resolve_unicode_property_value_expression("scx", false).is_err());
}

#[test]
fn string_properties_require_unicode_sets() {
  assert!(resolve_unicode_property_value_expression("RGI_Emoji", false).is_err());
  let resolved = resolve_unicode_property_value_expression("RGI_Emoji", true).unwrap();
  assert_eq!(
    resolved,
    ResolvedUnicodeProperty::String(UnicodeStringProperty::RgiEmoji)
  );
}

