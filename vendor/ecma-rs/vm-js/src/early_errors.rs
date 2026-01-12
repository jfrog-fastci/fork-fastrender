use crate::error::VmError;
use diagnostics::{Diagnostic, FileId};
use parse_js::ast::class_or_object::{
  ClassMember, ClassOrObjKey, ClassOrObjVal, ObjMember, ObjMemberType,
};
use parse_js::ast::expr::lit::{LitArrElem, LitTemplatePart};
use parse_js::ast::expr::pat::{ArrPat, ArrPatElem, ClassOrFuncName, ObjPat, ObjPatProp, Pat};
use parse_js::ast::expr::{
  ArrowFuncExpr, BinaryExpr, CallArg, CallExpr, ClassExpr, ComputedMemberExpr, CondExpr, Expr,
  FuncExpr, ImportExpr, MemberExpr, TaggedTemplateExpr, UnaryExpr, UnaryPostfixExpr,
};
use parse_js::ast::func::{Func, FuncBody};
use parse_js::ast::node::{Node, ParenthesizedExpr};
use parse_js::ast::stmt::decl::{ClassDecl, FuncDecl, ParamDecl, VarDecl};
use parse_js::ast::stmt::{
  BlockStmt, CatchBlock, ForBody, ForInOfLhs, ForInStmt, ForOfStmt, ForTripleStmt, ForTripleStmtInit,
  ContinueStmt, IfStmt, LabelStmt, ReturnStmt, Stmt, SwitchBranch, SwitchStmt, TryStmt, WhileStmt,
  WithStmt,
};
use parse_js::loc::Loc;
use parse_js::operator::OperatorName;
use parse_js::token::TT;
use std::collections::HashMap;

const EARLY_ERROR_CODE: &str = "VMJS0004";

fn is_restricted_identifier(name: &str) -> bool {
  // Restricted identifiers (ECMA-262 `IsRestrictedIdentifier`) are early errors in strict mode.
  name == "eval" || name == "arguments"
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct EarlyErrorOptions {
  pub(crate) strict: bool,
  pub(crate) allow_top_level_await: bool,
}

impl EarlyErrorOptions {
  pub(crate) fn script(strict: bool) -> Self {
    Self {
      strict,
      allow_top_level_await: false,
    }
  }

  pub(crate) fn module() -> Self {
    Self {
      strict: true,
      allow_top_level_await: true,
    }
  }
}

pub(crate) fn validate_top_level<F>(
  stmts: &[Node<Stmt>],
  opts: EarlyErrorOptions,
  tick: &mut F,
) -> Result<(), VmError>
where
  F: FnMut() -> Result<(), VmError>,
{
  let diags = collect_top_level(stmts, opts, tick)?;
  if diags.is_empty() {
    Ok(())
  } else {
    Err(VmError::Syntax(diags))
  }
}

pub(crate) fn collect_top_level<F>(
  stmts: &[Node<Stmt>],
  opts: EarlyErrorOptions,
  tick: &mut F,
) -> Result<Vec<Diagnostic>, VmError>
where
  F: FnMut() -> Result<(), VmError>,
{
  let mut walker = EarlyErrorWalker::new(tick);
  let mut ctx = ControlContext {
    strict: opts.strict,
    await_allowed: opts.allow_top_level_await,
    yield_allowed: false,
    super_call_allowed: false,
    return_allowed: false,
    loop_depth: 0,
    breakable_depth: 0,
    labels: Vec::new(),
  };
  walker.visit_stmt_list(&mut ctx, stmts)?;
  Ok(walker.diags)
}

struct LabelInfo {
  name: String,
  is_iteration: bool,
}

struct ControlContext {
  strict: bool,
  /// Whether `await` expressions are permitted in the current context.
  ///
  /// This is true at module top-level (top-level await) and inside async functions.
  await_allowed: bool,
  /// Whether `yield` expressions are permitted in the current context.
  ///
  /// This is true only inside generator function bodies.
  yield_allowed: bool,
  /// Whether `super()` calls are permitted in the current context.
  ///
  /// `super()` is only valid in derived class constructors (and arrow functions lexically nested
  /// within those constructors).
  super_call_allowed: bool,
  /// Whether `return` statements are permitted in the current statement list.
  ///
  /// This is true only inside function bodies. (Notably, class static blocks are **not** function
  /// bodies.)
  return_allowed: bool,
  loop_depth: u32,
  breakable_depth: u32,
  labels: Vec<LabelInfo>,
}

struct SavedFunctionContext {
  strict: bool,
  await_allowed: bool,
  yield_allowed: bool,
  super_call_allowed: bool,
  return_allowed: bool,
  loop_depth: u32,
  breakable_depth: u32,
  labels: Vec<LabelInfo>,
}

struct SavedScopeFlags {
  strict: bool,
  super_call_allowed: bool,
  return_allowed: bool,
}

struct EarlyErrorWalker<'a, F: FnMut() -> Result<(), VmError>> {
  tick: &'a mut F,
  steps: u32,
  diags: Vec<Diagnostic>,
}

impl<'a, F: FnMut() -> Result<(), VmError>> EarlyErrorWalker<'a, F> {
  fn new(tick: &'a mut F) -> Self {
    Self {
      tick,
      steps: 0,
      diags: Vec::new(),
    }
  }

  fn step(&mut self) -> Result<(), VmError> {
    self.steps = self.steps.wrapping_add(1);
    if self.steps % 256 == 0 {
      (self.tick)()?;
    }
    Ok(())
  }

  fn push_error(&mut self, loc: Loc, message: impl Into<String>) -> Result<(), VmError> {
    self.diags.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    let span = loc.to_diagnostics_span(FileId(0));
    self
      .diags
      .push(Diagnostic::error(EARLY_ERROR_CODE, message, span));
    Ok(())
  }

