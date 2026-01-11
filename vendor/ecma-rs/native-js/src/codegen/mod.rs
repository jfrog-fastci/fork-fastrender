use crate::strict::Entrypoint;
use diagnostics::{Diagnostic, Span, TextRange};
use hir_js::{BinaryOp, ExprId, ExprKind, Literal, UnaryOp};
use inkwell::attributes::AttributeLoc;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::IntType;
use inkwell::values::{FunctionValue, IntValue};
use inkwell::AddressSpace;
use typecheck_ts::{FileId, Program};

pub struct CodegenOptions {
  pub module_name: String,
}

impl Default for CodegenOptions {
  fn default() -> Self {
    Self {
      module_name: "native_js".to_string(),
    }
  }
}

pub fn codegen<'ctx>(
  context: &'ctx Context,
  program: &Program,
  entry_file: FileId,
  entrypoint: Entrypoint,
  options: CodegenOptions,
) -> Result<Module<'ctx>, Vec<Diagnostic>> {
  let lowered = program.hir_lowered(entry_file).ok_or_else(|| {
    vec![Diagnostic::error(
      "NJS0100",
      "failed to access lowered HIR for entry file",
      Span::new(entry_file, TextRange::new(0, 0)),
    )]
  })?;
  let hir_body = lowered.body(entrypoint.main_body).ok_or_else(|| {
    vec![Diagnostic::error(
      "NJS0101",
      "failed to access lowered HIR for `main` body",
      Span::new(entry_file, TextRange::new(0, 0)),
    )]
  })?;

  let module = context.create_module(&options.module_name);
  let builder = context.create_builder();
  let i32_ty = context.i32_type();

  let ts_main = declare_ts_main(context, &module, i32_ty);
  build_ts_main(context, &builder, i32_ty, ts_main, hir_body, entry_file)?;

  let c_main = declare_c_main(context, &module, i32_ty);
  build_c_main(context, &builder, c_main, ts_main, i32_ty);

  Ok(module)
}

fn declare_ts_main<'ctx>(
  context: &'ctx Context,
  module: &Module<'ctx>,
  i32_ty: IntType<'ctx>,
) -> FunctionValue<'ctx> {
  let func = module.add_function("ts_main", i32_ty.fn_type(&[], false), None);
  apply_stack_walking_attrs(context, func);
  func
}

fn declare_c_main<'ctx>(
  context: &'ctx Context,
  module: &Module<'ctx>,
  i32_ty: IntType<'ctx>,
) -> FunctionValue<'ctx> {
  // LLVM 15+ uses opaque pointers by default, so we represent `char **argv` as
  // a single `ptr` argument.
  let argv_ty = context.ptr_type(AddressSpace::default());
  let func = module.add_function(
    "main",
    i32_ty.fn_type(&[i32_ty.into(), argv_ty.into()], false),
    None,
  );
  apply_stack_walking_attrs(context, func);
  func
}

fn build_ts_main<'ctx>(
  context: &'ctx Context,
  builder: &Builder<'ctx>,
  i32_ty: IntType<'ctx>,
  func: FunctionValue<'ctx>,
  hir_body: &hir_js::Body,
  entry_file: FileId,
) -> Result<(), Vec<Diagnostic>> {
  let bb = context.append_basic_block(func, "entry");
  builder.position_at_end(bb);

  let return_expr = match &hir_body
    .function
    .as_ref()
    .expect("validated function body must have metadata")
    .body
  {
    hir_js::FunctionBody::Expr(expr) => Some(*expr),
    hir_js::FunctionBody::Block(stmts) => stmts
      .iter()
      .rev()
      .find_map(|stmt_id| match hir_body.stmts.get(stmt_id.0 as usize)?.kind {
        hir_js::StmtKind::Return(expr) => expr,
        _ => None,
      }),
  }
  .ok_or_else(|| {
    vec![Diagnostic::error(
      "NJS0102",
      "`main` must have a return expression",
      Span::new(entry_file, hir_body.span),
    )]
  })?;

  let value = codegen_expr(builder, i32_ty, hir_body, entry_file, return_expr)?;
  builder
    .build_return(Some(&value))
    .expect("failed to build return");
  Ok(())
}

fn build_c_main<'ctx>(
  context: &'ctx Context,
  builder: &Builder<'ctx>,
  c_main: FunctionValue<'ctx>,
  ts_main: FunctionValue<'ctx>,
  i32_ty: IntType<'ctx>,
) {
  let bb = context.append_basic_block(c_main, "entry");
  builder.position_at_end(bb);

  let call = builder
    .build_call(ts_main, &[], "ret")
    .expect("failed to build call");
  let ret_val = call
    .try_as_basic_value()
    .left()
    .map(|v| v.into_int_value())
    .unwrap_or_else(|| i32_ty.const_zero());
  builder
    .build_return(Some(&ret_val))
    .expect("failed to build return");
}

