use diagnostics::TextRange;
use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

#[test]
fn ambiguous_overload_emits_tc2001() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = concat!(
    "declare function f(x: string): 1;\n",
    "declare function f(x: string): 2;\n",
    "const v = f(\"a\");\n",
  );
  host.insert(file.clone(), Arc::from(source));

  let program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected a single diagnostic, got {diagnostics:?}"
  );
  let diag = &diagnostics[0];
  assert_eq!(
    diag.code.as_str(),
    codes::AMBIGUOUS_OVERLOAD.as_str(),
    "expected AMBIGUOUS_OVERLOAD diagnostic, got {diagnostics:?}"
  );

  let start = source
    .find("f(\"a\")")
    .expect("call expression present in source") as u32;
  let end = start + "f(\"a\")".len() as u32;
  assert_eq!(diag.primary.file, file_id);
  assert_eq!(diag.primary.range, TextRange::new(start, end));
}

