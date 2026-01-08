use xtask::webidl::{
  analyze_resolved_world, parse_webidl, BuiltinType, IdlLiteral, IdlType, InterfaceMember,
};
use xtask::webidl::resolve::resolve_webidl_world;

#[test]
fn parses_argument_level_ext_attrs_enforce_range() {
  let idl = r#"
    interface AbortSignal {
      static AbortSignal timeout([EnforceRange] unsigned long long milliseconds);
    };
  "#;

  let parsed = parse_webidl(idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);
  let analyzed = analyze_resolved_world(&resolved);

  let iface = analyzed.interfaces.get("AbortSignal").expect("AbortSignal parsed");
  let timeout_args = iface
    .members
    .iter()
    .find_map(|m| match &m.parsed {
      InterfaceMember::Operation {
        name: Some(name),
        arguments,
        ..
      } if name == "timeout" => Some(arguments),
      _ => None,
    })
    .expect("timeout operation parsed");

  assert_eq!(timeout_args.len(), 1);
  assert!(
    timeout_args[0].ext_attrs.iter().any(|a| a.name == "EnforceRange"),
    "expected [EnforceRange] on AbortSignal.timeout(milliseconds)"
  );

  assert_eq!(timeout_args[0].name, "milliseconds");
  assert_eq!(
    timeout_args[0].type_,
    IdlType::Builtin(BuiltinType::UnsignedLongLong)
  );
}

#[test]
fn parses_mixed_optional_default_variadic_with_selective_ext_attrs() {
  let idl = r#"
    interface Foo {
      undefined bar(
        [Clamp] long x,
        /* comment, [ignored] */ optional DOMString y = "a,b",
        [EnforceRange] unsigned long long... zs
      );
    };
  "#;

  let parsed = parse_webidl(idl).unwrap();
  let resolved = resolve_webidl_world(&parsed);
  let analyzed = analyze_resolved_world(&resolved);

  let iface = analyzed.interfaces.get("Foo").expect("Foo parsed");
  let bar_args = iface
    .members
    .iter()
    .find_map(|m| match &m.parsed {
      InterfaceMember::Operation {
        name: Some(name),
        arguments,
        ..
      } if name == "bar" => Some(arguments),
      _ => None,
    })
    .expect("bar operation parsed");

  assert_eq!(bar_args.len(), 3);

  let x = &bar_args[0];
  assert_eq!(x.name, "x");
  assert_eq!(x.type_, IdlType::Builtin(BuiltinType::Long));
  assert!(!x.optional);
  assert!(!x.variadic);
  assert!(x.ext_attrs.iter().any(|a| a.name == "Clamp"));

  let y = &bar_args[1];
  assert_eq!(y.name, "y");
  assert_eq!(y.type_, IdlType::Builtin(BuiltinType::DOMString));
  assert!(y.optional);
  assert_eq!(y.default, Some(IdlLiteral::String("a,b".to_string())));
  assert!(y.ext_attrs.is_empty());

  let zs = &bar_args[2];
  assert_eq!(zs.name, "zs");
  assert_eq!(zs.type_, IdlType::Builtin(BuiltinType::UnsignedLongLong));
  assert!(!zs.optional);
  assert!(zs.variadic);
  assert!(zs.ext_attrs.iter().any(|a| a.name == "EnforceRange"));
}
