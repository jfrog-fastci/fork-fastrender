use crate::resolve::BindingId;
use crate::strict::Entrypoint;
use crate::Resolver;
use diagnostics::{Diagnostic, Span, TextRange};
use hir_js::{AssignOp, BinaryOp, ExprId, ExprKind, Literal, StmtId, StmtKind, UnaryOp};
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::IntType;
use inkwell::values::{FunctionValue, IntValue, PointerValue};
use std::collections::HashMap;
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
  build_ts_main(context, &builder, i32_ty, ts_main, hir_body, entry_file, program)?;

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
  crate::stack_walking::apply_stack_walking_attrs(context, func);
  func
}

fn declare_c_main<'ctx>(
  context: &'ctx Context,
  module: &Module<'ctx>,
  i32_ty: IntType<'ctx>,
) -> FunctionValue<'ctx> {
  // Define `main` with no parameters (`int main(void)`), since our generated
  // wrapper does not currently use `argc`/`argv`.
  //
  // This also avoids passing a raw `ptr` argument through a function marked with
  // our GC strategy (`gc "coreclr"`), which would violate the GC pointer
  // discipline lint (all pointers in GC function signatures must be
  // `ptr addrspace(1)`).
  let func = module.add_function("main", i32_ty.fn_type(&[], false), None);
  crate::stack_walking::apply_stack_walking_attrs(context, func);
  func
}

fn build_ts_main<'ctx>(
  context: &'ctx Context,
  builder: &Builder<'ctx>,
  i32_ty: IntType<'ctx>,
  func: FunctionValue<'ctx>,
  hir_body: &hir_js::Body,
  entry_file: FileId,
  program: &Program,
) -> Result<(), Vec<Diagnostic>> {
  let bb = context.append_basic_block(func, "entry");
  builder.position_at_end(bb);

  let resolver = Resolver::new(program);
  let file_resolver = resolver.for_file(entry_file);
  let mut locals: HashMap<BindingId, PointerValue<'ctx>> = HashMap::new();

  let Some(function) = hir_body.function.as_ref() else {
    return Err(vec![Diagnostic::error(
      "NJS0102",
      "missing function metadata for `main` body",
      Span::new(entry_file, hir_body.span),
    )]);
  };

  let value = match &function.body {
    hir_js::FunctionBody::Expr(expr) => {
      codegen_expr(builder, i32_ty, hir_body, entry_file, *expr, &file_resolver, &mut locals)?
    }
    hir_js::FunctionBody::Block(stmts) => {
      let mut returned = None;
      for stmt in stmts {
        if let Some(ret) = codegen_stmt(
          builder,
          i32_ty,
          hir_body,
          entry_file,
          *stmt,
          &file_resolver,
          &mut locals,
        )? {
          returned = Some(ret);
          break;
        }
      }
      returned.ok_or_else(|| {
        vec![Diagnostic::error(
          "NJS0102",
          "`main` must return a value",
          Span::new(entry_file, hir_body.span),
        )]
      })?
    }
  };

  builder.build_return(Some(&value)).expect("failed to build return");
  Ok(())
}

