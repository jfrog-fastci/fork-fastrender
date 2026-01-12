#![cfg(target_os = "linux")]

use gimli::read::{Dwarf, EndianSlice};
use gimli::{RunTimeEndian, SectionId};
use native_js::{compile_program, BackendKind, CompilerOptions, EmitKind, OptLevel};
use object::{Object, ObjectSection};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

type Reader<'a> = EndianSlice<'a, RunTimeEndian>;

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

fn compile_to_obj(program: &Program, entry: typecheck_ts::FileId, backend: BackendKind) -> Vec<u8> {
  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Object;
  opts.debug = true;
  opts.opt_level = OptLevel::O0;
  opts.backend = backend;

  let artifact = compile_program(program, entry, &opts).expect("compile_program");
  let bytes = std::fs::read(&artifact.path).expect("read object bytes");
  let _ = std::fs::remove_file(&artifact.path);
  bytes
}

fn load_dwarf(obj: &[u8]) -> Dwarf<Reader<'_>> {
  let file = object::File::parse(obj).expect("parse object file");
  let endian = if file.is_little_endian() {
    RunTimeEndian::Little
  } else {
    RunTimeEndian::Big
  };

  let load_section = |id: SectionId| -> Result<Reader<'_>, gimli::Error> {
    let data = match file.section_by_name(id.name()) {
      Some(section) => section.data().unwrap_or(&[][..]),
      None => &[][..],
    };
    Ok(EndianSlice::new(data, endian))
  };

  Dwarf::load(&load_section).expect("load DWARF sections")
}

fn row_file_name(
  dwarf: &Dwarf<Reader<'_>>,
  header: &gimli::read::LineProgramHeader<Reader<'_>>,
  row: &gimli::read::LineRow,
) -> Option<String> {
  let file = row.file(header)?;
  let path = file.path_name();
  let s = match path {
    gimli::read::AttributeValue::String(s) => s.to_string_lossy().to_string(),
    gimli::read::AttributeValue::DebugStrRef(off) => dwarf
      .debug_str
      .get_str(off)
      .ok()?
      .to_string_lossy()
      .to_string(),
    gimli::read::AttributeValue::DebugLineStrRef(off) => dwarf
      .debug_line_str
      .get_str(off)
      .ok()?
      .to_string_lossy()
      .to_string(),
    other => {
      // This test only cares that filenames can be resolved; if the DWARF encoding uses an
      // unexpected string form, ignore the row.
      let _ = other;
      return None;
    }
  };
  Some(s)
}

#[test]
fn dwarf_line_table_columns_are_utf8_byte_offsets_hir_backend() {
  dwarf_line_table_columns_are_utf8_byte_offsets(BackendKind::Hir);
}

#[test]
fn dwarf_line_table_columns_are_utf8_byte_offsets_ssa_backend() {
  dwarf_line_table_columns_are_utf8_byte_offsets(BackendKind::Ssa);
}

fn dwarf_line_table_columns_are_utf8_byte_offsets(backend: BackendKind) {
  let mut host = es5_host();
  let key = FileKey::new("main.ts");
  let source = r#"export function add(x: number, y: number): number {
  /* α */ return x + y;
}

export function main(): number {
  return add(1, 2);
}"#;
  host.insert(key.clone(), source);

  let program = Program::new(host, vec![key.clone()]);
  let entry = program.file_id(&key).expect("file id");

  // Compute expected (line, col) for the `x` in `return x + y`, using UTF-8 byte
  // offsets within the line (DWARF convention).
  let needle = "return x + y";
  let x_offset = source
    .find(needle)
    .map(|i| i + "return ".len())
    .expect("needle exists");
  let line_start = source[..x_offset]
    .rfind('\n')
    .map(|i| i + 1)
    .unwrap_or(0);
  let expected_line = source[..x_offset].bytes().filter(|b| *b == b'\n').count() + 1;
  let expected_col_bytes = (x_offset - line_start) + 1;

  // Sanity check: this test should actually exercise multibyte handling.
  let expected_col_chars = source[line_start..x_offset].chars().count() + 1;
  assert_ne!(
    expected_col_bytes, expected_col_chars,
    "expected the multibyte `α` to make byte and char columns differ"
  );

  let obj = compile_to_obj(&program, entry, backend);
  let dwarf = load_dwarf(&obj);

  let mut found_exact = false;
  let mut found_any_nonzero_col = false;
  let mut seen_cols: Vec<u64> = Vec::new();

  let mut iter = dwarf.units();
  while let Some(header) = iter.next().expect("unit header") {
    let unit = dwarf.unit(header).expect("unit");
    let Some(program) = unit.line_program.clone() else {
      continue;
    };

    let mut rows = program.rows();
    while let Some((header, row)) = rows.next_row().expect("next_row") {
      let Some(file_name) = row_file_name(&dwarf, header, row) else {
        continue;
      };
      if !file_name.ends_with("main.ts") {
        continue;
      }
      let Some(line) = row.line() else {
        continue;
      };
      let line = line.get() as usize;
      if line != expected_line {
        continue;
      }

      let col = match row.column() {
        gimli::read::ColumnType::Column(c) => c.get() as u64,
        gimli::read::ColumnType::LeftEdge => 0,
      };
      if col != 0 {
        found_any_nonzero_col = true;
      }
      seen_cols.push(col);
      if col == expected_col_bytes as u64 {
        found_exact = true;
      }
    }
  }

  assert!(
    found_any_nonzero_col,
    "expected to find at least one line-table row for line {expected_line} with a non-zero column; seen cols: {seen_cols:?}"
  );
  assert!(
    found_exact,
    "expected to find a line-table row at line {expected_line} with byte column {expected_col_bytes}; seen cols: {seen_cols:?}"
  );
}