  fn detect_use_strict_directive(&mut self, stmts: &[Node<Stmt>]) -> Result<bool, VmError> {
    for stmt in stmts {
      self.step()?;
      let Stmt::Expr(expr_stmt) = &*stmt.stx else {
        break;
      };
      let expr = &expr_stmt.stx.expr;
      if expr.assoc.get::<ParenthesizedExpr>().is_some() {
        break;
      }
      let Expr::LitStr(lit) = &*expr.stx else {
        break;
      };
      if lit.stx.value == "use strict" {
        return Ok(true);
      }
    }
    Ok(false)
  }

  fn is_iteration_statement(stmt: &Node<Stmt>) -> bool {
    match &*stmt.stx {
      Stmt::While(_) | Stmt::DoWhile(_) | Stmt::ForIn(_) | Stmt::ForOf(_) | Stmt::ForTriple(_) => {
        true
      }
      Stmt::Label(label) => Self::is_iteration_statement(&label.stx.statement),
      _ => false,
    }
  }

  fn save_and_enter_function(
    &mut self,
    ctx: &mut ControlContext,
    strict: bool,
    await_allowed: bool,
    yield_allowed: bool,
    super_call_allowed: bool,
  ) -> SavedFunctionContext {
    let saved = SavedFunctionContext {
      strict: ctx.strict,
      await_allowed: ctx.await_allowed,
      yield_allowed: ctx.yield_allowed,
      super_call_allowed: ctx.super_call_allowed,
      return_allowed: ctx.return_allowed,
      loop_depth: ctx.loop_depth,
      breakable_depth: ctx.breakable_depth,
      labels: std::mem::take(&mut ctx.labels),
    };
    ctx.strict = strict;
    ctx.await_allowed = await_allowed;
    ctx.yield_allowed = yield_allowed;
    ctx.super_call_allowed = super_call_allowed;
    ctx.return_allowed = true;
    ctx.loop_depth = 0;
    ctx.breakable_depth = 0;
    ctx.labels.clear();
    saved
  }

  fn restore_function(&mut self, ctx: &mut ControlContext, saved: SavedFunctionContext) {
    ctx.strict = saved.strict;
    ctx.await_allowed = saved.await_allowed;
    ctx.yield_allowed = saved.yield_allowed;
    ctx.super_call_allowed = saved.super_call_allowed;
    ctx.return_allowed = saved.return_allowed;
    ctx.loop_depth = saved.loop_depth;
    ctx.breakable_depth = saved.breakable_depth;
    ctx.labels = saved.labels;
  }

  fn save_scope_flags(&self, ctx: &ControlContext) -> SavedScopeFlags {
    SavedScopeFlags {
      strict: ctx.strict,
      super_call_allowed: ctx.super_call_allowed,
      return_allowed: ctx.return_allowed,
    }
  }

  fn restore_scope_flags(&self, ctx: &mut ControlContext, saved: SavedScopeFlags) {
    ctx.strict = saved.strict;
    ctx.super_call_allowed = saved.super_call_allowed;
    ctx.return_allowed = saved.return_allowed;
  }

  fn visit_stmt_list(&mut self, ctx: &mut ControlContext, stmts: &[Node<Stmt>]) -> Result<(), VmError> {
    for stmt in stmts {
      self.visit_stmt(ctx, stmt)?;
    }
    Ok(())
  }

  fn visit_stmt(&mut self, ctx: &mut ControlContext, stmt: &Node<Stmt>) -> Result<(), VmError> {
    self.step()?;
    match &*stmt.stx {
      Stmt::Block(block) => self.visit_block(ctx, &block.stx),
      Stmt::Break(b) => self.visit_break(ctx, stmt.loc, &b.stx),
      Stmt::Continue(c) => self.visit_continue(ctx, stmt.loc, &c.stx),
      Stmt::DoWhile(do_while) => self.visit_do_while(ctx, &do_while.stx),
      Stmt::Empty(_) | Stmt::Debugger(_) => Ok(()),
      Stmt::Expr(expr_stmt) => self.visit_expr(ctx, &expr_stmt.stx.expr),
      Stmt::ForIn(for_in) => self.visit_for_in(ctx, &for_in.stx),
      Stmt::ForOf(for_of) => self.visit_for_of(ctx, stmt.loc, &for_of.stx),
      Stmt::ForTriple(for_triple) => self.visit_for_triple(ctx, &for_triple.stx),
      Stmt::If(if_stmt) => self.visit_if(ctx, &if_stmt.stx),
      Stmt::Label(label) => self.visit_label(ctx, stmt.loc, &label.stx),
      Stmt::Return(ret) => self.visit_return(ctx, stmt.loc, &ret.stx),
      Stmt::Switch(sw) => self.visit_switch(ctx, &sw.stx),
      Stmt::Throw(th) => self.visit_expr(ctx, &th.stx.value),
      Stmt::Try(try_stmt) => self.visit_try(ctx, &try_stmt.stx),
      Stmt::While(while_stmt) => self.visit_while(ctx, &while_stmt.stx),
      Stmt::With(with_stmt) => self.visit_with(ctx, stmt.loc, &with_stmt.stx),

      Stmt::ClassDecl(class) => self.visit_class_decl(ctx, &class.stx),
      Stmt::FunctionDecl(func) => self.visit_func_decl(ctx, &func.stx),
      Stmt::VarDecl(var) => self.visit_var_decl(ctx, &var.stx),

      // Module-only statement forms; still traverse nested expressions where present.
      Stmt::ExportDefaultExpr(export) => self.visit_expr(ctx, &export.stx.expression),
      Stmt::ExportList(export) => {
        if let Some(attrs) = &export.stx.attributes {
          self.visit_expr(ctx, attrs)?;
        }
        Ok(())
      }
      Stmt::Import(import) => {
        if let Some(attrs) = &import.stx.attributes {
          self.visit_expr(ctx, attrs)?;
        }
        Ok(())
      }

      // TypeScript statement variants are not expected in dialect=Ecma.
      _ => Ok(()),
    }
  }

