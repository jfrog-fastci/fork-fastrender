use inkwell::attributes::AttributeLoc;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::FunctionType;
use inkwell::values::FunctionValue;

use crate::llvm::gc::GC_STRATEGY;

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

pub(crate) fn apply_stack_walking_attrs(context: &Context, func: FunctionValue<'_>) {
  func.set_gc(GC_STRATEGY);
  apply_stack_walking_frame_attrs(context, func);
}

/// LLVM code generator.
///
/// This is currently a minimal façade around `inkwell` that guarantees stack-walkability
/// invariants needed by precise GC (see `native-js/docs/gc_stack_walking.md`).
///
/// It also marks all generated functions with the LLVM GC strategy used for statepoint lowering
/// (see `native-js/docs/llvm_gc_strategy.md`).
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
      apply_stack_walking_attrs(self.context, f);
      func = f.get_next_function();
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

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
}
