//! Host-provided TypeScript declaration (`.d.ts`) builtins for the typechecked
//! `native-js` pipeline.
//!
//! The typechecked/HIR backend treats a small set of global identifiers as
//! *intrinsics* (they are not implemented in user-land TS). Frontends embedding
//! `typecheck-ts` should inject this `.d.ts` file via `Host::lib_files()` so the
//! checker assigns correct types to intrinsic references.
//!
//! Keeping the declarations and the intrinsic registry in the `native-js` crate
//! ensures that all frontends share the same intrinsic surface.

use std::sync::Arc;
use typecheck_ts::lib_support::{FileKind, LibFile};
use typecheck_ts::FileKey;

/// Stable "virtual path" for the native-js intrinsic declarations.
///
/// This is intentionally not a real filesystem path; it is used as a `FileKey`
/// for `Host::lib_files()` entries so module graphs and diagnostics remain
/// deterministic across environments.
pub const NATIVE_JS_BUILTINS_LIB_KEY: &str = "native-js:builtins.d.ts";

/// Intrinsics implemented by the native-js typechecked backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NativeJsIntrinsic {
  /// Print a number to stdout with a trailing newline.
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

const BUILTINS_D_TS: &str = r#"// native-js intrinsic declarations (very small subset)
// These are provided for the typechecked AOT pipeline (`native-js` binary).
//
// Note: keep signatures free of `any` so they are accepted by the strict validator.

declare function print(value: number): void;
"#;

/// Return the native-js `.d.ts` source for intrinsic declarations.
pub fn native_js_builtins_d_ts() -> &'static str {
  BUILTINS_D_TS
}

/// Return the host-provided `.d.ts` lib file describing native-js intrinsics.
pub fn native_js_builtins_lib() -> LibFile {
  // Keep the key stable and clearly separated from the TypeScript bundled libs
  // (`lib.*.d.ts`). This is a `Host::lib_files()` entry, so it does not need to
  // correspond to a real filesystem path.
  LibFile {
    key: FileKey::new(NATIVE_JS_BUILTINS_LIB_KEY),
    name: Arc::from("native-js builtins"),
    kind: FileKind::Dts,
    text: Arc::from(BUILTINS_D_TS),
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
}
