use std::collections::HashMap;
use std::sync::Arc;

mod common;

use diagnostics::TextRange;
use typecheck_ts::lib_support::{CompilerOptions, FileKind, LibFile};
use typecheck_ts::{Diagnostic, FileKey, Host, HostError, Program};

const TS2669_MESSAGE: &str =
  "Augmentations for the global scope can only be directly nested in external modules or ambient module declarations.";

#[derive(Default)]
struct TestHost {
  files: HashMap<FileKey, Arc<str>>,
  options: CompilerOptions,
  libs: Vec<LibFile>,
}

impl TestHost {
  fn new(options: CompilerOptions) -> Self {
    TestHost {
      files: HashMap::new(),
      options,
      libs: Vec::new(),
    }
  }

  fn insert(&mut self, key: FileKey, source: impl Into<Arc<str>>) {
    self.files.insert(key, source.into());
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

  fn resolve(&self, _from: &FileKey, _spec: &str) -> Option<FileKey> {
    None
  }

  fn compiler_options(&self) -> CompilerOptions {
    self.options.clone()
  }

  fn lib_files(&self) -> Vec<LibFile> {
    self.libs.clone()
  }

  fn file_kind(&self, file: &FileKey) -> FileKind {
    let name = file.as_str();
    if name.ends_with(".d.ts") {
      FileKind::Dts
    } else if name.ends_with(".tsx") {
      FileKind::Tsx
    } else if name.ends_with(".ts") {
      FileKind::Ts
    } else if name.ends_with(".jsx") {
      FileKind::Jsx
    } else if name.ends_with(".js") {
      FileKind::Js
    } else {
      FileKind::Ts
    }
  }
}

fn host_with_libs() -> TestHost {
  if cfg!(feature = "bundled-libs") {
    TestHost::default()
  } else {
    let mut options = CompilerOptions::default();
    options.no_default_lib = true;
    let mut host = TestHost::new(options);
    host.libs.push(common::core_globals_lib());
    host
  }
}

fn find_ts2669<'a>(diagnostics: &'a [Diagnostic]) -> Vec<&'a Diagnostic> {
  diagnostics
    .iter()
    .filter(|diag| diag.code.as_str() == "TS2669")
    .collect()
}

#[test]
fn module_augmentation_global_in_script_errors() {
  let mut host = host_with_libs();
  let key = FileKey::new("main.ts");
  let source = "declare global { interface Array<T> { x: number; } }";
  host.insert(key.clone(), Arc::<str>::from(source));

  let program = Program::new(host, vec![key.clone()]);
  let file_id = program.file_id(&key).expect("file id");
  let diagnostics = program.check();

  let matches = find_ts2669(&diagnostics);
  assert_eq!(
    matches.len(),
    1,
    "expected exactly one TS2669 diagnostic, got: {diagnostics:?}"
  );
  let diag = matches[0];
  assert_eq!(diag.message, TS2669_MESSAGE);
  assert_eq!(diag.primary.file, file_id);

  let start = source
    .find("global")
    .expect("global keyword should be present") as u32;
  assert_eq!(diag.primary.range, TextRange::new(start, start + 6));
}

#[test]
fn module_augmentation_global_in_dts_script_errors() {
  let mut host = host_with_libs();
  // Match TypeScript's default behavior (do not suppress diagnostics in `.d.ts`).
  host.options.skip_lib_check = false;
  let key = FileKey::new("main.d.ts");
  let source = "declare global { interface Array<T> { x: number; } }";
  host.insert(key.clone(), Arc::<str>::from(source));

  let program = Program::new(host, vec![key.clone()]);
  let file_id = program.file_id(&key).expect("file id");
  let diagnostics = program.check();

  let matches = find_ts2669(&diagnostics);
  assert_eq!(
    matches.len(),
    1,
    "expected exactly one TS2669 diagnostic, got: {diagnostics:?}"
  );
  let diag = matches[0];
  assert_eq!(diag.message, TS2669_MESSAGE);
  assert_eq!(diag.primary.file, file_id);

  let start = source
    .find("global")
    .expect("global keyword should be present") as u32;
  assert_eq!(diag.primary.range, TextRange::new(start, start + 6));
}

#[test]
fn module_augmentation_global_nested_in_namespace_errors() {
  let mut host = host_with_libs();
  let key = FileKey::new("main.ts");
  let source = "namespace A { declare global { interface Array<T> { x: number; } } }";
  host.insert(key.clone(), Arc::<str>::from(source));

  let program = Program::new(host, vec![key.clone()]);
  let file_id = program.file_id(&key).expect("file id");
  let diagnostics = program.check();

  let matches = find_ts2669(&diagnostics);
  assert_eq!(
    matches.len(),
    1,
    "expected exactly one TS2669 diagnostic, got: {diagnostics:?}"
  );
  let diag = matches[0];
  assert_eq!(diag.message, TS2669_MESSAGE);
  assert_eq!(diag.primary.file, file_id);

  let start = source
    .find("global")
    .expect("global keyword should be present") as u32;
  assert_eq!(diag.primary.range, TextRange::new(start, start + 6));
}

#[test]
fn module_augmentation_global_in_external_module_is_allowed() {
  let mut host = host_with_libs();
  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    Arc::<str>::from("export {};\ndeclare global { interface Array<T> { x: number; } }"),
  );

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();

  assert!(
    find_ts2669(&diagnostics).is_empty(),
    "did not expect TS2669 diagnostics, got: {diagnostics:?}"
  );
}