fn codegen_stmt<'ctx>(
  builder: &Builder<'ctx>,
  i32_ty: IntType<'ctx>,
  body: &hir_js::Body,
  entry_file: FileId,
  stmt_id: StmtId,
  file_resolver: &crate::resolve::FileResolver<'_, '_>,
  locals: &mut HashMap<BindingId, PointerValue<'ctx>>,
) -> Result<Option<IntValue<'ctx>>, Vec<Diagnostic>> {
  let Some(stmt) = body.stmts.get(stmt_id.0 as usize) else {
    return Err(vec![Diagnostic::error(
      "NJS0120",
      "statement id out of bounds",
      Span::new(entry_file, body.span),
    )]);
  };

  let span = Span::new(entry_file, stmt.span);
  match &stmt.kind {
    StmtKind::Expr(expr) => {
      let _ = codegen_expr(builder, i32_ty, body, entry_file, *expr, file_resolver, locals)?;
      Ok(None)
    }
    StmtKind::Return(Some(expr)) => {
      let value = codegen_expr(builder, i32_ty, body, entry_file, *expr, file_resolver, locals)?;
      Ok(Some(value))
    }
    StmtKind::Return(None) => Err(vec![Diagnostic::error(
      "NJS0121",
      "return without value is not supported",
      span,
    )]),
    StmtKind::Block(stmts) => {
      for stmt in stmts {
        if let Some(ret) =
          codegen_stmt(builder, i32_ty, body, entry_file, *stmt, file_resolver, locals)?
        {
          return Ok(Some(ret));
        }
      }
      Ok(None)
    }
    StmtKind::Var(var) => {
      for decl in &var.declarators {
        let binding = file_resolver.resolve_pat_ident(body, decl.pat).ok_or_else(|| {
          let pat_span = body
            .pats
            .get(decl.pat.0 as usize)
            .map(|pat| pat.span)
            .unwrap_or(stmt.span);
          vec![Diagnostic::error(
            "NJS0122",
            "unsupported variable binding pattern",
            Span::new(entry_file, pat_span),
          )]
        })?;

        let slot = locals.get(&binding).copied().unwrap_or_else(|| {
          let slot = builder
            .build_alloca(i32_ty, "local")
            .expect("failed to build alloca");
          locals.insert(binding, slot);
          slot
        });

        let init = decl.init.ok_or_else(|| {
          vec![Diagnostic::error(
            "NJS0123",
            "variable declarations must have initializers in this codegen subset",
            span,
          )]
        })?;
        let value = codegen_expr(builder, i32_ty, body, entry_file, init, file_resolver, locals)?;
        builder
          .build_store(slot, value)
          .expect("failed to build store");
      }
      Ok(None)
    }
    other => Err(vec![Diagnostic::error(
      "NJS0124",
      format!("unsupported statement in `main`: {other:?}"),
      span,
    )]),
  }
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
  file_resolver: &crate::resolve::FileResolver<'_, '_>,
  locals: &mut HashMap<BindingId, PointerValue<'ctx>>,
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
    ExprKind::TypeAssertion { expr, .. } => {
      codegen_expr(builder, i32_ty, body, entry_file, *expr, file_resolver, locals)
    }
    ExprKind::NonNull { expr } => {
      codegen_expr(builder, i32_ty, body, entry_file, *expr, file_resolver, locals)
    }
    ExprKind::Satisfies { expr, .. } => {
      codegen_expr(builder, i32_ty, body, entry_file, *expr, file_resolver, locals)
    }
    ExprKind::Ident(_) => {
      let binding = file_resolver.resolve_expr_ident(body, expr).ok_or_else(|| {
        vec![Diagnostic::error("NJS0130", "failed to resolve identifier", span)]
      })?;
      let slot = locals.get(&binding).copied().ok_or_else(|| {
        vec![Diagnostic::error("NJS0131", "use of unbound local", span)]
      })?;
      let loaded = builder
        .build_load(i32_ty, slot, "load")
        .expect("failed to build load")
        .into_int_value();
      Ok(loaded)
    }
    ExprKind::Literal(Literal::Number(raw)) => parse_i32_const(i32_ty, raw).ok_or_else(|| {
      vec![Diagnostic::error(
        "NJS0104",
        format!("unsupported numeric literal `{raw}` (expected 32-bit integer)"),
        span,
      )]
    }),
    ExprKind::Unary { op, expr } => {
      let inner =
        codegen_expr(builder, i32_ty, body, entry_file, *expr, file_resolver, locals)?;
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
      let lhs =
        codegen_expr(builder, i32_ty, body, entry_file, *left, file_resolver, locals)?;
      let rhs =
        codegen_expr(builder, i32_ty, body, entry_file, *right, file_resolver, locals)?;
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
        BinaryOp::BitAnd => builder
          .build_and(lhs, rhs, "and")
          .expect("failed to build and"),
        BinaryOp::BitOr => builder
          .build_or(lhs, rhs, "or")
          .expect("failed to build or"),
        BinaryOp::BitXor => builder
          .build_xor(lhs, rhs, "xor")
          .expect("failed to build xor"),
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
    ExprKind::Assignment { op, target, value } => {
      let value =
        codegen_expr(builder, i32_ty, body, entry_file, *value, file_resolver, locals)?;
      let binding = file_resolver.resolve_pat_ident(body, *target).ok_or_else(|| {
        let pat_span = body
          .pats
          .get(target.0 as usize)
          .map(|pat| pat.span)
          .unwrap_or(expr_data.span);
        vec![Diagnostic::error(
          "NJS0132",
          "unsupported assignment target",
          Span::new(entry_file, pat_span),
        )]
      })?;
      let slot = locals.get(&binding).copied().ok_or_else(|| {
        vec![Diagnostic::error(
          "NJS0133",
          "assignment to unbound local",
          span,
        )]
      })?;

      match op {
        AssignOp::Assign => {
          builder
            .build_store(slot, value)
            .expect("failed to build store");
          Ok(value)
        }
        AssignOp::AddAssign => {
          let old = builder
            .build_load(i32_ty, slot, "old")
            .expect("failed to build load")
            .into_int_value();
          let out = builder
            .build_int_add(old, value, "add_assign")
            .expect("failed to build add");
          builder
            .build_store(slot, out)
            .expect("failed to build store");
          Ok(out)
        }
        other => Err(vec![Diagnostic::error(
          "NJS0134",
          format!("unsupported assignment operator `{other:?}`"),
          span,
        )]),
      }
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

mod builtins;
mod llvm;
pub mod safepoint;

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

pub fn emit_llvm_module(
  ast: &Node<TopLevel>,
  opts: CompileOptions,
) -> Result<String, CodegenError> {
  llvm::emit_llvm_module(ast, opts)
}
