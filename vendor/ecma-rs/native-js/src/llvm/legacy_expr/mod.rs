//! Legacy expression-only HIR → LLVM backend.
//!
//! This backend predates the checked `native_js::codegen` pipeline and is kept only for debugging
//! and bisecting old behavior. It is intentionally feature-gated to avoid accidental dependency
//! creep in tests and tooling.
//!
//! Prefer the checked backend (`native_js::codegen`) for all new development.
#![cfg(feature = "legacy-expr-backend")]

use crate::codes;
use crate::llvm::LlvmBackend;
use diagnostics::{Diagnostic, Span, TextRange};
use hir_js::{
  BinaryOp, Body, BodyId, CallExpr, ExprId, ExprKind, FunctionBody, Literal, NameId, PatId, PatKind, StmtId,
  StmtKind, UnaryOp, VarDecl,
};
use inkwell::types::{BasicTypeEnum, FloatType, IntType};
use inkwell::values::{
  BasicMetadataValueEnum, BasicValue, BasicValueEnum, FloatValue, FunctionValue, IntValue, PointerValue,
};
use inkwell::{FloatPredicate, IntPredicate};
use parse_js::num::JsNumber;
use std::collections::HashMap;
use typecheck_ts::{BodyCheckResult, Program, TypeKindSummary};

/// Minimal set of primitive kinds supported by the legacy expression backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueKind {
  Number,
  Boolean,
  Void,
}

impl ValueKind {
  pub fn from_type_kind(kind: &TypeKindSummary) -> Option<Self> {
    match kind {
      TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_) => Some(ValueKind::Number),
      TypeKindSummary::Boolean | TypeKindSummary::BooleanLiteral(_) => Some(ValueKind::Boolean),
      TypeKindSummary::Void | TypeKindSummary::Undefined => Some(ValueKind::Void),
      _ => None,
    }
  }

  pub fn as_str(self) -> &'static str {
    match self {
      ValueKind::Number => "number",
      ValueKind::Boolean => "boolean",
      ValueKind::Void => "void",
    }
  }
}

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
  /// Note: With opaque pointers, we must keep track of the pointee type separately.
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

#[derive(Clone, Copy)]
pub struct LocalSlot<'ctx> {
  pub ptr: PointerValue<'ctx>,
  pub ty: BasicTypeEnum<'ctx>,
}

pub struct LocalMap<'ctx> {
  scopes: Vec<HashMap<NameId, LocalSlot<'ctx>>>,
}

impl<'ctx> LocalMap<'ctx> {
  pub fn new() -> Self {
    Self {
      scopes: vec![HashMap::new()],
    }
  }

  pub fn push_scope(&mut self) {
    self.scopes.push(HashMap::new());
  }

  pub fn pop_scope(&mut self) {
    if self.scopes.len() > 1 {
      self.scopes.pop();
    }
  }

  pub fn insert(&mut self, name: NameId, slot: LocalSlot<'ctx>) {
    self
      .scopes
      .last_mut()
      .expect("LocalMap must have at least one scope")
      .insert(name, slot);
  }

  pub fn get(&self, name: NameId) -> Option<LocalSlot<'ctx>> {
    for scope in self.scopes.iter().rev() {
      if let Some(slot) = scope.get(&name) {
        return Some(*slot);
      }
    }
    None
  }
}

#[derive(Clone)]
pub struct FunctionSymbol<'ctx> {
  pub function: FunctionValue<'ctx>,
  pub params: Vec<ValueKind>,
  pub ret: ValueKind,
}

pub struct FunctionCodegen<'a, 'ctx> {
  pub backend: &'a mut LlvmBackend<'ctx>,
  pub program: &'a Program,
  pub body_id: BodyId,
  pub body: &'a Body,
  pub types: &'a BodyCheckResult,
  pub locals: LocalMap<'ctx>,
  pub functions: &'a HashMap<NameId, FunctionSymbol<'ctx>>,
  pub diagnostics: &'a mut Vec<Diagnostic>,
  pub function: FunctionValue<'ctx>,
}

