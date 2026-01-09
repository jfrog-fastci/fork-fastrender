use webidl_ir::{
  parse_default_value, parse_idl_type_complete, DefaultValue, IdlType, NamedType, NamedTypeKind,
  NumericLiteral, NumericType, StringType, TypeAnnotation,
};

#[test]
fn parse_nested_types() {
  let ty = parse_idl_type_complete("(DOMString or sequence<long> or (Foo or Bar)?)?").unwrap();
  assert_eq!(
    ty,
    IdlType::Nullable(Box::new(IdlType::Union(vec![
      IdlType::String(StringType::DomString),
      IdlType::Sequence(Box::new(IdlType::Numeric(NumericType::Long))),
      IdlType::Nullable(Box::new(IdlType::Union(vec![
        IdlType::Named(NamedType {
          name: "Foo".to_string(),
          kind: NamedTypeKind::Unresolved,
        }),
        IdlType::Named(NamedType {
          name: "Bar".to_string(),
          kind: NamedTypeKind::Unresolved,
        }),
      ]))),
    ])))
  );
}

#[test]
fn flattened_union_members() {
  let ty = parse_idl_type_complete("(DOMString or sequence<long> or (Foo or Bar)?)?").unwrap();
  let flattened = ty.flattened_union_member_types();
  assert_eq!(
    flattened,
    vec![
      IdlType::String(StringType::DomString),
      IdlType::Sequence(Box::new(IdlType::Numeric(NumericType::Long))),
      IdlType::Named(NamedType {
        name: "Foo".to_string(),
        kind: NamedTypeKind::Unresolved,
      }),
      IdlType::Named(NamedType {
        name: "Bar".to_string(),
        kind: NamedTypeKind::Unresolved,
      }),
    ]
  );
}

#[test]
fn parse_default_values() {
  assert_eq!(
    parse_default_value("true").unwrap(),
    DefaultValue::Boolean(true)
  );
  assert_eq!(parse_default_value("null").unwrap(), DefaultValue::Null);
  assert_eq!(
    parse_default_value("-1").unwrap(),
    DefaultValue::Number(NumericLiteral::Integer("-1".to_string()))
  );
  assert_eq!(
    parse_default_value("012").unwrap(),
    DefaultValue::Number(NumericLiteral::Integer("012".to_string()))
  );
  assert_eq!(
    parse_default_value("3.14").unwrap(),
    DefaultValue::Number(NumericLiteral::Decimal("3.14".to_string()))
  );
  assert_eq!(
    parse_default_value("Infinity").unwrap(),
    DefaultValue::Number(NumericLiteral::Infinity { negative: false })
  );
  assert_eq!(
    parse_default_value("NaN").unwrap(),
    DefaultValue::Number(NumericLiteral::NaN)
  );
  assert_eq!(
    parse_default_value("undefined").unwrap(),
    DefaultValue::Undefined
  );
  assert_eq!(
    parse_default_value("\"abc\"").unwrap(),
    DefaultValue::String("abc".to_string())
  );
  assert_eq!(
    parse_default_value("[]").unwrap(),
    DefaultValue::EmptySequence
  );
  assert_eq!(
    parse_default_value("{}").unwrap(),
    DefaultValue::EmptyDictionary
  );

  // WebIDL integer is octal after a leading `0`, so digits 8/9 are not allowed.
  assert!(parse_default_value("08").is_err());
}

#[test]
fn parse_annotated_integer_types() {
  assert_eq!(
    parse_idl_type_complete("[Clamp] unsigned short").unwrap(),
    IdlType::Annotated {
      annotations: vec![TypeAnnotation::Clamp],
      inner: Box::new(IdlType::Numeric(NumericType::UnsignedShort)),
    }
  );
  assert_eq!(
    parse_idl_type_complete("[EnforceRange] long").unwrap(),
    IdlType::Annotated {
      annotations: vec![TypeAnnotation::EnforceRange],
      inner: Box::new(IdlType::Numeric(NumericType::Long)),
    }
  );
}

#[test]
fn failure_cases() {
  assert!(parse_idl_type_complete("(DOMString or long").is_err());
  assert!(parse_idl_type_complete("sequence<long").is_err());
  assert!(parse_default_value("tru").is_err());
}
