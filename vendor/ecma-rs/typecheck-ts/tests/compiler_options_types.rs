use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn compiler_options_types_includes_ambient_type_packages() {
  let mut options = CompilerOptions::default();
  options.types = vec!["example".to_string()];
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  host.add_lib(common::core_globals_lib());
  let entry = FileKey::new("entry.ts");
  let types = FileKey::new("example.d.ts");

  host.insert(entry.clone(), Arc::from("const value = example;"));
  host.insert(types.clone(), Arc::from("declare const example: string;"));
  host.link(entry.clone(), "example", types);

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics, got {diagnostics:?}"
  );
}

#[test]
fn compiler_options_types_missing_reports_ts2688_once_with_multiple_roots() {
  let mut options = CompilerOptions::default();
  options.types = vec!["definitely_missing_type_pkg".to_string()];
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  host.add_lib(common::core_globals_lib());
  let root_a = FileKey::new("a.ts");
  let root_b = FileKey::new("b.ts");

  host.insert(root_a.clone(), Arc::from("export {};"));
  host.insert(root_b.clone(), Arc::from("export {};"));

  let program = Program::new(host, vec![root_a, root_b]);
  let diagnostics = program.check();
  let ts2688: Vec<_> = diagnostics
    .iter()
    .filter(|diag| diag.code.as_str() == "TS2688")
    .collect();
  assert_eq!(
    ts2688.len(),
    1,
    "expected exactly one TS2688 diagnostic for missing compilerOptions.types entry, got {diagnostics:?}"
  );
}

#[test]
fn compiler_options_types_resolves_via_at_types_fallback() {
  let mut options = CompilerOptions::default();
  options.types = vec!["node".to_string()];
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  host.add_lib(common::core_globals_lib());
  let entry = FileKey::new("entry.ts");
  let types = FileKey::new("@types/node/index.d.ts");

  host.insert(entry.clone(), Arc::from("const value = process;"));
  host.insert(types.clone(), Arc::from("declare const process: number;"));
  // Simulate a host that only knows about the explicit `@types/*` package path.
  host.link(entry.clone(), "@types/node", types);

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics, got {diagnostics:?}"
  );
}

#[test]
fn compiler_options_types_order_does_not_affect_file_ids() {
  fn run(types: Vec<String>) -> Vec<(u32, String)> {
    let mut options = CompilerOptions::default();
    options.types = types;
    options.no_default_lib = true;

    let mut host = MemoryHost::with_options(options);
    host.add_lib(common::core_globals_lib());
    let entry = FileKey::new("entry.ts");
    let a = FileKey::new("a.d.ts");
    let b = FileKey::new("b.d.ts");

    host.insert(entry.clone(), Arc::from("const a1 = AGlobal; const b1 = BGlobal;"));
    host.insert(a.clone(), Arc::from("declare const AGlobal: string;"));
    host.insert(b.clone(), Arc::from("declare const BGlobal: string;"));

    host.link(entry.clone(), "a", a);
    host.link(entry.clone(), "b", b);

    let program = Program::new(host, vec![entry]);
    assert!(program.check().is_empty());

    program
      .files()
      .into_iter()
      .filter_map(|id| program.file_key(id).map(|key| (id.0, key.to_string())))
      .collect()
  }

  let first = run(vec!["b".to_string(), "a".to_string()]);
  let second = run(vec!["a".to_string(), "b".to_string()]);
  assert_eq!(first, second);
}
