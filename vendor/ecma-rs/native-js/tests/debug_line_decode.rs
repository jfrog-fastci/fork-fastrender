#![cfg(target_os = "linux")]

use addr2line::Context;
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

fn compile_unit_language(unit: &gimli::read::Unit<Reader<'_>, usize>) -> Option<gimli::DwLang> {
  let mut entries = unit.entries();
  let (_, entry) = entries.next_dfs().ok()??;
  let attr = entry.attr(gimli::DW_AT_language).ok()??;
  match attr.value() {
    gimli::read::AttributeValue::Language(lang) => Some(lang),
    other => other
      .udata_value()
      .and_then(|v| u16::try_from(v).ok())
      .map(gimli::DwLang),
  }
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

fn addr2line_location_matches<R: gimli::read::Reader>(
  ctx: &Context<R>,
  addr: u64,
  file_suffix: &str,
  line: u32,
) -> bool {
  let loc = ctx.find_location(addr).expect("addr2line find_location");
  let Some(loc) = loc else { return false };
  let Some(path) = loc.file else { return false };
  path.ends_with(file_suffix) && loc.line == Some(line)
}

#[test]
fn dwarf_compile_unit_language_is_c_plus_plus_fallback() {
  let mut host = es5_host();
  let main_key = FileKey::new("main.ts");
  host.insert(main_key.clone(), "export function main(): number { return 0; }\n");

  let program = Program::new(host, vec![main_key.clone()]);
  let entry = program.file_id(&main_key).expect("entry file id");

  let obj = compile_to_obj(&program, entry);
  let dwarf = load_dwarf(&obj);

  let mut units = dwarf.units();
  let mut seen = Vec::new();

  while let Some(header) = units.next().expect("iterate units") {
    let unit = dwarf.unit(header).expect("parse unit");
    let name = compile_unit_name(&dwarf, &unit);
    let lang = compile_unit_language(&unit);
    let lang = lang.expect("compile unit should have DW_AT_language");
    seen.push((name.clone(), Some(lang)));
    assert!(
      matches!(lang, gimli::DW_LANG_C | gimli::DW_LANG_C_plus_plus),
      "unexpected DW_AT_language for compile unit {name:?}: {lang:?}"
    );
  }

  assert!(
    !seen.is_empty(),
    "expected at least one DWARF compile unit; seen={seen:#?}"
  );
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
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");
  let entry = program.file_id(&main_key).expect("entry file id");

  let obj = compile_to_obj(&program, entry);
  let dwarf = load_dwarf(&obj);
  let addr_ctx = Context::from_dwarf(load_dwarf(&obj)).expect("addr2line context");

  let mut units = dwarf.units();
  let mut cu_names = Vec::new();
  let mut found_return_line_row = false;
  let mut return_addr = None;

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
        return_addr = Some(row.address());
        break;
      }
    }
    if found_return_line_row {
      break;
    }
  }

  assert!(
    found_return_line_row,
    "did not find any decoded DWARF line-table row mapping a machine address back to main.ts:4 (`return x + y;`). \
compile_units={cu_names:#?}"
  );

  let return_addr = return_addr.expect("missing address for main.ts:4 row");
  assert!(
    addr2line_location_matches(&addr_ctx, return_addr, "main.ts", 4),
    "addr2line could not map the decoded line-row address {return_addr:#x} back to main.ts:4 (`return x + y;`)"
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
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");
  let entry = program.file_id(&main_key).expect("entry file id");

  let obj = compile_to_obj(&program, entry);
  let dwarf = load_dwarf(&obj);
  let addr_ctx = Context::from_dwarf(load_dwarf(&obj)).expect("addr2line context");

  let mut units = dwarf.units();
  let mut found_main = false;
  let mut found_math = false;
  let mut main_return_addr = None;
  let mut math_return_addr = None;

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
        if main_return_addr.is_none() {
          if let Some(line) = row.line() {
            if line.get() == 4 {
              main_return_addr = Some(row.address());
            }
          }
        }
      }
      if file_name.ends_with("math.ts") {
        found_math = true;
        if math_return_addr.is_none() {
          if let Some(line) = row.line() {
            if line.get() == 2 {
              math_return_addr = Some(row.address());
            }
          }
        }
      }
      if found_main && found_math && main_return_addr.is_some() && math_return_addr.is_some() {
        break;
      }
    }
  }

  assert!(
    found_main && found_math,
    "expected decoded DWARF line-table rows to reference both `main.ts` and `math.ts`; found_main={found_main} found_math={found_math}"
  );

  let main_return_addr = main_return_addr.expect("missing address for main.ts:4 row");
  let math_return_addr = math_return_addr.expect("missing address for math.ts:2 row");
  assert!(
    addr2line_location_matches(&addr_ctx, main_return_addr, "main.ts", 4),
    "addr2line could not map the decoded line-row address {main_return_addr:#x} back to main.ts:4"
  );
  assert!(
    addr2line_location_matches(&addr_ctx, math_return_addr, "math.ts", 2),
    "addr2line could not map the decoded line-row address {math_return_addr:#x} back to math.ts:2"
  );
}
