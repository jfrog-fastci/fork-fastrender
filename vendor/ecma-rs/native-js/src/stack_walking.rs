use inkwell::attributes::AttributeLoc;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::FunctionType;
use inkwell::values::FunctionValue;

/// LLVM code generator.
///
/// This is currently a minimal façade around `inkwell` that guarantees stack-walkability
/// invariants needed by precise GC (see `native-js/docs/gc_stack_walking.md`).
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

  pub fn module_ir(&self) -> String {
    self.module.print_to_string().to_string()
  }

  pub fn define_function(&self, name: &str, ty: FunctionType<'ctx>) -> FunctionValue<'ctx> {
    let func = self.module.add_function(name, ty, None);
    self.apply_stack_walking_attrs(func);
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

  fn apply_stack_walking_attrs(&self, func: FunctionValue<'ctx>) {
    // Required for deterministic GC stack walking:
    //
    // - `frame-pointer="all"`: force a stable frame chain we can walk.
    // - `disable-tail-calls="true"`: prevent tail-call elimination from collapsing frames.
    let frame_pointer = self.context.create_string_attribute("frame-pointer", "all");
    let disable_tail_calls = self
      .context
      .create_string_attribute("disable-tail-calls", "true");

    func.add_attribute(AttributeLoc::Function, frame_pointer);
    func.add_attribute(AttributeLoc::Function, disable_tail_calls);
  }
}
