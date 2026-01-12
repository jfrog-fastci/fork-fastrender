use std::sync::Arc;

use diagnostics::TextRange;

mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program, TextRange};

#[test]
fn ambiguous_overload_prefers_first_signature_and_reports_type_mismatch_at_assignment() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = concat!(
    "declare function f(x: string): 1;\n",
    "declare function f(x: string): 2;\n",
    "const v: 2 = f(\"a\");\n",
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
    codes::TYPE_MISMATCH.as_str(),
    "expected TYPE_MISMATCH diagnostic, got {diagnostics:?}"
  );

  let start = source
    .find("v: 2")
    .expect("binding name present in source") as u32;
  let end = start + "v".len() as u32;
  assert_eq!(diag.primary.file, file_id);
  assert_eq!(diag.primary.range, TextRange::new(start, end));
}
