use webidl_ir::{DefaultValue, IdlType, NumericType, TypeAnnotation};
use xtask::webidl::parse_dictionary_member;

#[test]
fn parses_dom_style_dictionary_member_boolean_default() {
  let parsed = parse_dictionary_member("boolean capture = false;").unwrap();
  assert!(parsed.ext_attrs.is_empty());
  assert_eq!(parsed.schema.name, "capture");
  assert!(!parsed.schema.required);
  assert_eq!(parsed.schema.ty, IdlType::Boolean);
  assert_eq!(parsed.schema.default, Some(DefaultValue::Boolean(false)));
}

#[test]
fn parses_required_dictionary_member_with_type_annotations() {
  let parsed = parse_dictionary_member("[EnforceRange] required unsigned long long milliseconds;").unwrap();
  assert!(parsed.ext_attrs.iter().any(|a| a.name == "EnforceRange"));
  assert!(parsed.schema.required);
  assert_eq!(parsed.schema.name, "milliseconds");
  assert_eq!(
    parsed.schema.ty,
    IdlType::Annotated {
      annotations: vec![TypeAnnotation::EnforceRange],
      inner: Box::new(IdlType::Numeric(NumericType::UnsignedLongLong)),
    }
  );
}

