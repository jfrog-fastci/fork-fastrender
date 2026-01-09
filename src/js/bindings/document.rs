use crate::dom2::Document;
use crate::js::bindings::DomExceptionClass;
use std::cell::RefCell;
use std::rc::Rc;
use vm_js::{PropertyKey, Value, VmError};
use webidl_js_runtime::{JsRuntime as _, VmJsRuntime, WebIdlJsRuntime as _};

/// Install a minimal `document` object exposing `querySelector` and `querySelectorAll`.
///
/// This is not a full Web IDL binding layer. It exists so we can test spec-shaped exception
/// behavior (DOMException, WebIDL conversions) before the full DOM surface area lands.
pub fn install_document_query_selector_bindings(
  rt: &mut VmJsRuntime,
  global: Value,
  document: Rc<RefCell<Document>>,
  dom_exception: DomExceptionClass,
) -> Result<Value, VmError> {
  let key_document = prop_key(rt, "document")?;
  let key_query_selector = prop_key(rt, "querySelector")?;
  let key_query_selector_all = prop_key(rt, "querySelectorAll")?;

  let document_obj = rt.alloc_object_value()?;

  // `Document.prototype.querySelector(selectors)`
  {
    let doc = Rc::clone(&document);
    let dom_ex = dom_exception;
    let query_selector_fn = rt.alloc_function_value(move |rt, _this, args| {
      let selectors_value = args.get(0).copied().ok_or_else(|| {
        rt.throw_type_error("Document.querySelector requires 1 argument")
      })?;
      let selectors_value = rt.to_string(selectors_value)?;
      let selectors = value_to_rust_string(rt, selectors_value)?;

      match doc.borrow_mut().query_selector(&selectors, None) {
        Ok(_maybe_node) => Ok(Value::Null),
        Err(err) => {
          let exc = dom_ex.from_dom_exception(rt, &err)?;
          Err(VmError::Throw(exc))
        }
      }
    })?;
    rt.define_data_property(document_obj, key_query_selector, query_selector_fn, false)?;
  }

  // `Document.prototype.querySelectorAll(selectors)`
  {
    let doc = Rc::clone(&document);
    let dom_ex = dom_exception;
    let query_selector_all_fn = rt.alloc_function_value(move |rt, _this, args| {
      let selectors_value = args.get(0).copied().ok_or_else(|| {
        rt.throw_type_error("Document.querySelectorAll requires 1 argument")
      })?;
      let selectors_value = rt.to_string(selectors_value)?;
      let selectors = value_to_rust_string(rt, selectors_value)?;

      match doc.borrow_mut().query_selector_all(&selectors, None) {
        Ok(_nodes) => Ok(rt.alloc_object_value()?),
        Err(err) => {
          let exc = dom_ex.from_dom_exception(rt, &err)?;
          Err(VmError::Throw(exc))
        }
      }
    })?;
    rt.define_data_property(
      document_obj,
      key_query_selector_all,
      query_selector_all_fn,
      false,
    )?;
  }

  rt.define_data_property(global, key_document, document_obj, false)?;

  Ok(document_obj)
}

fn prop_key(rt: &mut VmJsRuntime, s: &str) -> Result<PropertyKey, VmError> {
  let v = rt.alloc_string_value(s)?;
  let Value::String(handle) = v else {
    return Err(rt.throw_type_error("expected string value"));
  };
  Ok(PropertyKey::String(handle))
}

fn value_to_rust_string(rt: &VmJsRuntime, value: Value) -> Result<String, VmError> {
  let Value::String(s) = value else {
    return Err(VmError::Unimplemented("expected string value"));
  };
  Ok(rt.heap().get_string(s)?.to_utf8_lossy())
}
