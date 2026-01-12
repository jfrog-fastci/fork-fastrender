use std::sync::Arc;

use typecheck_ts::lib_support::{CompilerOptions, LibName};
use typecheck_ts::{FileId, FileKey, MemoryHost, Program, TextRange};

#[test]
fn compiler_options_nolib_and_lib_conflict_emits_ts5053_and_ts2318() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.libs = vec![LibName::parse("es2020").expect("parse es2020 lib")];

  let mut host = MemoryHost::with_options(options);
  let entry = FileKey::new("entry.ts");
  host.insert(entry.clone(), Arc::from("export const value = 1;"));

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  let has_ts5053 = diagnostics.iter().any(|diag| {
    diag.code.as_str() == "TS5053"
      && diag.message == "Option 'lib' cannot be specified with option 'noLib'."
      && diag.primary.file == FileId(u32::MAX)
      && diag.primary.range == TextRange::new(0, 0)
  });
  assert!(
    has_ts5053,
    "expected TS5053 diagnostic for lib/noLib conflict, got {diagnostics:?}"
  );

  let has_ts2318 = diagnostics.iter().any(|diag| {
    diag.code.as_str() == "TS2318"
      && diag.message.starts_with("Cannot find global type '")
      && diag.primary.file == FileId(u32::MAX)
      && diag.primary.range == TextRange::new(0, 0)
  });
  assert!(
    has_ts2318,
    "expected TS2318 required-global-type diagnostics when no libs are loaded, got {diagnostics:?}"
  );
}

