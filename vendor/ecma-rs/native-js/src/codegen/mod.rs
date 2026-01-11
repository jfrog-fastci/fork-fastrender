//! Native code generation backends for `native-js`.
//!
//! This module currently contains:
//! - `emit_llvm_module`: a minimal `parse-js`-driven LLVM IR emitter (used by
//!   `compile_typescript_to_llvm_ir` / the `native-js-cli` binary).
//! - [`codegen`]: an experimental HIR-driven backend used by the typechecked
//!   `native-js` CLI (`native-js-cli --bin native-js`).
//!
//! ## Diagnostic codes
//!
//! The HIR backend emits stable `NJS01xx` codes for codegen failures:
//!
//! - `NJS0100`: failed to access lowered HIR for entry file
//! - `NJS0101`: failed to access lowered HIR for `main` body
//! - `NJS0102`: missing function metadata for `main` body
//! - `NJS0103`: expression id out of bounds
//! - `NJS0104`: numeric literal cannot be represented as a 32-bit integer
//! - `NJS0105`: unsupported unary operator
//! - `NJS0106`: unsupported binary operator
//! - `NJS0107`: unsupported expression / assignment / update operator in `main`
//! - `NJS0112`: statement id out of bounds
//! - `NJS0113`: unsupported statement / variable declaration kind in `main`
//! - `NJS0114`: unknown identifier in `main`
//! - `NJS0115`: not all control-flow paths in `main` return a value
//! - `NJS0116`: `return` without a value is not supported in `main` yet
//! - `NJS0117`: unsupported pattern (expected identifier) / pattern id out of bounds
//! - `NJS0118`: variable declarations must have an initializer
//! - `NJS0119`: labeled `break` is not supported
//! - `NJS0120`: `break` is only supported inside loops
//! - `NJS0121`: labeled `continue` is not supported
//! - `NJS0122`: `continue` is only supported inside loops
//!
//! Entrypoint-related errors are emitted by [`crate::strict::entrypoint`]
//! (`NJS0108..NJS0111`).

use crate::strict::Entrypoint;
use diagnostics::{Diagnostic, Span, TextRange};
use hir_js::{
  AssignOp, BinaryOp, ExprId, ExprKind, ForInit, Literal, NameId, PatId, PatKind, StmtId, StmtKind,
  UnaryOp, UpdateOp, VarDecl, VarDeclKind,
};
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::IntType;
use inkwell::values::{FunctionValue, IntValue, PointerValue};
use inkwell::IntPredicate;
use std::collections::HashMap;
use typecheck_ts::{DefId, FileId, Program, TypeKindSummary};

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
  let i32_ty = context.i32_type();

  let ts_main = declare_ts_main(context, &module, i32_ty);
  let allow_void_return = main_allows_void_return(program, entrypoint.main_def, entry_file)?;
  build_ts_main(
    context,
    i32_ty,
    ts_main,
    hir_body,
    lowered.names.as_ref(),
    entry_file,
    allow_void_return,
  )?;

  let c_main = declare_c_main(context, &module, i32_ty);
  build_c_main(context, c_main, ts_main, i32_ty);

  Ok(module)
}

