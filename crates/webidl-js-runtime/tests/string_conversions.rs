use vm_js::Value;
use webidl_ir::{IdlType, StringType, TypeAnnotation, TypeContext};
use webidl_js_runtime::{convert_to_idl, ConvertedValue, JsRuntime, VmJsRuntime};

#[test]
fn usvstring_replaces_lone_surrogates() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  // Allocate a JS string containing a lone surrogate (U+D800) via raw UTF-16 code units.
  let surrogate = rt.alloc_string_from_code_units(&[0xD800]).unwrap();

  let ty = IdlType::String(StringType::UsvString);
  let out = convert_to_idl(&mut rt, surrogate, &ty, &ctx).unwrap();
  let ConvertedValue::String(out) = out else {
    panic!("expected USVString conversion to yield ConvertedValue::String");
  };
  assert_eq!(out, "\u{FFFD}");
}

#[test]
fn legacy_null_to_empty_string_domstring() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let ty = IdlType::Annotated {
    annotations: vec![TypeAnnotation::LegacyNullToEmptyString],
    inner: Box::new(IdlType::String(StringType::DomString)),
  };

  let out = convert_to_idl(&mut rt, Value::Null, &ty, &ctx).unwrap();
  let ConvertedValue::String(out) = out else {
    panic!("expected DOMString conversion to yield ConvertedValue::String");
  };
  assert_eq!(out, "");
}

#[test]
fn legacy_null_to_empty_string_usvstring() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let ty = IdlType::Annotated {
    annotations: vec![TypeAnnotation::LegacyNullToEmptyString],
    inner: Box::new(IdlType::String(StringType::UsvString)),
  };

  let out = convert_to_idl(&mut rt, Value::Null, &ty, &ctx).unwrap();
  let ConvertedValue::String(out) = out else {
    panic!("expected USVString conversion to yield ConvertedValue::String");
  };
  assert_eq!(out, "");
}

#[test]
fn domstring_null_uses_js_to_string_semantics_without_legacy_attr() {
  let mut rt = VmJsRuntime::new();
  let ctx = TypeContext::default();

  let ty = IdlType::String(StringType::DomString);
  let out = convert_to_idl(&mut rt, Value::Null, &ty, &ctx).unwrap();
  let ConvertedValue::String(out) = out else {
    panic!("expected DOMString conversion to yield ConvertedValue::String");
  };
  assert_eq!(out, "null");
}
