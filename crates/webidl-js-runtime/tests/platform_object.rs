use vm_js::Value;
use webidl_js_runtime::{VmJsRuntime, WebIdlJsRuntime};

#[test]
fn platform_object_branding_and_opaque() {
  let mut rt = VmJsRuntime::new();
  let id = 0x1bad_f00d_u64;

  let obj = rt
    .alloc_platform_object_value("Node", &["EventTarget"], id)
    .unwrap();

  assert!(rt.implements_interface(obj, "Node"));
  assert!(rt.implements_interface(obj, "EventTarget"));
  assert!(!rt.implements_interface(obj, "Document"));

  // Ensure the trait hook is usable by generic WebIDL conversion code.
  assert!(WebIdlJsRuntime::implements_interface(&rt, obj, "Node"));
  assert_eq!(WebIdlJsRuntime::platform_object_opaque(&rt, obj), Some(id));

  assert_eq!(rt.platform_object_primary_interface(obj), Some("Node"));
  assert_eq!(rt.platform_object_opaque(obj), Some(id));
}

#[test]
fn non_platform_objects_are_not_branded() {
  let mut rt = VmJsRuntime::new();

  let obj = rt.alloc_object_value().unwrap();
  assert!(!rt.implements_interface(obj, "Node"));
  assert!(!WebIdlJsRuntime::implements_interface(&rt, obj, "Node"));
  assert_eq!(rt.platform_object_primary_interface(obj), None);
  assert_eq!(rt.platform_object_opaque(obj), None);
  assert_eq!(WebIdlJsRuntime::platform_object_opaque(&rt, obj), None);

  let str_obj = rt.alloc_string_object_value("hello").unwrap();
  assert!(!rt.implements_interface(str_obj, "Node"));
  assert!(!WebIdlJsRuntime::implements_interface(&rt, str_obj, "Node"));
  assert_eq!(rt.platform_object_primary_interface(str_obj), None);
  assert_eq!(rt.platform_object_opaque(str_obj), None);
  assert_eq!(
    WebIdlJsRuntime::platform_object_opaque(&rt, str_obj),
    None
  );

  assert!(!rt.implements_interface(Value::Undefined, "Node"));
  assert!(!WebIdlJsRuntime::implements_interface(&rt, Value::Undefined, "Node"));
  assert_eq!(rt.platform_object_primary_interface(Value::Undefined), None);
  assert_eq!(rt.platform_object_opaque(Value::Undefined), None);
  assert_eq!(
    WebIdlJsRuntime::platform_object_opaque(&rt, Value::Undefined),
    None
  );
}