fn main_allows_void_return(
  program: &Program,
  main_def: DefId,
  entry_file: FileId,
) -> Result<bool, Vec<Diagnostic>> {
  let func_ty = program.type_of_def_interned(main_def);
  let sigs = program.call_signatures(func_ty);
  let Some(sig) = sigs.first() else {
    return Err(vec![Diagnostic::error(
      "NJS0123",
      "failed to resolve call signature for exported `main`",
      Span::new(entry_file, TextRange::new(0, 0)),
    )]);
  };
  let ret_kind = program.type_kind(sig.signature.ret);
  Ok(matches!(
    ret_kind,
    TypeKindSummary::Void | TypeKindSummary::Undefined
  ))
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
  i32_ty: IntType<'ctx>,
  func: FunctionValue<'ctx>,
  hir_body: &hir_js::Body,
  names: &hir_js::NameInterner,
  entry_file: FileId,
  allow_void_return: bool,
) -> Result<(), Vec<Diagnostic>> {
  let builder = context.create_builder();
  let alloca_builder = context.create_builder();

  let entry_bb = context.append_basic_block(func, "entry");
  builder.position_at_end(entry_bb);

  let mut cg = HirCodegen {
    context,
    builder,
    alloca_builder,
    func,
    i32_ty,
    bool_ty: context.bool_type(),
    body: hir_body,
    names,
    entry_file,
    locals: LocalEnv::new(),
    loop_stack: Vec::new(),
    allow_void_return,
  };

  let Some(function_meta) = &hir_body.function else {
    return Err(vec![Diagnostic::error(
      "NJS0102",
      "missing function metadata for `main` body",
      Span::new(entry_file, hir_body.span),
    )]);
  };

  match function_meta.body {
    hir_js::FunctionBody::Expr(expr) => {
      let value = cg.codegen_expr(expr)?;
      let ret = if cg.allow_void_return {
        i32_ty.const_zero()
      } else {
        value
      };
      cg.builder.build_return(Some(&ret)).expect("failed to build return");
    }
    hir_js::FunctionBody::Block(ref stmts) => {
      let mut fallthrough = true;
      for &stmt_id in stmts {
        fallthrough = cg.codegen_stmt(stmt_id)?;
        if !fallthrough {
          break;
        }
      }

      if fallthrough && !cg.allow_void_return {
        return Err(vec![Diagnostic::error(
          "NJS0115",
          "not all control-flow paths in `main` return a value",
          Span::new(entry_file, hir_body.span),
        )]);
      }
      if fallthrough {
        cg.builder
          .build_return(Some(&i32_ty.const_zero()))
          .expect("failed to build implicit return");
      }
    }
  }

  Ok(())
}

fn build_c_main<'ctx>(
  context: &'ctx Context,
  c_main: FunctionValue<'ctx>,
  ts_main: FunctionValue<'ctx>,
  i32_ty: IntType<'ctx>,
) {
  let builder = context.create_builder();
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

struct LocalEnv<'ctx> {
  scopes: Vec<HashMap<NameId, PointerValue<'ctx>>>,
}

impl<'ctx> LocalEnv<'ctx> {
  fn new() -> Self {
    Self {
      scopes: vec![HashMap::new()],
    }
  }

  fn push_scope(&mut self) {
    self.scopes.push(HashMap::new());
  }

  fn pop_scope(&mut self) {
    self.scopes.pop();
    if self.scopes.is_empty() {
      // Internal invariant: the function always has a root scope.
      self.scopes.push(HashMap::new());
    }
  }

  fn insert(&mut self, name: NameId, ptr: PointerValue<'ctx>) {
    if let Some(scope) = self.scopes.last_mut() {
      scope.insert(name, ptr);
    }
  }

  fn lookup(&self, name: NameId) -> Option<PointerValue<'ctx>> {
    for scope in self.scopes.iter().rev() {
      if let Some(ptr) = scope.get(&name).copied() {
        return Some(ptr);
      }
    }
    None
  }
}

#[derive(Clone, Copy)]
struct LoopContext<'ctx> {
  label: Option<NameId>,
  break_bb: BasicBlock<'ctx>,
  continue_bb: BasicBlock<'ctx>,
}

struct HirCodegen<'ctx, 'a> {
  context: &'ctx Context,
  builder: Builder<'ctx>,
  alloca_builder: Builder<'ctx>,
  func: FunctionValue<'ctx>,
  i32_ty: IntType<'ctx>,
  bool_ty: IntType<'ctx>,
  body: &'a hir_js::Body,
  names: &'a hir_js::NameInterner,
  entry_file: FileId,
  locals: LocalEnv<'ctx>,
  loop_stack: Vec<LoopContext<'ctx>>,
  allow_void_return: bool,
}

impl<'ctx, 'a> HirCodegen<'ctx, 'a> {
  fn span_of_stmt(&self, stmt: StmtId) -> Span {
    let range = self
      .body
      .stmts
      .get(stmt.0 as usize)
      .map(|s| s.span)
      .unwrap_or(self.body.span);
    Span::new(self.entry_file, range)
  }

