use inkwell::attributes::AttributeLoc;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::FunctionType;
use inkwell::values::AsValueRef;
use inkwell::values::CallSiteValue;
use inkwell::values::FunctionValue;

use crate::llvm::gc::GC_STRATEGY;

fn is_runtime_abi_wrapper(func: FunctionValue<'_>) -> bool {
  let name = func.get_name().to_string_lossy();
  name.starts_with("rt_") && name.ends_with("_gc")
}

pub(crate) fn apply_stack_walking_frame_attrs(context: &Context, func: FunctionValue<'_>) {
  // Required for deterministic GC stack walking:
  //
  // - `frame-pointer="all"`: force a stable frame chain we can walk.
  // - `disable-tail-calls="true"`: prevent tail-call elimination from collapsing frames.
  let frame_pointer = context.create_string_attribute("frame-pointer", "all");
  let disable_tail_calls = context.create_string_attribute("disable-tail-calls", "true");

  func.add_attribute(AttributeLoc::Function, frame_pointer);
  func.add_attribute(AttributeLoc::Function, disable_tail_calls);
}

/// Mark a call instruction as `notail`.
///
/// Even with the function attribute `"disable-tail-calls"="true"`, LLVM may still
/// consider calls eligible for tail-call formation in some pipelines.
/// Explicitly marking calls `notail` makes the intent unambiguous and provides an
/// extra layer of defense for stackmap-based stack walking.
pub(crate) fn mark_call_notail(call: CallSiteValue<'_>) {
  unsafe {
    // LLVM IR keyword: `notail call ...`
    //
    // Using the C API here because inkwell's high-level wrapper doesn't expose
    // the full tail-call-kind enum (`tail`/`musttail`/`notail`).
    llvm_sys::core::LLVMSetTailCallKind(
      call.as_value_ref(),
      llvm_sys::LLVMTailCallKind::LLVMTailCallKindNoTail,
    );
  }
}

pub(crate) fn apply_stack_walking_attrs(context: &Context, func: FunctionValue<'_>) {
  func.set_gc(GC_STRATEGY);
  apply_stack_walking_frame_attrs(context, func);
}

/// LLVM code generator.
///
/// This is currently a minimal façade around `inkwell` that guarantees stack-walkability
/// invariants needed by precise GC (see `native-js/docs/gc_stack_walking.md`).
///
/// It also marks generated (GC-managed) functions with the LLVM GC strategy used for statepoint
/// lowering (see `native-js/docs/llvm_gc_strategy.md`).
pub struct CodeGen<'ctx> {
  context: &'ctx Context,
  module: Module<'ctx>,
  builder: Builder<'ctx>,
}

impl<'ctx> CodeGen<'ctx> {
  pub fn new(context: &'ctx Context, module_name: &str) -> Self {
    Self {
      context,
      module: context.create_module(module_name),
      builder: context.create_builder(),
    }
  }

  pub fn runtime_abi(&self) -> crate::runtime_abi::RuntimeAbi<'ctx, '_> {
    crate::runtime_abi::RuntimeAbi::new(self.context, &self.module)
  }

  pub fn module_ir(&self) -> String {
    // Be defensive: future code may add functions without going through `define_function`.
    // Enforce the stack-walking invariant across the whole module before emitting IR.
    self.enforce_stack_walking_invariants();
    self.module.print_to_string().to_string()
  }

  pub fn define_function(&self, name: &str, ty: FunctionType<'ctx>) -> FunctionValue<'ctx> {
    let func = self.module.add_function(name, ty, None);
    apply_stack_walking_attrs(self.context, func);
    func
  }

  pub fn define_trivial_function(&self, name: &str) -> FunctionValue<'ctx> {
    let i32_ty = self.context.i32_type();
    let fn_ty = i32_ty.fn_type(&[], false);
    let func = self.define_function(name, fn_ty);

    let entry = self.context.append_basic_block(func, "entry");
    self.builder.position_at_end(entry);
    let _ = self
      .builder
      .build_return(Some(&i32_ty.const_int(0, false)));

    func
  }

