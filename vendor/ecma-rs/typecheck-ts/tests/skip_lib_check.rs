use std::sync::Arc;

mod common;

use typecheck_ts::codes;
use typecheck_ts::lib_support::{CompilerOptions, FileKind, LibFile};
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn skip_lib_check_suppresses_dts_type_diagnostics() {
  let lib_key = FileKey::new("broken.d.ts");
  let entry_key = FileKey::new("entry.ts");
  let lib_source = "declare const value: MissingType;";

  let build_program = |skip_lib_check: bool| {
    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    options.skip_lib_check = skip_lib_check;
    let mut host = MemoryHost::with_options(options);
    host.add_lib(common::core_globals_lib());
    host.add_lib(LibFile {
      key: lib_key.clone(),
      name: Arc::from("broken.d.ts"),
      kind: FileKind::Dts,
      text: Arc::from(lib_source),
    });
    host.insert(entry_key.clone(), "/* noop */");
    Program::new(host, vec![entry_key.clone()])
  };

  let program = build_program(false);
  let lib_id = program
    .file_id(&lib_key)
    .expect("broken .d.ts file should be loaded");
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.primary.file == lib_id
        && diag.code.as_str() == codes::UNRESOLVED_TYPE_REFERENCE.as_str()),
    "expected unresolved type reference diagnostic from .d.ts when skip_lib_check is disabled, got {diagnostics:?}"
  );

  let program = build_program(true);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected .d.ts diagnostics to be suppressed when skip_lib_check is enabled, got {diagnostics:?}"
  );
}

#[test]
fn skip_lib_check_suppresses_dts_module_resolution_diagnostics() {
  let lib_key = FileKey::new("broken.d.ts");
  let entry_key = FileKey::new("entry.ts");
  let lib_source = "import { Foo } from \"./missing\";";

  let build_program = |skip_lib_check: bool| {
    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    options.skip_lib_check = skip_lib_check;
    let mut host = MemoryHost::with_options(options);
    host.add_lib(common::core_globals_lib());
    host.add_lib(LibFile {
      key: lib_key.clone(),
      name: Arc::from("broken.d.ts"),
      kind: FileKind::Dts,
      text: Arc::from(lib_source),
    });
    host.insert(entry_key.clone(), "/* noop */");
    Program::new(host, vec![entry_key.clone()])
  };

  let program = build_program(false);
  let lib_id = program
    .file_id(&lib_key)
    .expect("broken .d.ts file should be loaded");
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().any(|diag| diag.primary.file == lib_id
      && diag.code.as_str() == codes::UNRESOLVED_MODULE.as_str()),
    "expected unresolved module diagnostic from .d.ts when skip_lib_check is disabled, got {diagnostics:?}"
  );

  let program = build_program(true);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected .d.ts module resolution diagnostics to be suppressed when skip_lib_check is enabled, got {diagnostics:?}"
  );
}

#[test]
fn skip_lib_check_suppresses_dts_triple_slash_reference_diagnostics() {
  let lib_key = FileKey::new("broken.d.ts");
  let entry_key = FileKey::new("entry.ts");
  let lib_source = "/// <reference path=\"./missing.d.ts\" />\n";

  let build_program = |skip_lib_check: bool| {
    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    options.skip_lib_check = skip_lib_check;
    let mut host = MemoryHost::with_options(options);
    host.add_lib(common::core_globals_lib());
    host.add_lib(LibFile {
      key: lib_key.clone(),
      name: Arc::from("broken.d.ts"),
      kind: FileKind::Dts,
      text: Arc::from(lib_source),
    });
    host.insert(entry_key.clone(), "/* noop */");
    Program::new(host, vec![entry_key.clone()])
  };

  let program = build_program(false);
  let lib_id = program
    .file_id(&lib_key)
    .expect("broken .d.ts file should be loaded");
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().any(|diag| diag.primary.file == lib_id
      && diag.code.as_str() == codes::FILE_NOT_FOUND.as_str()),
    "expected triple-slash reference diagnostics from .d.ts when skip_lib_check is disabled, got {diagnostics:?}"
  );

  let program = build_program(true);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected triple-slash reference diagnostics from .d.ts to be suppressed when skip_lib_check is enabled, got {diagnostics:?}"
  );
}

#[test]
fn skip_lib_check_suppresses_dts_triple_slash_reference_types_diagnostics() {
  let lib_key = FileKey::new("broken.d.ts");
  let entry_key = FileKey::new("entry.ts");
  let lib_source = "/// <reference types=\"missing-types\" />\n";

  let build_program = |skip_lib_check: bool| {
    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    options.skip_lib_check = skip_lib_check;
    let mut host = MemoryHost::with_options(options);
    host.add_lib(common::core_globals_lib());
    host.add_lib(LibFile {
      key: lib_key.clone(),
      name: Arc::from("broken.d.ts"),
      kind: FileKind::Dts,
      text: Arc::from(lib_source),
    });
    host.insert(entry_key.clone(), "/* noop */");
    Program::new(host, vec![entry_key.clone()])
  };

  let program = build_program(false);
  let lib_id = program
    .file_id(&lib_key)
    .expect("broken .d.ts file should be loaded");
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().any(|diag| diag.primary.file == lib_id
      && diag.code.as_str() == codes::TYPE_DEFINITION_FILE_NOT_FOUND.as_str()),
    "expected triple-slash reference types diagnostics from .d.ts when skip_lib_check is disabled, got {diagnostics:?}"
  );

  let program = build_program(true);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected triple-slash reference types diagnostics from .d.ts to be suppressed when skip_lib_check is enabled, got {diagnostics:?}"
  );
}

