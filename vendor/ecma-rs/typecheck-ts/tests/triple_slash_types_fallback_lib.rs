use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::{CompilerOptions, FileKind, LibFile};
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn triple_slash_types_in_libs_resolves_via_at_types_fallback() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  host.add_lib(common::core_globals_lib());

  let lib_key = FileKey::new("lib:custom_types_ref.d.ts");
  host.add_lib(LibFile {
    key: lib_key.clone(),
    name: Arc::from("custom_types_ref.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from("/// <reference types=\"node\" />\n"),
  });

  let entry = FileKey::new("entry.ts");
  host.insert(entry.clone(), Arc::from("const value = process;"));

  let types = FileKey::new("@types/node/index.d.ts");
  host.insert(types.clone(), Arc::from("declare const process: number;"));
  // Simulate a host that only knows about the explicit `@types/*` package path.
  host.link(lib_key.clone(), "@types/node", types.clone());

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected lib triple-slash types reference to resolve via @types fallback, got {diagnostics:?}"
  );

  let types_id = program
    .file_id(&types)
    .expect("@types/node/index.d.ts should be loaded");
  assert!(
    program.reachable_files().contains(&types_id),
    "expected lib triple-slash types to add @types/node to reachable files"
  );
}

