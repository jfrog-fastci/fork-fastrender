#![cfg(target_os = "linux")]

use gimli::read::{Dwarf, EndianSlice};
use gimli::{RunTimeEndian, SectionId};
use native_js::{compile_program, CompilerOptions, EmitKind, OptLevel};
use object::{Object, ObjectSection};
use std::borrow::Cow;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

type Reader<'a> = EndianSlice<'a, RunTimeEndian>;

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
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

fn load_dwarf(obj: &[u8]) -> Dwarf<Reader<'static>> {
  let file = object::File::parse(obj).expect("parse object file");
  let endian = if file.is_little_endian() {
    RunTimeEndian::Little
  } else {
    RunTimeEndian::Big
  };

  let load_section = |id: SectionId| -> Result<Reader<'static>, gimli::Error> {
    // Use `uncompressed_data()` so the DWARF decoder keeps working if a toolchain
    // starts emitting `.zdebug_*` sections.
    let data = file
      .section_by_name(id.name())
      .and_then(|section| section.uncompressed_data().ok())
      .unwrap_or(Cow::Borrowed(&[][..]));
    // `gimli`'s `EndianSlice` reader borrows the section bytes. `object` may
    // return owned decompressed buffers, so keep things simple (tests only) by
    // leaking the buffer for `'static` lifetime.
    //
    // This is bounded: native-js test objects are tiny, and the process exits
    // immediately after the test binary completes.
    let leaked: &'static [u8] = Box::leak(data.into_owned().into_boxed_slice());
    Ok(EndianSlice::new(leaked, endian))
  };

  Dwarf::load(&load_section).expect("load DWARF sections")
}

fn file_name_from_index(
  dwarf: &Dwarf<Reader<'_>>,
  unit: &gimli::read::Unit<Reader<'_>, usize>,
  line_program: &gimli::read::IncompleteLineProgram<Reader<'_>>,
  index: u64,
) -> Option<String> {
  // DWARF file indexes are 1-based. 0 means "no file".
  let index = usize::try_from(index).ok()?;
  let idx = index.checked_sub(1)?;
  let file = line_program.header().file_names().get(idx)?;
  let path = dwarf.attr_string(unit, file.path_name()).ok()?;
  Some(path.to_string_lossy().into_owned())
}

fn subprogram_pc_range(
  dwarf: &Dwarf<Reader<'_>>,
  unit: &gimli::read::Unit<Reader<'_>, usize>,
  entry: &gimli::read::DebuggingInformationEntry<Reader<'_>>,
) -> Option<(u64, u64)> {
  let low_attr = entry.attr(gimli::DW_AT_low_pc).ok()??;
  let high_attr = entry.attr(gimli::DW_AT_high_pc).ok()??;

  let low_pc = match low_attr.value() {
    gimli::read::AttributeValue::Addr(addr) => addr,
    gimli::read::AttributeValue::DebugAddrIndex(i) => dwarf.address(unit, i).ok()?,
    _ => return None,
  };

  let high_pc = match high_attr.value() {
    gimli::read::AttributeValue::Addr(addr) => addr,
    gimli::read::AttributeValue::Udata(size) => low_pc.checked_add(size)?,
    gimli::read::AttributeValue::DebugAddrIndex(i) => dwarf.address(unit, i).ok()?,
    _ => return None,
  };

  Some((low_pc, high_pc))
}

#[test]
fn dwarf_debug_info_contains_main_subprogram_with_decl_location_and_pc_range() {
  let mut host = es5_host();
  let main_key = FileKey::new("main.ts");

  // Keep line numbers stable and ensure the TS `main` function doesn't start on line 1, so we can
  // distinguish it from the synthetic "<module init>" subprogram that also uses the start-of-file
  // span for its debug metadata.
  let src = r#"// line 1 padding
export function main(): number {
  let x = 1;
  return x + 2;
}"#;
  host.insert(main_key.clone(), src);

  let program = Program::new(host, vec![main_key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");
  let entry = program.file_id(&main_key).expect("entry file id");

  let obj = compile_to_obj(&program, entry);
  let dwarf = load_dwarf(&obj);

  let mut units = dwarf.units();
  let mut seen_units = 0usize;
  let mut seen_subprograms: Vec<(Option<String>, Option<String>)> = Vec::new();
  let mut found = false;

  while let Some(header) = units.next().expect("iterate units") {
    seen_units += 1;
    let unit = dwarf.unit(header).expect("parse unit");

    let mut entries = unit.entries();
    while let Some((_delta, die)) = entries.next_dfs().expect("next_dfs") {
      if die.tag() != gimli::DW_TAG_subprogram {
        continue;
      }

      let name = die
        .attr(gimli::DW_AT_name)
        .ok()
        .flatten()
        .and_then(|attr| dwarf.attr_string(&unit, attr.value()).ok())
        .map(|s| s.to_string_lossy().into_owned());

      // `linkage_name` is the stable/unique symbol that survives LLVM inlining/merging; useful to
      // locate the TS `main` even if the producer decides to use the mangled name as DW_AT_name.
      let linkage = die
        .attr(gimli::DW_AT_linkage_name)
        .ok()
        .flatten()
        .and_then(|attr| dwarf.attr_string(&unit, attr.value()).ok())
        .map(|s| s.to_string_lossy().into_owned());

      seen_subprograms.push((name.clone(), linkage.clone()));

      let Some(line_program) = unit.line_program.clone() else {
        panic!("expected unit containing {name:?} to have a line program");
      };

      let decl_line = die
        .attr(gimli::DW_AT_decl_line)
        .expect("DW_AT_decl_line attr")
        .and_then(|a| a.udata_value())
        .unwrap_or(0);
      if decl_line != 2 {
        continue;
      }

      let decl_file = die
        .attr(gimli::DW_AT_decl_file)
        .expect("DW_AT_decl_file attr")
        .and_then(|a| a.udata_value())
        .unwrap_or(0);
      if decl_file == 0 {
        continue;
      }

      let file_name = file_name_from_index(&dwarf, &unit, &line_program, decl_file)
        .expect("resolve DW_AT_decl_file to file name");
      if !file_name.ends_with("main.ts") {
        continue;
      }

      let Some((low_pc, high_pc)) = subprogram_pc_range(&dwarf, &unit, die) else {
        panic!("expected subprogram {name:?} to have a low_pc/high_pc range");
      };
      assert!(
        high_pc > low_pc,
        "expected subprogram {name:?} to have non-empty PC range; low_pc={low_pc:#x} high_pc={high_pc:#x}"
      );

      found = true;
      break;
    }
  }

  assert!(seen_units > 0, "expected at least one DWARF unit");
  assert!(
    found,
    "did not find a DW_TAG_subprogram for TS `main` in decoded DWARF debug_info; seen_subprograms={seen_subprograms:#?}"
  );
}
