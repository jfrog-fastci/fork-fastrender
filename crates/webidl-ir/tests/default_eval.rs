use webidl_ir::{
  eval_default_value, parse_default_value, parse_idl_type_complete, DictionaryMemberSchema,
  DictionarySchema, IdlType, NamedType, NamedTypeKind, TypeContext, WebIdlException, WebIdlValue,
};

#[test]
fn optional_long_default_5() {
  let ctx = TypeContext::default();
  let ty = parse_idl_type_complete("long").unwrap();
  let dv = parse_default_value("5").unwrap();
  assert_eq!(
    eval_default_value(&ty, &dv, &ctx).unwrap(),
    WebIdlValue::Long(5)
  );
}

#[test]
fn octal_integer_defaults_follow_webidl_rules() {
  let ctx = TypeContext::default();
  let ty = parse_idl_type_complete("long").unwrap();

  // WebIDL `integer` uses octal when the token begins with `0` (and is not hex).
  let dv = parse_default_value("012").unwrap();
  assert_eq!(
    eval_default_value(&ty, &dv, &ctx).unwrap(),
    WebIdlValue::Long(10)
  );
}

#[test]
fn strings_domstring_usvstring_and_bytestring() {
  let ctx = TypeContext::default();
  let dv = parse_default_value("\"€\"").unwrap();

  let dom_ty = parse_idl_type_complete("DOMString").unwrap();
  assert_eq!(
    eval_default_value(&dom_ty, &dv, &ctx).unwrap(),
    WebIdlValue::String("€".to_string())
  );

  let usv_ty = parse_idl_type_complete("USVString").unwrap();
  assert_eq!(
    eval_default_value(&usv_ty, &dv, &ctx).unwrap(),
    WebIdlValue::String("€".to_string())
  );

  let byte_ty = parse_idl_type_complete("ByteString").unwrap();
  assert!(matches!(
    eval_default_value(&byte_ty, &dv, &ctx),
    Err(WebIdlException::TypeError { .. })
  ));
}

#[test]
fn enum_default_validation() {
  let mut ctx = TypeContext::default();
  ctx.add_enum("Color", ["red", "green"]);

  let ty = parse_idl_type_complete("Color").unwrap();

  let ok = parse_default_value("\"red\"").unwrap();
  assert_eq!(
    eval_default_value(&ty, &ok, &ctx).unwrap(),
    WebIdlValue::Enum("red".to_string())
  );

  let bad = parse_default_value("\"blue\"").unwrap();
  assert!(matches!(
    eval_default_value(&ty, &bad, &ctx),
    Err(WebIdlException::TypeError { .. })
  ));
}

#[test]
fn dictionary_empty_object_default_populates_member_defaults_including_inherited() {
  let mut ctx = TypeContext::default();
  ctx.add_dictionary(DictionarySchema {
    name: "Base".to_string(),
    inherits: None,
    members: vec![DictionaryMemberSchema {
      name: "base".to_string(),
      required: false,
      ty: parse_idl_type_complete("long").unwrap(),
      default: Some(parse_default_value("5").unwrap()),
    }],
  });
  ctx.add_dictionary(DictionarySchema {
    name: "Derived".to_string(),
    inherits: Some("Base".to_string()),
    members: vec![
      DictionaryMemberSchema {
        name: "flag".to_string(),
        required: false,
        ty: parse_idl_type_complete("boolean").unwrap(),
        default: Some(parse_default_value("false").unwrap()),
      },
      DictionaryMemberSchema {
        name: "optional".to_string(),
        required: false,
        ty: parse_idl_type_complete("DOMString").unwrap(),
        default: None,
      },
    ],
  });

  let ty = parse_idl_type_complete("Derived").unwrap();
  let dv = parse_default_value("{}").unwrap();
  let out = eval_default_value(&ty, &dv, &ctx).unwrap();

  let WebIdlValue::Dictionary { name, members } = out else {
    panic!("expected dictionary value");
  };
  assert_eq!(name, "Derived");
  assert_eq!(members.get("base"), Some(&WebIdlValue::Long(5)));
  assert_eq!(members.get("flag"), Some(&WebIdlValue::Boolean(false)));
  assert!(!members.contains_key("optional"));
}

