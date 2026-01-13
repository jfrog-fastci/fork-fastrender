use vm_js::{Heap, HeapLimits, JsRuntime, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_syntax_error(err: VmError) -> Vec<diagnostics::Diagnostic> {
  match err {
    VmError::Syntax(diags) => diags,
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn duplicate_private_field_and_method_is_syntax_error() {
  let mut rt = new_runtime();
  let diags = assert_syntax_error(rt.exec_script("class C { #x; #x(){} }").unwrap_err());
  assert!(
    diags.iter().any(|d| {
      d.message.contains("private name already declared") || d.message.contains("duplicate private name")
    }),
    "expected private-name duplicate diagnostic, got {diags:?}"
  );
}

#[test]
fn duplicate_private_field_and_field_is_syntax_error() {
  let mut rt = new_runtime();
  let diags = assert_syntax_error(rt.exec_script("class C { #x; #x; }").unwrap_err());
  assert!(
    diags.iter().any(|d| {
      d.message.contains("private name already declared") || d.message.contains("duplicate private name")
    }),
    "expected private-name duplicate diagnostic, got {diags:?}"
  );
}

#[test]
fn duplicate_private_method_is_syntax_error() {
  let mut rt = new_runtime();
  let diags = assert_syntax_error(rt.exec_script("class C { #x(){} #x(){} }").unwrap_err());
  assert!(
    diags.iter().any(|d| {
      d.message.contains("private name already declared") || d.message.contains("duplicate private name")
    }),
    "expected private-name duplicate diagnostic, got {diags:?}"
  );
}

#[test]
fn duplicate_private_static_and_instance_is_syntax_error() {
  let mut rt = new_runtime();
  let diags = assert_syntax_error(rt.exec_script("class C { #x(){} static #x(){} }").unwrap_err());
  assert!(
    diags.iter().any(|d| {
      d.message.contains("private name already declared") || d.message.contains("duplicate private name")
    }),
    "expected private-name duplicate diagnostic, got {diags:?}"
  );
}

#[test]
fn bare_private_identifier_is_syntax_error() {
  let mut rt = new_runtime();
  let diags = assert_syntax_error(rt.exec_script("class C { #x; m(){ #x; } }").unwrap_err());
  assert!(
    diags.iter().any(|d| d.code.as_str() == "VMJS0004" && d.message == "invalid private identifier"),
    "expected early error VMJS0004 invalid private identifier, got {diags:?}"
  );
}

#[test]
fn super_dot_private_name_is_syntax_error() {
  let mut rt = new_runtime();
  let diags = assert_syntax_error(
    rt.exec_script("class B{} class C extends B { #x; m(){ super.#x; } }")
      .unwrap_err(),
  );
  assert!(
    diags.iter().any(|d| {
      d.code.as_str() == "VMJS0004"
        && d.message == "super.#<name> is not a valid private member access"
    }),
    "expected early error VMJS0004 for super.#<name>, got {diags:?}"
  );
}

#[test]
fn private_getter_setter_pair_is_allowed() {
  let mut rt = new_runtime();
  rt.exec_script("class C { get #x(){return 1} set #x(v){} }")
    .unwrap();
}

#[test]
fn private_getter_setter_pair_plus_extra_is_syntax_error() {
  let mut rt = new_runtime();
  let diags = assert_syntax_error(
    rt.exec_script("class C { get #x(){return 1} set #x(v){} set #x(v){} }")
      .unwrap_err(),
  );
  assert!(
    diags.iter().any(|d| {
      d.message.contains("private name already declared") || d.message.contains("duplicate private name")
    }),
    "expected private-name duplicate diagnostic, got {diags:?}"
  );
}

#[test]
fn private_in_expression_is_allowed_when_declared() {
  let mut rt = new_runtime();
  // This must pass early errors, but does not require runtime `#x in obj` support since the method
  // body is never executed.
  rt.exec_script("class C { #x(){ return #x in {}; } }")
    .unwrap();
}

#[test]
fn private_in_expression_is_syntax_error_when_undeclared() {
  let mut rt = new_runtime();
  let diags = assert_syntax_error(rt.exec_script("#x in {};").unwrap_err());
  assert!(
    diags
      .iter()
      .any(|d| d.code.as_str() == "VMJS0004" && d.message == "invalid private name"),
    "expected early error VMJS0004 invalid private name, got {diags:?}"
  );
}

#[test]
fn parenthesized_private_in_expression_is_syntax_error_even_when_declared() {
  let mut rt = new_runtime();
  let diags = assert_syntax_error(
    rt.exec_script("class C { #x(){} m(){ return (#x) in {}; } }")
      .unwrap_err(),
  );
  assert!(
    diags
      .iter()
      .any(|d| d.code.as_str() == "VMJS0004" && d.message == "invalid private identifier"),
    "expected early error VMJS0004 invalid private identifier, got {diags:?}"
  );
}

#[test]
fn private_member_access_is_syntax_error_when_undeclared() {
  let mut rt = new_runtime();
  let diags = assert_syntax_error(rt.exec_script("class C { m(){ return this.#x; } }").unwrap_err());
  assert!(
    diags
      .iter()
      .any(|d| d.code.as_str() == "VMJS0004" && d.message == "invalid private name"),
    "expected early error VMJS0004 invalid private name, got {diags:?}"
  );
}
