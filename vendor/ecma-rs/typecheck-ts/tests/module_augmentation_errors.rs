use std::collections::HashMap;
use std::sync::Arc;

mod common;

use typecheck_ts::codes;
use typecheck_ts::lib_support::{CompilerOptions, FileKind, LibFile, ModuleKind};
use typecheck_ts::{Diagnostic, FileKey, Host, HostError, Program};

#[derive(Default)]
struct ModuleHost {
  files: HashMap<FileKey, Arc<str>>,
  edges: HashMap<(FileKey, String), FileKey>,
  options: CompilerOptions,
  libs: Vec<LibFile>,
}

impl ModuleHost {
  fn new(options: CompilerOptions) -> Self {
    Self {
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
    if file.as_str().ends_with(".d.ts") {
      FileKind::Dts
    } else {
      FileKind::Ts
    }
  }
}

fn has_code(diagnostics: &[Diagnostic], code: &str) -> bool {
  diagnostics.iter().any(|diag| diag.code.as_str() == code)
}

fn assert_has_code(diagnostics: &[Diagnostic], code: &str) {
  assert!(
    has_code(diagnostics, code),
    "expected diagnostics to include {code}, got {diagnostics:?}"
  );
}

fn assert_lacks_code(diagnostics: &[Diagnostic], code: &str) {
  assert!(
    !has_code(diagnostics, code),
    "expected diagnostics to exclude {code}, got {diagnostics:?}"
  );
}

#[test]
fn namespace_not_merged_with_function_default_export_emits_ts2395() {
  let options = CompilerOptions {
    no_default_lib: true,
    skip_lib_check: false,
    ..CompilerOptions::default()
  };

  let entry = FileKey::new("replace-in-file/types/index.d.ts");
  let mut host = ModuleHost::new(options);
  host.insert(
    entry.clone(),
    r#"declare module 'replace-in-file' {
  export function replaceInFile(config: unknown): Promise<unknown[]>;
  export default replaceInFile;

  namespace replaceInFile {
    export function sync(config: unknown): unknown[];
  }
}
"#,
  );

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();
  assert_has_code(&diagnostics, "TS2395");
}

#[test]
fn ambient_external_module_in_another_external_module_emits_ts2664_and_unresolved_import() {
  let options = CompilerOptions {
    module: Some(ModuleKind::CommonJs),
    no_default_lib: true,
    skip_lib_check: false,
    ..CompilerOptions::default()
  };

  let entry = FileKey::new("main.ts");
  let mut host = ModuleHost::new(options);
  host.insert(
    entry.clone(),
    r#"class D { }
export = D;

declare module "ext" { export class C { } }

import ext = require("ext");
var x = ext;
"#,
  );

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();
  assert_has_code(&diagnostics, "TS2664");
  assert_has_code(&diagnostics, codes::UNRESOLVED_MODULE.as_str());
}

#[test]
fn ambient_external_module_inside_non_ambient_external_module_emits_ts2668_and_ts2664() {
  let options = CompilerOptions {
    no_default_lib: true,
    skip_lib_check: false,
    ..CompilerOptions::default()
  };

  let entry = FileKey::new("main.ts");
  let mut host = ModuleHost::new(options);
  host.insert(entry.clone(), r#"export declare module "M" { }"#);

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();
  assert_has_code(&diagnostics, "TS2668");
  assert_has_code(&diagnostics, "TS2664");
}

#[test]
fn module_augmentation_in_dependency_does_not_report_ts2664() {
  let options = CompilerOptions {
    no_default_lib: true,
    skip_lib_check: false,
    ..CompilerOptions::default()
  };

  let entry = FileKey::new("dependency.d.ts");
  let mut host = ModuleHost::new(options);
  host.insert(
    entry.clone(),
    r#"declare module "ext" { }
export {};
"#,
  );

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();
  assert_lacks_code(&diagnostics, "TS2664");
}

#[test]
fn module_augmentation_errors_use_internal_unresolved_module_and_unknown_export_codes() {
  let options = CompilerOptions {
    no_default_lib: true,
    skip_lib_check: false,
    ..CompilerOptions::default()
  };

  let pkg = FileKey::new("pkg.ts");
  let augment = FileKey::new("augment.ts");
  let main = FileKey::new("main.ts");

  let mut host = ModuleHost::new(options);
  host.insert(pkg.clone(), "export const x = 1;");
  host.insert(
    augment.clone(),
    r#"export {};
import "./missing";
declare module "./pkg" {
  export const y: string;
}
"#,
  );
  host.insert(main.clone(), r#"export { z } from "./pkg";"#);

  host.link(augment.clone(), "./pkg", pkg.clone());
  host.link(main.clone(), "./pkg", pkg);

  let program = Program::new(host, vec![main, augment]);
  let diagnostics = program.check();

  assert_has_code(&diagnostics, codes::UNRESOLVED_MODULE.as_str());
  assert_has_code(&diagnostics, codes::UNKNOWN_EXPORT.as_str());
}