#[test]
fn empty_sequence_default_for_sequence_domstring() {
  let ctx = TypeContext::default();
  let ty = parse_idl_type_complete("sequence<DOMString>").unwrap();
  let dv = parse_default_value("[]").unwrap();
  let out = eval_default_value(&ty, &dv, &ctx).unwrap();
  assert!(matches!(
    out,
    WebIdlValue::Sequence { values, .. } if values.is_empty()
  ));
}

#[test]
fn invalid_defaults_are_type_errors() {
  let ctx = TypeContext::default();

  // `[]` for non-sequence.
  let ty = parse_idl_type_complete("DOMString").unwrap();
  let dv = parse_default_value("[]").unwrap();
  assert!(matches!(
    eval_default_value(&ty, &dv, &ctx),
    Err(WebIdlException::TypeError { .. })
  ));

  // `null` for non-nullable.
  let ty = parse_idl_type_complete("DOMString").unwrap();
  let dv = parse_default_value("null").unwrap();
  assert!(matches!(
    eval_default_value(&ty, &dv, &ctx),
    Err(WebIdlException::TypeError { .. })
  ));
}

#[test]
fn dictionary_empty_object_default_is_error_when_required_members_present() {
  let mut ctx = TypeContext::default();
  ctx.add_dictionary(DictionarySchema {
    name: "HasRequired".to_string(),
    inherits: None,
    members: vec![DictionaryMemberSchema {
      name: "must".to_string(),
      required: true,
      ty: parse_idl_type_complete("double").unwrap(),
      default: None,
    }],
  });

  let ty = parse_idl_type_complete("HasRequired").unwrap();
  let dv = parse_default_value("{}").unwrap();
  assert!(matches!(
    eval_default_value(&ty, &dv, &ctx),
    Err(WebIdlException::TypeError { .. })
  ));
}

#[test]
fn union_defaults_choose_the_unique_matching_member() {
  let mut ctx = TypeContext::default();
  ctx.add_dictionary(DictionarySchema {
    name: "Opts".to_string(),
    inherits: None,
    members: vec![DictionaryMemberSchema {
      name: "flag".to_string(),
      required: false,
      ty: parse_idl_type_complete("boolean").unwrap(),
      default: Some(parse_default_value("true").unwrap()),
    }],
  });

  let ty = parse_idl_type_complete("(Opts or DOMString)").unwrap();
  let dv = parse_default_value("{}").unwrap();
  let out = eval_default_value(&ty, &dv, &ctx).unwrap();

  let WebIdlValue::Union { member_ty, value } = out else {
    panic!("expected union value");
  };
  assert_eq!(
    *member_ty,
    IdlType::Named(NamedType {
      name: "Opts".to_string(),
      kind: NamedTypeKind::Unresolved,
    })
  );
  let WebIdlValue::Dictionary { members, .. } = *value else {
    panic!("expected dictionary value");
  };
  assert_eq!(members.get("flag"), Some(&WebIdlValue::Boolean(true)));

  // Ambiguous string default between DOMString and ByteString.
  let ty = parse_idl_type_complete("(DOMString or ByteString)").unwrap();
  let dv = parse_default_value("\"abc\"").unwrap();
  assert!(matches!(
    eval_default_value(&ty, &dv, &TypeContext::default()),
    Err(WebIdlException::TypeError { .. })
  ));
}

#[test]
fn enforce_range_integer_default_is_checked() {
  let ctx = TypeContext::default();
  let ty = parse_idl_type_complete("[EnforceRange] byte").unwrap();
  let dv = parse_default_value("200").unwrap();
  assert!(matches!(
    eval_default_value(&ty, &dv, &ctx),
    Err(WebIdlException::RangeError { .. })
  ));
}

#[test]
fn undefined_default_requires_undefined_or_any() {
  let ctx = TypeContext::default();
  let ty = parse_idl_type_complete("DOMString").unwrap();
  let dv = parse_default_value("undefined").unwrap();
  assert!(matches!(
    eval_default_value(&ty, &dv, &ctx),
    Err(WebIdlException::TypeError { .. })
  ));

  let ty = parse_idl_type_complete("any").unwrap();
  assert_eq!(
    eval_default_value(&ty, &dv, &ctx).unwrap(),
    WebIdlValue::Undefined
  );
}
