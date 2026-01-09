use crate::js::bindings::DomExceptionClass;
use crate::web::dom::selectors::parse_selector_list;
use vm_js::{PropertyKey, Value, VmError};
use webidl_js_runtime::{JsRuntime as _, VmJsRuntime, WebIdlJsRuntime as _};

pub fn install_scaffold_selector_bindings(
  rt: &mut VmJsRuntime,
  global: Value,
  dom_exception: DomExceptionClass,
) -> Result<(), VmError> {
  let k_dom_type: PropertyKey = rt.prop_key("__fastrender_dom_type")?;
  let k_prototype: PropertyKey = rt.prop_key("prototype")?;
  let k_document: PropertyKey = rt.prop_key("Document")?;
  let k_element: PropertyKey = rt.prop_key("Element")?;

  let ctor_document = rt.get(global, k_document)?;
  let proto_document = rt.get(ctor_document, k_prototype)?;

  let ctor_element = rt.get(global, k_element)?;
  let proto_element = rt.get(ctor_element, k_prototype)?;

  // ===========================================================================
  // Document
  // ===========================================================================
  {
    let k_query_selector: PropertyKey = rt.prop_key("querySelector")?;
    let k_query_selector_all: PropertyKey = rt.prop_key("querySelectorAll")?;

    let dom_ex = dom_exception;
    let query_selector_fn = rt.alloc_function_value(move |rt, this, args| {
      ensure_dom_type(rt, this, k_dom_type, "Document")?;
      if args.len() < 1 {
        return Err(rt.throw_type_error(&format!(
          "Document.querySelector: expected at least 1 arguments, got {}",
          args.len()
        )));
      }

      let selectors_value = rt.to_string(args[0])?;
      let selectors = value_to_rust_string(rt, selectors_value)?;
      match parse_selector_list(&selectors) {
        Ok(_) => Ok(Value::Null),
        Err(err) => {
          let exc = dom_ex.from_dom_exception(rt, &err)?;
          Err(VmError::Throw(exc))
        }
      }
    })?;
    rt.define_data_property(proto_document, k_query_selector, query_selector_fn, false)?;

    let dom_ex = dom_exception;
    let query_selector_all_fn = rt.alloc_function_value(move |rt, this, args| {
      ensure_dom_type(rt, this, k_dom_type, "Document")?;
      if args.len() < 1 {
        return Err(rt.throw_type_error(&format!(
          "Document.querySelectorAll: expected at least 1 arguments, got {}",
          args.len()
        )));
      }

      let selectors_value = rt.to_string(args[0])?;
      let selectors = value_to_rust_string(rt, selectors_value)?;
      match parse_selector_list(&selectors) {
        Ok(_) => rt.alloc_array(),
        Err(err) => {
          let exc = dom_ex.from_dom_exception(rt, &err)?;
          Err(VmError::Throw(exc))
        }
      }
    })?;
    rt.define_data_property(proto_document, k_query_selector_all, query_selector_all_fn, false)?;
  }

  // ===========================================================================
  // Element
  // ===========================================================================
  {
    let k_query_selector: PropertyKey = rt.prop_key("querySelector")?;
    let k_query_selector_all: PropertyKey = rt.prop_key("querySelectorAll")?;
    let k_matches: PropertyKey = rt.prop_key("matches")?;

    let dom_ex = dom_exception;
    let query_selector_fn = rt.alloc_function_value(move |rt, this, args| {
      ensure_dom_type(rt, this, k_dom_type, "Element")?;
      if args.len() < 1 {
        return Err(rt.throw_type_error(&format!(
          "Element.querySelector: expected at least 1 arguments, got {}",
          args.len()
        )));
      }

      let selectors_value = rt.to_string(args[0])?;
      let selectors = value_to_rust_string(rt, selectors_value)?;
      match parse_selector_list(&selectors) {
        Ok(_) => Ok(Value::Null),
        Err(err) => {
          let exc = dom_ex.from_dom_exception(rt, &err)?;
          Err(VmError::Throw(exc))
        }
      }
    })?;
    rt.define_data_property(proto_element, k_query_selector, query_selector_fn, false)?;

    let dom_ex = dom_exception;
    let query_selector_all_fn = rt.alloc_function_value(move |rt, this, args| {
      ensure_dom_type(rt, this, k_dom_type, "Element")?;
      if args.len() < 1 {
        return Err(rt.throw_type_error(&format!(
          "Element.querySelectorAll: expected at least 1 arguments, got {}",
          args.len()
        )));
      }

      let selectors_value = rt.to_string(args[0])?;
      let selectors = value_to_rust_string(rt, selectors_value)?;
      match parse_selector_list(&selectors) {
        Ok(_) => rt.alloc_array(),
        Err(err) => {
          let exc = dom_ex.from_dom_exception(rt, &err)?;
          Err(VmError::Throw(exc))
        }
      }
    })?;
    rt.define_data_property(proto_element, k_query_selector_all, query_selector_all_fn, false)?;

    let dom_ex = dom_exception;
    let matches_fn = rt.alloc_function_value(move |rt, this, args| {
      ensure_dom_type(rt, this, k_dom_type, "Element")?;
      if args.len() < 1 {
        return Err(rt.throw_type_error(&format!(
          "Element.matches: expected at least 1 arguments, got {}",
          args.len()
        )));
      }

      let selectors_value = rt.to_string(args[0])?;
      let selectors = value_to_rust_string(rt, selectors_value)?;
      match parse_selector_list(&selectors) {
        Ok(_) => Ok(Value::Bool(false)),
        Err(err) => {
          let exc = dom_ex.from_dom_exception(rt, &err)?;
          Err(VmError::Throw(exc))
        }
      }
    })?;
    rt.define_data_property(proto_element, k_matches, matches_fn, false)?;
  }

  Ok(())
}

fn ensure_dom_type(
  rt: &mut VmJsRuntime,
  this: Value,
  k_dom_type: PropertyKey,
  expected: &str,
) -> Result<(), VmError> {
  if !rt.is_object(this) {
    return Err(rt.throw_type_error("Illegal invocation"));
  }
  let this_type = rt.get(this, k_dom_type)?;
  let this_type = rt.to_string(this_type)?;
  let actual = value_to_rust_string(rt, this_type)?;
  if actual != expected {
    return Err(rt.throw_type_error("Illegal invocation"));
  }
  Ok(())
}

fn value_to_rust_string(rt: &VmJsRuntime, value: Value) -> Result<String, VmError> {
  let Value::String(s) = value else {
    return Err(VmError::Unimplemented("expected string value"));
  };
  Ok(rt.heap().get_string(s)?.to_utf8_lossy())
}