  fn visit_block(&mut self, ctx: &mut ControlContext, block: &BlockStmt) -> Result<(), VmError> {
    self.visit_stmt_list(ctx, &block.body)
  }

  fn visit_if(&mut self, ctx: &mut ControlContext, stmt: &IfStmt) -> Result<(), VmError> {
    self.visit_expr(ctx, &stmt.test)?;
    self.visit_stmt(ctx, &stmt.consequent)?;
    if let Some(alt) = &stmt.alternate {
      self.visit_stmt(ctx, alt)?;
    }
    Ok(())
  }

  fn visit_try(&mut self, ctx: &mut ControlContext, stmt: &TryStmt) -> Result<(), VmError> {
    self.visit_stmt_list(ctx, &stmt.wrapped.stx.body)?;
    if let Some(catch) = &stmt.catch {
      self.visit_catch(ctx, &catch.stx)?;
    }
    if let Some(finally) = &stmt.finally {
      self.visit_stmt_list(ctx, &finally.stx.body)?;
    }
    Ok(())
  }

  fn visit_catch(&mut self, ctx: &mut ControlContext, catch: &CatchBlock) -> Result<(), VmError> {
    // Catch binding patterns can contain identifier bindings that are restricted in strict mode
    // (e.g. `catch (eval) {}`) and destructuring defaults that may contain invalid `await`.
    if let Some(param) = &catch.parameter {
      self.visit_pat(ctx, &param.stx.pat, PatRole::Binding)?;
    }
    self.visit_stmt_list(ctx, &catch.body)
  }

  fn visit_while(&mut self, ctx: &mut ControlContext, stmt: &WhileStmt) -> Result<(), VmError> {
    self.visit_expr(ctx, &stmt.condition)?;
    ctx.loop_depth = ctx.loop_depth.saturating_add(1);
    ctx.breakable_depth = ctx.breakable_depth.saturating_add(1);
    let result = self.visit_stmt(ctx, &stmt.body);
    ctx.loop_depth = ctx.loop_depth.saturating_sub(1);
    ctx.breakable_depth = ctx.breakable_depth.saturating_sub(1);
    result
  }

  fn visit_do_while(
    &mut self,
    ctx: &mut ControlContext,
    stmt: &parse_js::ast::stmt::DoWhileStmt,
  ) -> Result<(), VmError> {
    // Body is executed before condition, but early errors are purely structural.
    ctx.loop_depth = ctx.loop_depth.saturating_add(1);
    ctx.breakable_depth = ctx.breakable_depth.saturating_add(1);
    self.visit_stmt(ctx, &stmt.body)?;
    ctx.loop_depth = ctx.loop_depth.saturating_sub(1);
    ctx.breakable_depth = ctx.breakable_depth.saturating_sub(1);
    self.visit_expr(ctx, &stmt.condition)
  }

  fn visit_for_triple(
    &mut self,
    ctx: &mut ControlContext,
    stmt: &ForTripleStmt,
  ) -> Result<(), VmError> {
    match &stmt.init {
      ForTripleStmtInit::None => {}
      ForTripleStmtInit::Expr(expr) => self.visit_expr(ctx, expr)?,
      ForTripleStmtInit::Decl(decl) => self.visit_var_decl(ctx, &decl.stx)?,
    }
    if let Some(cond) = &stmt.cond {
      self.visit_expr(ctx, cond)?;
    }
    if let Some(post) = &stmt.post {
      self.visit_expr(ctx, post)?;
    }
    ctx.loop_depth = ctx.loop_depth.saturating_add(1);
    ctx.breakable_depth = ctx.breakable_depth.saturating_add(1);
    let result = self.visit_for_body(ctx, &stmt.body.stx);
    ctx.loop_depth = ctx.loop_depth.saturating_sub(1);
    ctx.breakable_depth = ctx.breakable_depth.saturating_sub(1);
    result
  }

  fn visit_for_in(&mut self, ctx: &mut ControlContext, stmt: &ForInStmt) -> Result<(), VmError> {
    self.visit_for_in_of_lhs(ctx, &stmt.lhs)?;
    self.visit_expr(ctx, &stmt.rhs)?;
    ctx.loop_depth = ctx.loop_depth.saturating_add(1);
    ctx.breakable_depth = ctx.breakable_depth.saturating_add(1);
    let result = self.visit_for_body(ctx, &stmt.body.stx);
    ctx.loop_depth = ctx.loop_depth.saturating_sub(1);
    ctx.breakable_depth = ctx.breakable_depth.saturating_sub(1);
    result
  }

  fn visit_for_of(
    &mut self,
    ctx: &mut ControlContext,
    loc: Loc,
    stmt: &ForOfStmt,
  ) -> Result<(), VmError> {
    if stmt.await_ && !ctx.await_allowed {
      self.push_error(loc, "for-await-of is only valid in async functions and modules")?;
    }
    self.visit_for_in_of_lhs(ctx, &stmt.lhs)?;
    self.visit_expr(ctx, &stmt.rhs)?;
    ctx.loop_depth = ctx.loop_depth.saturating_add(1);
    ctx.breakable_depth = ctx.breakable_depth.saturating_add(1);
    let result = self.visit_for_body(ctx, &stmt.body.stx);
    ctx.loop_depth = ctx.loop_depth.saturating_sub(1);
    ctx.breakable_depth = ctx.breakable_depth.saturating_sub(1);
    result
  }

  fn visit_for_body(&mut self, ctx: &mut ControlContext, body: &ForBody) -> Result<(), VmError> {
    self.visit_stmt_list(ctx, &body.body)
  }

