//! Host-provided TypeScript declaration (`.d.ts`) builtins for `native-js`.
//!
//! Frontends embedding `typecheck-ts` inject these libraries via `Host::lib_files()` so programs
//! can typecheck when referencing native-js intrinsics.
//!
//! There are two pipelines today:
//! - `project`: legacy `parse-js` emitter used by `native-js-cli --pipeline project`.
//! - `checked`: HIR/typechecked pipeline used by `native-js` and `native-js-cli --pipeline checked`.
//!
//! Keeping the declaration strings and intrinsic registry in the `native-js` crate ensures that all
//! frontends share the same intrinsic surface.

use std::sync::Arc;
use typecheck_ts::lib_support::{FileKind, LibFile};
use typecheck_ts::FileKey;

/// Stable "virtual path" for the project-pipeline builtin declarations.
pub const PROJECT_BUILTINS_FILE_KEY: &str = "native-js:builtins.project.d.ts";

/// Stable "virtual path" for the checked-pipeline builtin declarations.
pub const CHECKED_BUILTINS_FILE_KEY: &str = "native-js:builtins.checked.d.ts";

/// Backwards-compatible alias for the checked-pipeline builtins lib key.
pub const NATIVE_JS_BUILTINS_LIB_KEY: &str = CHECKED_BUILTINS_FILE_KEY;

/// Intrinsics implemented by the native-js typechecked backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NativeJsIntrinsic {
  /// Print a `number` (or interned `string` id) to stdout with a trailing newline.
  Print,
}

impl NativeJsIntrinsic {
  /// All currently supported intrinsics.
  pub const ALL: &'static [NativeJsIntrinsic] = &[NativeJsIntrinsic::Print];

  /// Global identifier name exposed to TypeScript.
  pub const fn name(self) -> &'static str {
    match self {
      NativeJsIntrinsic::Print => "print",
    }
  }
}

/// Look up an intrinsic by its global identifier name.
pub fn intrinsic_by_name(name: &str) -> Option<NativeJsIntrinsic> {
  match name {
    "print" => Some(NativeJsIntrinsic::Print),
    _ => None,
  }
}

/// `.d.ts` declarations for the legacy `parse-js` based `native-js-cli --pipeline project` emitter.
///
/// Keep these signatures aligned with `native_js::codegen::builtins`:
/// - `console.log(...)` and `print(...)` accept varargs of a small primitive "printable" set.
/// - `assert`, `panic`, and `trap` match the backend's builtin recognizer.
/// - Avoid permissive types so the checked pipeline can keep using strict validation rules.
pub const PROJECT_BUILTINS_D_TS: &str = r#"
type __NativeJsPrintable = string | number | boolean | null | undefined;

declare const console: { log(...values: __NativeJsPrintable[]): void };

declare function print(...values: __NativeJsPrintable[]): void;
declare function assert(cond: __NativeJsPrintable, msg?: __NativeJsPrintable): void;
declare function panic(msg?: __NativeJsPrintable): void;
declare function trap(): void;
"#;

/// `.d.ts` declarations for the typechecked HIR-backed pipeline (`native-js` and
/// `native-js-cli --pipeline checked`).
///
/// Keep this restrictive so `typecheck-ts` rejects programs the backend cannot lower.
pub const CHECKED_BUILTINS_D_TS: &str = r#"// native-js intrinsic declarations (very small subset)
// These are provided for the typechecked AOT pipeline (`native-js` binary).
//
// Note: keep signatures free of `any` so they are accepted by the strict validator.

declare function print(value: number | string): void;
"#;

/// Return the checked-pipeline `.d.ts` source for intrinsic declarations.
///
/// This is kept as a convenience API for embedders that only care about the typechecked pipeline.
pub fn native_js_builtins_d_ts() -> &'static str {
  CHECKED_BUILTINS_D_TS
}

/// Return the host-provided `.d.ts` lib file describing native-js checked-pipeline intrinsics.
///
/// This is kept as a convenience alias for embedders of the typechecked pipeline.
pub fn native_js_builtins_lib() -> LibFile {
  checked_builtins_lib()
}