impl<'a, 'ctx> FunctionCodegen<'a, 'ctx> {
  #[allow(clippy::too_many_arguments)]
  pub fn new(
    backend: &'a mut LlvmBackend<'ctx>,
    program: &'a Program,
    body_id: BodyId,
    body: &'a Body,
    types: &'a BodyCheckResult,
    functions: &'a HashMap<NameId, FunctionSymbol<'ctx>>,
    diagnostics: &'a mut Vec<Diagnostic>,
    function: FunctionValue<'ctx>,
  ) -> Self {
    Self {
      backend,
      program,
      body_id,
      body,
      types,
      locals: LocalMap::new(),
      functions,
      diagnostics,
      function,
    }
  }

  pub fn emit_unsupported_expr(&mut self, expr: ExprId, message: impl Into<String>) {
    let span = self.span_for_expr(expr);
    self
      .diagnostics
      .push(codes::UNSUPPORTED_EXPR.error(message, span));
  }

  pub fn emit_unsupported_type(&mut self, expr: ExprId, message: impl Into<String>) {
    let span = self.span_for_expr(expr);
    self
      .diagnostics
      .push(codes::UNSUPPORTED_NATIVE_TYPE.error(message, span));
  }

  fn span_for_expr(&self, expr: ExprId) -> Span {
    self
      .program
      .expr_span(self.body_id, expr)
      .unwrap_or(Span::new(self.body_id.file(), TextRange::new(0, 0)))
  }

  fn kind_for_expr(&mut self, expr: ExprId) -> Option<ValueKind> {
    let ty = self.program.type_of_expr(self.body_id, expr);
    let kind = self.program.type_kind(ty);
    let Some(kind) = ValueKind::from_type_kind(&kind) else {
      self.emit_unsupported_type(expr, format!("unsupported type: {kind:?}"));
      return None;
    };
    Some(kind)
  }

  fn expect_kind(&mut self, expr: ExprId, expected: ValueKind) -> Option<()> {
    let actual = self.kind_for_expr(expr)?;
    if actual != expected {
      self.emit_unsupported_type(
        expr,
        format!("expected `{}`, got `{}`", expected.as_str(), actual.as_str()),
      );
      return None;
    }
    Some(())
  }

  fn expect_same_kind(&mut self, left: ExprId, right: ExprId) -> Option<ValueKind> {
    let left_kind = self.kind_for_expr(left)?;
    let right_kind = self.kind_for_expr(right)?;
    if left_kind != right_kind {
      self.emit_unsupported_type(
        right,
        format!(
          "type mismatch: left is `{}`, right is `{}`",
          left_kind.as_str(),
          right_kind.as_str()
        ),
      );
      return None;
    }
    Some(left_kind)
  }

