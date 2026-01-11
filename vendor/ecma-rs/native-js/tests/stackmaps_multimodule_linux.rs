#![cfg(target_os = "linux")]

use inkwell::attributes::AttributeLoc;
use inkwell::context::Context;
use inkwell::targets::{CodeModel, FileType, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::gc;
use native_js::llvm::passes;
use object::{Object, ObjectSection};
use runtime_native::stackmaps::{parse_all_stackmaps, StackMaps};
use runtime_native::statepoint_verify::{
  verify_statepoint_stackmap, DwarfArch, VerifyMode, VerifyStatepointOptions,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

fn define_void_function<'ctx>(ctx: &'ctx Context, module: &inkwell::module::Module<'ctx>, name: &str) {
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
    .section_by_name(".data.rel.ro.llvm_stackmaps")
    .or_else(|| file.section_by_name(".llvm_stackmaps"))
    .expect("missing stackmaps section (was it GC'd?)");
  section
    .data()
    .unwrap_or_else(|err| panic!("failed to read stackmaps section contents: {err}"))
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
    record_count_sum,
    num_records as u64,
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
  const STACKMAP_HEADER_SIZE: usize = 16;

  while off < bytes.len() {
    // Skip linker/section padding (zero-filled).
    while off < bytes.len() && bytes[off] == 0 {
      off += 1;
    }
    if off >= bytes.len() || bytes.len() - off < STACKMAP_HEADER_SIZE {
      break;
    }

    // StackMap v3 header prefix:
    //   u8  Version (3)
    //   u8  Reserved0 (0)
    //   u16 Reserved1 (0)
    let looks_like_header = bytes[off] == 3 && bytes[off + 1] == 0 && bytes[off + 2] == 0 && bytes[off + 3] == 0;
    if !looks_like_header {
      // Some toolchains have been observed to leave short non-zero bytes between concatenated
      // `.llvm_stackmaps` input sections. Recover by scanning forward for the next plausible v3
      // header (with a hard cap so we don't accidentally resync into the middle of a blob).
      const MAX_PADDING_SCAN: usize = 256;
      let scan_end = (off + MAX_PADDING_SCAN).min(bytes.len().saturating_sub(STACKMAP_HEADER_SIZE));
      let mut found: Option<usize> = None;
      for i in off + 1..=scan_end {
        // StackMap v3 payloads are 8-byte aligned in `.llvm_stackmaps`.
        if i % 8 != 0 {
          continue;
        }
        if bytes[i] == 3 && bytes[i + 1] == 0 && bytes[i + 2] == 0 && bytes[i + 3] == 0 {
          found = Some(i);
          break;
        }
      }
      if let Some(i) = found {
        off = i;
        continue;
      }

      // Short trailing bytes (< header size) cannot contain another blob; ignore them.
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
  assert!(!stackmaps.is_empty(), "expected non-empty stackmaps section");

  let blobs = parse_stackmap_blobs(&stackmaps);
  assert_eq!(
    blobs.len(),
    2,
    "expected two concatenated StackMap blobs when linking separate objects; got {}\nblobs={blobs:#?}",
    blobs.len()
  );
  let parsed =
    parse_all_stackmaps(&stackmaps).expect("runtime parser should parse concatenated blobs");
  assert_eq!(
    parsed.len(),
    blobs.len(),
    "runtime parser should agree with blob scanner about blob count"
  );

  assert_eq!(
    blobs[0].offset, 0,
    "expected first stackmap blob at offset 0; blobs={blobs:#?}"
  );
  assert!(
    blobs[0].offset + blobs[0].len <= blobs[1].offset,
    "expected blob[0] to end before blob[1] starts; blobs={blobs:#?}"
  );

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
  assert!(!stackmaps.is_empty(), "expected non-empty stackmaps section");

  // Ensure LTO codegen does not emit register-held statepoint roots.
  #[cfg(target_arch = "x86_64")]
  {
    let stackmaps_parsed = StackMaps::parse(&stackmaps).expect("parse .llvm_stackmaps");
    for raw in stackmaps_parsed.raws() {
      verify_statepoint_stackmap(
        raw,
        VerifyStatepointOptions {
          arch: DwarfArch::X86_64,
          mode: VerifyMode::StatepointsOnly,
        },
      )
      .expect("statepoint stackmap verification failed");
    }
  }

  let blobs = parse_stackmap_blobs(&stackmaps);
  assert_eq!(
    blobs.len(),
    1,
    "expected a single merged StackMap blob under LTO; got {}\nblobs={blobs:#?}",
    blobs.len()
  );
  let parsed = parse_all_stackmaps(&stackmaps).expect("runtime parser should parse stackmaps");
  assert_eq!(
    parsed.len(),
    blobs.len(),
    "runtime parser should agree with blob scanner about blob count"
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

  // Trailing bytes after the blob are typically zero padding, but some toolchains have been
  // observed to leave short non-zero alignment noise (< StackMap header size). Ignore such a tail.
  let tail = &stackmaps[blob.offset + blob.len..];
  if tail.len() >= 16 {
    assert!(
      tail.iter().all(|&b| b == 0),
      "unexpected non-zero tail bytes after stackmap blob (len={}); tail={:02x?}",
      tail.len(),
      tail
    );
  }
}

// -----------------------------------------------------------------------------
// Clang-produced `.llvm_stackmaps` section concatenation regression
// -----------------------------------------------------------------------------

fn clang() -> &'static str {
  for cand in ["clang-18", "clang"] {
    if Command::new(cand)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok()
    {
      return cand;
    }
  }
  panic!("unable to locate clang (expected `clang-18` or `clang`)");
}

fn write_file(path: &Path, contents: &str) {
  fs::write(path, contents).unwrap();
}

fn run(cmd: &mut Command) {
  let out = cmd.output().unwrap();
  if out.status.success() {
    return;
  }
  panic!(
    "command failed: {cmd:?}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
    out.status,
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr),
  );
}

fn compile_ll_to_obj(out_dir: &Path, name: &str, ll_src: &str) -> PathBuf {
  let ll_path = out_dir.join(format!("{name}.ll"));
  let obj_path = out_dir.join(format!("{name}.o"));
  write_file(&ll_path, ll_src);

  let mut cmd = Command::new(clang());
  cmd.args(["-c", "-o"]).arg(&obj_path).arg(&ll_path);
  run(&mut cmd);
  obj_path
}

fn llvm_stackmaps_obj_section_size(obj: &Path) -> usize {
  let bytes = fs::read(obj).unwrap();
  let file = object::File::parse(&*bytes).unwrap();
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  usize::try_from(section.size()).expect(".llvm_stackmaps section size overflows usize")
}

fn align_up(v: usize, align: usize) -> usize {
  assert!(align.is_power_of_two());
  (v + (align - 1)) & !(align - 1)
}

#[test]
fn object_link_concatenates_multiple_stackmap_blobs_from_clang_ir() {
  // Link two independent object files containing real StackMap v3 tables produced by LLVM, and
  // assert the final output contains multiple concatenated blobs (with possible alignment padding).
  let module_a = r#"
declare void @llvm.experimental.stackmap(i64, i32, ...)

define void @foo_a() {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 0, i32 0)
  ret void
}

declare void @foo_b()

define i32 @main() {
entry:
  call void @foo_a()
  call void @foo_b()
  ret i32 0
}
"#;

  let module_b = r#"
declare void @llvm.experimental.stackmap(i64, i32, ...)

define void @foo_b() {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 1, i32 0)
  ret void
}
"#;

  let td = tempfile::tempdir().unwrap();
  let obj_a = compile_ll_to_obj(td.path(), "a", module_a);
  let obj_b = compile_ll_to_obj(td.path(), "b", module_b);

  let size_a = llvm_stackmaps_obj_section_size(&obj_a);
  let size_b = llvm_stackmaps_obj_section_size(&obj_b);
  assert!(
    size_a > 0,
    "expected obj A to contain a non-empty .llvm_stackmaps section"
  );
  assert!(
    size_b > 0,
    "expected obj B to contain a non-empty .llvm_stackmaps section"
  );

  let elf = td.path().join("a.out");
  native_js::link::link_elf_executable(&elf, &[obj_a, obj_b]).unwrap();
  let stackmaps = llvm_stackmaps_section(&elf);
  assert!(!stackmaps.is_empty(), "expected non-empty stackmaps section");

  // StackMap v3 starts with:
  // - Version: u8 (3)
  // - Reserved0: u8 (0)
  // - Reserved1: u16 (0)
  assert_eq!(
    stackmaps.get(0..4),
    Some(&[3u8, 0, 0, 0][..]),
    "expected StackMap v3 header at start of output section"
  );

  let expected_start = align_up(size_a, 8);
  let expected_min = size_a;
  let expected_max = size_a + 7;
  assert!(
    stackmaps.len() >= size_a + size_b,
    "output stackmaps section too small for concatenation: size_a={size_a} size_b={size_b} out={}",
    stackmaps.len()
  );

  let mut found_second = None;
  for off in expected_min..=expected_max {
    if stackmaps.get(off..off + 4) == Some(&[3u8, 0, 0, 0][..]) {
      found_second = Some(off);
      break;
    }
  }

  let Some(second_off) = found_second else {
    panic!(
      "failed to find a second StackMap v3 header in output stackmaps section in the expected range \
 [size_a, size_a+7] = [{expected_min}, {expected_max}] (size_a={size_a}, expected_start={expected_start}, out_len={})",
      stackmaps.len()
    );
  };
  let _ = second_off;

  // Also ensure the runtime parser can decode the concatenated section.
  let parsed = parse_all_stackmaps(&stackmaps).expect("runtime parser should parse stackmaps");
  assert!(
    parsed.len() >= 2,
    "expected runtime parser to find at least 2 concatenated stackmap blobs; got {}",
    parsed.len()
  );
}