  fn visit_for_in_of_lhs(
    &mut self,
    ctx: &mut ControlContext,
    lhs: &ForInOfLhs,
  ) -> Result<(), VmError> {
    match lhs {
      ForInOfLhs::Assign(pat) => self.visit_assignment_target_pat(ctx, pat),
      ForInOfLhs::Decl((_mode, pat)) => {
        // Binding patterns: walk for nested patterns/defaults (if any).
        self.visit_pat(ctx, &pat.stx.pat, PatRole::Binding)
      }
    }
  }

  fn visit_switch(&mut self, ctx: &mut ControlContext, stmt: &SwitchStmt) -> Result<(), VmError> {
    self.visit_expr(ctx, &stmt.test)?;
    ctx.breakable_depth = ctx.breakable_depth.saturating_add(1);
    for branch in &stmt.branches {
      self.visit_switch_branch(ctx, &branch.stx)?;
    }
    ctx.breakable_depth = ctx.breakable_depth.saturating_sub(1);
    Ok(())
  }

  fn visit_switch_branch(
    &mut self,
    ctx: &mut ControlContext,
    branch: &SwitchBranch,
  ) -> Result<(), VmError> {
    if let Some(case) = &branch.case {
      self.visit_expr(ctx, case)?;
    }
    self.visit_stmt_list(ctx, &branch.body)
  }

  fn visit_label(&mut self, ctx: &mut ControlContext, loc: Loc, stmt: &LabelStmt) -> Result<(), VmError> {
    let is_iteration = Self::is_iteration_statement(&stmt.statement);
    if ctx.labels.iter().any(|l| l.name == stmt.name) {
      self.push_error(loc, format!("duplicate label '{}'", stmt.name))?;
    }
    ctx.labels.push(LabelInfo {
      name: stmt.name.clone(),
      is_iteration,
    });
    let res = self.visit_stmt(ctx, &stmt.statement);
    ctx.labels.pop();
    res
  }

  fn visit_return(
    &mut self,
    ctx: &mut ControlContext,
    loc: Loc,
    stmt: &ReturnStmt,
  ) -> Result<(), VmError> {
    if !ctx.return_allowed {
      self.push_error(loc, "return statement must be inside a function body")?;
    }
    if let Some(value) = &stmt.value {
      self.visit_expr(ctx, value)?;
    }
    Ok(())
  }

  fn visit_break(&mut self, ctx: &mut ControlContext, loc: Loc, stmt: &parse_js::ast::stmt::BreakStmt) -> Result<(), VmError> {
    match stmt.label.as_ref() {
      Some(label) => {
        if !ctx.labels.iter().any(|l| l.name == *label) {
          self.push_error(loc, format!("undefined label '{label}'"))?;
        }
      }
      None => {
        if ctx.breakable_depth == 0 {
          self.push_error(loc, "break statement must be inside a loop or switch")?;
        }
      }
    }
    Ok(())
  }

  fn visit_continue(
    &mut self,
    ctx: &mut ControlContext,
    loc: Loc,
    stmt: &ContinueStmt,
  ) -> Result<(), VmError> {
    match stmt.label.as_ref() {
      Some(label) => {
        if !ctx
          .labels
          .iter()
          .any(|l| l.name == *label && l.is_iteration)
        {
          self.push_error(loc, format!("undefined loop label '{label}'"))?;
        }
      }
      None => {
        if ctx.loop_depth == 0 {
          self.push_error(loc, "continue statement must be inside a loop")?;
        }
      }
    }
    Ok(())
  }

  fn visit_with(&mut self, ctx: &mut ControlContext, loc: Loc, stmt: &WithStmt) -> Result<(), VmError> {
    if ctx.strict {
      self.push_error(loc, "with statements are not allowed in strict mode")?;
    }
    self.visit_expr(ctx, &stmt.object)?;
    self.visit_stmt(ctx, &stmt.body)
  }

  fn visit_class_decl(&mut self, ctx: &mut ControlContext, decl: &ClassDecl) -> Result<(), VmError> {
    if ctx.strict {
      if let Some(name) = &decl.name {
        if is_restricted_identifier(&name.stx.name) {
          self.push_error(
            name.loc,
            format!("restricted identifier '{}' is not allowed in strict mode", name.stx.name),
          )?;
        }
      }
    }
    if let Some(extends) = &decl.extends {
      self.visit_expr(ctx, extends)?;
    }
    self.visit_class_members(ctx, &decl.members, decl.extends.is_some())
  }

  fn visit_class_expr(&mut self, ctx: &mut ControlContext, expr: &ClassExpr) -> Result<(), VmError> {
    if ctx.strict {
      if let Some(name) = &expr.name {
        if is_restricted_identifier(&name.stx.name) {
          self.push_error(
            name.loc,
            format!("restricted identifier '{}' is not allowed in strict mode", name.stx.name),
          )?;
        }
      }
    }
    if let Some(extends) = &expr.extends {
      self.visit_expr(ctx, extends)?;
    }
    self.visit_class_members(ctx, &expr.members, expr.extends.is_some())
  }

  fn visit_class_members(
    &mut self,
    ctx: &mut ControlContext,
    members: &[Node<ClassMember>],
    derived: bool,
  ) -> Result<(), VmError> {
    // Class bodies are always strict mode code.
    let saved = self.save_scope_flags(ctx);
    ctx.strict = true;
    // Do not allow `super()` to leak into nested classes (e.g. from derived constructors), since
    // `super()` is only valid within derived constructor bodies.
    ctx.super_call_allowed = false;

    for member in members {
      self.visit_class_member(ctx, &member.stx, derived)?;
    }

    self.restore_scope_flags(ctx, saved);
    Ok(())
  }

