#![cfg(target_os = "linux")]

use gimli::read::{Dwarf, EndianSlice};
use gimli::{RunTimeEndian, SectionId};
use native_js::{compile_program, CompilerOptions, EmitKind, OptLevel};
use object::{Object, ObjectSection};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

type Reader<'a> = EndianSlice<'a, RunTimeEndian>;

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

fn compile_unit_name(
  dwarf: &Dwarf<Reader<'_>>,
  unit: &gimli::read::Unit<Reader<'_>, usize>,
) -> Option<String> {
  let mut entries = unit.entries();
  let (_, entry) = entries.next_dfs().ok()??;
  let attr = entry.attr(gimli::DW_AT_name).ok()??;
  let name = dwarf.attr_string(unit, attr.value()).ok()?;
  Some(name.to_string_lossy().to_string())
}

fn row_file_name(
  dwarf: &Dwarf<Reader<'_>>,
  unit: &gimli::read::Unit<Reader<'_>, usize>,
  header: &gimli::read::LineProgramHeader<Reader<'_>>,
  row: &gimli::read::LineRow,
) -> Option<String> {
  let file = row.file(header)?;
  let path = dwarf.attr_string(unit, file.path_name()).ok()?;
  Some(path.to_string_lossy().into_owned())
}

fn compile_to_obj(program: &Program, entry: typecheck_ts::FileId) -> Vec<u8> {
  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Object;
  opts.debug = true;
  opts.opt_level = OptLevel::O0;

  let artifact = compile_program(program, entry, &opts).expect("compile_program");
  let bytes = std::fs::read(&artifact.path).expect("read object bytes");
  let _ = std::fs::remove_file(&artifact.path);
  bytes
}

#[test]
fn dwarf_line_program_has_main_ts_rows_for_real_code() {
  let mut host = es5_host();
  let main_key = FileKey::new("main.ts");

  // Line numbers here are part of the test: keep each statement on its own line.
  let main_src = r#"export function main(): number {
  let x = 1;
  let y = 2;
  return x + y;
}"#;
  host.insert(main_key.clone(), main_src);

  let program = Program::new(host, vec![main_key.clone()]);
  let entry = program.file_id(&main_key).expect("entry file id");

  let obj = compile_to_obj(&program, entry);
  let dwarf = load_dwarf(&obj);

  let mut units = dwarf.units();
  let mut cu_names = Vec::new();
  let mut found_return_line_row = false;

  while let Some(header) = units.next().expect("iterate units") {
    let unit = dwarf.unit(header).expect("parse unit");
    if let Some(name) = compile_unit_name(&dwarf, &unit) {
      cu_names.push(name);
    }

    let Some(program) = unit.line_program.clone() else {
      continue;
    };

    let mut rows = program.rows();
    while let Some((header, row)) = rows.next_row().expect("next_row") {
      let Some(file_name) = row_file_name(&dwarf, &unit, header, row) else {
        continue;
      };
      if !file_name.ends_with("main.ts") {
        continue;
      }
      let Some(line) = row.line() else {
        continue;
      };
      // `return x + y;`
      if line.get() == 4 {
        found_return_line_row = true;
        break;
      }
    }
  }

  assert!(
    found_return_line_row,
    "did not find any decoded DWARF line-table row mapping a machine address back to main.ts:4 (`return x + y;`). \
compile_units={cu_names:#?}"
  );
}

#[test]
fn dwarf_line_program_references_both_main_and_math_files() {
  let mut host = es5_host();
  let main_key = FileKey::new("main.ts");
  let math_key = FileKey::new("math.ts");

  let math_src = r#"export function add(x: number, y: number): number {
  return x + y;
}"#;

  let main_src = r#"import { add } from "./math.ts";

export function main(): number {
  return add(1, 2);
}"#;

  host.insert(main_key.clone(), main_src);
  host.insert(math_key.clone(), math_src);
  host.link(main_key.clone(), "./math.ts", math_key.clone());

  let program = Program::new(host, vec![main_key.clone()]);
  let entry = program.file_id(&main_key).expect("entry file id");

  let obj = compile_to_obj(&program, entry);
  let dwarf = load_dwarf(&obj);

  let mut units = dwarf.units();
  let mut found_main = false;
  let mut found_math = false;

  while let Some(header) = units.next().expect("iterate units") {
    let unit = dwarf.unit(header).expect("parse unit");
    let Some(program) = unit.line_program.clone() else {
      continue;
    };
    let mut rows = program.rows();
    while let Some((header, row)) = rows.next_row().expect("next_row") {
      let Some(file_name) = row_file_name(&dwarf, &unit, header, row) else {
        continue;
      };
      if file_name.ends_with("main.ts") {
        found_main = true;
      }
      if file_name.ends_with("math.ts") {
        found_math = true;
      }
      if found_main && found_math {
        break;
      }
    }
  }

  assert!(
    found_main && found_math,
    "expected decoded DWARF line-table rows to reference both `main.ts` and `math.ts`; found_main={found_main} found_math={found_math}"
  );
}
