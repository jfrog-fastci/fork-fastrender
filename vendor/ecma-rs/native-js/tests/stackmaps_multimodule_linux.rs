#![cfg(target_os = "linux")]

use inkwell::attributes::AttributeLoc;
use inkwell::context::Context;
use inkwell::targets::{CodeModel, FileType, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::gc;
use native_js::llvm::passes;
use object::{Object, ObjectSection};
use std::fs;
use std::path::Path;

fn init_llvm() {
  native_js::llvm::init_native_target().expect("failed to initialize native LLVM target");
}

fn host_target_machine() -> TargetMachine {
  init_llvm();

  let triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&triple).expect("host target");
  let cpu = TargetMachine::get_host_cpu_name().to_string();
  let features = TargetMachine::get_host_cpu_features().to_string();

  target
    .create_target_machine(
      &triple,
      &cpu,
      &features,
      OptimizationLevel::None,
      RelocMode::Default,
      CodeModel::Default,
    )
    .expect("create target machine")
}

fn define_void_function<'ctx>(
  ctx: &'ctx Context,
  module: &inkwell::module::Module<'ctx>,
  name: &str,
) {
  let builder = ctx.create_builder();
  let void_ty = ctx.void_type();
  let ty = void_ty.fn_type(&[], false);
  let f = module.add_function(name, ty, None);
  let entry = ctx.append_basic_block(f, "entry");
  builder.position_at_end(entry);
  builder.build_return(None).unwrap();
}

fn define_statepoint_function<'ctx>(
  ctx: &'ctx Context,
  module: &inkwell::module::Module<'ctx>,
  name: &str,
  callee_name: &str,
) -> inkwell::values::FunctionValue<'ctx> {
  let builder = ctx.create_builder();
  let gc_ptr = gc::gc_ptr_type(ctx);

  let fn_ty = gc_ptr.fn_type(&[gc_ptr.into()], false);
  let f = module.add_function(name, fn_ty, None);
  // Use the same production GC strategy as `native-js` codegen.
  f.set_gc(gc::GC_STRATEGY);
  // Ensure LTO does not inline away the per-module functions; we want one StackMaps entry per
  // compilation unit in the merged table.
  let noinline = ctx.create_enum_attribute(
    inkwell::attributes::Attribute::get_named_enum_kind_id("noinline"),
    0,
  );
  f.add_attribute(AttributeLoc::Function, noinline);

  define_void_function(ctx, module, callee_name);
  let callee = module.get_function(callee_name).expect("callee exists");

  let entry = ctx.append_basic_block(f, "entry");
  builder.position_at_end(entry);
  builder.build_call(callee, &[], "call").unwrap();
  let arg0 = f
    .get_first_param()
    .expect("missing arg0")
    .into_pointer_value();
  builder.build_return(Some(&arg0)).unwrap();

  f
}

fn define_main_calls<'ctx>(
  ctx: &'ctx Context,
  module: &inkwell::module::Module<'ctx>,
  callee_a: inkwell::values::FunctionValue<'ctx>,
  callee_b: inkwell::values::FunctionValue<'ctx>,
) {
  let builder = ctx.create_builder();
  let i32_ty = ctx.i32_type();
  let main_ty = i32_ty.fn_type(&[], false);
  let main = module.add_function("main", main_ty, None);

  let entry = ctx.append_basic_block(main, "entry");
  builder.position_at_end(entry);

  let gc_ptr = gc::gc_ptr_type(ctx);
  let null = gc_ptr.const_null();
  builder
    .build_call(callee_a, &[null.into()], "call_a")
    .unwrap();
  builder
    .build_call(callee_b, &[null.into()], "call_b")
    .unwrap();

  builder
    .build_return(Some(&i32_ty.const_int(0, false)))
    .unwrap();
}

fn build_two_modules<'ctx>(
  ctx: &'ctx Context,
  tm: &TargetMachine,
) -> (inkwell::module::Module<'ctx>, inkwell::module::Module<'ctx>) {
  let gc_ptr = gc::gc_ptr_type(ctx);
  let gc_func_ty = gc_ptr.fn_type(&[gc_ptr.into()], false);

  let module_b = ctx.create_module("m_b");
  module_b.set_triple(&tm.get_triple());
  module_b.set_data_layout(&tm.get_target_data().get_data_layout());
  let _gc_b = define_statepoint_function(ctx, &module_b, "gc_func_b", "callee_b");

  let module_a = ctx.create_module("m_a");
  module_a.set_triple(&tm.get_triple());
  module_a.set_data_layout(&tm.get_target_data().get_data_layout());
  let gc_a = define_statepoint_function(ctx, &module_a, "gc_func_a", "callee_a");

  // Declare `gc_func_b` so `main` can call it and keep module B alive under gc-sections/LTO.
  let gc_b_decl = module_a.add_function("gc_func_b", gc_func_ty, None);
  define_main_calls(ctx, &module_a, gc_a, gc_b_decl);

  (module_a, module_b)
}