  fn stmt(&self, stmt: StmtId) -> Result<&hir_js::Stmt, Vec<Diagnostic>> {
    self.body.stmts.get(stmt.0 as usize).ok_or_else(|| {
      vec![Diagnostic::error(
        "NJS0112",
        "statement id out of bounds",
        Span::new(self.entry_file, self.body.span),
      )]
    })
  }

  fn expr(&self, expr: ExprId) -> Result<&hir_js::Expr, Vec<Diagnostic>> {
    self.body.exprs.get(expr.0 as usize).ok_or_else(|| {
      vec![Diagnostic::error(
        "NJS0103",
        "expression id out of bounds",
        Span::new(self.entry_file, self.body.span),
      )]
    })
  }

  fn pat(&self, pat: PatId) -> Result<&hir_js::Pat, Vec<Diagnostic>> {
    self.body.pats.get(pat.0 as usize).ok_or_else(|| {
      vec![Diagnostic::error(
        "NJS0117",
        "pattern id out of bounds",
        Span::new(self.entry_file, self.body.span),
      )]
    })
  }

  fn codegen_stmt(&mut self, stmt_id: StmtId) -> Result<bool, Vec<Diagnostic>> {
    let (kind, span) = {
      let stmt = self.stmt(stmt_id)?;
      (stmt.kind.clone(), Span::new(self.entry_file, stmt.span))
    };

    match kind {
      StmtKind::Empty | StmtKind::Debugger => Ok(true),
      StmtKind::Expr(expr) => {
        let _ = self.codegen_expr(expr)?;
        Ok(true)
      }
      StmtKind::Return(Some(expr)) => {
        let value = self.codegen_expr(expr)?;
        let ret = if self.allow_void_return {
          self.i32_ty.const_zero()
        } else {
          value
        };
        self.builder.build_return(Some(&ret)).expect("failed to build return");
        Ok(false)
      }
      StmtKind::Return(None) => {
        if !self.allow_void_return {
          return Err(vec![Diagnostic::error(
            "NJS0116",
            "`return` without a value is not supported in `main` yet",
            span,
          )]);
        }
        self
          .builder
          .build_return(Some(&self.i32_ty.const_zero()))
          .expect("failed to build return");
        Ok(false)
      }
      StmtKind::Block(stmts) => {
        self.locals.push_scope();
        let mut fallthrough = true;
        for stmt_id in stmts {
          fallthrough = self.codegen_stmt(stmt_id)?;
          if !fallthrough {
            break;
          }
        }
        self.locals.pop_scope();
        Ok(fallthrough)
      }
      StmtKind::If {
        test,
        consequent,
        alternate,
      } => self.codegen_if(test, consequent, alternate),
      StmtKind::While { test, body } => self.codegen_while(None, test, body),
      StmtKind::DoWhile { test, body } => self.codegen_do_while(None, test, body),
      StmtKind::For {
        init,
        test,
        update,
        body,
      } => self.codegen_for(None, init.as_ref(), test, update, body),
      StmtKind::Var(decl) => {
        self.codegen_var_decl(&decl, span)?;
        Ok(true)
      }
      StmtKind::Break(label) => self.codegen_break(label, span),
      StmtKind::Continue(label) => self.codegen_continue(label, span),
      StmtKind::Labeled { label, body } => self.codegen_labeled(label, body, span),
      StmtKind::Switch { .. } => Err(vec![Diagnostic::error(
        "NJS0113",
        "`switch` statements are not supported yet",
        span,
      )]),
      StmtKind::Try { .. } => Err(vec![Diagnostic::error(
        "NJS0113",
        "`try` statements are not supported yet",
        span,
      )]),
      StmtKind::Throw(_) => Err(vec![Diagnostic::error(
        "NJS0113",
        "`throw` statements are not supported yet",
        span,
      )]),
      StmtKind::ForIn { .. } => Err(vec![Diagnostic::error(
        "NJS0113",
        "`for-in` / `for-of` loops are not supported yet",
        span,
      )]),
      StmtKind::With { .. } => Err(vec![Diagnostic::error(
        "NJS0113",
        "`with` statements are not supported yet",
        span,
      )]),
      other => Err(vec![Diagnostic::error(
        "NJS0113",
        format!("unsupported statement in `main`: {other:?}"),
        span,
      )]),
    }
  }

