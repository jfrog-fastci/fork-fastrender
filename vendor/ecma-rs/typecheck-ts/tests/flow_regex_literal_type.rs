use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn flow_checker_keeps_regex_literals_unknown_without_regexp_lib() {
  let options = CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  };
  let mut host = MemoryHost::with_options(options);

  let file = FileKey::new("entry.ts");
  let src = "export const r = /foo/;";
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let exports = program.exports_of(file_id);
  let r_def = exports
    .get("r")
    .and_then(|entry| entry.def)
    .expect("r definition");
  let r_ty = program.type_of_def(r_def);
  assert_eq!(program.display_type(r_ty).to_string(), "unknown");

  let offset = src
    .find("/foo/")
    .map(|idx| idx as u32 + 1)
    .expect("offset for regex literal");
  let lit_ty = program.type_at(file_id, offset).expect("type at /foo/");
  assert_eq!(program.display_type(lit_ty).to_string(), "unknown");
}