#[test]
fn skip_lib_check_suppresses_dts_triple_slash_reference_lib_diagnostics() {
  let lib_key = FileKey::new("broken.d.ts");
  let entry_key = FileKey::new("entry.ts");
  let lib_source = "/// <reference lib=\"missing-lib\" />\n";

  let build_program = |skip_lib_check: bool| {
    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    options.skip_lib_check = skip_lib_check;
    let mut host = MemoryHost::with_options(options);
    host.add_lib(common::core_globals_lib());
    host.add_lib(LibFile {
      key: lib_key.clone(),
      name: Arc::from("broken.d.ts"),
      kind: FileKind::Dts,
      text: Arc::from(lib_source),
    });
    host.insert(entry_key.clone(), "/* noop */");
    Program::new(host, vec![entry_key.clone()])
  };

  let program = build_program(false);
  let lib_id = program
    .file_id(&lib_key)
    .expect("broken .d.ts file should be loaded");
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().any(|diag| diag.primary.file == lib_id
      && diag.code.as_str() == codes::LIB_DEFINITION_FILE_NOT_FOUND.as_str()),
    "expected triple-slash reference lib diagnostics from .d.ts when skip_lib_check is disabled, got {diagnostics:?}"
  );

  let program = build_program(true);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected triple-slash reference lib diagnostics from .d.ts to be suppressed when skip_lib_check is enabled, got {diagnostics:?}"
  );
}

#[test]
fn skip_lib_check_does_not_cascade_unresolved_dts_types_into_ts_diagnostics() {
  // When a `.d.ts` file contains an unresolved type reference, TypeScript emits
  // the diagnostic at the declaration site but still treats the declaration as
  // usable (the error type behaves like `any`). This avoids spurious follow-on
  // type errors in `.ts` sources that consume the declaration.
  //
  // With `skipLibCheck=true`, the `.d.ts` diagnostic itself is suppressed, so
  // the entire program should type-check without errors.
  let lib_key = FileKey::new("broken.d.ts");
  let entry_key = FileKey::new("entry.ts");
  let lib_source = "declare const value: MissingType;";
  let entry_source = "const n: number = value;";

  let build_program = |skip_lib_check: bool| {
    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    options.skip_lib_check = skip_lib_check;
    let mut host = MemoryHost::with_options(options);
    host.add_lib(common::core_globals_lib());
    host.add_lib(LibFile {
      key: lib_key.clone(),
      name: Arc::from("broken.d.ts"),
      kind: FileKind::Dts,
      text: Arc::from(lib_source),
    });
    host.insert(entry_key.clone(), entry_source);
    Program::new(host, vec![entry_key.clone()])
  };

  let program = build_program(false);
  let lib_id = program
    .file_id(&lib_key)
    .expect("broken .d.ts file should be loaded");
  let entry_id = program
    .file_id(&entry_key)
    .expect("entry file should be loaded");
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().any(|diag| diag.primary.file == lib_id
      && diag.code.as_str() == codes::UNRESOLVED_TYPE_REFERENCE.as_str()),
    "expected unresolved type reference diagnostic from .d.ts when skip_lib_check is disabled, got {diagnostics:?}"
  );
  assert!(
    !diagnostics
      .iter()
      .any(|diag| diag.primary.file == entry_id && diag.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected unresolved .d.ts types not to cascade into TS2322 in entry.ts, got {diagnostics:?}"
  );

  let program = build_program(true);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected unresolved .d.ts diagnostics to be suppressed and not cascade when skip_lib_check is enabled, got {diagnostics:?}"
  );
}

#[test]
fn skip_lib_check_does_not_cascade_unresolved_import_types_in_dts() {
  // Like unresolved named type references, unresolved `import("...")` types in
  // `.d.ts` files should behave like `any` (error type) and must not cascade
  // into follow-on diagnostics in `.ts` sources that consume the declaration.
  let lib_key = FileKey::new("broken.d.ts");
  let entry_key = FileKey::new("entry.ts");
  let lib_source = "declare const value: import(\"./missing\").Foo;";
  let entry_source = "const n: number = value;";

  let build_program = |skip_lib_check: bool| {
    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    options.skip_lib_check = skip_lib_check;
    let mut host = MemoryHost::with_options(options);
    host.add_lib(common::core_globals_lib());
    host.add_lib(LibFile {
      key: lib_key.clone(),
      name: Arc::from("broken.d.ts"),
      kind: FileKind::Dts,
      text: Arc::from(lib_source),
    });
    host.insert(entry_key.clone(), entry_source);
    Program::new(host, vec![entry_key.clone()])
  };

  let program = build_program(false);
  let lib_id = program
    .file_id(&lib_key)
    .expect("broken .d.ts file should be loaded");
  let entry_id = program
    .file_id(&entry_key)
    .expect("entry file should be loaded");
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().any(|diag| {
      diag.primary.file == lib_id
        && matches!(
          diag.code.as_str(),
          code if code == codes::UNRESOLVED_IMPORT_TYPE.as_str()
            || code == codes::UNRESOLVED_MODULE.as_str()
        )
    }),
    "expected unresolved import type diagnostics from .d.ts when skip_lib_check is disabled, got {diagnostics:?}"
  );
  assert!(
    !diagnostics
      .iter()
      .any(|diag| diag.primary.file == entry_id && diag.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected unresolved import types in .d.ts not to cascade into TS2322 in entry.ts, got {diagnostics:?}"
  );

  let program = build_program(true);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected unresolved import type diagnostics to be suppressed and not cascade when skip_lib_check is enabled, got {diagnostics:?}"
  );
}
