use inkwell::context::Context;
use native_js::{codegen, strict};
use std::collections::HashMap;
use std::sync::Arc;
use typecheck_ts::lib_support::FileKind;
use typecheck_ts::{FileKey, Host, HostError, Program};

#[derive(Clone, Default)]
struct TestHost {
  files: HashMap<FileKey, Arc<str>>,
  kinds: HashMap<FileKey, FileKind>,
}

impl TestHost {
  fn insert(&mut self, key: FileKey, kind: FileKind, source: &str) {
    self.files.insert(key.clone(), Arc::from(source.to_string()));
    self.kinds.insert(key, kind);
  }
}

impl Host for TestHost {
  fn file_text(&self, file: &FileKey) -> Result<Arc<str>, HostError> {
    self
      .files
      .get(file)
      .cloned()
      .ok_or_else(|| HostError::new(format!("missing file {file:?}")))
  }

  fn resolve(&self, _from: &FileKey, _specifier: &str) -> Option<FileKey> {
    None
  }

  fn file_kind(&self, file: &FileKey) -> FileKind {
    self.kinds.get(file).copied().unwrap_or(FileKind::Ts)
  }
}

fn extract_attr_group(ir: &str, needle: &str) -> u32 {
  let idx = ir
    .find(needle)
    .unwrap_or_else(|| panic!("missing function `{needle}` in IR:\n{ir}"));
  let line = ir[idx..].lines().next().expect("line");
  let hash_idx = line
    .find('#')
    .unwrap_or_else(|| panic!("missing attribute group on line:\n{line}\n\nIR:\n{ir}"));
  let digits = line[hash_idx + 1..]
    .chars()
    .take_while(|c| c.is_ascii_digit())
    .collect::<String>();
  digits
    .parse::<u32>()
    .unwrap_or_else(|_| panic!("invalid attribute group `{digits}` in line:\n{line}\n\nIR:\n{ir}"))
}

fn extract_def_line<'a>(ir: &'a str, needle: &str) -> &'a str {
  let idx = ir
    .find(needle)
    .unwrap_or_else(|| panic!("missing function `{needle}` in IR:\n{ir}"));
  ir[idx..].lines().next().expect("line")
}

fn assert_attr_group_has_stack_walking_attrs(ir: &str, group: u32) {
  let prefix = format!("attributes #{group} =");
  let idx = ir
    .find(&prefix)
    .unwrap_or_else(|| panic!("missing attribute group `{prefix}` in IR:\n{ir}"));
  let line = ir[idx..].lines().next().expect("attr line");
  assert!(
    line.contains("\"frame-pointer\"=\"all\""),
    "attribute group #{group} missing frame-pointer:\n{line}\n\nIR:\n{ir}"
  );
  assert!(
    line.contains("\"disable-tail-calls\"=\"true\"") || line.contains("disable-tail-calls"),
    "attribute group #{group} missing disable-tail-calls:\n{line}\n\nIR:\n{ir}"
  );
}

#[test]
fn hir_codegen_emits_stack_walking_attributes() {
  let source = r#"
    export function main(): number {
      return 1 + 2;
    }
  "#;

  let mut host = TestHost::default();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), FileKind::Ts, source);

  let program = Program::new(host, vec![key.clone()]);
  let tc_diags = program.check();
  assert!(
    tc_diags.is_empty(),
    "expected sample to typecheck cleanly, got: {tc_diags:#?}"
  );

  let file = program.file_id(&key).expect("file id");
  let strict_diags = strict::validate(&program, &[file]);
  assert!(
    strict_diags.is_empty(),
    "expected sample to pass strict validation, got: {strict_diags:#?}"
  );

  let entrypoint = strict::entrypoint(&program, file).expect("valid entrypoint");

  let context = Context::create();
  let module = codegen::codegen(
    &context,
    &program,
    file,
    entrypoint,
    codegen::CodegenOptions::default(),
  )
  .expect("codegen");

  let ir = module.print_to_string().to_string();
  let ts_def = extract_def_line(&ir, "define i32 @ts_main");
  let main_def = extract_def_line(&ir, "define i32 @main");

  assert!(
    ts_def.contains("gc \"coreclr\""),
    "ts_main missing `gc \"coreclr\"`:\n{ts_def}\n\nIR:\n{ir}"
  );
  assert!(
    main_def.contains("gc \"coreclr\""),
    "main missing `gc \"coreclr\"`:\n{main_def}\n\nIR:\n{ir}"
  );

  let ts_group = extract_attr_group(&ir, "define i32 @ts_main");
  let main_group = extract_attr_group(&ir, "define i32 @main");
  assert_attr_group_has_stack_walking_attrs(&ir, ts_group);
  assert_attr_group_has_stack_walking_attrs(&ir, main_group);
}
