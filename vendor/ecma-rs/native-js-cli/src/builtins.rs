use std::sync::Arc;
use typecheck_ts::lib_support::{FileKind, LibFile};
use typecheck_ts::FileKey;

pub fn native_js_builtins_lib() -> LibFile {
  // Keep the key stable and clearly separated from the TypeScript bundled libs (`lib.*.d.ts`).
  // This is a `Host::lib_files()` entry, so it does not need to correspond to a real filesystem
  // path.
  LibFile {
    key: FileKey::new("native-js:builtins.d.ts"),
    name: Arc::from("native-js builtins"),
    kind: FileKind::Dts,
    text: Arc::from(
      r#"// native-js-cli intrinsic declarations (very small subset)
// These are provided for the typechecked AOT pipeline (`native-js` binary).
//
// Note: keep signatures free of `any` so they are accepted by the strict validator.

declare function print(value: number): void;
"#,
    ),
  }
}

