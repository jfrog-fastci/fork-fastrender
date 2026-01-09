use vm_js::{Value, VmError};
use webidl_js_runtime::{JsRuntime, VmJsRuntime, WebIdlJsRuntime};

fn assert_type_error(rt: &mut VmJsRuntime, err: VmError) {
  let VmError::Throw(thrown) = err else {
    panic!("expected TypeError throw, got {err:?}");
  };
  let s = rt.to_string(thrown).expect("error to_string should not throw");
  let Value::String(s) = s else {
    panic!("expected string");
  };
  let text = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert!(
    text.starts_with("TypeError"),
    "expected TypeError, got {text:?}"
  );
}

#[test]
fn to_object_throws_on_undefined_and_null() {
  let mut rt = VmJsRuntime::new();

  let err = rt.to_object(Value::Undefined).unwrap_err();
  assert_type_error(&mut rt, err);

  let err = rt.to_object(Value::Null).unwrap_err();
  assert_type_error(&mut rt, err);
}

#[test]
fn to_object_wraps_string() {
  let mut rt = VmJsRuntime::new();

  let s = rt.alloc_string_value("x").unwrap();
  let obj = rt.to_object(s).unwrap();
  assert!(rt.is_object(obj));
  assert!(rt.is_string_object(obj));
}

#[test]
fn to_object_wraps_number_and_to_number_roundtrips() {
  let mut rt = VmJsRuntime::new();

  let obj = rt.to_object(Value::Number(1.0)).unwrap();
  assert!(rt.is_object(obj));
  assert_eq!(rt.to_number(obj).unwrap(), 1.0);
}

#[test]
fn call_invokes_host_function_with_this_and_args() {
  let mut rt = VmJsRuntime::new();

  let callee = rt
    .alloc_function_value(|rt, this, args| {
      assert_eq!(rt.to_number(this)?, 10.0);
      assert_eq!(args.len(), 2);
      assert_eq!(rt.to_number(args[0])?, 1.0);
      assert_eq!(rt.to_number(args[1])?, 2.0);
      Ok(Value::Number(123.0))
    })
    .unwrap();

  let result = rt
    .call(
      callee,
      Value::Number(10.0),
      &[Value::Number(1.0), Value::Number(2.0)],
    )
    .unwrap();
  assert_eq!(rt.to_number(result).unwrap(), 123.0);
}

