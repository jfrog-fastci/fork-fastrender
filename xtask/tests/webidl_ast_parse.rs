use xtask::webidl::ast::{Argument, BuiltinType, IdlLiteral, IdlType, InterfaceMember};
use xtask::webidl::parse::{parse_idl_type, parse_interface_member};
use xtask::webidl::{parse_webidl, ParsedDefinition};

#[test]
fn parses_complex_operation_signature() {
  let member = parse_interface_member(
    "undefined addEventListener(DOMString type, EventListener? callback, optional (AddEventListenerOptions or boolean) options = {});",
  )
  .unwrap();

  assert_eq!(
    member,
    InterfaceMember::Operation {
      name: Some("addEventListener".to_string()),
      return_type: IdlType::Builtin(BuiltinType::Undefined),
      arguments: vec![
        Argument {
          name: "type".to_string(),
          type_: IdlType::Builtin(BuiltinType::DOMString),
          optional: false,
          variadic: false,
          default: None,
        },
        Argument {
          name: "callback".to_string(),
          type_: IdlType::Nullable(Box::new(IdlType::Named("EventListener".to_string()))),
          optional: false,
          variadic: false,
          default: None,
        },
        Argument {
          name: "options".to_string(),
          type_: IdlType::Union(vec![
            IdlType::Named("AddEventListenerOptions".to_string()),
            IdlType::Builtin(BuiltinType::Boolean),
          ]),
          optional: true,
          variadic: false,
          default: Some(IdlLiteral::EmptyObject),
        },
      ],
      static_: false,
      stringifier: false,
      special: None,
    }
  );
}

#[test]
fn parses_typedef_union_with_generics() {
  let idl = "typedef (sequence<sequence<ByteString>> or record<ByteString, ByteString>) HeadersInit;";
  let parsed = parse_webidl(idl).unwrap();
  let td = match parsed.definitions.first() {
    Some(ParsedDefinition::Typedef(td)) => td,
    other => panic!("expected typedef, got {other:?}"),
  };

  assert_eq!(td.name, "HeadersInit");

  let ty = parse_idl_type(&td.type_).unwrap();
  assert_eq!(
    ty,
    IdlType::Union(vec![
      IdlType::Sequence(Box::new(IdlType::Sequence(Box::new(IdlType::Builtin(
        BuiltinType::ByteString
      ))))),
      IdlType::Record {
        key: Box::new(IdlType::Builtin(BuiltinType::ByteString)),
        value: Box::new(IdlType::Builtin(BuiltinType::ByteString)),
      }
    ])
  );
}

#[test]
fn parses_iterable_member() {
  let member = parse_interface_member("iterable<ByteString, ByteString>;").unwrap();
  assert_eq!(
    member,
    InterfaceMember::Iterable {
      async_: false,
      key_type: Some(IdlType::Builtin(BuiltinType::ByteString)),
      value_type: IdlType::Builtin(BuiltinType::ByteString),
    }
  );
}

#[test]
fn parses_stringifier_attribute() {
  let member = parse_interface_member("stringifier attribute USVString href;").unwrap();
  assert_eq!(
    member,
    InterfaceMember::Attribute {
      name: "href".to_string(),
      type_: IdlType::Builtin(BuiltinType::USVString),
      readonly: false,
      inherit: false,
      stringifier: true,
      static_: false,
    }
  );
}

