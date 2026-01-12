use native_js::{compile_program, CompilerOptions, EmitKind};
use object::{Object, ObjectSection};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

#[test]
#[cfg(target_os = "linux")]
fn debug_info_emits_global_var_names() {
  let mut host = es5_host();
  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    r#"
      const global_debug_test_var = 123;
      export function main(): number {
        return global_debug_test_var;
      }
    "#,
  );

  let program = Program::new(host, vec![key.clone()]);
  let entry = program.file_id(&key).unwrap();

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Object;
  opts.debug = true;

  let artifact = compile_program(&program, entry, &opts).unwrap();
  assert_eq!(artifact.kind, EmitKind::Object);
  assert!(artifact.path.exists(), "missing artifact {}", artifact.path.display());

  let bytes = std::fs::read(&artifact.path).unwrap();
  let file = object::File::parse(bytes.as_slice()).expect("parse object file");

  let debug_info = file
    .section_by_name(".debug_info")
    .and_then(|s| s.data().ok())
    .unwrap_or_default();
  assert!(
    !debug_info.is_empty(),
    "expected object file to contain DWARF .debug_info when CompilerOptions.debug=true"
  );

  let needle = b"global_debug_test_var";
  let debug_str = file
    .section_by_name(".debug_str")
    .and_then(|s| s.data().ok())
    .unwrap_or_default();

  let found = debug_info
    .windows(needle.len())
    .any(|w| w == needle)
    || debug_str.windows(needle.len()).any(|w| w == needle);
  assert!(
    found,
    "expected DWARF to contain global variable name `{}`",
    std::str::from_utf8(needle).unwrap()
  );

  let _ = std::fs::remove_file(&artifact.path);
}