  fn visit_class_member(
    &mut self,
    ctx: &mut ControlContext,
    member: &ClassMember,
    derived: bool,
  ) -> Result<(), VmError> {
    self.visit_class_or_obj_key(ctx, &member.key)?;
    match &member.val {
      ClassOrObjVal::Getter(getter) => self.visit_func(
        ctx,
        None,
        &getter.stx.func,
        /* unique */ true,
        /* super_call_allowed */ false,
      ),
      ClassOrObjVal::Setter(setter) => self.visit_func(
        ctx,
        None,
        &setter.stx.func,
        /* unique */ true,
        /* super_call_allowed */ false,
      ),
      ClassOrObjVal::Method(method) => {
        let is_constructor = !member.static_
          && matches!(
            &member.key,
            ClassOrObjKey::Direct(key)
              if key.stx.key == "constructor" && key.stx.tt == TT::KeywordConstructor
          );
        self.visit_func(
          ctx,
          None,
          &method.stx.func,
          /* unique */ true,
          /* super_call_allowed */ derived && is_constructor,
        )
      }
      ClassOrObjVal::Prop(Some(expr)) => self.visit_expr(ctx, expr),
      ClassOrObjVal::Prop(None) => Ok(()),
      ClassOrObjVal::StaticBlock(block) => self.visit_class_static_block(ctx, &block.stx.body),
      // TypeScript-only members ignored here.
      _ => Ok(()),
    }
  }

  fn visit_class_static_block(
    &mut self,
    ctx: &mut ControlContext,
    stmts: &[Node<Stmt>],
  ) -> Result<(), VmError> {
    // Static initialization blocks introduce early-error boundaries:
    // - `return` is always invalid (they are not function bodies),
    // - `await` is always invalid (even inside async functions or modules),
    // - `yield` is always invalid (even inside generator functions),
    // - `break`/`continue` target resolution must not cross static-block boundaries.
    //
    // Spec tests:
    // - language/statements/class/static-init-invalid-{await,yield,return}.js
    // - language/statements/{break,continue}/static-init-*.js
    let saved = self.save_and_enter_function(
      ctx,
      /* strict */ true,
      /* await_allowed */ false,
      /* yield_allowed */ false,
      /* super_call_allowed */ false,
    );
    ctx.return_allowed = false;
    let res = self.visit_stmt_list(ctx, stmts);
    self.restore_function(ctx, saved);
    res
  }

  fn visit_class_or_obj_key(
    &mut self,
    ctx: &mut ControlContext,
    key: &ClassOrObjKey,
  ) -> Result<(), VmError> {
    match key {
      ClassOrObjKey::Direct(_) => Ok(()),
      ClassOrObjKey::Computed(expr) => self.visit_expr(ctx, expr),
    }
  }

  fn visit_func_decl(&mut self, ctx: &mut ControlContext, decl: &FuncDecl) -> Result<(), VmError> {
    self.visit_func(
      ctx,
      decl.name.as_ref(),
      &decl.function,
      /* unique */ false,
      /* super_call_allowed */ false,
    )
  }

  fn visit_var_decl(&mut self, ctx: &mut ControlContext, decl: &VarDecl) -> Result<(), VmError> {
    for declarator in &decl.declarators {
      // Destructuring `var`/`let` declarations require an initializer (early error).
      //
      // Note: `for (var {x} in obj)` / `for (let {x} of iter)` are valid because the binding
      // pattern is parsed as `ForInOfLhs::Decl` (not a `VarDecl` with an omitted initializer).
      if declarator.initializer.is_none()
        && !matches!(&*declarator.pattern.stx.pat.stx, Pat::Id(_))
      {
        self.push_error(
          declarator.pattern.loc,
          "Missing initializer in destructuring declaration",
        )?;
      }
      self.visit_pat(ctx, &declarator.pattern.stx.pat, PatRole::Binding)?;
      if let Some(expr) = &declarator.initializer {
        self.visit_expr(ctx, expr)?;
      }
    }
    Ok(())
  }

  fn is_simple_parameter_list(params: &[Node<ParamDecl>]) -> bool {
    params.iter().all(|p| {
      !p.stx.rest
        && p.stx.default_value.is_none()
        && matches!(&*p.stx.pattern.stx.pat.stx, Pat::Id(_))
    })
  }

  fn collect_bound_names_from_pat(pat: &Node<Pat>, out: &mut Vec<(String, Loc)>) {
    match &*pat.stx {
      Pat::Id(id) => out.push((id.stx.name.clone(), pat.loc)),
      Pat::Arr(arr) => {
        for elem in &arr.stx.elements {
          let Some(elem) = elem else { continue };
          Self::collect_bound_names_from_pat(&elem.target, out);
        }
        if let Some(rest) = &arr.stx.rest {
          Self::collect_bound_names_from_pat(rest, out);
        }
      }
      Pat::Obj(obj) => {
        for prop in &obj.stx.properties {
          Self::collect_bound_names_from_pat(&prop.stx.target, out);
        }
        if let Some(rest) = &obj.stx.rest {
          Self::collect_bound_names_from_pat(rest, out);
        }
      }
      Pat::AssignTarget(_) => {
        // Assignment targets are not binding patterns; ignore.
      }
    }
  }