fn codegen_expr<'ctx>(
  builder: &Builder<'ctx>,
  i32_ty: IntType<'ctx>,
  body: &hir_js::Body,
  entry_file: FileId,
  expr: ExprId,
) -> Result<IntValue<'ctx>, Vec<Diagnostic>> {
  let expr_data = body.exprs.get(expr.0 as usize).ok_or_else(|| {
    vec![Diagnostic::error(
      "NJS0103",
      "expression id out of bounds",
      Span::new(entry_file, body.span),
    )]
  })?;
  let span = Span::new(entry_file, expr_data.span);

  match &expr_data.kind {
    ExprKind::Literal(Literal::Number(raw)) => parse_i32_const(i32_ty, raw).ok_or_else(|| {
      vec![Diagnostic::error(
        "NJS0104",
        format!("unsupported numeric literal `{raw}` (expected 32-bit integer)"),
        span,
      )]
    }),
    ExprKind::Unary { op, expr } => {
      let inner = codegen_expr(builder, i32_ty, body, entry_file, *expr)?;
      match op {
        UnaryOp::Plus => Ok(inner),
        UnaryOp::Minus => Ok(
          builder
            .build_int_neg(inner, "neg")
            .expect("failed to build negation"),
        ),
        _ => Err(vec![Diagnostic::error(
          "NJS0105",
          format!("unsupported unary operator `{op:?}`"),
          span,
        )]),
      }
    }
    ExprKind::Binary { op, left, right } => {
      let lhs = codegen_expr(builder, i32_ty, body, entry_file, *left)?;
      let rhs = codegen_expr(builder, i32_ty, body, entry_file, *right)?;
      let v = match op {
        BinaryOp::Add => builder
          .build_int_add(lhs, rhs, "add")
          .expect("failed to build add"),
        BinaryOp::Subtract => builder
          .build_int_sub(lhs, rhs, "sub")
          .expect("failed to build sub"),
        BinaryOp::Multiply => builder
          .build_int_mul(lhs, rhs, "mul")
          .expect("failed to build mul"),
        BinaryOp::Divide => builder
          .build_int_signed_div(lhs, rhs, "div")
          .expect("failed to build div"),
        BinaryOp::Remainder => builder
          .build_int_signed_rem(lhs, rhs, "rem")
          .expect("failed to build rem"),
        BinaryOp::BitAnd => builder.build_and(lhs, rhs, "and").expect("failed to build and"),
        BinaryOp::BitOr => builder.build_or(lhs, rhs, "or").expect("failed to build or"),
        BinaryOp::BitXor => builder.build_xor(lhs, rhs, "xor").expect("failed to build xor"),
        BinaryOp::ShiftLeft => builder
          .build_left_shift(lhs, rhs, "shl")
          .expect("failed to build shl"),
        BinaryOp::ShiftRight => builder
          .build_right_shift(lhs, rhs, true, "shr")
          .expect("failed to build shr"),
        _ => {
          return Err(vec![Diagnostic::error(
            "NJS0106",
            format!("unsupported binary operator `{op:?}`"),
            span,
          )]);
        }
      };
      Ok(v)
    }
    _ => Err(vec![Diagnostic::error(
      "NJS0107",
      "unsupported expression in `main`",
      span,
    )]),
  }
}

fn parse_i32_const<'ctx>(i32_ty: IntType<'ctx>, raw: &str) -> Option<IntValue<'ctx>> {
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  let normalized: String = raw.chars().filter(|c| *c != '_').collect();
  let (radix, digits) = if let Some(rest) = normalized.strip_prefix("0x") {
    (16, rest)
  } else if let Some(rest) = normalized.strip_prefix("0X") {
    (16, rest)
  } else if let Some(rest) = normalized.strip_prefix("0b") {
    (2, rest)
  } else if let Some(rest) = normalized.strip_prefix("0B") {
    (2, rest)
  } else if let Some(rest) = normalized.strip_prefix("0o") {
    (8, rest)
  } else if let Some(rest) = normalized.strip_prefix("0O") {
    (8, rest)
  } else {
    if normalized.contains('.') || normalized.contains('e') || normalized.contains('E') {
      return None;
    }
    (10, normalized.as_str())
  };

  let value = i64::from_str_radix(digits, radix).ok()?;
  let value = i32::try_from(value).ok()?;
  Some(i32_ty.const_int(value as u64, true))
}

fn apply_stack_walking_attrs<'ctx>(context: &'ctx Context, func: FunctionValue<'ctx>) {
  let frame_pointer = context.create_string_attribute("frame-pointer", "all");
  let disable_tail_calls = context.create_string_attribute("disable-tail-calls", "true");

  func.add_attribute(AttributeLoc::Function, frame_pointer);
  func.add_attribute(AttributeLoc::Function, disable_tail_calls);
}

mod builtins;
mod llvm;

use crate::CompileOptions;
use parse_js::ast::node::Node;
use parse_js::ast::stx::TopLevel;

#[derive(thiserror::Error, Debug)]
pub enum CodegenError {
  #[error("unsupported statement")]
  UnsupportedStmt,

  #[error("unsupported expression")]
  UnsupportedExpr,

  #[error("unsupported operator: {0:?}")]
  UnsupportedOperator(parse_js::operator::OperatorName),

  #[error("builtins disabled")]
  BuiltinsDisabled,

  #[error("type error: {0}")]
  TypeError(String),
}

pub fn emit_llvm_module(ast: &Node<TopLevel>, opts: CompileOptions) -> Result<String, CodegenError> {
  llvm::emit_llvm_module(ast, opts)
}

