use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn flow_checker_keeps_regex_literals_unknown_without_regexp_lib() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });

  let file = FileKey::new("entry.ts");
  let src = "const r = /x/;";
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let _diagnostics = program.check();

  let file_id = program.file_id(&file).expect("file id");
  let offset = src
    .find("/x/")
    .map(|idx| idx as u32 + 1)
    .expect("offset for regex literal");
  let ty = program.type_at(file_id, offset).expect("type at /x/");
  assert_eq!(program.display_type(ty).to_string(), "unknown");
}

