use diagnostics::{Diagnostic, FileId, Span, TextRange};
use inkwell::context::Context;
use inkwell::types::{BasicType, BasicTypeEnum};
use typecheck_ts::{Program, TypeId, TypeKindSummary};

/// Subset of native ABI types supported by the initial native-js backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeType {
  /// IEEE-754 double (`double`).
  F64,
  /// 1-bit integer (`i1`).
  I1,
  /// No value (`void`).
  Void,
}

/// Map a [`NativeType`] to an LLVM `BasicTypeEnum`.
///
/// Note that `void` has no `BasicTypeEnum` representation, so callers must
/// handle [`NativeType::Void`] separately (via `Context::void_type()`).
pub fn llvm_type<'ctx>(ctx: &'ctx Context, ty: NativeType) -> BasicTypeEnum<'ctx> {
  match ty {
    NativeType::F64 => ctx.f64_type().as_basic_type_enum(),
    NativeType::I1 => ctx.bool_type().as_basic_type_enum(),
    NativeType::Void => panic!("NativeType::Void has no LLVM BasicTypeEnum representation"),
  }
}

/// Classify a TypeScript type into a native ABI type supported by this backend.
///
/// Today this only supports:
/// - `number` → `double`
/// - `boolean` → `i1`
/// - `void` / `undefined` → `void`
pub fn classify_type(program: &Program, type_id: TypeId) -> Result<NativeType, Diagnostic> {
  let kind = program.type_kind(type_id);
  match kind {
    TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_) => Ok(NativeType::F64),
    TypeKindSummary::Boolean | TypeKindSummary::BooleanLiteral(_) => Ok(NativeType::I1),
    TypeKindSummary::Void | TypeKindSummary::Undefined => Ok(NativeType::Void),
    other => Err(Diagnostic::error(
      "NATIVE0001",
      format!(
        "unsupported type for native codegen: {} ({other:?})",
        program.display_type(type_id)
      ),
      Span::new(FileId(0), TextRange::new(0, 0)),
    )),
  }
}
