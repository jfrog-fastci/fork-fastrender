use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

#[test]
fn super_call_in_derived_constructor_checks_base_constructor_args() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = r#"
class B { constructor(x: number) {} }
class C extends B { constructor() { super("hi"); } }
"#;
  host.insert(file.clone(), Arc::from(source));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::ARGUMENT_TYPE_MISMATCH.as_str()),
    "expected ARGUMENT_TYPE_MISMATCH diagnostic, got {diagnostics:?}"
  );
}

#[test]
fn super_call_in_derived_constructor_records_call_signature() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = r#"
class B { constructor(x: number) {} }
class C extends B { constructor() { super(1); } }
"#;
  host.insert(file.clone(), Arc::from(source));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let call_start = source
    .find("super(1)")
    .expect("expected super(1) call in source") as u32;
  let call_offset = call_start + 5;
  let sig_id = program
    .call_signature_at(file_id, call_offset)
    .expect("call signature recorded");
  let sig = program.signature(sig_id).expect("signature data");
  assert_eq!(program.display_type(sig.params[0].ty).to_string(), "number");
}

