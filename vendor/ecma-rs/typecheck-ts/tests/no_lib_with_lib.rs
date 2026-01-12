use std::sync::Arc;

use typecheck_ts::lib_support::{CompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn no_lib_with_explicit_lib_emits_ts5053_and_ignores_libs() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;
  options.libs = vec![LibName::parse("es2020").expect("parse lib name")];
  options.skip_lib_check = true;

  let mut host = MemoryHost::with_options(options);
  let entry = FileKey::new("entry.ts");
  host.insert(entry.clone(), Arc::from("export const x = 1;"));

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  assert!(
    diagnostics.iter().any(|diag| diag.code.as_str() == "TS5053"),
    "expected TS5053 diagnostic when no_default_lib=true and libs is non-empty, got {diagnostics:?}"
  );
  assert_eq!(
    diagnostics
      .iter()
      .filter(|diag| diag.code.as_str() == "TS5053")
      .count(),
    1,
    "expected TS5053 to be emitted once, got {diagnostics:?}"
  );

  assert!(
    diagnostics.iter().any(|diag| diag.code.as_str() == "TS2318"),
    "expected TS2318 diagnostics to prove libs were ignored under noLib, got {diagnostics:?}"
  );

  // Sanity-check deterministic ordering.
  assert_eq!(diagnostics, program.check());
}