fn rewrite_statepoints(module: &inkwell::module::Module<'_>, tm: &TargetMachine) {
  passes::rewrite_statepoints_for_gc(module, tm).expect("rewrite-statepoints-for-gc failed");
  if let Err(err) = module.verify() {
    panic!(
      "LLVM module verification failed after rewrite-statepoints-for-gc: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }
}

fn emit_object(tm: &TargetMachine, module: &inkwell::module::Module<'_>, path: &Path) {
  tm.write_to_file(module, FileType::Object, path)
    .expect("failed to emit object file");
}

fn emit_bitcode(module: &inkwell::module::Module<'_>, path: &Path) {
  assert!(
    module.write_bitcode_to_path(path),
    "failed to emit bitcode to {}",
    path.display()
  );
}

fn llvm_stackmaps_section(elf: &Path) -> Vec<u8> {
  let data = fs::read(elf).unwrap();
  let file = object::File::parse(&*data).unwrap();
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section (was it GC'd?)");
  section
    .data()
    .unwrap_or_else(|err| panic!("failed to read .llvm_stackmaps contents: {err}"))
    .to_vec()
}

#[derive(Debug, Clone, Copy)]
struct StackMapHeader {
  version: u8,
  num_functions: u32,
  num_constants: u32,
  num_records: u32,
}

#[derive(Debug)]
struct StackMapBlob {
  offset: usize,
  len: usize,
  header: StackMapHeader,
}

struct Reader<'a> {
  bytes: &'a [u8],
  pos: usize,
}

impl<'a> Reader<'a> {
  fn new(bytes: &'a [u8]) -> Self {
    Self { bytes, pos: 0 }
  }

  fn read_exact<const N: usize>(&mut self) -> [u8; N] {
    let end = self.pos + N;
    let slice = self
      .bytes
      .get(self.pos..end)
      .unwrap_or_else(|| panic!("unexpected EOF reading {N} bytes at offset {}", self.pos));
    let mut out = [0u8; N];
    out.copy_from_slice(slice);
    self.pos = end;
    out
  }

  fn read_u8(&mut self) -> u8 {
    self.read_exact::<1>()[0]
  }

  fn read_u16(&mut self) -> u16 {
    u16::from_le_bytes(self.read_exact::<2>())
  }

  fn read_u32(&mut self) -> u32 {
    u32::from_le_bytes(self.read_exact::<4>())
  }

  fn read_u64(&mut self) -> u64 {
    u64::from_le_bytes(self.read_exact::<8>())
  }

  fn pad_to_align(&mut self, align: usize) {
    while self.pos % align != 0 {
      let b = self.read_u8();
      assert_eq!(
        b,
        0,
        "expected zero padding byte at offset {} (align={align})",
        self.pos - 1
      );
    }
  }
}

fn parse_stackmap_blob(bytes: &[u8]) -> (StackMapHeader, usize) {
  let mut r = Reader::new(bytes);

  let version = r.read_u8();
  let _reserved0 = r.read_u8();
  let _reserved1 = r.read_u16();
  let num_functions = r.read_u32();
  let num_constants = r.read_u32();
  let num_records = r.read_u32();

  assert_eq!(
    version, 3,
    "unexpected stackmap version {version} (expected 3)"
  );

  let mut record_count_sum: u64 = 0;
  for _ in 0..num_functions {
    let _function_address = r.read_u64();
    let _stack_size = r.read_u64();
    let record_count = r.read_u64();
    record_count_sum = record_count_sum
      .checked_add(record_count)
      .expect("record_count sum overflow");
  }

  for _ in 0..num_constants {
    let _ = r.read_u64();
  }

  assert_eq!(
    record_count_sum, num_records as u64,
    "stackmap function RecordCount sum ({record_count_sum}) != header NumRecords ({num_records})"
  );

  for _ in 0..num_records {
    let _patchpoint_id = r.read_u64();
    let _instruction_offset = r.read_u32();
    let _reserved = r.read_u16();
    let num_locations = r.read_u16() as usize;

    // Each Location record is 12 bytes:
    // Kind(u8), Reserved(u8), Size(u16), DwarfRegNum(u16), Reserved(u16), OffsetOrSmallConstant(i32).
    for _ in 0..num_locations {
      let _kind = r.read_u8();
      let _reserved0 = r.read_u8();
      let _size = r.read_u16();
      let _dwarf_reg = r.read_u16();
      let _reserved1 = r.read_u16();
      let _offset_or_const = r.read_u32();
    }

    // StackMap v3 aligns the live-outs header/array to an 8-byte boundary.
    r.pad_to_align(8);
    let num_live_outs = r.read_u16() as usize;
    let reserved = r.read_u16();
    assert_eq!(reserved, 0, "expected live-outs reserved field to be 0");
    for _ in 0..num_live_outs {
      let _dwarf_reg = r.read_u16();
      let _size = r.read_u8();
      let _reserved = r.read_u8();
    }
    r.pad_to_align(8);
  }

  (
    StackMapHeader {
      version,
      num_functions,
      num_constants,
      num_records,
    },
    r.pos,
  )
}

fn parse_stackmap_blobs(bytes: &[u8]) -> Vec<StackMapBlob> {
  let mut blobs = Vec::new();
  let mut off = 0usize;

  while off < bytes.len() {
    // Skip linker/section padding (zero-filled).
    while off < bytes.len() && bytes[off] == 0 {
      off += 1;
    }
    if off >= bytes.len() {
      break;
    }

    let (header, len) = parse_stackmap_blob(&bytes[off..]);
    assert!(len > 0, "parsed stackmap blob length is 0");
    blobs.push(StackMapBlob {
      offset: off,
      len,
      header,
    });
    off += len;
  }

  blobs
}

#[test]
fn object_link_concatenates_multiple_stackmap_blobs() {
  let tm = host_target_machine();
  let ctx = Context::create();
  let (module_a, module_b) = build_two_modules(&ctx, &tm);

  rewrite_statepoints(&module_a, &tm);
  rewrite_statepoints(&module_b, &tm);

  let td = tempfile::tempdir().unwrap();
  let obj_a = td.path().join("a.o");
  let obj_b = td.path().join("b.o");
  emit_object(&tm, &module_a, &obj_a);
  emit_object(&tm, &module_b, &obj_b);

  let elf = td.path().join("a.out");
  native_js::link::link_elf_executable(&elf, &[obj_a, obj_b]).unwrap();
  let stackmaps = llvm_stackmaps_section(&elf);
  assert!(!stackmaps.is_empty(), "expected non-empty .llvm_stackmaps");

  let blobs = parse_stackmap_blobs(&stackmaps);
  assert_eq!(
    blobs.len(),
    2,
    "expected two concatenated StackMap blobs when linking separate objects; got {}\nblobs={blobs:#?}",
    blobs.len()
  );

  assert_eq!(
    blobs[0].offset, 0,
    "expected first stackmap blob at offset 0; blobs={blobs:#?}"
  );
  assert!(
    blobs[0].offset + blobs[0].len <= blobs[1].offset,
    "expected blob[0] to end before blob[1] starts; blobs={blobs:#?}"
  );

  // Any bytes between blobs should be zero-filled padding (section alignment).
  for (i, b) in stackmaps[blobs[0].offset + blobs[0].len..blobs[1].offset]
    .iter()
    .enumerate()
  {
    assert_eq!(
      *b,
      0,
      "expected padding byte 0 at {}",
      blobs[0].offset + blobs[0].len + i
    );
  }

  for (idx, blob) in blobs.iter().enumerate() {
    let header_u32 = u32::from_le_bytes(
      stackmaps[blob.offset..blob.offset + 4]
        .try_into()
        .expect("stackmap header u32"),
    );
    assert_eq!(
      header_u32, 3,
      "expected stackmap header u32==3 at blob[{idx}] offset {}; blobs={blobs:#?}",
      blob.offset
    );

    assert_eq!(blob.header.version, 3, "blob[{idx}] has unexpected version");
    assert!(
      blob.header.num_functions >= 1,
      "blob[{idx}] should have at least one function"
    );
    assert!(
      blob.header.num_records >= 1,
      "blob[{idx}] should have at least one record"
    );
    let _ = blob.header.num_constants;
  }
}

#[test]
fn lto_link_merges_stackmap_blobs_into_one_table() {
  let tm = host_target_machine();
  let ctx = Context::create();
  let (module_a, module_b) = build_two_modules(&ctx, &tm);

  rewrite_statepoints(&module_a, &tm);
  rewrite_statepoints(&module_b, &tm);

  let td = tempfile::tempdir().unwrap();
  let bc_a = td.path().join("a.bc");
  let bc_b = td.path().join("b.bc");
  emit_bitcode(&module_a, &bc_a);
  emit_bitcode(&module_b, &bc_b);

  let elf = td.path().join("a.out");
  native_js::link::link_elf_executable_lto(&elf, &[bc_a, bc_b]).unwrap();
  let stackmaps = llvm_stackmaps_section(&elf);
  assert!(!stackmaps.is_empty(), "expected non-empty .llvm_stackmaps");

  let blobs = parse_stackmap_blobs(&stackmaps);
  assert_eq!(
    blobs.len(),
    1,
    "expected a single merged StackMap blob under LTO; got {}\nblobs={blobs:#?}",
    blobs.len()
  );

  let blob = &blobs[0];
  assert_eq!(
    blob.offset, 0,
    "expected merged stackmap blob to start at offset 0"
  );

  let header_u32 = u32::from_le_bytes(
    stackmaps[blob.offset..blob.offset + 4]
      .try_into()
      .expect("stackmap header u32"),
  );
  assert_eq!(header_u32, 3, "expected stackmap header u32==3");

  let hdr = blob.header;
  assert!(
    hdr.num_functions >= 2,
    "expected merged stackmaps to have NumFunctions >= 2; header={hdr:?}"
  );
  assert!(
    hdr.num_records >= 2,
    "expected merged stackmaps to have >=2 records (one per module); header={hdr:?}"
  );
  let _ = hdr.num_constants;

  // Trailing bytes after the blob should be zero-filled padding.
  for (i, b) in stackmaps[blob.offset + blob.len..].iter().enumerate() {
    assert_eq!(
      *b,
      0,
      "expected trailing padding byte 0 at {}",
      blob.offset + blob.len + i
    );
  }
}
