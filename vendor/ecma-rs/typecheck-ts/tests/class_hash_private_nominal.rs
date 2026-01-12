mod common;

use std::sync::Arc;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

#[test]
fn class_hash_private_fields_are_nominal() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("file0.ts");
  let source = r#"
class A { #x: number = 1 }
class B { #x: number = 1 }
let a: A;
a = new B();
"#;
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();

  let mismatches: Vec<_> = diagnostics
    .iter()
    .filter(|diag| diag.code.as_str() == codes::TYPE_MISMATCH.as_str())
    .collect();
  assert_eq!(mismatches.len(), 1, "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = u32::try_from(source.find("new B()").expect("offset of `new B()`"))
    .expect("offset fits in u32");
  let mismatch = mismatches[0];
  assert_eq!(mismatch.primary.file, file_id);
  assert!(
    mismatch.primary.range.start <= offset && mismatch.primary.range.end >= offset + 1,
    "expected mismatch span {:?} to cover `new B()` at offset {offset}",
    mismatch.primary.range
  );
}

