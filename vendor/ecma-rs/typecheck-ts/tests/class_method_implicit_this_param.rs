mod common;

use std::sync::Arc;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

#[test]
fn class_method_has_implicit_this_param() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("file0.ts");
  let source = r#"
class C { x = 1; m() { return this.x; } }
const f = new C().m;
f();
"#;
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();

  let no_overloads: Vec<_> = diagnostics
    .iter()
    .filter(|diag| diag.code.as_str() == codes::NO_OVERLOAD.as_str())
    .collect();
  assert_eq!(no_overloads.len(), 1, "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = u32::try_from(source.rfind("f();").expect("offset of `f();`"))
    .expect("offset fits in u32");
  let diag = no_overloads[0];
  assert_eq!(diag.primary.file, file_id);
  assert!(
    diag.primary.range.start <= offset && diag.primary.range.end >= offset + 1,
    "expected NO_OVERLOAD span {:?} to cover `f()` at offset {offset}",
    diag.primary.range
  );
}

