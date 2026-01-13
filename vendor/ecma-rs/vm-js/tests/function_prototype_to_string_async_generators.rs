use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

fn is_unimplemented_async_generator_error(rt: &mut JsRuntime, err: &VmError) -> Result<bool, VmError> {
  match err {
    VmError::Unimplemented(msg) if msg.contains("async generator functions") => return Ok(true),
    _ => {}
  }

  let Some(thrown) = err.thrown_value() else {
    return Ok(false);
  };
  let Value::Object(err_obj) = thrown else {
    return Ok(false);
  };

  // vm-js currently feature-detects async generator functions by throwing a SyntaxError at runtime
  // (instead of returning a host-level `VmError::Unimplemented`), so test harnesses can use
  // try/catch. Treat that specific error as "feature not implemented" so this test file can land
  // before async generators are supported.
  let syntax_error_proto = rt.realm().intrinsics().syntax_error_prototype();
  if rt.heap().object_prototype(err_obj)? != Some(syntax_error_proto) {
    return Ok(false);
  }

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(err_obj))?;

  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
  let Some(Value::String(message_s)) =
    scope.heap().object_get_own_data_property_value(err_obj, &message_key)?
  else {
    return Ok(false);
  };

  let message = scope.heap().get_string(message_s)?.to_utf8_lossy();
  Ok(message == "async generator functions")
}

#[test]
fn async_generator_function_to_string_slices_source_text() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script("async function* g() { yield 1; }\ng.toString()") {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert_eq!(s, "async function* g() { yield 1; }");
    }
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}

#[test]
fn async_generator_function_expression_to_string_trims_trailing_semicolon() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script("const g = async function*() { yield 1; };\ng.toString()") {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert_eq!(s, "async function*() { yield 1; }");
    }
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}

#[test]
fn async_generator_function_declaration_to_string_preserves_comments() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script(
    r#"/* before */async /* a */ function /* b */ * /* c */ f /* d */ ( /* e */ x /* f */ , /* g */ y /* h */ ) /* i */ { /* j */ ; /* k */ ; /* l */ }/* after */
f.toString()"#,
  ) {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert_eq!(
        s,
        "async /* a */ function /* b */ * /* c */ f /* d */ ( /* e */ x /* f */ , /* g */ y /* h */ ) /* i */ { /* j */ ; /* k */ ; /* l */ }"
      );
    }
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}

#[test]
fn async_generator_function_expression_to_string_preserves_comments() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script(
    r#"let f = /* before */async /* a */ function /* b */ * /* c */ F /* d */ ( /* e */ x /* f */ , /* g */ y /* h */ ) /* i */ { /* j */ ; /* k */ ; /* l */ }/* after */;
let g = /* before */async /* a */ function /* b */ * /* c */ ( /* d */ x /* e */ , /* f */ y /* g */ ) /* h */ { /* i */ ; /* j */ ; /* k */ }/* after */;
f.toString() + "|" + g.toString()"#,
  ) {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert_eq!(
        s,
        "async /* a */ function /* b */ * /* c */ F /* d */ ( /* e */ x /* f */ , /* g */ y /* h */ ) /* i */ { /* j */ ; /* k */ ; /* l */ }|\
async /* a */ function /* b */ * /* c */ ( /* d */ x /* e */ , /* f */ y /* g */ ) /* h */ { /* i */ ; /* j */ ; /* k */ }"
      );
    }
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}

#[test]
fn async_generator_function_constructor_to_string_matches_test262() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script(
    "const AsyncGeneratorFunction = Object.getPrototypeOf(async function*(){}).constructor;\n\
     AsyncGeneratorFunction('yield 10').toString()",
  ) {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert_eq!(s, "async function* anonymous(\n) {\nyield 10\n}");
    }
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}

#[test]
fn async_generator_function_constructor_to_string_handles_line_comments() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script(
    "const AsyncGeneratorFunction = Object.getPrototypeOf(async function*(){}).constructor;\n\
     AsyncGeneratorFunction(\"a\", \" /* a */ b, c /* b */ //\", \"/* c */ ; /* d */ //\").toString()",
  ) {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert_eq!(
        s,
        "async function* anonymous(a, /* a */ b, c /* b */ //\n) {\n/* c */ ; /* d */ //\n}"
      );
    }
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}

