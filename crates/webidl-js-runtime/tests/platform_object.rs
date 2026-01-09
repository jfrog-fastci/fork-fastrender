use vm_js::Value;
use webidl_js_runtime::{InterfaceId, VmJsRuntime, WebIdlJsRuntime};

#[test]
fn platform_object_branding_and_opaque() {
  let mut rt = VmJsRuntime::new();
  let id = 0x1bad_f00d_u64;

  let obj = rt
    .alloc_platform_object_value("Node", &["EventTarget"], id)
    .unwrap();

  // String-based interface checks (used by current WebIDL interface conversions).
  assert!(rt.implements_interface(obj, "Node"));
  assert!(rt.implements_interface(obj, "EventTarget"));
  assert!(!rt.implements_interface(obj, "Document"));

  // Ensure the trait hook is usable by generic WebIDL conversion code.
  assert!(WebIdlJsRuntime::implements_interface(&rt, obj, "Node"));
  assert_eq!(WebIdlJsRuntime::platform_object_opaque(&rt, obj), Some(id));

  // InterfaceId-based hooks (intended for generated bindings).
  let node = InterfaceId::from_name("Node");
  let event_target = InterfaceId::from_name("EventTarget");
  let document = InterfaceId::from_name("Document");
  assert!(rt.hooks().is_platform_object(obj));
  assert!(rt.hooks().implements_interface(obj, node));
  assert!(rt.hooks().implements_interface(obj, event_target));
  assert!(!rt.hooks().implements_interface(obj, document));

  assert_eq!(rt.platform_object_primary_interface(obj), Some("Node"));
  assert_eq!(rt.platform_object_opaque(obj), Some(id));
}

#[test]
fn non_platform_objects_are_not_branded() {
  let mut rt = VmJsRuntime::new();

  let obj = rt.alloc_object_value().unwrap();
  assert!(!rt.implements_interface(obj, "Node"));
  assert!(!WebIdlJsRuntime::implements_interface(&rt, obj, "Node"));
  assert!(!rt.hooks().is_platform_object(obj));
  assert!(!rt.hooks().implements_interface(obj, InterfaceId::from_name("Node")));
  assert_eq!(rt.platform_object_primary_interface(obj), None);
  assert_eq!(rt.platform_object_opaque(obj), None);
  assert_eq!(WebIdlJsRuntime::platform_object_opaque(&rt, obj), None);

  let str_obj = rt.alloc_string_object_value("hello").unwrap();
  assert!(!rt.implements_interface(str_obj, "Node"));
  assert!(!WebIdlJsRuntime::implements_interface(&rt, str_obj, "Node"));
  assert!(!rt.hooks().is_platform_object(str_obj));
  assert!(!rt.hooks().implements_interface(str_obj, InterfaceId::from_name("Node")));
  assert_eq!(rt.platform_object_primary_interface(str_obj), None);
  assert_eq!(rt.platform_object_opaque(str_obj), None);
  assert_eq!(
    WebIdlJsRuntime::platform_object_opaque(&rt, str_obj),
    None
  );

  assert!(!rt.implements_interface(Value::Undefined, "Node"));
  assert!(!WebIdlJsRuntime::implements_interface(&rt, Value::Undefined, "Node"));
  assert!(!rt.hooks().is_platform_object(Value::Undefined));
  assert!(!rt.hooks().implements_interface(Value::Undefined, InterfaceId::from_name("Node")));
  assert_eq!(rt.platform_object_primary_interface(Value::Undefined), None);
  assert_eq!(rt.platform_object_opaque(Value::Undefined), None);
  assert_eq!(
    WebIdlJsRuntime::platform_object_opaque(&rt, Value::Undefined),
    None
  );
}