  fn as_f64(&mut self, value: BasicValueEnum<'ctx>, expr: ExprId) -> Option<FloatValue<'ctx>> {
    match value {
      BasicValueEnum::FloatValue(v) => Some(v),
      _ => {
        self.emit_unsupported_type(expr, "expected number value");
        None
      }
    }
  }

  fn as_bool(&mut self, value: BasicValueEnum<'ctx>, expr: ExprId) -> Option<IntValue<'ctx>> {
    match value {
      BasicValueEnum::IntValue(v) => Some(v),
      _ => {
        self.emit_unsupported_type(expr, "expected boolean value");
        None
      }
    }
  }

  pub fn codegen_params(&mut self, names: &[NameId], kinds: &[ValueKind]) {
    for (idx, (name, kind)) in names.iter().copied().zip(kinds.iter().copied()).enumerate() {
      let Some(arg) = self.function.get_nth_param(idx as u32) else {
        continue;
      };
      let ty = self.backend.llvm_type(kind);
      let ptr = self.backend.build_entry_alloca(self.function, ty, "param");
      let _ = self.backend.builder.build_store(ptr, arg);
      self.locals.insert(name, LocalSlot { ptr, ty });
    }
  }

  pub fn codegen_function_body(&mut self, func: &hir_js::FunctionData, ret_kind: ValueKind) -> bool {
    match &func.body {
      FunctionBody::Expr(expr) => match ret_kind {
        ValueKind::Void => {
          // Evaluate the expression for side effects, then return `void`.
          let _ = self.codegen_expr(*expr);
          let _ = self.backend.builder.build_return(None);
          true
        }
        ValueKind::Number | ValueKind::Boolean => {
          let Some(value) = self.codegen_expr(*expr) else {
            return false;
          };
          let llvm_ret: BasicValueEnum<'ctx> = match ret_kind {
            ValueKind::Number => match self.as_f64(value, *expr) {
              Some(v) => v.as_basic_value_enum(),
              None => return false,
            },
            ValueKind::Boolean => match self.as_bool(value, *expr) {
              Some(v) => v.as_basic_value_enum(),
              None => return false,
            },
            ValueKind::Void => unreachable!(),
          };
          let _ = self.backend.builder.build_return(Some(&llvm_ret));
          true
        }
      },
      FunctionBody::Block(stmts) => self.codegen_block(stmts, ret_kind),
    }
  }

  fn codegen_block(&mut self, stmts: &[StmtId], ret_kind: ValueKind) -> bool {
    self.locals.push_scope();
    for stmt in stmts.iter().copied() {
      if self.codegen_stmt(stmt, ret_kind) {
        self.locals.pop_scope();
        return true;
      }
    }
    self.locals.pop_scope();
    false
  }

  fn codegen_stmt(&mut self, stmt: StmtId, ret_kind: ValueKind) -> bool {
    let Some(stmt) = self.body.stmts.get(stmt.0 as usize) else {
      return false;
    };
    match &stmt.kind {
      StmtKind::Return(expr) => match (ret_kind, expr) {
        (ValueKind::Void, None) => {
          let _ = self.backend.builder.build_return(None);
          true
        }
        (ValueKind::Void, Some(expr)) => {
          let _ = self.codegen_expr(*expr);
          let _ = self.backend.builder.build_return(None);
          true
        }
        (_, None) => {
          self.diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
            "return without value not supported",
            Span {
              file: self.body_id.file(),
              range: stmt.span,
            },
          ));
          false
        }
        (_, Some(expr)) => {
          let Some(value) = self.codegen_expr(*expr) else {
            return false;
          };
          let llvm_ret = match ret_kind {
            ValueKind::Number => match self.as_f64(value, *expr) {
              Some(v) => v.as_basic_value_enum(),
              None => return false,
            },
            ValueKind::Boolean => match self.as_bool(value, *expr) {
              Some(v) => v.as_basic_value_enum(),
              None => return false,
            },
            ValueKind::Void => unreachable!(),
          };
          let _ = self.backend.builder.build_return(Some(&llvm_ret));
          true
        }
      },
      StmtKind::Expr(expr) => {
        self.codegen_expr(*expr);
        false
      }
      StmtKind::ExportDefaultExpr(expr) => {
        self.codegen_expr(*expr);
        false
      }
      StmtKind::Block(stmts) => self.codegen_block(stmts, ret_kind),
      StmtKind::Var(var_decl) => {
        self.codegen_var_decl(var_decl);
        false
      }
      _ => {
        self.diagnostics.push(codes::UNSUPPORTED_EXPR.error(
          "unsupported statement",
          Span {
            file: self.body_id.file(),
            range: stmt.span,
          },
        ));
        false
      }
    }
  }

  fn codegen_var_decl(&mut self, decl: &VarDecl) {
    for declarator in decl.declarators.iter() {
      let pat_id = declarator.pat;
      let pat = match self.body.pats.get(pat_id.0 as usize) {
        Some(pat) => pat,
        None => continue,
      };
      let PatKind::Ident(name) = pat.kind else {
        self.diagnostics.push(codes::UNSUPPORTED_EXPR.error(
          "unsupported variable binding pattern",
          Span {
            file: self.body_id.file(),
            range: pat.span,
          },
        ));
        continue;
      };
      let Some(init) = declarator.init else {
        self.diagnostics.push(codes::UNSUPPORTED_EXPR.error(
          "variable declaration without initializer",
          Span {
            file: self.body_id.file(),
            range: pat.span,
          },
        ));
        continue;
      };
      let kind = match self.kind_for_pat(pat_id) {
        Some(kind) => kind,
        None => continue,
      };
      let init_value = match self.codegen_expr(init) {
        Some(v) => v,
        None => continue,
      };
      let ty = self.backend.llvm_type(kind);
      let ptr = self.backend.build_entry_alloca(self.function, ty, "local");
      let _ = self.backend.builder.build_store(ptr, init_value);
      self.locals.insert(name, LocalSlot { ptr, ty });
    }
  }

  fn kind_for_pat(&mut self, pat: PatId) -> Option<ValueKind> {
    let Some(ty) = self.types.pat_type(pat) else {
      self.diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
        "missing type for pattern",
        self
          .program
          .pat_span(self.body_id, pat)
          .unwrap_or(Span::new(self.body_id.file(), TextRange::new(0, 0))),
      ));
      return None;
    };
    let kind = self.program.type_kind(ty);
    let Some(kind) = ValueKind::from_type_kind(&kind) else {
      self.diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
        format!("unsupported type: {kind:?}"),
        self
          .program
          .pat_span(self.body_id, pat)
          .unwrap_or(Span::new(self.body_id.file(), TextRange::new(0, 0))),
      ));
      return None;
    };
    if kind == ValueKind::Void {
      self.diagnostics.push(codes::UNSUPPORTED_NATIVE_TYPE.error(
        "`void`/`undefined` local bindings are not supported by native-js yet",
        self
          .program
          .pat_span(self.body_id, pat)
          .unwrap_or(Span::new(self.body_id.file(), TextRange::new(0, 0))),
      ));
      return None;
    }
    Some(kind)
  }

  pub fn codegen_expr(&mut self, expr: ExprId) -> Option<BasicValueEnum<'ctx>> {
    let expr_node = self.body.exprs.get(expr.0 as usize)?;
    match &expr_node.kind {
      ExprKind::Literal(lit) => self.codegen_literal(expr, lit),
      ExprKind::Ident(name) => self.codegen_ident(expr, *name),
      ExprKind::Unary { op, expr: inner } => self.codegen_unary(expr, *op, *inner),
      ExprKind::Binary { op, left, right } => self.codegen_binary(expr, *op, *left, *right),
      ExprKind::Conditional {
        test,
        consequent,
        alternate,
      } => self.codegen_conditional(expr, *test, *consequent, *alternate),
      ExprKind::Call(call) => self.codegen_call(expr, call),
      ExprKind::TypeAssertion { expr: inner, .. }
      | ExprKind::Instantiation { expr: inner, .. }
      | ExprKind::NonNull { expr: inner }
      | ExprKind::Satisfies { expr: inner, .. } => self.codegen_expr(*inner),
      _ => {
        self.emit_unsupported_expr(expr, "unsupported expression");
        None
      }
    }
  }

  fn codegen_literal(&mut self, expr: ExprId, lit: &Literal) -> Option<BasicValueEnum<'ctx>> {
    match lit {
      Literal::Number(raw) => {
        let Some(number) = JsNumber::from_literal(raw).map(|n| n.0) else {
          self.emit_unsupported_expr(expr, "invalid number literal");
          return None;
        };
        Some(self.backend.f64_type().const_float(number).into())
      }
      Literal::Boolean(v) => Some(self.backend.bool_type().const_int(*v as u64, false).into()),
      Literal::Null | Literal::Undefined => {
        self.emit_unsupported_type(expr, "null/undefined not supported");
        None
      }
      _ => {
        self.emit_unsupported_expr(expr, "unsupported literal");
        None
      }
    }
  }

  fn codegen_ident(&mut self, expr: ExprId, name: NameId) -> Option<BasicValueEnum<'ctx>> {
    let Some(slot) = self.locals.get(name) else {
      self.emit_unsupported_expr(expr, "unresolved identifier");
      return None;
    };
    Some(
      self
        .backend
        .builder
        .build_load(slot.ty, slot.ptr, "loadtmp")
        .ok()?,
    )
  }

  fn codegen_unary(&mut self, expr: ExprId, op: UnaryOp, inner: ExprId) -> Option<BasicValueEnum<'ctx>> {
    match op {
      UnaryOp::Plus => {
        self.expect_kind(inner, ValueKind::Number)?;
        self.codegen_expr(inner)
      }
      UnaryOp::Minus => {
        self.expect_kind(inner, ValueKind::Number)?;
        let value = self.codegen_expr(inner)?;
        let value = self.as_f64(value, inner)?;
        Some(
          self
            .backend
            .builder
            .build_float_neg(value, "negtmp")
            .ok()?
            .into(),
        )
      }
      UnaryOp::Not => {
        self.expect_kind(inner, ValueKind::Boolean)?;
        let value = self.codegen_expr(inner)?;
        let value = self.as_bool(value, inner)?;
        Some(self.backend.builder.build_not(value, "nottmp").ok()?.into())
      }
      _ => {
        self.emit_unsupported_expr(expr, "unsupported unary operator");
        None
      }
    }
  }

  fn codegen_binary(&mut self, expr: ExprId, op: BinaryOp, left: ExprId, right: ExprId) -> Option<BasicValueEnum<'ctx>> {
    match op {
      BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide | BinaryOp::Remainder => {
        self.codegen_numeric_binary(expr, op, left, right)
      }
      BinaryOp::LessThan
      | BinaryOp::LessEqual
      | BinaryOp::GreaterThan
      | BinaryOp::GreaterEqual
      | BinaryOp::Equality
      | BinaryOp::Inequality
      | BinaryOp::StrictEquality
      | BinaryOp::StrictInequality => self.codegen_compare(expr, op, left, right),
      BinaryOp::LogicalAnd => self.codegen_logical_and(expr, left, right),
      BinaryOp::LogicalOr => self.codegen_logical_or(expr, left, right),
      _ => {
        self.emit_unsupported_expr(expr, "unsupported binary operator");
        None
      }
    }
  }

  fn codegen_numeric_binary(&mut self, expr: ExprId, op: BinaryOp, left: ExprId, right: ExprId) -> Option<BasicValueEnum<'ctx>> {
    self.expect_kind(left, ValueKind::Number)?;
    self.expect_kind(right, ValueKind::Number)?;
    let left_raw = self.codegen_expr(left)?;
    let left_val = self.as_f64(left_raw, left)?;
    let right_raw = self.codegen_expr(right)?;
    let right_val = self.as_f64(right_raw, right)?;
    let value = match op {
      BinaryOp::Add => self
        .backend
        .builder
        .build_float_add(left_val, right_val, "addtmp")
        .ok()?,
      BinaryOp::Subtract => self
        .backend
        .builder
        .build_float_sub(left_val, right_val, "subtmp")
        .ok()?,
      BinaryOp::Multiply => self
        .backend
        .builder
        .build_float_mul(left_val, right_val, "multmp")
        .ok()?,
      BinaryOp::Divide => self
        .backend
        .builder
        .build_float_div(left_val, right_val, "divtmp")
        .ok()?,
      BinaryOp::Remainder => self
        .backend
        .builder
        .build_float_rem(left_val, right_val, "remtmp")
        .ok()?,
      _ => {
        self.emit_unsupported_expr(expr, "unsupported numeric binary operator");
        return None;
      }
    };
    Some(value.into())
  }

  fn codegen_compare(&mut self, expr: ExprId, op: BinaryOp, left: ExprId, right: ExprId) -> Option<BasicValueEnum<'ctx>> {
    let operand_kind = self.expect_same_kind(left, right)?;
    let pred_float = match op {
      BinaryOp::LessThan => FloatPredicate::OLT,
      BinaryOp::LessEqual => FloatPredicate::OLE,
      BinaryOp::GreaterThan => FloatPredicate::OGT,
      BinaryOp::GreaterEqual => FloatPredicate::OGE,
      BinaryOp::Equality | BinaryOp::StrictEquality => FloatPredicate::OEQ,
      // `NaN !== NaN` is true in JS, so use unordered-neq rather than ordered-neq.
      BinaryOp::Inequality | BinaryOp::StrictInequality => FloatPredicate::UNE,
      _ => {
        self.emit_unsupported_expr(expr, "unsupported comparison operator");
        return None;
      }
    };
    let pred_int = match op {
      BinaryOp::LessThan => IntPredicate::ULT,
      BinaryOp::LessEqual => IntPredicate::ULE,
      BinaryOp::GreaterThan => IntPredicate::UGT,
      BinaryOp::GreaterEqual => IntPredicate::UGE,
      BinaryOp::Equality | BinaryOp::StrictEquality => IntPredicate::EQ,
      BinaryOp::Inequality | BinaryOp::StrictInequality => IntPredicate::NE,
      _ => {
        self.emit_unsupported_expr(expr, "unsupported comparison operator");
        return None;
      }
    };

    let value = match operand_kind {
      ValueKind::Number => {
        let left_raw = self.codegen_expr(left)?;
        let left_val = self.as_f64(left_raw, left)?;
        let right_raw = self.codegen_expr(right)?;
        let right_val = self.as_f64(right_raw, right)?;
        self
          .backend
          .builder
          .build_float_compare(pred_float, left_val, right_val, "cmptmp")
          .ok()?
          .into()
      }
      ValueKind::Boolean => {
        let left_raw = self.codegen_expr(left)?;
        let left_val = self.as_bool(left_raw, left)?;
        let right_raw = self.codegen_expr(right)?;
        let right_val = self.as_bool(right_raw, right)?;
        self
          .backend
          .builder
          .build_int_compare(pred_int, left_val, right_val, "cmptmp")
          .ok()?
          .into()
      }
      ValueKind::Void => {
        self.emit_unsupported_type(expr, "comparisons on `void`/`undefined` values are not supported");
        return None;
      }
    };
    Some(value)
  }

  fn codegen_logical_and(&mut self, _expr: ExprId, left: ExprId, right: ExprId) -> Option<BasicValueEnum<'ctx>> {
    self.expect_kind(left, ValueKind::Boolean)?;
    self.expect_kind(right, ValueKind::Boolean)?;

    let lhs_raw = self.codegen_expr(left)?;
    let lhs_val = self.as_bool(lhs_raw, left)?;
    let lhs_block = self.backend.builder.get_insert_block()?;

    let rhs_block = self.backend.context.append_basic_block(self.function, "and.rhs");
    let merge_block = self.backend.context.append_basic_block(self.function, "and.merge");

    self
      .backend
      .builder
      .build_conditional_branch(lhs_val, rhs_block, merge_block)
      .ok()?;

    self.backend.builder.position_at_end(rhs_block);
    let rhs_raw = self.codegen_expr(right)?;
    let rhs_val = self.as_bool(rhs_raw, right)?;
    let rhs_end_block = self.backend.builder.get_insert_block()?;
    self.backend.builder.build_unconditional_branch(merge_block).ok()?;

    self.backend.builder.position_at_end(merge_block);
    let phi = self
      .backend
      .builder
      .build_phi(self.backend.bool_type(), "andtmp")
      .ok()?;
    phi.add_incoming(&[(&lhs_val, lhs_block), (&rhs_val, rhs_end_block)]);
    Some(phi.as_basic_value())
  }

  fn codegen_logical_or(&mut self, _expr: ExprId, left: ExprId, right: ExprId) -> Option<BasicValueEnum<'ctx>> {
    self.expect_kind(left, ValueKind::Boolean)?;
    self.expect_kind(right, ValueKind::Boolean)?;

    let lhs_raw = self.codegen_expr(left)?;
    let lhs_val = self.as_bool(lhs_raw, left)?;
    let lhs_block = self.backend.builder.get_insert_block()?;

    let rhs_block = self.backend.context.append_basic_block(self.function, "or.rhs");
    let merge_block = self.backend.context.append_basic_block(self.function, "or.merge");

    self
      .backend
      .builder
      .build_conditional_branch(lhs_val, merge_block, rhs_block)
      .ok()?;

    self.backend.builder.position_at_end(rhs_block);
    let rhs_raw = self.codegen_expr(right)?;
    let rhs_val = self.as_bool(rhs_raw, right)?;
    let rhs_end_block = self.backend.builder.get_insert_block()?;
    self.backend.builder.build_unconditional_branch(merge_block).ok()?;

    self.backend.builder.position_at_end(merge_block);
    let phi = self
      .backend
      .builder
      .build_phi(self.backend.bool_type(), "ortmp")
      .ok()?;
    phi.add_incoming(&[(&lhs_val, lhs_block), (&rhs_val, rhs_end_block)]);
    Some(phi.as_basic_value())
  }

  fn codegen_conditional(
    &mut self,
    expr: ExprId,
    test: ExprId,
    consequent: ExprId,
    alternate: ExprId,
  ) -> Option<BasicValueEnum<'ctx>> {
    self.expect_kind(test, ValueKind::Boolean)?;
    let result_kind = self.expect_same_kind(consequent, alternate)?;

    let test_raw = self.codegen_expr(test)?;
    let test_val = self.as_bool(test_raw, test)?;

    let then_block = self.backend.context.append_basic_block(self.function, "cond.then");
    let else_block = self.backend.context.append_basic_block(self.function, "cond.else");
    let merge_block = self.backend.context.append_basic_block(self.function, "cond.merge");

    self
      .backend
      .builder
      .build_conditional_branch(test_val, then_block, else_block)
      .ok()?;

    self.backend.builder.position_at_end(then_block);
    let then_val = self.codegen_expr(consequent)?;
    let then_end_block = self.backend.builder.get_insert_block()?;
    self.backend.builder.build_unconditional_branch(merge_block).ok()?;

    self.backend.builder.position_at_end(else_block);
    let else_val = self.codegen_expr(alternate)?;
    let else_end_block = self.backend.builder.get_insert_block()?;
    self.backend.builder.build_unconditional_branch(merge_block).ok()?;

    self.backend.builder.position_at_end(merge_block);
    if result_kind == ValueKind::Void {
      self.emit_unsupported_type(expr, "conditional expressions of type `void` are not supported");
      return None;
    }
    let phi_ty = self.backend.llvm_type(result_kind);
    let phi = self.backend.builder.build_phi(phi_ty, "condtmp").ok()?;
    phi.add_incoming(&[(&then_val, then_end_block), (&else_val, else_end_block)]);
    Some(phi.as_basic_value())
  }

  fn codegen_call(&mut self, expr: ExprId, call: &CallExpr) -> Option<BasicValueEnum<'ctx>> {
    if call.optional || call.is_new || call.args.iter().any(|arg| arg.spread) {
      self.emit_unsupported_expr(expr, "unsupported call form");
      return None;
    }

    let callee_expr = self.body.exprs.get(call.callee.0 as usize)?;
    let ExprKind::Ident(name) = callee_expr.kind else {
      self.emit_unsupported_expr(expr, "callee must be an identifier");
      return None;
    };
    let Some(symbol) = self.functions.get(&name) else {
      self.emit_unsupported_expr(expr, "unknown callee");
      return None;
    };
    if call.args.len() != symbol.params.len() {
      self.emit_unsupported_expr(expr, "argument count mismatch");
      return None;
    }

    let mut args: Vec<BasicMetadataValueEnum<'ctx>> = Vec::with_capacity(call.args.len());
    for (arg, expected) in call.args.iter().zip(symbol.params.iter().copied()) {
      self.expect_kind(arg.expr, expected)?;
      let value = self.codegen_expr(arg.expr)?;
      args.push(value.into());
    }

    let callsite = self
      .backend
      .builder
      .build_call(symbol.function, &args, "calltmp")
      .ok()?;
    crate::stack_walking::mark_call_notail(callsite);
    callsite.try_as_basic_value().left()
  }
}
