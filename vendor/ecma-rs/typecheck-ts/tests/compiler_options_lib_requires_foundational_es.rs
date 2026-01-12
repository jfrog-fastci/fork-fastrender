use std::sync::Arc;

use typecheck_ts::lib_support::{CompilerOptions, LibName};
use typecheck_ts::{FileId, FileKey, MemoryHost, Program, TextRange};

#[test]
fn compiler_options_lib_without_foundational_es_emits_all_ts2318() {
  let mut options = CompilerOptions::default();
  options.libs = vec![
    LibName::parse("es2015.iterable").expect("parse es2015.iterable lib"),
    LibName::parse("es2015.promise").expect("parse es2015.promise lib"),
  ];

  let mut host = MemoryHost::with_options(options);
  let entry = FileKey::new("entry.ts");
  host.insert(entry.clone(), Arc::from("export const x = 1;"));

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();

  let ts2318: Vec<_> = diagnostics
    .iter()
    .filter(|diag| diag.code.as_str() == "TS2318")
    .collect();
  assert_eq!(
    ts2318.len(),
    8,
    "expected 8 TS2318 diagnostics when --lib omits a foundational ES lib, got {diagnostics:?}"
  );

  for name in [
    "Array",
    "Boolean",
    "Function",
    "IArguments",
    "Number",
    "Object",
    "RegExp",
    "String",
  ] {
    assert!(
      ts2318
        .iter()
        .any(|diag| diag.message == format!("Cannot find global type '{name}'.")),
      "missing TS2318 for {name}, got {diagnostics:?}"
    );
  }

  assert!(
    ts2318
      .iter()
      .all(|diag| diag.primary.file == FileId(u32::MAX) && diag.primary.range == TextRange::new(0, 0)),
    "expected TS2318 diagnostics to use the null 0..0 span, got {diagnostics:?}"
  );
}

