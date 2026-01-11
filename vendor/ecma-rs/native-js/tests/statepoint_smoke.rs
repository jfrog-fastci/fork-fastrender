use inkwell::context::Context;
use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::gc;
use native_js::emit;
use object::{Object, ObjectSection, ObjectSymbol, SymbolKind};
use runtime_native::stackmap::StackMap;

#[test]
fn statepoint_smoke() {
  native_js::llvm::init_native_target().expect("failed to init native target");

  let context = Context::create();
  let module = context.create_module("statepoint_smoke");
  let builder = context.create_builder();

  let gc_ptr = gc::gc_ptr_type(&context);

  // declare void @callee()
  let callee_ty = context.void_type().fn_type(&[], false);
  let callee = module.add_function("callee", callee_ty, None);

  // define ptr addrspace(1) @ts_fn(ptr addrspace(1)) gc "<strategy>"
  let ts_ty = gc_ptr.fn_type(&[gc_ptr.into()], false);
  let ts_fn = module.add_function("ts_fn", ts_ty, None);
  gc::set_default_gc_strategy(&ts_fn).expect("GC strategy contains NUL byte");

  // Force frame pointers for runtime stack walking.
  let frame_pointer = context.create_string_attribute("frame-pointer", "all");
  ts_fn.add_attribute(inkwell::attributes::AttributeLoc::Function, frame_pointer);

  let entry = context.append_basic_block(ts_fn, "entry");
  builder.position_at_end(entry);

  // Ensure the GC pointer argument is live across the call, so statepoint rewriting
  // produces a `"gc-live"` entry and a `(base, derived)` location pair in stackmaps.
  builder.build_call(callee, &[], "call_callee").unwrap();
  let arg0 = ts_fn
    .get_first_param()
    .expect("missing arg0")
    .into_pointer_value();
  builder.build_return(Some(&arg0)).unwrap();

  let triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&triple).expect("host target");
  let tm = target
    .create_target_machine(
      &triple,
      "generic",
      "",
      OptimizationLevel::None,
      RelocMode::Static,
      CodeModel::Small,
    )
    .expect("create target machine");

  module.set_triple(&triple);
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  let obj = emit::emit_object_with_statepoints(
    &module,
    emit::TargetConfig {
      triple,
      cpu: "generic".to_string(),
      features: "".to_string(),
      opt_level: OptimizationLevel::None,
      reloc_mode: RelocMode::Static,
      code_model: CodeModel::Small,
    },
  )
  .expect("emit object with rewrite-statepoints-for-gc");

  let file = object::File::parse(obj.as_slice()).expect("parse object");
  let stackmaps_section = file
    .section_by_name(".llvm_stackmaps")
    .expect("object must contain .llvm_stackmaps");
  let stackmaps = stackmaps_section
    .data()
    .expect("read .llvm_stackmaps section");
  let stackmap = StackMap::parse(stackmaps).expect("parse stackmap v3");

  assert_eq!(stackmap.version, runtime_native::stackmaps::STACKMAP_VERSION);
  assert!(
    !stackmap.records.is_empty(),
    "expected at least one stackmap record"
  );

  let call_ret_off = call_return_offset(&file, "ts_fn").expect("locate call instruction");
  let record = stackmap
    .records
    .iter()
    .find(|r| r.instruction_offset as usize == call_ret_off)
    .unwrap_or_else(|| {
      let offs: Vec<usize> = stackmap
        .records
        .iter()
        .map(|r| r.instruction_offset as usize)
        .collect();
      panic!(
        "expected a stackmap record at instruction_offset={call_ret_off}, got offsets: {offs:?}"
      )
    });

  let has_indirect = record.locations.iter().any(|l| {
    matches!(l, runtime_native::stackmaps::Location::Indirect { .. })
  });
  assert!(has_indirect, "expected at least one Indirect stack location");

  let nonconst = record
    .locations
    .iter()
    .filter(|l| {
      !matches!(
        l,
        runtime_native::stackmaps::Location::Constant { .. }
          | runtime_native::stackmaps::Location::ConstIndex { .. }
      )
    })
    .count();
  assert!(nonconst > 0);
  assert_eq!(nonconst % 2, 0, "expected base/derived pairs in stackmap");
}

fn call_return_offset(obj: &object::File<'_>, fn_name: &str) -> Option<usize> {
  let sym = obj
    .symbols()
    .find(|s| s.kind() == SymbolKind::Text && s.name().ok() == Some(fn_name) && s.address() != 0);
  let sym = sym.or_else(|| {
    obj.symbols()
      .find(|s| s.kind() == SymbolKind::Text && s.name().ok() == Some(fn_name))
  })?;

  let section_index = sym.section_index()?;
  let section = obj.section_by_index(section_index).ok()?;
  let data = section.data().ok()?;
  let start = sym.address() as usize;
  let mut size = sym.size() as usize;
  if size == 0 {
    size = data.len().saturating_sub(start);
  }
  if start + size > data.len() {
    return None;
  }
  let func_bytes = &data[start..start + size];

  // Look for the last direct CALL rel32 encoding: E8 <imm32>.
  //
  // `place-safepoints` may insert an entry safepoint poll which becomes an extra
  // statepoint call before the user-authored call in this test. Using the last
  // CALL targets the original callee callsite.
  let mut last_call = None;
  for i in 0..func_bytes.len().saturating_sub(4) {
    if func_bytes[i] == 0xE8 {
      last_call = Some(i + 5);
    }
  }
  last_call
}