  fn codegen_break(&mut self, label: Option<NameId>, span: Span) -> Result<bool, Vec<Diagnostic>> {
    let target = if let Some(label) = label {
      self
        .loop_stack
        .iter()
        .rev()
        .find(|ctx| ctx.label == Some(label))
        .copied()
    } else {
      self.loop_stack.last().copied()
    };
    let Some(ctx) = target else {
      return Err(vec![Diagnostic::error(
        if label.is_some() { "NJS0119" } else { "NJS0120" },
        if let Some(label) = label {
          let lbl = self.names.resolve(label).unwrap_or("<label>");
          format!("unknown loop label `{lbl}` for `break`")
        } else {
          "`break` is only supported inside loops".to_string()
        },
        span,
      )]);
    };
    self
      .builder
      .build_unconditional_branch(ctx.break_bb)
      .expect("failed to build break branch");
    Ok(false)
  }

  fn codegen_continue(&mut self, label: Option<NameId>, span: Span) -> Result<bool, Vec<Diagnostic>> {
    let target = if let Some(label) = label {
      self
        .loop_stack
        .iter()
        .rev()
        .find(|ctx| ctx.label == Some(label))
        .copied()
    } else {
      self.loop_stack.last().copied()
    };
    let Some(ctx) = target else {
      return Err(vec![Diagnostic::error(
        if label.is_some() { "NJS0121" } else { "NJS0122" },
        if let Some(label) = label {
          let lbl = self.names.resolve(label).unwrap_or("<label>");
          format!("unknown loop label `{lbl}` for `continue`")
        } else {
          "`continue` is only supported inside loops".to_string()
        },
        span,
      )]);
    };
    self
      .builder
      .build_unconditional_branch(ctx.continue_bb)
      .expect("failed to build continue branch");
    Ok(false)
  }

  fn codegen_labeled(&mut self, label: NameId, body: StmtId, span: Span) -> Result<bool, Vec<Diagnostic>> {
    let kind = self.stmt(body)?.kind.clone();
    match kind {
      StmtKind::While { test, body } => self.codegen_while(Some(label), test, body),
      StmtKind::DoWhile { test, body } => self.codegen_do_while(Some(label), test, body),
      StmtKind::For {
        init,
        test,
        update,
        body,
      } => self.codegen_for(Some(label), init.as_ref(), test, update, body),
      _ => Err(vec![Diagnostic::error(
        "NJS0124",
        "only labeled loops are supported in native-js codegen",
        span,
      )]),
    }
  }

  fn ensure_entry_alloca(&mut self, name: NameId) -> PointerValue<'ctx> {
    let entry_bb = self
      .func
      .get_first_basic_block()
      .expect("ts_main must have an entry block");
    if let Some(first) = entry_bb.get_first_instruction() {
      self.alloca_builder.position_before(&first);
    } else {
      self.alloca_builder.position_at_end(entry_bb);
    }

