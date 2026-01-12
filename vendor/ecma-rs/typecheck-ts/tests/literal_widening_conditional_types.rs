mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn literal_widening_conditional_types_reduce_through_typeof_refs() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = r#"
type IsExactlyString<T> = [T] extends [string]
  ? [string] extends [T]
    ? true
    : false
  : false;

const c = "a";
let l = "a";

export const const_is_string: IsExactlyString<typeof c> = false;
export const let_is_string: IsExactlyString<typeof l> = true;
"#;
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let exports = program.exports_of(file_id);

  let const_def = exports
    .get("const_is_string")
    .and_then(|entry| entry.def)
    .expect("missing const_is_string export def");
  let const_ty = program.type_of_def(const_def);
  assert_eq!(
    program.display_type(const_ty).to_string(),
    "false",
    "expected IsExactlyString<typeof c> to reduce to false"
  );

  let let_def = exports
    .get("let_is_string")
    .and_then(|entry| entry.def)
    .expect("missing let_is_string export def");
  let let_ty = program.type_of_def(let_def);
  assert_eq!(
    program.display_type(let_ty).to_string(),
    "true",
    "expected IsExactlyString<typeof l> to reduce to true"
  );
}
