use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::{CompilerOptions, FileKind, LibFile};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn promise_lib() -> LibFile {
  LibFile {
    key: FileKey::new("core_promise.d.ts"),
    name: Arc::from("core_promise.d.ts"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"
interface Promise<T> {
  then(onfulfilled: (value: T) => any): any;
}
"#,
    ),
  }
}

#[test]
fn dynamic_import_returns_module_namespace_types() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  host.add_lib(promise_lib());

  let entry = FileKey::new("entry.ts");
  let module = FileKey::new("mod.ts");
  let entry_source = r#"
async function f() {
  const m = await import("./mod");
  const d = m.default;
  const n = m.named;
  return [d, n];
}
"#;
  host.insert(entry.clone(), Arc::from(entry_source.to_string()));
  host.insert(
    module.clone(),
    Arc::from(
      r#"
export default 123;
export const named = "ok";
"#
      .to_string(),
    ),
  );
  host.link(entry.clone(), "./mod", module.clone());

  let program = Program::new(host, vec![entry.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&entry).expect("file id for entry");
  let module_id = program.file_id(&module).expect("file id for module");
  assert_eq!(
    program.resolve_module(file_id, "./mod"),
    Some(module_id),
    "expected dynamic import specifier to be recorded as a module dependency",
  );
  let return_offset = entry_source
    .find("return [d, n]")
    .expect("return statement offset") as u32;
  let d_offset = return_offset + "return [".len() as u32;
  let n_offset = return_offset + "return [d, ".len() as u32;

  let d_ty = program.type_at(file_id, d_offset).expect("type of d");
  assert_eq!(program.display_type(d_ty).to_string(), "number");
  let n_ty = program.type_at(file_id, n_offset).expect("type of n");
  assert_eq!(program.display_type(n_ty).to_string(), "string");
}

#[test]
fn import_meta_resolves_importmeta_type() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("meta.ts");
  host.insert(
    file.clone(),
    Arc::from(
      r#"
interface ImportMeta { url: string }
export const url = import.meta.url;
"#
      .to_string(),
    ),
  );

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let exports = program.exports_of(file_id);
  let url_ty = exports
    .get("url")
    .and_then(|entry| entry.type_id)
    .expect("type for export url");
  assert_eq!(program.display_type(url_ty).to_string(), "string");
}