  fn visit_func(
    &mut self,
    ctx: &mut ControlContext,
    name: Option<&Node<ClassOrFuncName>>,
    func: &Node<Func>,
    unique_formals: bool,
    super_call_allowed: bool,
  ) -> Result<(), VmError> {
    self.step()?;

    let params = &func.stx.parameters;
    for (idx, param) in params.iter().enumerate() {
      if param.stx.rest && idx + 1 != params.len() {
        self.push_error(param.loc, "rest parameter must be last")?;
      }
    }

    let body_strict = match &func.stx.body {
      Some(FuncBody::Block(stmts)) => self.detect_use_strict_directive(stmts)?,
      _ => false,
    };

    // `eval` and `arguments` are restricted identifiers in strict mode (ES5 strict).
    let func_strict = ctx.strict || body_strict;
    if func_strict {
      if let Some(name) = name {
        if is_restricted_identifier(&name.stx.name) {
          self.push_error(
            name.loc,
            format!("restricted identifier '{}' is not allowed in strict mode", name.stx.name),
          )?;
        }
      }
    }

    let simple = Self::is_simple_parameter_list(params);
    // It's a syntax error for a function with a non-simple parameter list to contain a `"use strict"`
    // directive (ECMA-262 `ContainsUseStrict` / `IsSimpleParameterList` early errors).
    //
    // This applies even if the function is already strict due to its surrounding context (e.g.
    // class bodies are always strict mode code).
    if body_strict && !simple {
      self.push_error(
        func.loc,
        "Illegal 'use strict' directive in function with non-simple parameter list.",
      )?;
    }

    let disallow_duplicates = unique_formals || func_strict || !simple || func.stx.arrow;
    if disallow_duplicates {
      let mut seen: HashMap<String, Loc> = HashMap::new();
      for param in params {
        self.step()?;
        let mut names: Vec<(String, Loc)> = Vec::new();
        Self::collect_bound_names_from_pat(&param.stx.pattern.stx.pat, &mut names);
        for (name, loc) in names {
          if let Some(_first) = seen.get(&name) {
            self.push_error(loc, format!("duplicate parameter name '{name}'"))?;
          } else {
            seen.insert(name, loc);
          }
        }
      }
    }

    // Enter the function context when traversing parameter initializers and the function body.
    let saved = self.save_and_enter_function(
      ctx,
      func_strict,
      func.stx.async_,
      func.stx.generator,
      super_call_allowed,
    );

    for param in params {
      self.visit_pat(ctx, &param.stx.pattern.stx.pat, PatRole::Binding)?;
      if let Some(default_value) = &param.stx.default_value {
        self.visit_expr(ctx, default_value)?;
      }
    }

    match &func.stx.body {
      Some(FuncBody::Block(stmts)) => self.visit_stmt_list(ctx, stmts)?,
      Some(FuncBody::Expression(expr)) => self.visit_expr(ctx, expr)?,
      None => {}
    }

    self.restore_function(ctx, saved);
    Ok(())
  }

  fn visit_expr(&mut self, ctx: &mut ControlContext, expr: &Node<Expr>) -> Result<(), VmError> {
    self.step()?;
    match &*expr.stx {
      Expr::ArrowFunc(arrow) => self.visit_arrow_func_expr(ctx, &arrow.stx),
      Expr::Binary(binary) => self.visit_binary(ctx, &binary.stx),
      Expr::Call(call) => self.visit_call(ctx, &call.stx),
      Expr::Class(class) => self.visit_class_expr(ctx, &class.stx),
      Expr::ComputedMember(member) => self.visit_computed_member(ctx, &member.stx),
      Expr::Cond(cond) => self.visit_cond(ctx, &cond.stx),
      Expr::Func(func) => self.visit_func_expr(ctx, &func.stx),
      Expr::Import(import) => self.visit_import(ctx, &import.stx),
      Expr::Member(member) => self.visit_member(ctx, &member.stx),
      Expr::TaggedTemplate(tagged) => self.visit_tagged_template(ctx, &tagged.stx),
      Expr::Unary(unary) => self.visit_unary(ctx, &unary.stx, expr.loc),
      Expr::UnaryPostfix(unary) => self.visit_unary_postfix(ctx, &unary.stx, expr.loc),

      // Literals/patterns that contain nested expressions.
      Expr::LitArr(arr) => self.visit_lit_arr(ctx, &arr.stx),
      Expr::LitObj(obj) => self.visit_lit_obj(ctx, &obj.stx.members),
      Expr::LitTemplate(template) => self.visit_lit_template(ctx, &template.stx.parts),
      Expr::ObjPat(obj) => self.visit_obj_pat(ctx, &obj.stx, PatRole::Assignment),
      Expr::ArrPat(arr) => self.visit_arr_pat(ctx, &arr.stx, PatRole::Assignment),

      // Leaves (or TS-only nodes) are ignored for this early error set.
      _ => Ok(()),
    }
  }

  fn visit_arrow_func_expr(&mut self, ctx: &mut ControlContext, expr: &ArrowFuncExpr) -> Result<(), VmError> {
    self.visit_func(
      ctx,
      None,
      &expr.func,
      /* unique */ true,
      /* super_call_allowed */ ctx.super_call_allowed,
    )
  }

  fn visit_func_expr(&mut self, ctx: &mut ControlContext, expr: &FuncExpr) -> Result<(), VmError> {
    self.visit_func(
      ctx,
      expr.name.as_ref(),
      &expr.func,
      /* unique */ false,
      /* super_call_allowed */ false,
    )
  }

  fn visit_import(&mut self, ctx: &mut ControlContext, expr: &ImportExpr) -> Result<(), VmError> {
    self.visit_expr(ctx, &expr.module)?;
    if let Some(attrs) = &expr.attributes {
      self.visit_expr(ctx, attrs)?;
    }
    Ok(())
  }

  fn visit_cond(&mut self, ctx: &mut ControlContext, expr: &CondExpr) -> Result<(), VmError> {
    self.visit_expr(ctx, &expr.test)?;
    self.visit_expr(ctx, &expr.consequent)?;
    self.visit_expr(ctx, &expr.alternate)
  }

  fn visit_member(&mut self, ctx: &mut ControlContext, expr: &MemberExpr) -> Result<(), VmError> {
    self.visit_expr(ctx, &expr.left)
  }

  fn visit_computed_member(
    &mut self,
    ctx: &mut ControlContext,
    expr: &ComputedMemberExpr,
  ) -> Result<(), VmError> {
    self.visit_expr(ctx, &expr.object)?;
    self.visit_expr(ctx, &expr.member)
  }

