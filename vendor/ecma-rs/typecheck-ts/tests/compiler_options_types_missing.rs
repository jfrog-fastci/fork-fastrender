use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileId, FileKey, MemoryHost, Program, TextRange};

#[test]
fn compiler_options_types_missing_package_uses_placeholder_span() {
  let mut options = CompilerOptions::default();
  options.types = vec!["missing".to_string()];
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  host.add_lib(common::core_globals_lib());

  let entry = FileKey::new("entry.ts");
  host.insert(entry.clone(), Arc::from("export {};"));

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == "TS2688"
        && diag.primary.file == FileId(u32::MAX)
        && diag.primary.range == TextRange::new(0, 0)
    }),
    "expected TS2688 diagnostic anchored to a placeholder span, got {diagnostics:?}"
  );
}
