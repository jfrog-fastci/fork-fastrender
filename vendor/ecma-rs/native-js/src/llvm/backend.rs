use super::{LlvmBackend, ValueKind};
use inkwell::types::{BasicTypeEnum, FloatType, IntType};
use inkwell::values::{FunctionValue, PointerValue};

impl<'ctx> LlvmBackend<'ctx> {
  pub fn f64_type(&self) -> FloatType<'ctx> {
    self.context.f64_type()
  }

  pub fn bool_type(&self) -> IntType<'ctx> {
    self.context.bool_type()
  }

  pub fn llvm_type(&self, kind: ValueKind) -> BasicTypeEnum<'ctx> {
    match kind {
      ValueKind::Number => self.f64_type().into(),
      ValueKind::Boolean => self.bool_type().into(),
      ValueKind::Void => panic!("ValueKind::Void has no LLVM BasicTypeEnum representation"),
    }
  }

  /// Create an `alloca` in the entry block of `function`.
  ///
  /// Note: With opaque pointers, we must keep track of the pointee type
  /// separately (see `LocalSlot`).
  pub fn build_entry_alloca(
    &self,
    function: FunctionValue<'ctx>,
    ty: BasicTypeEnum<'ctx>,
    name: &str,
  ) -> PointerValue<'ctx> {
    let entry = function
      .get_first_basic_block()
      .expect("function must have an entry block before allocating locals");
    let builder = self.context.create_builder();
    match entry.get_first_instruction() {
      Some(inst) => builder.position_before(&inst),
      None => builder.position_at_end(entry),
    }
    builder.build_alloca(ty, name).expect("alloca should succeed")
  }
}
