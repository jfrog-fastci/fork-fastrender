use std::cell::RefCell;
use std::rc::Rc;

use vm_js::{PropertyKey, Value, VmError};

use webidl_js_runtime::{JsRuntime as _, VmJsRuntime, WebIdlJsRuntime as _};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionsSource {
  DefaultDict,
  Dict,
  Boolean,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct AddEventListenerOptions {
  capture: bool,
  once: bool,
  passive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AddEventListenerCall {
  type_: String,
  callback_is_null: bool,
  options: AddEventListenerOptions,
  options_source: OptionsSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlConstructorCall {
  url: String,
  base: Option<String>,
}

#[derive(Debug, Default)]
struct FakeHost {
  add_event_listener_calls: Vec<AddEventListenerCall>,
  url_constructor_calls: Vec<UrlConstructorCall>,
}

struct InstalledBindings {
  global: Value,
}

fn pk(rt: &mut VmJsRuntime, name: &str) -> Result<PropertyKey, VmError> {
  rt.property_key_from_str(name)
}

fn string_value_to_utf16_lossy(rt: &mut VmJsRuntime, v: Value) -> Result<String, VmError> {
  let units = rt.with_string_code_units(v, |view| view.to_vec())?;
  Ok(String::from_utf16_lossy(&units))
}

fn to_dom_string(rt: &mut VmJsRuntime, v: Value) -> Result<String, VmError> {
  let s = rt.to_string(v)?;
  string_value_to_utf16_lossy(rt, s)
}

fn to_usv_string(rt: &mut VmJsRuntime, v: Value) -> Result<String, VmError> {
  // `String::from_utf16_lossy` performs surrogate replacement, which is sufficient for these
  // binding-level smoke tests.
  to_dom_string(rt, v)
}

const BYTESTRING_RANGE_ERR: &str = "ByteString value must only contain code units in range 0..=255";

fn to_byte_string(rt: &mut VmJsRuntime, v: Value) -> Result<Vec<u8>, VmError> {
  let s = rt.to_string(v)?;
  let units = rt.with_string_code_units(s, |view| view.to_vec())?;

  let mut out = Vec::new();
  out
    .try_reserve_exact(units.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for u in units {
    if u > 0xFF {
      return Err(rt.throw_type_error(BYTESTRING_RANGE_ERR));
    }
    out.push(u as u8);
  }
  Ok(out)
}

fn convert_add_event_listener_options_union(
  rt: &mut VmJsRuntime,
  v: Value,
) -> Result<(AddEventListenerOptions, OptionsSource), VmError> {
  if let Value::Bool(capture) = v {
    return Ok((
      AddEventListenerOptions {
        capture,
        ..AddEventListenerOptions::default()
      },
      OptionsSource::Boolean,
    ));
  }

  Ok((
    convert_add_event_listener_options_dict(rt, v)?,
    OptionsSource::Dict,
  ))
}

fn get_optional_bool_member(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Result<bool, VmError> {
  let key = pk(rt, name)?;
  let v = rt.get(obj, key)?;
  if matches!(v, Value::Undefined) {
    return Ok(false);
  }
  rt.to_boolean(v)
}

fn convert_add_event_listener_options_dict(
  rt: &mut VmJsRuntime,
  v: Value,
) -> Result<AddEventListenerOptions, VmError> {
  if matches!(v, Value::Undefined | Value::Null) {
    return Ok(AddEventListenerOptions::default());
  }
  if !rt.is_object(v) {
    return Err(
      rt.throw_type_error("addEventListener options must be an object (dictionary) or boolean"),
    );
  }

  Ok(AddEventListenerOptions {
    capture: get_optional_bool_member(rt, v, "capture")?,
    once: get_optional_bool_member(rt, v, "once")?,
    passive: get_optional_bool_member(rt, v, "passive")?,
  })
}

fn alloc_symbol(rt: &mut VmJsRuntime, description: &str) -> Result<Value, VmError> {
  let desc = rt.alloc_string(description)?;
  let Value::String(desc) = desc else {
    return Err(VmError::Unimplemented("expected JS string value"));
  };
  let sym = rt.heap_mut().symbol_for(desc)?;
  rt.heap_mut().add_root(Value::Symbol(sym))?;
  Ok(Value::Symbol(sym))
}

fn assert_type_error(rt: &mut VmJsRuntime, err: VmError, expected_message: &str) {
  let VmError::Throw(thrown) = err else {
    panic!("expected VmError::Throw, got {err:?}");
  };
  let Value::Object(_) = thrown else {
    panic!("expected throw value to be an object, got {thrown:?}");
  };

  let name_key = pk(rt, "name").unwrap();
  let message_key = pk(rt, "message").unwrap();
  let name = rt
    .get(thrown, name_key)
    .and_then(|v| rt.to_string(v))
    .and_then(|v| string_value_to_utf16_lossy(rt, v))
    .unwrap();
  let message = rt
    .get(thrown, message_key)
    .and_then(|v| rt.to_string(v))
    .and_then(|v| string_value_to_utf16_lossy(rt, v))
    .unwrap();

  assert_eq!(name, "TypeError");
  assert_eq!(message, expected_message);
}

fn install_bindings(
  rt: &mut VmJsRuntime,
  host: Rc<RefCell<FakeHost>>,
) -> Result<InstalledBindings, VmError> {
  let global = rt.alloc_object_value()?;

  // EventTarget + EventTarget.prototype.addEventListener.
  let event_target_proto = rt.alloc_object_value()?;

  let host_for_add = host.clone();
  let add_event_listener = rt.alloc_function_value(move |rt, _this, args| {
    if args.len() < 2 {
      return Err(
        rt.throw_type_error("EventTarget.addEventListener requires at least 2 arguments"),
      );
    }

    let type_ = to_dom_string(rt, args[0])?;
    let callback_is_null = matches!(args[1], Value::Null | Value::Undefined);

    let (options, options_source) = if args.len() < 3 {
      (
        AddEventListenerOptions::default(),
        OptionsSource::DefaultDict,
      )
    } else {
      convert_add_event_listener_options_union(rt, args[2])?
    };

    host_for_add
      .borrow_mut()
      .add_event_listener_calls
      .push(AddEventListenerCall {
        type_,
        callback_is_null,
        options,
        options_source,
      });

    Ok(Value::Undefined)
  })?;

  let add_event_listener_key = pk(rt, "addEventListener")?;
  rt.define_data_property(
    event_target_proto,
    add_event_listener_key,
    add_event_listener,
    true,
  )?;

  let event_target_ctor = rt.alloc_function_value(|_rt, _this, _args| Ok(Value::Undefined))?;
  let prototype_key = pk(rt, "prototype")?;
  rt.define_data_property(event_target_ctor, prototype_key, event_target_proto, false)?;
  let event_target_key = pk(rt, "EventTarget")?;
  rt.define_data_property(global, event_target_key, event_target_ctor, true)?;

  // URL constructor (enough to test overload behavior and USVString conversion).
  let host_for_url = host.clone();
  let url_proto = rt.alloc_object_value()?;
  let url_proto_for_ctor = url_proto;
  let url_ctor = rt.alloc_function_value(move |rt, _this, args| {
    if args.is_empty() {
      return Err(rt.throw_type_error("URL constructor requires at least 1 argument"));
    }
    let url = to_usv_string(rt, args[0])?;
    let base = if args.len() >= 2 {
      Some(to_usv_string(rt, args[1])?)
    } else {
      None
    };

    host_for_url
      .borrow_mut()
      .url_constructor_calls
      .push(UrlConstructorCall { url, base });

    let obj = rt.alloc_object_value()?;
    rt.set_prototype(obj, Some(url_proto_for_ctor))?;
    Ok(obj)
  })?;
  // New prototype key handle for this runtime instance (same string contents).
  let url_prototype_key = pk(rt, "prototype")?;
  rt.define_data_property(url_ctor, url_prototype_key, url_proto, false)?;
  let url_key = pk(rt, "URL")?;
  rt.define_data_property(global, url_key, url_ctor, true)?;

  // ByteString conversion tester.
  let bytestring_tester = rt.alloc_function_value(move |rt, _this, args| {
    if args.is_empty() {
      return Err(rt.throw_type_error("__test_bytestring requires 1 argument"));
    }
    let _ = to_byte_string(rt, args[0])?;
    Ok(Value::Undefined)
  })?;
  let tester_key = pk(rt, "__test_bytestring")?;
  rt.define_data_property(global, tester_key, bytestring_tester, true)?;

  Ok(InstalledBindings { global })
}

fn setup() -> (VmJsRuntime, Rc<RefCell<FakeHost>>, InstalledBindings) {
  let mut rt = VmJsRuntime::new();
  let host = Rc::new(RefCell::new(FakeHost::default()));
  let bindings = install_bindings(&mut rt, host.clone()).unwrap();
  (rt, host, bindings)
}

#[test]
fn event_target_add_event_listener_defaults_options_to_empty_dict() {
  let (mut rt, host, bindings) = setup();

  let event_target_key = pk(&mut rt, "EventTarget").unwrap();
  let event_target_ctor = rt.get(bindings.global, event_target_key).unwrap();
  let prototype_key = pk(&mut rt, "prototype").unwrap();
  let event_target_proto = rt.get(event_target_ctor, prototype_key).unwrap();

  let event_target = rt.alloc_object_value().unwrap();
  rt.set_prototype(event_target, Some(event_target_proto))
    .unwrap();

  let add_event_listener_key = pk(&mut rt, "addEventListener").unwrap();
  let add_event_listener = rt.get(event_target, add_event_listener_key).unwrap();

  let click = rt.alloc_string_value("click").unwrap();
  rt.call_function(add_event_listener, event_target, &[click, Value::Null])
    .unwrap();

  let calls = &host.borrow().add_event_listener_calls;
  assert_eq!(calls.len(), 1);
  assert_eq!(
    calls[0],
    AddEventListenerCall {
      type_: "click".to_string(),
      callback_is_null: true,
      options: AddEventListenerOptions::default(),
      options_source: OptionsSource::DefaultDict,
    }
  );
}

#[test]
fn event_target_add_event_listener_disambiguates_boolean_options() {
  let (mut rt, host, bindings) = setup();

  let event_target_key = pk(&mut rt, "EventTarget").unwrap();
  let event_target_ctor = rt.get(bindings.global, event_target_key).unwrap();
  let prototype_key = pk(&mut rt, "prototype").unwrap();
  let event_target_proto = rt.get(event_target_ctor, prototype_key).unwrap();

  let event_target = rt.alloc_object_value().unwrap();
  rt.set_prototype(event_target, Some(event_target_proto))
    .unwrap();

  let add_event_listener_key = pk(&mut rt, "addEventListener").unwrap();
  let add_event_listener = rt.get(event_target, add_event_listener_key).unwrap();

  let click = rt.alloc_string_value("click").unwrap();
  rt.call_function(
    add_event_listener,
    event_target,
    &[click, Value::Null, Value::Bool(true)],
  )
  .unwrap();

  let calls = &host.borrow().add_event_listener_calls;
  assert_eq!(calls.len(), 1);
  assert_eq!(
    calls[0],
    AddEventListenerCall {
      type_: "click".to_string(),
      callback_is_null: true,
      options: AddEventListenerOptions {
        capture: true,
        ..AddEventListenerOptions::default()
      },
      options_source: OptionsSource::Boolean,
    }
  );
}

#[test]
fn url_constructor_selects_1_arg_vs_2_arg_paths() {
  let (mut rt, host, bindings) = setup();

  let url_key = pk(&mut rt, "URL").unwrap();
  let url_ctor = rt.get(bindings.global, url_key).unwrap();

  let url1 = rt.alloc_string_value("https://example.com/").unwrap();
  let result = rt
    .call_function(url_ctor, Value::Undefined, &[url1])
    .unwrap();
  assert!(rt.is_object(result));

  let url2 = rt.alloc_string_value("foo").unwrap();
  let base = rt.alloc_string_value("https://base.example/dir/").unwrap();
  let _ = rt
    .call_function(url_ctor, Value::Undefined, &[url2, base])
    .unwrap();

  assert_eq!(
    host.borrow().url_constructor_calls,
    vec![
      UrlConstructorCall {
        url: "https://example.com/".to_string(),
        base: None,
      },
      UrlConstructorCall {
        url: "foo".to_string(),
        base: Some("https://base.example/dir/".to_string()),
      },
    ]
  );
}

#[test]
fn bytestring_and_usvstring_conversion_failures_throw_type_error() {
  let (mut rt, _host, bindings) = setup();

  // ByteString: reject code units > 0xFF.
  let tester_key = pk(&mut rt, "__test_bytestring").unwrap();
  let bytestring_tester = rt.get(bindings.global, tester_key).unwrap();
  let invalid = rt.alloc_string_value("\u{0100}").unwrap();
  let err = rt
    .call_function(bytestring_tester, Value::Undefined, &[invalid])
    .unwrap_err();
  assert_type_error(&mut rt, err, BYTESTRING_RANGE_ERR);

  // USVString: ToString(Symbol) throws.
  let url_key = pk(&mut rt, "URL").unwrap();
  let url_ctor = rt.get(bindings.global, url_key).unwrap();
  let sym = alloc_symbol(&mut rt, "sym").unwrap();
  let err = rt
    .call_function(url_ctor, Value::Undefined, &[sym])
    .unwrap_err();
  assert_type_error(&mut rt, err, "Cannot convert a Symbol value to a string");
}
