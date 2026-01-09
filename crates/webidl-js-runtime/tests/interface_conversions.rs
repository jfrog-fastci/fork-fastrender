use webidl_ir::{IdlType, NamedType, NamedTypeKind};
use webidl_js_runtime::conversions::{convert_to_interface, convert_to_interface_opaque, to_interface_opaque};
use webidl_js_runtime::{VmJsRuntime, WebIdlJsRuntime};
use vm_js::Value;

fn interface_type(name: &str) -> IdlType {
  IdlType::Named(NamedType {
    name: name.to_string(),
    kind: NamedTypeKind::Interface,
  })
}

#[test]
fn interface_conversion_accepts_platform_object() {
  let mut rt = VmJsRuntime::new();
  let opaque = 1234u64;
  let obj = rt
    .alloc_platform_object_value("Node", &["EventTarget"], opaque)
    .unwrap();

  let ty = interface_type("Node");
  let got = convert_to_interface(&mut rt, obj, &ty).unwrap();
  assert_eq!(got, obj);
  assert_eq!(rt.platform_object_opaque(got), Some(opaque));
  assert!(rt.implements_interface(got, "Node"));
  assert_eq!(to_interface_opaque(&mut rt, got, "Node").unwrap(), opaque);
  assert_eq!(
    convert_to_interface_opaque(&mut rt, got, &ty).unwrap(),
    Some(opaque)
  );
}

#[test]
fn interface_conversion_rejects_wrong_interface() {
  let mut rt = VmJsRuntime::new();
  let obj = rt
    .alloc_platform_object_value("Node", &["EventTarget"], 1)
    .unwrap();

  let ty = interface_type("Document");
  assert!(convert_to_interface(&mut rt, obj, &ty).is_err());
  assert!(convert_to_interface_opaque(&mut rt, obj, &ty).is_err());
}

#[test]
fn interface_conversion_rejects_non_platform_objects() {
  let mut rt = VmJsRuntime::new();
  let obj = rt.alloc_object_value().unwrap();

  let ty = interface_type("Node");
  assert!(convert_to_interface(&mut rt, obj, &ty).is_err());
  assert!(convert_to_interface_opaque(&mut rt, obj, &ty).is_err());
  assert!(!WebIdlJsRuntime::implements_interface(&rt, obj, "Node"));
}

#[test]
fn nullable_interface_conversion_coerces_null_or_undefined_to_null() {
  let mut rt = VmJsRuntime::new();
  let ty = IdlType::Nullable(Box::new(interface_type("Node")));

  let got = convert_to_interface(&mut rt, Value::Undefined, &ty).unwrap();
  assert_eq!(got, Value::Null);
  assert_eq!(
    convert_to_interface_opaque(&mut rt, Value::Undefined, &ty).unwrap(),
    None
  );

  let got = convert_to_interface(&mut rt, Value::Null, &ty).unwrap();
  assert_eq!(got, Value::Null);
  assert_eq!(
    convert_to_interface_opaque(&mut rt, Value::Null, &ty).unwrap(),
    None
  );
}
