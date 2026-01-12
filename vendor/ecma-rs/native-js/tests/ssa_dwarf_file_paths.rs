#![cfg(target_os = "linux")]

use native_js::{compile_program, BackendKind, CompilerOptions, EmitKind, OptLevel};
use object::{Object, ObjectSection};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
  if needle.is_empty() {
    return true;
  }
  haystack.windows(needle.len()).any(|w| w == needle)
}

fn debug_str_contains_nul_terminated<'data>(file: &object::File<'data>, s: &str) -> bool {
  let bytes = s.as_bytes();
  let mut needle = Vec::with_capacity(bytes.len() + 2);
  needle.push(0);
  needle.extend_from_slice(bytes);
  needle.push(0);

  for section in file.sections() {
    let Ok(name) = section.name() else {
      continue;
    };
    // Most strings live in `.debug_str`. Some toolchains emit `.zdebug_str`.
    if name != ".debug_str" && name != ".zdebug_str" {
      continue;
    }

    let Ok(data) = section.uncompressed_data() else {
      continue;
    };
    let data = data.as_ref();

    // `.debug_str` is a NUL-terminated string table; the first entry may start at offset 0.
    if data.starts_with(bytes) && data.get(bytes.len()) == Some(&0) {
      return true;
    }
    if contains_subslice(data, &needle) {
      return true;
    }
  }

  false
}

#[test]
fn ssa_debug_info_splits_nested_file_key_paths_into_filename_and_directory() {
  let mut host = es5_host();
  let key = FileKey::new("src/main.ts");
  host.insert(key.clone(), "export function main(): number { return 0; }\n");

  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");
  let entry = program.file_id(&key).unwrap();

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Object;
  opts.backend = BackendKind::Ssa;
  opts.debug = true;
  opts.opt_level = OptLevel::O0;

  let artifact = compile_program(&program, entry, &opts).unwrap();
  let bytes = std::fs::read(&artifact.path).unwrap();
  let _ = std::fs::remove_file(&artifact.path);

  let file = object::File::parse(&*bytes).unwrap();
  assert!(
    file.section_by_name(".debug_info").is_some(),
    "expected .debug_info section in debug object"
  );
  assert!(
    file.section_by_name(".debug_line").is_some(),
    "expected .debug_line section in debug object"
  );

  assert!(
    debug_str_contains_nul_terminated(&file, "main.ts"),
    "expected DWARF to store `main.ts` as a basename in `.debug_str`"
  );
  assert!(
    debug_str_contains_nul_terminated(&file, "src"),
    "expected DWARF to store `src` as a directory component in `.debug_str`"
  );
  assert!(
    !debug_str_contains_nul_terminated(&file, "src/main.ts"),
    "expected DWARF to store `src` and `main.ts` as separate directory/filename fields (not a single `src/main.ts` string)"
  );
}

