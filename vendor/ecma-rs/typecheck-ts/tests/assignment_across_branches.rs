mod common;

use std::sync::Arc;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn let_assignments_across_branches_do_not_narrow_binding_type() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = r#"
declare function unknown_cond(): boolean;
declare function unknown_func(x: number): void;
let x = 0;
if (unknown_cond()) {
  x = 1;
} else {
  x = 2;
}
unknown_func(x);
"#;
  host.insert(file.clone(), Arc::from(source));

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
}