  fn visit_call(&mut self, ctx: &mut ControlContext, expr: &CallExpr) -> Result<(), VmError> {
    if matches!(&*expr.callee.stx, Expr::Super(_)) && !ctx.super_call_allowed {
      self.push_error(
        expr.callee.loc,
        "super() is only valid in derived class constructors",
      )?;
    }
    self.visit_expr(ctx, &expr.callee)?;
    for arg in &expr.arguments {
      self.visit_call_arg(ctx, &arg.stx)?;
    }
    Ok(())
  }

  fn visit_call_arg(&mut self, ctx: &mut ControlContext, arg: &CallArg) -> Result<(), VmError> {
    let _ = arg.spread;
    self.visit_expr(ctx, &arg.value)
  }

  fn optional_chain_in_assignment_target_expr(expr: &Node<Expr>) -> Option<Loc> {
    match &*expr.stx {
      Expr::Member(member) => {
        if member.stx.optional_chaining {
          Some(expr.loc)
        } else {
          Self::optional_chain_in_assignment_target_expr(&member.stx.left)
        }
      }
      Expr::ComputedMember(member) => {
        if member.stx.optional_chaining {
          Some(expr.loc)
        } else {
          Self::optional_chain_in_assignment_target_expr(&member.stx.object)
        }
      }
      Expr::Call(call) => {
        if call.stx.optional_chaining {
          Some(expr.loc)
        } else {
          Self::optional_chain_in_assignment_target_expr(&call.stx.callee)
        }
      }
      _ => None,
    }
  }

  fn visit_binary(&mut self, ctx: &mut ControlContext, expr: &BinaryExpr) -> Result<(), VmError> {
    if expr.operator.is_assignment() {
      // `eval = ...` and `arguments = ...` are strict-mode early errors.
      if ctx.strict {
        match &*expr.left.stx {
          Expr::Id(id) if is_restricted_identifier(&id.stx.name) => {
            self.push_error(
              expr.left.loc,
              format!("cannot assign to '{}' in strict mode", id.stx.name),
            )?;
          }
          Expr::IdPat(id) if is_restricted_identifier(&id.stx.name) => {
            self.push_error(
              expr.left.loc,
              format!("cannot assign to '{}' in strict mode", id.stx.name),
            )?;
          }
          _ => {}
        }
      }

      // Optional chaining is a static early error in assignment targets.
      if matches!(&*expr.left.stx, Expr::ArrPat(_) | Expr::ObjPat(_)) {
        // Destructuring patterns handle optional chaining only in `Pat::AssignTarget` positions.
      } else if let Some(loc) = Self::optional_chain_in_assignment_target_expr(&expr.left) {
        self.push_error(loc, "optional chaining cannot appear in assignment targets")?;
      }
    }

    self.visit_expr(ctx, &expr.left)?;
    self.visit_expr(ctx, &expr.right)
  }

  fn visit_unary(
    &mut self,
    ctx: &mut ControlContext,
    expr: &UnaryExpr,
    loc: Loc,
  ) -> Result<(), VmError> {
    match expr.operator {
      OperatorName::Await => {
        if !ctx.await_allowed {
          self.push_error(loc, "await is only valid in async functions and modules")?;
        }
      }
      OperatorName::Yield | OperatorName::YieldDelegated => {
        if !ctx.yield_allowed {
          self.push_error(loc, "yield is only valid in generator functions")?;
        }
      }
      OperatorName::Delete => {
        // `delete IdentifierReference` is a strict mode early error (ECMA-262 14.5.1 / 13.5.1.1).
        //
        // The evaluator also checks this at runtime as a safety net, but spec-compliant behavior
        // requires rejecting such code before any statements in the containing Script/FunctionBody
        // execute (so earlier side effects do not occur).
        if ctx.strict {
          match &*expr.argument.stx {
            Expr::Id(_) | Expr::IdPat(_) => {
              self.push_error(
                expr.argument.loc,
                "Delete of an unqualified identifier in strict mode.",
              )?;
            }
            _ => {}
          }
        }
      }
      OperatorName::PrefixIncrement | OperatorName::PrefixDecrement => {
        if ctx.strict {
          match &*expr.argument.stx {
            Expr::Id(id) if is_restricted_identifier(&id.stx.name) => {
              self.push_error(
                expr.argument.loc,
                format!("cannot assign to '{}' in strict mode", id.stx.name),
              )?;
            }
            Expr::IdPat(id) if is_restricted_identifier(&id.stx.name) => {
              self.push_error(
                expr.argument.loc,
                format!("cannot assign to '{}' in strict mode", id.stx.name),
              )?;
            }
            _ => {}
          }
        }
        if let Some(loc) = Self::optional_chain_in_assignment_target_expr(&expr.argument) {
          self.push_error(loc, "optional chaining cannot appear in assignment targets")?;
        }
      }
      _ => {}
    }

    self.visit_expr(ctx, &expr.argument)
  }

  fn visit_unary_postfix(
    &mut self,
    ctx: &mut ControlContext,
    expr: &UnaryPostfixExpr,
    loc: Loc,
  ) -> Result<(), VmError> {
    match expr.operator {
      OperatorName::PostfixIncrement | OperatorName::PostfixDecrement => {
        if ctx.strict {
          match &*expr.argument.stx {
            Expr::Id(id) if is_restricted_identifier(&id.stx.name) => {
              self.push_error(
                expr.argument.loc,
                format!("cannot assign to '{}' in strict mode", id.stx.name),
              )?;
            }
            Expr::IdPat(id) if is_restricted_identifier(&id.stx.name) => {
              self.push_error(
                expr.argument.loc,
                format!("cannot assign to '{}' in strict mode", id.stx.name),
              )?;
            }
            _ => {}
          }
        }
        if let Some(loc) = Self::optional_chain_in_assignment_target_expr(&expr.argument) {
          self.push_error(loc, "optional chaining cannot appear in assignment targets")?;
        }
      }
      _ => {}
    }
    self.visit_expr(ctx, &expr.argument)?;
    let _ = loc;
    Ok(())
  }

