use std::collections::HashMap;
use std::sync::Arc;

mod common;

use typecheck_ts::codes;
use typecheck_ts::lib_support::{CompilerOptions, FileKind, LibFile};
use typecheck_ts::{FileKey, Host, HostError, Program};

#[derive(Default)]
struct ModuleHost {
  files: HashMap<FileKey, Arc<str>>,
  edges: HashMap<(FileKey, String), FileKey>,
  options: CompilerOptions,
  libs: Vec<LibFile>,
}

impl ModuleHost {
  fn new(options: CompilerOptions) -> Self {
    ModuleHost {
      files: HashMap::new(),
      edges: HashMap::new(),
      options,
      libs: vec![common::core_globals_lib()],
    }
  }

  fn insert(&mut self, key: FileKey, text: &str) {
    self.files.insert(key, Arc::from(text));
  }

  fn link(&mut self, from: FileKey, specifier: &str, to: FileKey) {
    self.edges.insert((from, specifier.to_string()), to);
  }
}

impl Host for ModuleHost {
  fn file_text(&self, file: &FileKey) -> Result<Arc<str>, HostError> {
    self
      .files
      .get(file)
      .cloned()
      .ok_or_else(|| HostError::new(format!("missing file {file:?}")))
  }

  fn resolve(&self, from: &FileKey, specifier: &str) -> Option<FileKey> {
    self
      .edges
      .get(&(from.clone(), specifier.to_string()))
      .cloned()
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

#[test]
fn untypedModuleImport_withAugmentation_emits_ts2665() {
  let options = CompilerOptions {
    no_default_lib: true,
    skip_lib_check: false,
    ..CompilerOptions::default()
  };

  let entry = FileKey::new("/a.ts");
  let foo_js = FileKey::new("/node_modules/foo/index.js");

  let mut host = ModuleHost::new(options);
  host.insert(
    entry.clone(),
    r#"declare module "foo" { export const x: number; }
import { x } from "foo";
x;
"#,
  );
  host.insert(foo_js.clone(), "");
  host.link(entry.clone(), "foo", foo_js);

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().any(|diag| diag.code.as_str() == "TS2665"),
    "expected TS2665 when module augmentation resolves to JS: {diagnostics:?}"
  );
  assert!(
    !diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::UNRESOLVED_MODULE.as_str()),
    "unexpected unresolved module diagnostics: {diagnostics:?}"
  );
  assert!(
    !diagnostics.iter().any(|diag| diag.code.as_str() == "TS2664"),
    "unexpected TS2664 diagnostics: {diagnostics:?}"
  );
}

#[test]
fn untypedModuleImport_withAugmentation2_emits_ts2665_in_dts_external_module() {
  let options = CompilerOptions {
    no_default_lib: true,
    skip_lib_check: false,
    ..CompilerOptions::default()
  };

  let entry = FileKey::new("/a.ts");
  let augmenter = FileKey::new("/node_modules/augmenter/index.d.ts");
  let js = FileKey::new("/node_modules/js/index.js");

  let mut host = ModuleHost::new(options);
  host.insert(entry.clone(), r#"import { } from "augmenter";"#);
  host.insert(
    augmenter.clone(),
    r#"declare module "js" { export const j: number; }
export {};
"#,
  );
  host.insert(js.clone(), "");
  host.link(entry.clone(), "augmenter", augmenter.clone());
  host.link(augmenter.clone(), "js", js.clone());

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();
  let augmenter_id = program.file_id(&augmenter).expect("augmenter file id");
  let js_id = program.file_id(&js).expect("js module file id");
  assert_eq!(
    program.resolve_module(augmenter_id, "js"),
    Some(js_id),
    "expected host module resolution to map \"js\" from augmenter: {diagnostics:?}"
  );
  assert!(
    diagnostics.iter().any(|diag| diag.code.as_str() == "TS2665"),
    "expected TS2665 when module augmentation resolves to JS: {diagnostics:?}"
  );
}
