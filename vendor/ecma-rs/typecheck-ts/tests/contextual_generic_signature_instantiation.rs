mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn contextual_generic_signature_is_instantiated_from_body() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("main.ts");
  let source = r#"
export const f: <T>(x: T) => T = x => { const n: number = x; return 1; };
"#;
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let x_offset = source
    .find("number = x")
    .map(|idx| idx as u32 + "number = ".len() as u32)
    .expect("offset for x usage");
  let x_ty = program.type_at(file_id, x_offset).expect("type at x usage");
  assert_eq!(program.display_type(x_ty).to_string(), "number");
}

#[test]
fn contextual_generic_signature_instantiation_respects_param_annotation() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("main.ts");
  let source = r#"
export const f: <T>(x: T) => T = (x: string) => x;
"#;
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );
}

