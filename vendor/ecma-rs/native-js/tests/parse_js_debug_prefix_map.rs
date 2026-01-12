#![cfg(target_os = "linux")]

use gimli::read::{Dwarf, EndianSlice};
use gimli::{RunTimeEndian, SectionId};
use native_js::compiler::compile_llvm_ir_to_artifact;
use native_js::{compile_project_to_llvm_ir, CompileOptions, EmitKind, OptLevel};
use object::{Object, ObjectSection, ObjectSymbol};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use typecheck_ts::lib_support::CompilerOptions as TsCompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

type Reader<'a> = EndianSlice<'a, RunTimeEndian>;

fn write_u32(endian: RunTimeEndian, buf: &mut [u8], offset: usize, value: u32) {
  let bytes = match endian {
    RunTimeEndian::Little => value.to_le_bytes(),
    RunTimeEndian::Big => value.to_be_bytes(),
  };
  buf[offset..offset + 4].copy_from_slice(&bytes);
}

fn write_u64(endian: RunTimeEndian, buf: &mut [u8], offset: usize, value: u64) {
  let bytes = match endian {
    RunTimeEndian::Little => value.to_le_bytes(),
    RunTimeEndian::Big => value.to_be_bytes(),
  };
  buf[offset..offset + 8].copy_from_slice(&bytes);
}

fn apply_relocations(
  file: &object::File<'_>,
  section: &object::Section<'_, '_>,
  data: &mut [u8],
  endian: RunTimeEndian,
) {
  for (offset, relocation) in section.relocations() {
    let offset = usize::try_from(offset).expect("relocation offset fits in usize");
    let size = relocation.size();

    // `object` exposes relocations at a higher level than raw ELF types. We only need absolute
    // relocations to decode DWARF (section offsets and address table entries).
    if relocation.kind() != object::RelocationKind::Absolute {
      continue;
    }

    let target_address = match relocation.target() {
      object::RelocationTarget::Symbol(sym) => file
        .symbol_by_index(sym)
        .map(|sym| sym.address())
        .unwrap_or(0),
      object::RelocationTarget::Section(sec) => file
        .section_by_index(sec)
        .map(|sec| sec.address())
        .unwrap_or(0),
      _ => 0,
    };

    // The object crate models addends as i64; DWARF relocations should be non-negative.
    let value: u64 = u64::try_from(target_address as i128 + relocation.addend() as i128)
      .expect("DWARF relocation should be non-negative");

    match size {
      32 => {
        let value = u32::try_from(value).expect("32-bit relocation should fit in u32");
        write_u32(endian, data, offset, value);
      }
      64 => write_u64(endian, data, offset, value),
      0 => {
        // Some formats report 0 as "unknown". Our test only relies on the relocations above.
      }
      _ => panic!("unsupported DWARF relocation size {size}"),
    }
  }
}

fn load_dwarf(obj: &[u8]) -> Dwarf<Reader<'static>> {
  let file = object::File::parse(obj).expect("parse object file");
  let endian = if file.is_little_endian() {
    RunTimeEndian::Little
  } else {
    RunTimeEndian::Big
  };

  let load_section = |id: SectionId| -> Result<Reader<'static>, gimli::Error> {
    let Some(section) = file.section_by_name(id.name()) else {
      return Ok(EndianSlice::new(&[][..], endian));
    };

    let mut data = section.data().unwrap_or(&[][..]).to_vec();
    apply_relocations(&file, &section, &mut data, endian);
    // `gimli`'s `Dwarf` structure holds borrows into section data. For DWARF sections that need
    // relocations applied, we copy and then leak the relocated bytes so the returned `Dwarf` can
    // reference them for the rest of the test process.
    let data: &'static [u8] = Box::leak(data.into_boxed_slice());
    Ok(EndianSlice::new(data, endian))
  };

  Dwarf::load(&load_section).expect("load DWARF sections")
}