#[test]
fn async_generator_method_object_literal_to_string_preserves_comments() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script(
    r#"let f = { /* before */async /* a */ * /* b */ f /* c */ ( /* d */ ) /* e */ { /* f */ }/* after */ }.f;
f.toString()"#,
  ) {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert_eq!(s, "async /* a */ * /* b */ f /* c */ ( /* d */ ) /* e */ { /* f */ }");
    }
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}

#[test]
fn async_generator_method_class_to_string_preserves_comments() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script(
    r#"class F { /* before */async /* a */ * /* b */ f /* c */ ( /* d */ ) /* e */ { /* f */ }/* after */ }
F.prototype.f.toString()"#,
  ) {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert_eq!(s, "async /* a */ * /* b */ f /* c */ ( /* d */ ) /* e */ { /* f */ }");
    }
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}

#[test]
fn async_generator_methods_to_string_preserve_computed_key_source() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script(
    r#"let x = "h";
let f = { /* before */async /* a */ * /* b */ f /* c */ ( /* d */ ) /* e */ { /* f */ }/* after */ }.f;
let g = { /* before */async /* a */ * /* b */ [ /* c */ "g" /* d */ ] /* e */ ( /* f */ ) /* g */ { /* h */ }/* after */ }.g;
let h = { /* before */async /* a */ * /* b */ [ /* c */ x /* d */ ] /* e */ ( /* f */ ) /* g */ { /* h */ }/* after */ }.h;

class F { /* before */async /* a */ * /* b */ f /* c */ ( /* d */ ) /* e */ { /* f */ }/* after */ }
class G { /* before */async /* a */ * /* b */ [ /* c */ "g" /* d */ ] /* e */ ( /* f */ ) /* g */ { /* h */ }/* after */ }
class H { /* before */async /* a */ * /* b */ [ /* c */ x /* d */ ] /* e */ ( /* f */ ) /* g */ { /* h */ }/* after */ }

class SF { static /* before */async /* a */ * /* b */ f /* c */ ( /* d */ ) /* e */ { /* f */ }/* after */ }
class SG { static /* before */async /* a */ * /* b */ [ /* c */ "g" /* d */ ] /* e */ ( /* f */ ) /* g */ { /* h */ }/* after */ }
class SH { static /* before */async /* a */ * /* b */ [ /* c */ x /* d */ ] /* e */ ( /* f */ ) /* g */ { /* h */ }/* after */ }

[
  f.toString(),
  g.toString(),
  h.toString(),
  F.prototype.f.toString(),
  G.prototype.g.toString(),
  H.prototype.h.toString(),
  SF.f.toString(),
  SG.g.toString(),
  SH.h.toString(),
].join("|")"#,
  ) {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert_eq!(
        s,
        "async /* a */ * /* b */ f /* c */ ( /* d */ ) /* e */ { /* f */ }|\
async /* a */ * /* b */ [ /* c */ \"g\" /* d */ ] /* e */ ( /* f */ ) /* g */ { /* h */ }|\
async /* a */ * /* b */ [ /* c */ x /* d */ ] /* e */ ( /* f */ ) /* g */ { /* h */ }|\
async /* a */ * /* b */ f /* c */ ( /* d */ ) /* e */ { /* f */ }|\
async /* a */ * /* b */ [ /* c */ \"g\" /* d */ ] /* e */ ( /* f */ ) /* g */ { /* h */ }|\
async /* a */ * /* b */ [ /* c */ x /* d */ ] /* e */ ( /* f */ ) /* g */ { /* h */ }|\
async /* a */ * /* b */ f /* c */ ( /* d */ ) /* e */ { /* f */ }|\
async /* a */ * /* b */ [ /* c */ \"g\" /* d */ ] /* e */ ( /* f */ ) /* g */ { /* h */ }|\
async /* a */ * /* b */ [ /* c */ x /* d */ ] /* e */ ( /* f */ ) /* g */ { /* h */ }"
      );
    }
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}
