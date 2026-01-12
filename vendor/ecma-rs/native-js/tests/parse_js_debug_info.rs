use native_js::compiler::compile_typescript_to_artifact;
use native_js::{CompileOptions, EmitKind, OptLevel};
use object::{Object, ObjectSection};

fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
  haystack
    .windows(needle.len())
    .any(|window| window == needle)
}

#[test]
fn parse_js_emits_dwarf_debug_info_in_object() {
  let dir = tempfile::tempdir().expect("create tempdir");
  let obj_path = dir.path().join("out.o");

  // Keep the source simple: avoid builtins/stdlib calls so the test is fully self-contained.
  let source = r#"
    let x = 1;
    let y = x + 2;
    y;
  "#;

  let mut opts = CompileOptions::default();
  opts.emit = EmitKind::Object;
  opts.debug = true;
  // Keep compilation fast/deterministic; we only care about debug info presence.
  opts.opt_level = OptLevel::O0;

  let out = compile_typescript_to_artifact(source, opts, Some(obj_path.clone()))
    .expect("compile_typescript_to_artifact");
  assert_eq!(out.path, obj_path);

  let bytes = std::fs::read(&out.path).expect("read object file");
  let obj = object::File::parse(bytes.as_slice()).expect("parse object file");

  // DWARF section naming differs by object format. Prefer the canonical ELF names but fall back to
  // Mach-O section names so the test can still run on non-Linux hosts.
  let (debug_info_name, debug_line_name) = match obj.format() {
    object::BinaryFormat::Elf | object::BinaryFormat::Coff => (".debug_info", ".debug_line"),
    object::BinaryFormat::MachO => ("__debug_info", "__debug_line"),
    other => panic!("unsupported object format for debug info test: {other:?}"),
  };

  let debug_info = obj
    .section_by_name(debug_info_name)
    .unwrap_or_else(|| panic!("missing {debug_info_name} section"));
  let debug_line = obj
    .section_by_name(debug_line_name)
    .unwrap_or_else(|| panic!("missing {debug_line_name} section"));

  let _debug_info_bytes = debug_info.data().expect("read debug_info section");
  let _debug_line_bytes = debug_line.data().expect("read debug_line section");

  // The parse-js pipeline emits a synthetic filename for in-memory compilation.
  let needle = b"<input>.ts";
  assert!(
    bytes_contain(bytes.as_slice(), needle),
    "expected object to contain synthetic file name in DWARF, but it did not"
  );
}