    let debug_name = self.names.resolve(name).unwrap_or("local");
    self
      .alloca_builder
      .build_alloca(self.i32_ty, debug_name)
      .expect("failed to build alloca")
  }

  fn bool_to_i32(&self, v: IntValue<'ctx>) -> IntValue<'ctx> {
    self
      .builder
      .build_int_z_extend(v, self.i32_ty, "bool")
      .expect("failed to zext bool")
  }

  fn is_truthy_i1(&self, v: IntValue<'ctx>) -> IntValue<'ctx> {
    self
      .builder
      .build_int_compare(IntPredicate::NE, v, self.i32_ty.const_zero(), "truthy")
      .expect("failed to build truthy compare")
  }

  fn codegen_if(
    &mut self,
    test: ExprId,
    consequent: StmtId,
    alternate: Option<StmtId>,
  ) -> Result<bool, Vec<Diagnostic>> {
    let cond_val = self.codegen_expr(test)?;
    let cond_i1 = self.is_truthy_i1(cond_val);

    let then_bb = self.context.append_basic_block(self.func, "if.then");

    // If there is no alternate, the false branch falls through directly.
    if alternate.is_none() {
      let cont_bb = self.context.append_basic_block(self.func, "if.end");
      self
        .builder
        .build_conditional_branch(cond_i1, then_bb, cont_bb)
        .expect("failed to build conditional branch");

      self.builder.position_at_end(then_bb);
      let then_fallthrough = self.codegen_stmt(consequent)?;
      if then_fallthrough {
        self
          .builder
          .build_unconditional_branch(cont_bb)
          .expect("failed to build branch");
      }

      self.builder.position_at_end(cont_bb);
      return Ok(true);
    }

    let else_bb = self.context.append_basic_block(self.func, "if.else");
    self
      .builder
      .build_conditional_branch(cond_i1, then_bb, else_bb)
      .expect("failed to build conditional branch");

    self.builder.position_at_end(then_bb);
    let then_fallthrough = self.codegen_stmt(consequent)?;

    let mut cont_bb = None;
    if then_fallthrough {
      let bb = self.context.append_basic_block(self.func, "if.end");
      self
        .builder
        .build_unconditional_branch(bb)
        .expect("failed to build branch");
      cont_bb = Some(bb);
    }

    self.builder.position_at_end(else_bb);
    let else_fallthrough = self.codegen_stmt(alternate.expect("checked above"))?;
    if else_fallthrough {
      let bb = cont_bb.unwrap_or_else(|| self.context.append_basic_block(self.func, "if.end"));
      self
        .builder
        .build_unconditional_branch(bb)
        .expect("failed to build branch");
      cont_bb = Some(bb);
    }

    if let Some(cont) = cont_bb {
      self.builder.position_at_end(cont);
      Ok(true)
    } else {
      Ok(false)
    }
  }

  fn codegen_while(&mut self, label: Option<NameId>, test: ExprId, body: StmtId) -> Result<bool, Vec<Diagnostic>> {
    let cond_bb = self.context.append_basic_block(self.func, "while.cond");
    let body_bb = self.context.append_basic_block(self.func, "while.body");
    let end_bb = self.context.append_basic_block(self.func, "while.end");

    self
      .builder
      .build_unconditional_branch(cond_bb)
      .expect("failed to build branch");

    self.builder.position_at_end(cond_bb);
    let cond_val = self.codegen_expr(test)?;
    let cond_i1 = self.is_truthy_i1(cond_val);
    self
      .builder
      .build_conditional_branch(cond_i1, body_bb, end_bb)
      .expect("failed to build conditional branch");

    self.builder.position_at_end(body_bb);
    self.loop_stack.push(LoopContext {
      label,
      break_bb: end_bb,
      continue_bb: cond_bb,
    });
    let body_fallthrough = self.codegen_stmt(body)?;
    if body_fallthrough {
      self
        .builder
        .build_unconditional_branch(cond_bb)
        .expect("failed to build branch");
    }
    self.loop_stack.pop();

    self.builder.position_at_end(end_bb);
    Ok(true)
  }

  fn codegen_do_while(&mut self, label: Option<NameId>, test: ExprId, body: StmtId) -> Result<bool, Vec<Diagnostic>> {
    let body_bb = self.context.append_basic_block(self.func, "do.body");
    let cond_bb = self.context.append_basic_block(self.func, "do.cond");
    let end_bb = self.context.append_basic_block(self.func, "do.end");

    self
      .builder
      .build_unconditional_branch(body_bb)
      .expect("failed to build branch");

    self.loop_stack.push(LoopContext {
      label,
      break_bb: end_bb,
      continue_bb: cond_bb,
    });

    self.builder.position_at_end(body_bb);
    let body_fallthrough = self.codegen_stmt(body)?;
    if body_fallthrough {
      self
        .builder
        .build_unconditional_branch(cond_bb)
        .expect("failed to build branch");
    }

    self.builder.position_at_end(cond_bb);
    let cond_val = self.codegen_expr(test)?;
    let cond_i1 = self.is_truthy_i1(cond_val);
    self
      .builder
      .build_conditional_branch(cond_i1, body_bb, end_bb)
      .expect("failed to build conditional branch");

    self.loop_stack.pop();

    self.builder.position_at_end(end_bb);
    Ok(true)
  }

  fn codegen_for(
    &mut self,
    label: Option<NameId>,
    init: Option<&ForInit>,
    test: Option<ExprId>,
    update: Option<ExprId>,
    body: StmtId,
  ) -> Result<bool, Vec<Diagnostic>> {
    // `for (let i = 0; ... )` introduces a scope for the loop.
    self.locals.push_scope();

    if let Some(init) = init {
      match init {
        ForInit::Expr(expr) => {
          let _ = self.codegen_expr(*expr)?;
        }
        ForInit::Var(decl) => {
          let span = self.span_of_stmt(body);
          self.codegen_var_decl(decl, span)?;
        }
      }
    }

    let cond_bb = self.context.append_basic_block(self.func, "for.cond");
    let body_bb = self.context.append_basic_block(self.func, "for.body");
    let update_bb = self.context.append_basic_block(self.func, "for.update");
    let end_bb = self.context.append_basic_block(self.func, "for.end");

    self
      .builder
      .build_unconditional_branch(cond_bb)
      .expect("failed to build branch");

    self.builder.position_at_end(cond_bb);
    let cond_i1 = if let Some(test) = test {
      let v = self.codegen_expr(test)?;
      self.is_truthy_i1(v)
    } else {
      self.bool_ty.const_int(1, false)
    };
    self
      .builder
      .build_conditional_branch(cond_i1, body_bb, end_bb)
      .expect("failed to build conditional branch");

    self.builder.position_at_end(body_bb);
    self.loop_stack.push(LoopContext {
      label,
      break_bb: end_bb,
      continue_bb: update_bb,
    });
    let body_fallthrough = self.codegen_stmt(body)?;
    if body_fallthrough {
      self
        .builder
        .build_unconditional_branch(update_bb)
        .expect("failed to build branch");
    }
    self.loop_stack.pop();

    self.builder.position_at_end(update_bb);
    if let Some(update) = update {
      let _ = self.codegen_expr(update)?;
    }
    self
      .builder
      .build_unconditional_branch(cond_bb)
      .expect("failed to build branch");

    self.builder.position_at_end(end_bb);
    self.locals.pop_scope();
    Ok(true)
  }

  fn codegen_var_decl(&mut self, decl: &VarDecl, span: Span) -> Result<(), Vec<Diagnostic>> {
    match decl.kind {
      VarDeclKind::Var | VarDeclKind::Let | VarDeclKind::Const => {}
      _ => {
        return Err(vec![Diagnostic::error(
          "NJS0113",
          "unsupported variable declaration kind in native-js codegen",
          span,
        )]);
      }
    }

    for declarator in decl.declarators.iter() {
      let (name, pat_span) = {
        let pat = self.pat(declarator.pat)?;
        let name = match &pat.kind {
          PatKind::Ident(name) => *name,
          _ => {
            return Err(vec![Diagnostic::error(
              "NJS0117",
              "unsupported pattern in variable declaration (expected identifier)",
              Span::new(self.entry_file, pat.span),
            )]);
          }
        };
        (name, pat.span)
      };

      let Some(init) = declarator.init else {
        return Err(vec![Diagnostic::error(
          "NJS0118",
          "variable declarations must have an initializer in native-js codegen",
          Span::new(self.entry_file, pat_span),
        )]);
      };

      let value = self.codegen_expr(init)?;
      let ptr = self.ensure_entry_alloca(name);
      self
        .builder
        .build_store(ptr, value)
        .expect("failed to build store");
      self.locals.insert(name, ptr);
    }
    Ok(())
  }

  fn codegen_expr(&mut self, expr: ExprId) -> Result<IntValue<'ctx>, Vec<Diagnostic>> {
    let (kind, span) = {
      let expr_data = self.expr(expr)?;
      (
        expr_data.kind.clone(),
        Span::new(self.entry_file, expr_data.span),
      )
    };

    match kind {
      ExprKind::TypeAssertion { expr, .. }
      | ExprKind::NonNull { expr }
      | ExprKind::Satisfies { expr, .. } => self.codegen_expr(expr),
      ExprKind::Literal(Literal::Number(raw)) => parse_i32_const(self.i32_ty, &raw).ok_or_else(|| {
        vec![Diagnostic::error(
          "NJS0104",
          format!("unsupported numeric literal `{raw}` (expected 32-bit integer)"),
          span,
        )]
      }),
      ExprKind::Literal(Literal::Boolean(b)) => Ok(self.i32_ty.const_int(u64::from(b), false)),
      ExprKind::Unary { op, expr } => {
        let inner = self.codegen_expr(expr)?;
        match op {
          UnaryOp::Plus => Ok(inner),
          UnaryOp::Minus => Ok(
            self
              .builder
              .build_int_neg(inner, "neg")
              .expect("failed to build negation"),
          ),
          UnaryOp::Not => {
            let is_false = self
              .builder
              .build_int_compare(IntPredicate::EQ, inner, self.i32_ty.const_zero(), "not")
              .expect("failed to build compare");
            Ok(self.bool_to_i32(is_false))
          }
          UnaryOp::BitNot => Ok(self
            .builder
            .build_not(inner, "bitnot")
            .expect("failed to build bitnot")),
          _ => Err(vec![Diagnostic::error(
            "NJS0105",
            format!("unsupported unary operator `{op:?}`"),
            span,
          )]),
        }
      }
      ExprKind::Binary { op, left, right } => {
        let lhs = self.codegen_expr(left)?;
        let rhs = self.codegen_expr(right)?;
        let v = match op {
          BinaryOp::Add => self
            .builder
            .build_int_add(lhs, rhs, "add")
            .expect("failed to build add"),
          BinaryOp::Subtract => self
            .builder
            .build_int_sub(lhs, rhs, "sub")
            .expect("failed to build sub"),
          BinaryOp::Multiply => self
            .builder
            .build_int_mul(lhs, rhs, "mul")
            .expect("failed to build mul"),
          BinaryOp::Divide => self
            .builder
            .build_int_signed_div(lhs, rhs, "div")
            .expect("failed to build div"),
          BinaryOp::Remainder => self
            .builder
            .build_int_signed_rem(lhs, rhs, "rem")
            .expect("failed to build rem"),
          BinaryOp::BitAnd => self.builder.build_and(lhs, rhs, "and").expect("failed to build and"),
          BinaryOp::BitOr => self.builder.build_or(lhs, rhs, "or").expect("failed to build or"),
          BinaryOp::BitXor => self.builder.build_xor(lhs, rhs, "xor").expect("failed to build xor"),
          BinaryOp::ShiftLeft => self
            .builder
            .build_left_shift(lhs, rhs, "shl")
            .expect("failed to build shl"),
          BinaryOp::ShiftRight => self
            .builder
            .build_right_shift(lhs, rhs, true, "shr")
            .expect("failed to build shr"),
          BinaryOp::LessThan
          | BinaryOp::LessEqual
          | BinaryOp::GreaterThan
          | BinaryOp::GreaterEqual
          | BinaryOp::Equality
          | BinaryOp::Inequality
          | BinaryOp::StrictEquality
          | BinaryOp::StrictInequality => {
            let pred = match op {
              BinaryOp::LessThan => IntPredicate::SLT,
              BinaryOp::LessEqual => IntPredicate::SLE,
              BinaryOp::GreaterThan => IntPredicate::SGT,
              BinaryOp::GreaterEqual => IntPredicate::SGE,
              BinaryOp::Equality | BinaryOp::StrictEquality => IntPredicate::EQ,
              BinaryOp::Inequality | BinaryOp::StrictInequality => IntPredicate::NE,
              _ => unreachable!(),
            };
            let cmp = self
              .builder
              .build_int_compare(pred, lhs, rhs, "cmp")
              .expect("failed to build compare");
            self.bool_to_i32(cmp)
          }
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
      ExprKind::Ident(name) => {
        let Some(ptr) = self.locals.lookup(name) else {
          let label = self.names.resolve(name).unwrap_or("<unknown>");
          return Err(vec![Diagnostic::error(
            "NJS0114",
            format!("unknown identifier `{label}` in native-js codegen"),
            span,
          )]);
        };
        Ok(
          self
            .builder
            .build_load(self.i32_ty, ptr, "load")
            .expect("failed to build load")
            .into_int_value(),
        )
      }
      ExprKind::Assignment { op, target, value } => {
        let (name, pat_span) = {
          let pat = self.pat(target)?;
          let name = match &pat.kind {
            PatKind::Ident(name) => *name,
            _ => {
              return Err(vec![Diagnostic::error(
                "NJS0117",
                "unsupported assignment target (expected identifier)",
                Span::new(self.entry_file, pat.span),
              )]);
            }
          };
          (name, pat.span)
        };

        let Some(ptr) = self.locals.lookup(name) else {
          let label = self.names.resolve(name).unwrap_or("<unknown>");
          return Err(vec![Diagnostic::error(
            "NJS0114",
            format!("unknown identifier `{label}` in assignment"),
            Span::new(self.entry_file, pat_span),
          )]);
        };

        let rhs = self.codegen_expr(value)?;
        let out = match op {
          AssignOp::Assign => rhs,
          AssignOp::AddAssign
          | AssignOp::SubAssign
          | AssignOp::MulAssign
          | AssignOp::DivAssign
          | AssignOp::RemAssign => {
            let cur = self
              .builder
              .build_load(self.i32_ty, ptr, "load")
              .expect("failed to build load")
              .into_int_value();
            match op {
              AssignOp::AddAssign => self
                .builder
                .build_int_add(cur, rhs, "addassign")
                .expect("failed to build add"),
              AssignOp::SubAssign => self
                .builder
                .build_int_sub(cur, rhs, "subassign")
                .expect("failed to build sub"),
              AssignOp::MulAssign => self
                .builder
                .build_int_mul(cur, rhs, "mulassign")
                .expect("failed to build mul"),
              AssignOp::DivAssign => self
                .builder
                .build_int_signed_div(cur, rhs, "divassign")
                .expect("failed to build div"),
              AssignOp::RemAssign => self
                .builder
                .build_int_signed_rem(cur, rhs, "remassign")
                .expect("failed to build rem"),
              _ => unreachable!(),
            }
          }
          _ => {
            return Err(vec![Diagnostic::error(
              "NJS0107",
              format!("unsupported assignment operator `{op:?}`"),
              span,
            )]);
          }
        };

        self
          .builder
          .build_store(ptr, out)
          .expect("failed to build store");
        Ok(out)
      }
      ExprKind::Update { op, expr, prefix } => {
        let (name, target_span) = {
          let inner = self.expr(expr)?;
          let name = match &inner.kind {
            ExprKind::Ident(name) => *name,
            _ => {
              return Err(vec![Diagnostic::error(
                "NJS0107",
                "unsupported update target (expected identifier)",
                Span::new(self.entry_file, inner.span),
              )]);
            }
          };
          (name, inner.span)
        };
        let Some(ptr) = self.locals.lookup(name) else {
          let label = self.names.resolve(name).unwrap_or("<unknown>");
          return Err(vec![Diagnostic::error(
            "NJS0114",
            format!("unknown identifier `{label}` in update"),
            Span::new(self.entry_file, target_span),
          )]);
        };

        let old = self
          .builder
          .build_load(self.i32_ty, ptr, "load")
          .expect("failed to build load")
          .into_int_value();
        let one = self.i32_ty.const_int(1, false);
        let new = match op {
          UpdateOp::Increment => self
            .builder
            .build_int_add(old, one, "inc")
            .expect("failed to build inc"),
          UpdateOp::Decrement => self
            .builder
            .build_int_sub(old, one, "dec")
            .expect("failed to build dec"),
        };
        self
          .builder
          .build_store(ptr, new)
          .expect("failed to build store");
        Ok(if prefix { new } else { old })
      }
      _ => Err(vec![Diagnostic::error(
        "NJS0107",
        "unsupported expression in `main`",
        span,
      )]),
    }
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
pub mod safepoint;
pub(crate) mod llvm;

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