  fn enforce_stack_walking_invariants(&self) {
    let mut func = self.module.get_first_function();
    while let Some(f) = func {
      // Only apply these attributes to functions with bodies. External declarations (including
      // runtime ABI entrypoints like `rt_alloc` and LLVM intrinsics) should not be tagged as
      // GC-managed codegen functions.
      if f.get_first_basic_block().is_some() {
        if is_runtime_abi_wrapper(f) {
          apply_stack_walking_frame_attrs(self.context, f);
        } else {
          apply_stack_walking_attrs(self.context, f);
        }
      }
      func = f.get_next_function();
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn find_line<'a>(ir: &'a str, needle: &str) -> &'a str {
    ir.lines()
      .find(|l| l.contains(needle))
      .unwrap_or_else(|| panic!("missing `{needle}` in IR:\n{ir}"))
  }

  fn attr_group_on_line(line: &str) -> Option<u32> {
    let idx = line.find('#')?;
    let digits: String = line[idx + 1..]
      .chars()
      .take_while(|c| c.is_ascii_digit())
      .collect();
    digits.parse().ok()
  }

  fn assert_decl_not_gc_managed(ir: &str, needle: &str) {
    let line = find_line(ir, needle);
    assert!(
      !line.contains("gc \"coreclr\""),
      "expected `{needle}` to be a plain declaration (no gc strategy), got:\n{line}\n\nIR:\n{ir}"
    );

    if let Some(group) = attr_group_on_line(line) {
      let attr_line = find_line(ir, &format!("attributes #{group} ="));
      assert!(
        !attr_line.contains("\"frame-pointer\"=\"all\""),
        "did not expect stack-walking attrs on `{needle}` declaration; attr group was:\n{attr_line}\n\nIR:\n{ir}"
      );
      assert!(
        !(attr_line.contains("\"disable-tail-calls\"=\"true\"") || attr_line.contains("disable-tail-calls")),
        "did not expect stack-walking attrs on `{needle}` declaration; attr group was:\n{attr_line}\n\nIR:\n{ir}"
      );
    }
  }

  #[test]
  fn module_ir_enforces_stack_walking_attrs_for_all_functions() {
    let context = Context::create();
    let cg = CodeGen::new(&context, "test");

    // Bypass `define_function` to simulate accidental direct `Module::add_function` usage.
    let i32_ty = context.i32_type();
    let fn_ty = i32_ty.fn_type(&[], false);
    let func = cg.module.add_function("raw", fn_ty, None);
    let entry = context.append_basic_block(func, "entry");
    cg.builder.position_at_end(entry);
    let _ = cg.builder.build_return(Some(&i32_ty.const_int(0, false)));

    let ir = cg.module_ir();
    assert!(ir.contains("gc \"coreclr\""), "IR:\n{ir}");
    assert!(ir.contains("\"frame-pointer\"=\"all\""), "IR:\n{ir}");
    assert!(
      ir.contains("\"disable-tail-calls\"=\"true\"") || ir.contains("disable-tail-calls"),
      "IR:\n{ir}"
    );
  }

  #[test]
  fn module_ir_does_not_mark_runtime_declarations_as_gc_managed() {
    let context = Context::create();
    let cg = CodeGen::new(&context, "test");
    cg.runtime_abi().declare_all();

    let ir = cg.module_ir();

    // Runtime ABI declarations are imported into the module but should not be treated as
    // compiler-generated GC-managed code (no `gc "coreclr"` and no stack-walking attrs).
    assert_decl_not_gc_managed(&ir, "declare ptr @rt_alloc");
    assert_decl_not_gc_managed(&ir, "declare ptr @rt_alloc_pinned");
    assert_decl_not_gc_managed(&ir, "declare void @rt_gc_safepoint");
    assert_decl_not_gc_managed(&ir, "declare void @rt_gc_safepoint_slow");
    assert_decl_not_gc_managed(&ir, "declare ptr @rt_gc_safepoint_relocate_h");
    assert_decl_not_gc_managed(&ir, "declare void @rt_gc_collect");

    // NoGC runtime declarations may carry `gc-leaf-function`, but still must not be
    // marked as GC-managed functions.
    assert_decl_not_gc_managed(&ir, "declare i1 @rt_gc_poll");
    assert_decl_not_gc_managed(&ir, "declare void @rt_write_barrier");
    assert_decl_not_gc_managed(&ir, "declare void @rt_write_barrier_range");
    assert_decl_not_gc_managed(&ir, "declare void @rt_keep_alive_gc_ref");
  }
}
