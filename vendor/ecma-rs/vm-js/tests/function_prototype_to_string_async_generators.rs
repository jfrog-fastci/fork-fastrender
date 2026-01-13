use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

mod _async_generator_support;

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

#[test]
fn async_generator_function_to_string_slices_source_text() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script("async function* g() { yield 1; }\ng.toString()") {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert_eq!(s, "async function* g() { yield 1; }");
    }
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
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
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
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
    Err(err) if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
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
    Err(err) if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
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
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
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
    Err(err) if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
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
    Err(err) if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
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
    Err(err) if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
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
    Err(err) if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}

#[test]
fn proxy_async_generator_function_to_string_is_native() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script("new Proxy(async function*() {}, {}).toString()") {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert!(s.contains("[native code]"));
    }
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}

#[test]
fn proxy_async_generator_method_to_string_is_native() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script("new Proxy({ async * method() {} }.method, {}).toString()") {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert!(s.contains("[native code]"));
    }
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}

#[test]
fn async_generator_to_string_preserves_crlf_line_terminators() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  // Mirrors test262's line terminator normalisation checks, but for `async function*`.
  match rt.exec_script("async function* g(\r\n) {\r\n  yield 1;\r\n}\r\ng.toString()") {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert_eq!(s, "async function* g(\r\n) {\r\n  yield 1;\r\n}");
    }
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}

#[test]
fn async_generator_to_string_preserves_cr_line_terminators() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  match rt.exec_script("async function* g(\r) {\r  yield 1;\r}\rg.toString()") {
    Ok(value) => {
      let s = value_to_utf8(&rt, value);
      assert_eq!(s, "async function* g(\r) {\r  yield 1;\r}");
    }
    Err(err)
      if _async_generator_support::is_unimplemented_async_generator_error(&mut rt, &err)? => {}
    Err(err) => return Err(err),
  }
  Ok(())
}