  fn visit_tagged_template(
    &mut self,
    ctx: &mut ControlContext,
    expr: &TaggedTemplateExpr,
  ) -> Result<(), VmError> {
    self.visit_expr(ctx, &expr.function)?;
    for part in &expr.parts {
      let LitTemplatePart::Substitution(sub) = part else {
        continue;
      };
      self.visit_expr(ctx, sub)?;
    }
    Ok(())
  }

  fn visit_lit_template(
    &mut self,
    ctx: &mut ControlContext,
    parts: &[LitTemplatePart],
  ) -> Result<(), VmError> {
    for part in parts {
      let LitTemplatePart::Substitution(sub) = part else {
        continue;
      };
      self.visit_expr(ctx, sub)?;
    }
    Ok(())
  }

  fn visit_lit_arr(&mut self, ctx: &mut ControlContext, arr: &parse_js::ast::expr::lit::LitArrExpr) -> Result<(), VmError> {
    for elem in &arr.elements {
      match elem {
        LitArrElem::Single(expr) | LitArrElem::Rest(expr) => self.visit_expr(ctx, expr)?,
        LitArrElem::Empty => {}
      }
    }
    Ok(())
  }

  fn visit_lit_obj(&mut self, ctx: &mut ControlContext, members: &[Node<ObjMember>]) -> Result<(), VmError> {
    for member in members {
      match &member.stx.typ {
        ObjMemberType::Valued { key, val } => {
          self.visit_class_or_obj_key(ctx, key)?;
          match val {
            ClassOrObjVal::Getter(getter) => self.visit_func(
              ctx,
              None,
              &getter.stx.func,
              /* unique */ true,
              /* super_call_allowed */ false,
            )?,
            ClassOrObjVal::Setter(setter) => self.visit_func(
              ctx,
              None,
              &setter.stx.func,
              /* unique */ true,
              /* super_call_allowed */ false,
            )?,
            ClassOrObjVal::Method(method) => self.visit_func(
              ctx,
              None,
              &method.stx.func,
              /* unique */ true,
              /* super_call_allowed */ false,
            )?,
            ClassOrObjVal::Prop(Some(expr)) => self.visit_expr(ctx, expr)?,
            ClassOrObjVal::Prop(None) => {}
            // Static blocks not valid in object literals; ignore others.
            _ => {}
          }
        }
        ObjMemberType::Shorthand { .. } => {}
        ObjMemberType::Rest { val } => self.visit_expr(ctx, val)?,
      }
    }
    Ok(())
  }

  fn visit_assignment_target_pat(
    &mut self,
    ctx: &mut ControlContext,
    pat: &Node<Pat>,
  ) -> Result<(), VmError> {
    self.visit_pat(ctx, pat, PatRole::AssignmentTarget)
  }

  fn visit_arr_pat(
    &mut self,
    ctx: &mut ControlContext,
    pat: &ArrPat,
    role: PatRole,
  ) -> Result<(), VmError> {
    for elem in &pat.elements {
      let Some(elem) = elem else { continue };
      self.visit_arr_pat_elem(ctx, elem, role)?;
    }
    if let Some(rest) = &pat.rest {
      self.visit_pat(ctx, rest, role)?;
    }
    Ok(())
  }

  fn visit_arr_pat_elem(
    &mut self,
    ctx: &mut ControlContext,
    elem: &ArrPatElem,
    role: PatRole,
  ) -> Result<(), VmError> {
    self.visit_pat(ctx, &elem.target, role)?;
    if let Some(default) = &elem.default_value {
      self.visit_expr(ctx, default)?;
    }
    Ok(())
  }

  fn visit_obj_pat(
    &mut self,
    ctx: &mut ControlContext,
    pat: &ObjPat,
    role: PatRole,
  ) -> Result<(), VmError> {
    for prop in &pat.properties {
      self.visit_obj_pat_prop(ctx, &prop.stx, role)?;
    }
    if let Some(rest) = &pat.rest {
      self.visit_pat(ctx, rest, role)?;
    }
    Ok(())
  }

  fn visit_obj_pat_prop(
    &mut self,
    ctx: &mut ControlContext,
    prop: &ObjPatProp,
    role: PatRole,
  ) -> Result<(), VmError> {
    self.visit_class_or_obj_key(ctx, &prop.key)?;
    self.visit_pat(ctx, &prop.target, role)?;
    if let Some(default) = &prop.default_value {
      self.visit_expr(ctx, default)?;
    }
    Ok(())
  }

  fn visit_pat(&mut self, ctx: &mut ControlContext, pat: &Node<Pat>, role: PatRole) -> Result<(), VmError> {
    self.step()?;
    match &*pat.stx {
      Pat::Arr(arr) => self.visit_arr_pat(ctx, &arr.stx, role),
      Pat::Id(id) => {
        if ctx.strict && is_restricted_identifier(&id.stx.name) {
          self.push_error(
            pat.loc,
            format!("restricted identifier '{}' is not allowed in strict mode", id.stx.name),
          )?;
        }
        Ok(())
      }
      Pat::Obj(obj) => self.visit_obj_pat(ctx, &obj.stx, role),
      Pat::AssignTarget(expr) => {
        if matches!(role, PatRole::AssignmentTarget | PatRole::Assignment) {
          if let Some(loc) = Self::optional_chain_in_assignment_target_expr(expr) {
            self.push_error(loc, "optional chaining cannot appear in assignment targets")?;
          }
        }
        self.visit_expr(ctx, expr)
      }
    }
  }
}

#[derive(Clone, Copy, Debug)]
enum PatRole {
  /// A binding pattern (function params, `let`/`const` declarations, etc).
  Binding,
  /// A destructuring assignment pattern (`({a} = b)`).
  Assignment,
  /// An assignment target pattern position (e.g. for-in/of LHS).
  AssignmentTarget,
}
