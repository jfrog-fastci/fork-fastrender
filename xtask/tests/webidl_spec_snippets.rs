use xtask::webidl::ast::{BuiltinType, IdlLiteral, IdlType, InterfaceMember};
use xtask::webidl::{
  extract_webidl_blocks_from_bikeshed, parse_idl_type, parse_interface_member, parse_webidl,
  ParsedDefinition,
};

#[test]
fn extracts_and_parses_urlsearchparams_constructor_union_init() {
  // Minimal Bikeshed snippet based on the WHATWG URL spec.
  let src = r#"
<pre class=idl>
[Exposed=*]
interface URLSearchParams {
  constructor(optional (sequence<sequence<USVString>> or record<USVString, USVString> or USVString) init = "");
};
</pre>
"#;

  let blocks = extract_webidl_blocks_from_bikeshed(src);
  assert_eq!(blocks.len(), 1);

  let parsed = parse_webidl(&blocks[0]).unwrap();
  let iface = match parsed.definitions.first() {
    Some(ParsedDefinition::Interface(iface)) => iface,
    other => panic!("expected interface definition, got {other:?}"),
  };

  let ctor = iface
    .members
    .iter()
    .find(|m| m.name.as_deref() == Some("constructor"))
    .expect("URLSearchParams constructor member extracted");

  let member = parse_interface_member(&ctor.raw).unwrap();
  let InterfaceMember::Constructor { arguments } = member else {
    panic!("expected constructor member, got {member:?}");
  };

  assert_eq!(arguments.len(), 1);
  let init = &arguments[0];
  assert!(init.optional);
  assert_eq!(init.name, "init");
  assert_eq!(init.default, Some(IdlLiteral::String("".to_string())));

  assert_eq!(
    init.type_,
    IdlType::Union(vec![
      IdlType::Sequence(Box::new(IdlType::Sequence(Box::new(IdlType::Builtin(
        BuiltinType::USVString
      ))))),
      IdlType::Record {
        key: Box::new(IdlType::Builtin(BuiltinType::USVString)),
        value: Box::new(IdlType::Builtin(BuiltinType::USVString)),
      },
      IdlType::Builtin(BuiltinType::USVString),
    ])
  );
}

#[test]
fn extracts_and_parses_fetch_headersinit_typedef_union() {
  // Minimal Bikeshed snippet based on the WHATWG Fetch spec.
  let src = r#"
<pre class=idl>
typedef (sequence<sequence<ByteString>> or record<ByteString, ByteString>) HeadersInit;
</pre>
"#;

  let blocks = extract_webidl_blocks_from_bikeshed(src);
  assert_eq!(blocks.len(), 1);

  let parsed = parse_webidl(&blocks[0]).unwrap();
  let td = match parsed.definitions.first() {
    Some(ParsedDefinition::Typedef(td)) => td,
    other => panic!("expected typedef definition, got {other:?}"),
  };

  assert_eq!(td.name, "HeadersInit");

  // Ensure the type parser handles nested generics inside unions.
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