pub fn project_builtins_lib() -> LibFile {
  LibFile {
    key: FileKey::new(PROJECT_BUILTINS_FILE_KEY),
    name: Arc::from("native-js project builtins"),
    kind: FileKind::Dts,
    text: Arc::from(PROJECT_BUILTINS_D_TS),
  }
}

pub fn checked_builtins_lib() -> LibFile {
  LibFile {
    key: FileKey::new(CHECKED_BUILTINS_FILE_KEY),
    name: Arc::from("native-js checked builtins"),
    kind: FileKind::Dts,
    text: Arc::from(CHECKED_BUILTINS_D_TS),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
  use typecheck_ts::{FileKey, MemoryHost, Program};

  #[test]
  fn builtins_lib_is_loaded_into_program() {
    let builtins = native_js_builtins_lib();
    assert_eq!(builtins.key.as_str(), NATIVE_JS_BUILTINS_LIB_KEY);

    let mut host = MemoryHost::with_options(TsCompilerOptions {
      libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
      ..Default::default()
    });
    host.add_lib(builtins.clone());

    let entry_key = FileKey::new("entry.ts");
    host.insert(
      entry_key.clone(),
      "export function main(): number { print(1 + 2); return 0; }\n",
    );

    let program = Program::new(host, vec![entry_key]);
    let diagnostics = program.check();
    assert!(
      diagnostics.is_empty(),
      "expected no diagnostics, got: {diagnostics:#?}"
    );

    let builtins_file = program
      .file_id(&builtins.key)
      .expect("expected native-js builtins lib to be loaded");
    assert_eq!(program.file_key(builtins_file), Some(builtins.key.clone()));
    let text = program
      .file_text(builtins_file)
      .expect("expected native-js builtins lib to have text");
    assert!(
      text.contains("declare function print"),
      "expected builtins lib to declare `print`, got:\n{text}"
    );
  }

  #[test]
  fn project_builtins_d_ts_has_expected_decls() {
    let text = PROJECT_BUILTINS_D_TS;
    assert!(
      text.contains("type __NativeJsPrintable = string | number | boolean | null | undefined;"),
      "expected printable union, got:\n{text}"
    );
    assert!(
      text.contains("declare const console: { log(...values: __NativeJsPrintable[]): void };"),
      "expected console.log builtin, got:\n{text}"
    );
    assert!(
      text.contains("declare function print(...values: __NativeJsPrintable[]): void;"),
      "expected print builtin, got:\n{text}"
    );
    assert!(
      text.contains("declare function assert(cond: __NativeJsPrintable, msg?: __NativeJsPrintable): void;"),
      "expected assert builtin, got:\n{text}"
    );
    assert!(
      text.contains("declare function panic(msg?: __NativeJsPrintable): void;"),
      "expected panic builtin, got:\n{text}"
    );
    assert!(
      text.contains("declare function trap(): void;"),
      "expected trap builtin, got:\n{text}"
    );

    let lib = project_builtins_lib();
    assert_eq!(lib.key.as_str(), PROJECT_BUILTINS_FILE_KEY);
    assert_eq!(lib.kind, FileKind::Dts);
    assert_eq!(lib.text.as_ref(), PROJECT_BUILTINS_D_TS);
  }

  #[test]
  fn checked_builtins_d_ts_has_expected_decls() {
    let text = CHECKED_BUILTINS_D_TS;
    assert!(
      text.contains("declare function print(value: number | string): void;"),
      "expected print builtin, got:\n{text}"
    );
    let code_only = text
      .lines()
      .filter(|line| !line.trim_start().starts_with("//"))
      .collect::<Vec<_>>()
      .join("\n");
    assert!(
      !code_only.contains("any"),
      "checked pipeline builtins must not use the `any` type, got:\n{code_only}"
    );

    let lib = checked_builtins_lib();
    assert_eq!(lib.key.as_str(), CHECKED_BUILTINS_FILE_KEY);
    assert_eq!(lib.kind, FileKind::Dts);
    assert_eq!(lib.text.as_ref(), CHECKED_BUILTINS_D_TS);
  }
}