fn dwarf_attr_string(
  dwarf: &Dwarf<Reader<'static>>,
  unit: &gimli::read::Unit<Reader<'static>, usize>,
  value: gimli::read::AttributeValue<Reader<'static>>,
) -> Option<String> {
  let s = dwarf.attr_string(unit, value).ok()?;
  Some(s.to_string_lossy().to_string())
}

fn compile_unit_comp_dir(
  dwarf: &Dwarf<Reader<'static>>,
  unit: &gimli::read::Unit<Reader<'static>, usize>,
) -> Option<String> {
  let mut entries = unit.entries();
  let (_, entry) = entries.next_dfs().ok()??;
  let attr = entry.attr(gimli::DW_AT_comp_dir).ok()??;
  dwarf_attr_string(dwarf, unit, attr.value())
}

fn row_file_name(
  dwarf: &Dwarf<Reader<'static>>,
  unit: &gimli::read::Unit<Reader<'static>, usize>,
  header: &gimli::read::LineProgramHeader<Reader<'static>>,
  row: &gimli::read::LineRow,
) -> Option<String> {
  let comp_dir = compile_unit_comp_dir(dwarf, unit);
  let file = row.file(header)?;
  let file_name = dwarf_attr_string(dwarf, unit, file.path_name())?;
  if Path::new(&file_name).is_absolute() {
    return Some(file_name);
  }

  let dir = file
    .directory(header)
    .and_then(|dir| dwarf_attr_string(dwarf, unit, dir));
  if let Some(dir) = dir {
    if !dir.is_empty() && dir != "." {
      let joined = if Path::new(&dir).is_absolute() {
        PathBuf::from(dir).join(&file_name)
      } else if let Some(comp_dir) = comp_dir.as_deref().filter(|d| !d.is_empty() && *d != ".") {
        PathBuf::from(comp_dir).join(dir).join(&file_name)
      } else {
        PathBuf::from(dir).join(&file_name)
      };
      return Some(joined.to_string_lossy().to_string());
    }
  }

  if let Some(comp_dir) = comp_dir.as_deref().filter(|d| !d.is_empty() && *d != ".") {
    return Some(PathBuf::from(comp_dir).join(file_name).to_string_lossy().to_string());
  }

  Some(file_name)
}

#[test]
fn parse_js_project_debug_prefix_map_remaps_dwarf_paths() {
  let mut host = MemoryHost::with_options(TsCompilerOptions {
    no_default_lib: true,
    ..Default::default()
  });
  let entry_key = FileKey::new("/tmp/proj/main.ts");
  host.insert(
    entry_key.clone(),
    "export function main(): number {\n  return 1;\n}\n",
  );

  let program = Program::new(host, vec![entry_key.clone()]);
  let _ = program.check();
  let entry_id = program.file_id(&entry_key).expect("entry file id");

  let mut opts = CompileOptions::default();
  opts.emit = EmitKind::Object;
  opts.debug = true;
  opts.opt_level = OptLevel::O0;
  opts.debug_path_prefix_map = vec![(PathBuf::from("/tmp/proj"), PathBuf::from("/src"))];

  let ir = compile_project_to_llvm_ir(&program, &program, entry_id, opts.clone(), None)
    .expect("compile_project_to_llvm_ir");

  let dir = tempfile::tempdir().expect("create tempdir");
  let obj_path = dir.path().join("out.o");
  let out = compile_llvm_ir_to_artifact(&ir, opts, Some(obj_path.clone()))
    .expect("compile_llvm_ir_to_artifact");
  assert_eq!(out.path, obj_path);

  let bytes = std::fs::read(&out.path).expect("read object file");
  let dwarf = load_dwarf(&bytes);

  let mut units = dwarf.units();
  let mut found_row = false;
  let mut seen_files = BTreeSet::<String>::new();

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
      seen_files.insert(file_name.clone());
      if file_name == "/src/main.ts" {
        found_row = true;
        break;
      }
    }
  }

  assert!(
    found_row,
    "expected a DWARF line-table row referencing `/src/main.ts`; seen_files={seen_files:?}"
  );
}
