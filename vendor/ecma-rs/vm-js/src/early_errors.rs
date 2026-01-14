use crate::error::VmError;
use crate::fallible_format::try_format_error_message;
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
use parse_js::ast::import_export::ImportNames;
use parse_js::ast::func::{Func, FuncBody};
use parse_js::ast::node::{Node, ParenthesizedExpr};
use parse_js::ast::stmt::decl::{ClassDecl, FuncDecl, ParamDecl, PatDecl, VarDecl, VarDeclMode};
use parse_js::ast::stmt::{
  BlockStmt, CatchBlock, ContinueStmt, ForBody, ForInOfLhs, ForInStmt, ForOfStmt, ForTripleStmt,
  ForTripleStmtInit, IfStmt, LabelStmt, ReturnStmt, Stmt, SwitchBranch, SwitchStmt, TryStmt,
  WhileStmt, WithStmt,
};
use parse_js::loc::Loc;
use parse_js::operator::OperatorName;
use parse_js::token::TT;
use std::collections::{HashMap, HashSet};

const EARLY_ERROR_CODE: &str = "VMJS0004";

#[inline]
fn try_clone_string(value: &str) -> Result<String, VmError> {
  let mut out = String::new();
  out
    .try_reserve_exact(value.len())
    .map_err(|_| VmError::OutOfMemory)?;
  out.push_str(value);
  Ok(out)
}

fn is_restricted_identifier(name: &str) -> bool {
  // Restricted identifiers (ECMA-262 `IsRestrictedIdentifier`) are early errors in strict mode.
  name == "eval" || name == "arguments"
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct EarlyErrorOptions {
  pub(crate) strict: bool,
  pub(crate) allow_top_level_await: bool,
  pub(crate) is_module: bool,
  pub(crate) allow_super_call: bool,
  pub(crate) allow_super_property: bool,
}

impl EarlyErrorOptions {
  pub(crate) fn script(strict: bool) -> Self {
    Self {
      strict,
      allow_top_level_await: false,
      is_module: false,
      allow_super_call: false,
      allow_super_property: false,
    }
  }

  pub(crate) fn script_with_top_level_await(strict: bool, allow_top_level_await: bool) -> Self {
    Self {
      strict,
      allow_top_level_await,
      is_module: false,
      allow_super_call: false,
      allow_super_property: false,
    }
  }

  pub(crate) fn script_with_super_call(
    strict: bool,
    allow_super_call: bool,
    allow_super_property: bool,
  ) -> Self {
    Self {
      strict,
      allow_top_level_await: false,
      is_module: false,
      allow_super_call,
      allow_super_property,
    }
  }

  pub(crate) fn module() -> Self {
    Self {
      strict: true,
      allow_top_level_await: true,
      is_module: true,
      allow_super_call: false,
      allow_super_property: false,
    }
  }
}

pub(crate) fn validate_top_level<F>(
  stmts: &[Node<Stmt>],
  opts: EarlyErrorOptions,
  source: Option<&str>,
  tick: &mut F,
) -> Result<(), VmError>
where
  F: FnMut() -> Result<(), VmError>,
{
  let diags = collect_top_level_with_enclosing_private_names(stmts, opts, source, None, tick)?;
  if diags.is_empty() {
    Ok(())
  } else {
    Err(VmError::Syntax(diags))
  }
}

pub(crate) fn collect_top_level<F>(
  stmts: &[Node<Stmt>],
  opts: EarlyErrorOptions,
  source: Option<&str>,
  tick: &mut F,
) -> Result<Vec<Diagnostic>, VmError>
where
  F: FnMut() -> Result<(), VmError>,
{
  collect_top_level_with_enclosing_private_names(stmts, opts, source, None, tick)
}

pub(crate) fn validate_top_level_with_enclosing_private_names<F>(
  stmts: &[Node<Stmt>],
  opts: EarlyErrorOptions,
  source: Option<&str>,
  enclosing_private_names: Option<HashSet<String>>,
  tick: &mut F,
) -> Result<(), VmError>
where
  F: FnMut() -> Result<(), VmError>,
{
  let diags =
    collect_top_level_with_enclosing_private_names(stmts, opts, source, enclosing_private_names, tick)?;
  if diags.is_empty() {
    Ok(())
  } else {
    Err(VmError::Syntax(diags))
  }
}

pub(crate) fn collect_top_level_with_enclosing_private_names<F>(
  stmts: &[Node<Stmt>],
  opts: EarlyErrorOptions,
  source: Option<&str>,
  enclosing_private_names: Option<HashSet<String>>,
  tick: &mut F,
) -> Result<Vec<Diagnostic>, VmError>
where
  F: FnMut() -> Result<(), VmError>,
{
  let mut walker = EarlyErrorWalker::new(source, tick);
  let mut ctx = ControlContext {
    strict: opts.strict,
    await_allowed: opts.allow_top_level_await,
    is_module: opts.is_module,
    yield_allowed: false,
    await_is_reserved: opts.allow_top_level_await,
    yield_is_reserved: false,
    super_call_allowed: opts.allow_super_call,
    super_property_allowed: opts.allow_super_property,
    arguments_allowed: true,
    return_allowed: false,
    using_allowed: opts.is_module,
    loop_depth: 0,
    breakable_depth: 0,
    labels: Vec::new(),
    private_names: Vec::new(),
  };
  if let Some(private_names) = enclosing_private_names {
    if !private_names.is_empty() {
      ctx
        .private_names
        .try_reserve(1)
        .map_err(|_| VmError::OutOfMemory)?;
      ctx.private_names.push(private_names);
    }
  }
  walker.visit_stmt_list(&mut ctx, StmtListKind::VarScope, stmts)?;
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
  /// Whether we're validating code parsed with the module grammar (`SourceType::Module`).
  ///
  /// Some syntax forms are valid only in modules (e.g. `import.meta`).
  is_module: bool,
  /// Whether `yield` expressions are permitted in the current context.
  ///
  /// This is true only inside generator function bodies.
  yield_allowed: bool,
  /// Whether `await` is reserved as an identifier in the current syntactic context.
  ///
  /// This is true:
  /// - at module top-level,
  /// - inside async function bodies and parameter lists, and
  /// - inside class static initialization blocks.
  ///
  /// Note: This is **not** equivalent to [`ControlContext::await_allowed`]. For example, class static
  /// blocks treat `await` as a reserved identifier even when `await` expressions are not permitted
  /// in the surrounding context.
  await_is_reserved: bool,
  /// Whether `yield` is reserved as an identifier due to being inside a generator function.
  ///
  /// In addition to this flag, strict mode also reserves `yield` as an identifier.
  ///
  /// Note: This is **not** equivalent to [`ControlContext::yield_allowed`]. For example, generator
  /// function parameter lists temporarily set `yield_allowed = false` to enforce
  /// `ContainsYieldExpression` early errors, but `yield` remains a reserved identifier throughout
  /// the parameter list.
  yield_is_reserved: bool,
  /// Whether `super()` calls are permitted in the current context.
  ///
  /// `super()` is only valid in derived class constructors (and arrow functions lexically nested
  /// within those constructors).
  super_call_allowed: bool,
  /// Whether `super.prop`/`super[expr]` are permitted in the current context.
  ///
  /// Super property accesses are valid only in contexts that have a `[[HomeObject]]` (class
  /// methods/constructors, object literal methods/accessors, class field initializers, and class
  /// static blocks), and in arrow functions lexically nested within those contexts.
  super_property_allowed: bool,
  /// Whether `arguments` identifier references are permitted in the current context.
  ///
  /// Class field initializer expressions and class static initialization blocks disallow
  /// `arguments` (ECMA-262 `ContainsArguments` early error). This restriction is lexical, so arrow
  /// functions inherit this flag from their surrounding context.
  arguments_allowed: bool,
  /// Whether `return` statements are permitted in the current statement list.
  ///
  /// This is true only inside function bodies. (Notably, class static blocks are **not** function
  /// bodies.)
  return_allowed: bool,
  /// Whether `using` / `await using` declarations are permitted in the current context.
  ///
  /// In classic scripts, `using` declarations are early errors unless they are contained within
  /// specific syntactic containers (Blocks, for-statements, function bodies, etc). We model this
  /// by toggling `using_allowed` as the walker enters and exits those containers.
  ///
  /// This flag is also used to enforce container-specific restrictions such as switch-clause
  /// statement lists.
  using_allowed: bool,
  loop_depth: u32,
  breakable_depth: u32,
  labels: Vec<LabelInfo>,
  /// Stack of declared private names for the innermost active class body.
  ///
  /// Private names are lexically scoped and can be referenced from nested classes/functions.
  /// Nested classes push an additional private-name set but do not discard outer sets, so
  /// validation consults the full stack when resolving a private identifier.
  private_names: Vec<HashSet<String>>,
}

struct SavedFunctionContext {
  strict: bool,
  await_allowed: bool,
  yield_allowed: bool,
  await_is_reserved: bool,
  yield_is_reserved: bool,
  super_call_allowed: bool,
  super_property_allowed: bool,
  arguments_allowed: bool,
  return_allowed: bool,
  using_allowed: bool,
  loop_depth: u32,
  breakable_depth: u32,
  labels: Vec<LabelInfo>,
}

struct SavedScopeFlags {
  strict: bool,
  super_call_allowed: bool,
  super_property_allowed: bool,
  return_allowed: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct PrivateNameDeclState {
  /// Whether this private name has been declared as a static or instance element.
  ///
  /// ECMA-262 does not permit the same private name to be used for both static and instance
  /// elements within a single class body.
  is_static: Option<bool>,
  has_getter: bool,
  has_setter: bool,
  has_other: bool,
}

struct EarlyErrorWalker<'a, F: FnMut() -> Result<(), VmError>> {
  source: Option<&'a str>,
  tick: &'a mut F,
  steps: u32,
  diags: Vec<Diagnostic>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StmtListKind {
  /// A statement list that forms a var scope (ScriptBody / FunctionBody).
  VarScope,
  /// A statement list that forms a block-like lexical scope (Block / Catch / etc).
  BlockLike,
  /// A `CaseClause` / `DefaultClause` StatementList within a switch.
  SwitchClause,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LexicalNameKind {
  OrdinaryFunction,
  NonOrdinaryFunction,
  Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FuncNameKind {
  /// A function declaration name, which is bound in the surrounding scope.
  Decl,
  /// A named function expression name, which is bound within the function itself.
  Expr,
}

impl<'a, F: FnMut() -> Result<(), VmError>> EarlyErrorWalker<'a, F> {
  fn new(source: Option<&'a str>, tick: &'a mut F) -> Self {
    Self {
      source,
      tick,
      steps: 0,
      diags: Vec::new(),
    }
  }

  fn step(&mut self) -> Result<(), VmError> {
    self.steps = self.steps.wrapping_add(1);
    (self.tick)()?;
    Ok(())
  }

  fn push_error(&mut self, loc: Loc, message: impl Into<String>) -> Result<(), VmError> {
    self
      .diags
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    let span = loc.to_diagnostics_span(FileId(0));
    self
      .diags
      .push(Diagnostic::error(EARLY_ERROR_CODE, message, span));
    Ok(())
  }

  fn validate_reserved_identifier(
    &mut self,
    ctx: &ControlContext,
    loc: Loc,
    name: &str,
  ) -> Result<(), VmError> {
    self.validate_reserved_identifier_flags(
      ctx.strict,
      ctx.is_module,
      ctx.await_is_reserved,
      ctx.yield_is_reserved,
      loc,
      name,
    )
  }

  fn validate_reserved_identifier_flags(
    &mut self,
    strict: bool,
    is_module: bool,
    await_is_reserved: bool,
    yield_is_reserved: bool,
    loc: Loc,
    name: &str,
  ) -> Result<(), VmError> {
    let reserved = match name {
      // `yield` is reserved in strict mode code and within generator bodies.
      "yield" => strict || yield_is_reserved,
      // `await` is reserved in:
      // - Module code (everywhere, even inside non-async nested functions),
      // - async function bodies/params, and
      // - class static initialization blocks.
      "await" => is_module || await_is_reserved,
      // ES strict mode reserved words (web legacy / ES5 strict compatibility).
      // See also `parse-js` `Parser::is_strict_mode_reserved_word`.
      "implements"
      | "interface"
      | "let"
      | "package"
      | "private"
      | "protected"
      | "public"
      | "static" => strict,
      _ => false,
    };
    if reserved {
      let message = try_format_error_message("invalid use of reserved word '", name, "'")?;
      self.push_error(loc, message)?;
    }
    Ok(())
  }

  fn validate_declared_private_name(
    &mut self,
    ctx: &ControlContext,
    loc: Loc,
    name: &str,
  ) -> Result<(), VmError> {
    // Private identifiers are lexically scoped and can be referenced from nested classes/functions
    // as long as some enclosing class body declared the name.
    let ok = ctx.private_names.iter().rev().any(|names| names.contains(name));
    if !ok {
      self.push_error(loc, "invalid private name")?;
    }
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
        if let Some(source) = self.source {
          let start = expr.loc.0.min(source.len());
          let end = expr.loc.1.min(source.len());
          let raw = source.get(start..end).unwrap_or("");
          if raw == "\"use strict\"" || raw == "'use strict'" {
            return Ok(true);
          }
        } else {
          return Ok(true);
        }
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
    await_is_reserved: bool,
    yield_is_reserved: bool,
    super_call_allowed: bool,
    super_property_allowed: bool,
    arguments_allowed: bool,
  ) -> SavedFunctionContext {
    let saved = SavedFunctionContext {
      strict: ctx.strict,
      await_allowed: ctx.await_allowed,
      yield_allowed: ctx.yield_allowed,
      await_is_reserved: ctx.await_is_reserved,
      yield_is_reserved: ctx.yield_is_reserved,
      super_call_allowed: ctx.super_call_allowed,
      super_property_allowed: ctx.super_property_allowed,
      arguments_allowed: ctx.arguments_allowed,
      return_allowed: ctx.return_allowed,
      using_allowed: ctx.using_allowed,
      loop_depth: ctx.loop_depth,
      breakable_depth: ctx.breakable_depth,
      labels: std::mem::take(&mut ctx.labels),
    };
    ctx.strict = strict;
    ctx.await_allowed = await_allowed;
    ctx.yield_allowed = yield_allowed;
    ctx.await_is_reserved = await_is_reserved;
    ctx.yield_is_reserved = yield_is_reserved;
    ctx.super_call_allowed = super_call_allowed;
    ctx.super_property_allowed = super_property_allowed;
    ctx.arguments_allowed = arguments_allowed;
    ctx.return_allowed = true;
    ctx.using_allowed = true;
    ctx.loop_depth = 0;
    ctx.breakable_depth = 0;
    ctx.labels.clear();
    saved
  }

  fn restore_function(&mut self, ctx: &mut ControlContext, saved: SavedFunctionContext) {
    ctx.strict = saved.strict;
    ctx.await_allowed = saved.await_allowed;
    ctx.yield_allowed = saved.yield_allowed;
    ctx.await_is_reserved = saved.await_is_reserved;
    ctx.yield_is_reserved = saved.yield_is_reserved;
    ctx.super_call_allowed = saved.super_call_allowed;
    ctx.super_property_allowed = saved.super_property_allowed;
    ctx.arguments_allowed = saved.arguments_allowed;
    ctx.return_allowed = saved.return_allowed;
    ctx.using_allowed = saved.using_allowed;
    ctx.loop_depth = saved.loop_depth;
    ctx.breakable_depth = saved.breakable_depth;
    ctx.labels = saved.labels;
  }

  fn save_scope_flags(&self, ctx: &ControlContext) -> SavedScopeFlags {
    SavedScopeFlags {
      strict: ctx.strict,
      super_call_allowed: ctx.super_call_allowed,
      super_property_allowed: ctx.super_property_allowed,
      return_allowed: ctx.return_allowed,
    }
  }

  fn restore_scope_flags(&self, ctx: &mut ControlContext, saved: SavedScopeFlags) {
    ctx.strict = saved.strict;
    ctx.super_call_allowed = saved.super_call_allowed;
    ctx.super_property_allowed = saved.super_property_allowed;
    ctx.return_allowed = saved.return_allowed;
  }

  fn visit_stmt_list(
    &mut self,
    ctx: &mut ControlContext,
    kind: StmtListKind,
    stmts: &[Node<Stmt>],
  ) -> Result<(), VmError> {
    let saved_using_allowed = ctx.using_allowed;
    match kind {
      StmtListKind::VarScope => {}
      // Block-like statement lists are valid `using` containers.
      StmtListKind::BlockLike => {
        ctx.using_allowed = true;
      }
      // Switch clause statement lists are valid `using` containers (a `switch` CaseBlock forms a
      // single lexical scope for all clauses).
      StmtListKind::SwitchClause => {
        ctx.using_allowed = true;
      }
    }

    self.check_declaration_early_errors_in_stmt_list(ctx, kind, stmts)?;
    for stmt in stmts {
      self.visit_stmt(ctx, stmt)?;
    }
    ctx.using_allowed = saved_using_allowed;
    Ok(())
  }

  fn check_declaration_early_errors_in_stmt_list(
    &mut self,
    ctx: &ControlContext,
    kind: StmtListKind,
    stmts: &[Node<Stmt>],
  ) -> Result<(), VmError> {
    // Mirror the minimal declaration early errors performed during instantiation:
    // - Duplicate lexical declarations (let/const/class) in the same statement list.
    // - Lexical declarations may not collide with var-scoped names (var + function declarations).
    //
    // These must be caught during `validate_top_level` so dynamic parsing contexts (`Function(...)`,
    // `%GeneratorFunction%`) reject invalid bodies at construction time, rather than deferring until
    // first execution (which can surface as a non-catchable `VmError::Syntax`).
    let mut var_names = HashSet::<String>::new();
    for stmt in stmts {
      self.collect_var_names(&stmt.stx, &mut var_names)?;
    }

    // `VarDeclaredNames` depends on the kind of statement list we're validating:
    // - ScriptBody/FunctionBody: function declarations are var-scoped (and in non-strict code,
    //   Annex B extends this to some nested block functions).
    // - Block-like lists (Block/Catch/switch clause bodies): function declarations are lexically
    //   scoped, so they must *not* be included here (otherwise they would always conflict with
    //   themselves when we include them in `LexicallyDeclaredNames` below).
    if kind == StmtListKind::VarScope {
      if ctx.strict {
        // Strict mode: only top-level function declarations are var-scoped; block function
        // declarations are instantiated at block entry.
        for stmt in stmts {
          self.step()?;
          let Stmt::FunctionDecl(decl) = &*stmt.stx else {
            continue;
          };
          let Some(name) = &decl.stx.name else {
            // `export default function() {}` is parsed as an anonymous function declaration with an
            // engine-internal `*default*` binding created during module linking. It does not
            // participate in var-scoped name collision checks.
            if decl.stx.export_default {
              continue;
            }
            self.push_error(stmt.loc, "anonymous function declaration")?;
            continue;
          };
          let name_str = name.stx.name.as_str();
          if !var_names.contains(name_str) {
            var_names.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
            var_names.insert(try_clone_string(name_str)?);
          }
        }
      } else {
        // Non-strict mode: treat block function declarations as var-scoped (Annex B-ish).
        for stmt in stmts {
          self.collect_sloppy_function_decl_names(&stmt.stx, &mut var_names)?;
        }
      }
    }

    let mut lexical_seen = HashMap::<String, LexicalNameKind>::new();
    for stmt in stmts {
      self.step()?;
      match &*stmt.stx {
        Stmt::VarDecl(var)
          if matches!(
            var.stx.mode,
            VarDeclMode::Let | VarDeclMode::Const | VarDeclMode::Using | VarDeclMode::AwaitUsing
          ) =>
        {
          for declarator in &var.stx.declarators {
            self.step()?;
            self.collect_lexical_decl_names_from_pat(
              ctx,
              &declarator.pattern.stx.pat,
              stmt.loc,
              &mut lexical_seen,
              &var_names,
            )?;
          }
        }
        Stmt::ClassDecl(class) => {
          let Some(name) = class.stx.name.as_ref() else {
            continue;
          };
          self.insert_lexical_name(
            ctx,
            &name.stx.name,
            LexicalNameKind::Other,
            name.loc,
            &mut lexical_seen,
            &var_names,
          )?;
        }
        Stmt::FunctionDecl(decl) if matches!(kind, StmtListKind::BlockLike | StmtListKind::SwitchClause) => {
          let Some(name) = decl.stx.name.as_ref() else {
            if decl.stx.export_default {
              continue;
            }
            self.push_error(stmt.loc, "anonymous function declaration")?;
            continue;
          };
          let func = &decl.stx.function.stx;
          let kind = if func.async_ || func.generator {
            LexicalNameKind::NonOrdinaryFunction
          } else {
            LexicalNameKind::OrdinaryFunction
          };
          self.insert_lexical_name(
            ctx,
            &name.stx.name,
            kind,
            name.loc,
            &mut lexical_seen,
            &var_names,
          )?;
        }
        _ => {}
      }
    }

    Ok(())
  }

  fn check_declaration_early_errors_in_switch_case_block(
    &mut self,
    ctx: &ControlContext,
    branches: &[Node<SwitchBranch>],
  ) -> Result<(), VmError> {
    // `CaseBlock` forms a single lexical environment for all clause bodies, so declaration early
    // errors must be checked across *all* branches (not per-branch).
    let mut var_names = HashSet::<String>::new();
    const BRANCH_STEP_EVERY: usize = 32;
    for (i, branch) in branches.iter().enumerate() {
      if i % BRANCH_STEP_EVERY == 0 {
        self.step()?;
      }
      for stmt in &branch.stx.body {
        self.collect_var_names(&stmt.stx, &mut var_names)?;
      }
    }

    let mut lexical_seen = HashMap::<String, LexicalNameKind>::new();
    for (i, branch) in branches.iter().enumerate() {
      if i % BRANCH_STEP_EVERY == 0 {
        self.step()?;
      }
      for stmt in &branch.stx.body {
        self.step()?;
        match &*stmt.stx {
          Stmt::VarDecl(var)
            if matches!(
              var.stx.mode,
              VarDeclMode::Let | VarDeclMode::Const | VarDeclMode::Using | VarDeclMode::AwaitUsing
            ) =>
          {
            for declarator in &var.stx.declarators {
              self.step()?;
              self.collect_lexical_decl_names_from_pat(
                ctx,
                &declarator.pattern.stx.pat,
                stmt.loc,
                &mut lexical_seen,
                &var_names,
              )?;
            }
          }
          Stmt::ClassDecl(class) => {
            let Some(name) = class.stx.name.as_ref() else {
              continue;
            };
            self.insert_lexical_name(
              ctx,
              &name.stx.name,
              LexicalNameKind::Other,
              name.loc,
              &mut lexical_seen,
              &var_names,
            )?;
          }
          Stmt::FunctionDecl(decl) => {
            let Some(name) = decl.stx.name.as_ref() else {
              if decl.stx.export_default {
                continue;
              }
              self.push_error(stmt.loc, "anonymous function declaration")?;
              continue;
            };
            let func = &decl.stx.function.stx;
            let kind = if func.async_ || func.generator {
              LexicalNameKind::NonOrdinaryFunction
            } else {
              LexicalNameKind::OrdinaryFunction
            };
            self.insert_lexical_name(
              ctx,
              &name.stx.name,
              kind,
              name.loc,
              &mut lexical_seen,
              &var_names,
            )?;
          }
          _ => {}
        }
      }
    }

    Ok(())
  }

  fn check_for_in_of_decl_head_early_errors(
    &mut self,
    ctx: &ControlContext,
    lhs: &ForInOfLhs,
    body: &ForBody,
  ) -> Result<(), VmError> {
    let ForInOfLhs::Decl((mode, pat)) = lhs else {
      return Ok(());
    };
    if *mode == VarDeclMode::Var {
      return Ok(());
    }

    // BoundNames(ForDeclaration)
    let mut bound_names: Vec<(String, Loc)> = Vec::new();
    Self::collect_bound_names_from_pat(&pat.stx.pat, &mut bound_names)?;

    // Duplicate BoundNames(ForDeclaration)
    let mut seen: HashMap<String, Loc> = HashMap::new();
    for (name, loc) in &bound_names {
      self.step()?;
      if let Some(_first) = seen.get(name) {
        self.push_error(*loc, "duplicate binding name")?;
      } else {
        seen.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
        seen.insert(try_clone_string(name.as_str())?, *loc);
      }
    }

    // In non-strict code, BoundNames(ForDeclaration) must not contain `"let"`.
    if !ctx.strict {
      for (name, loc) in &bound_names {
        self.step()?;
        if name == "let" {
          self.push_error(*loc, "invalid binding name 'let'")?;
        }
      }
    }

    // VarDeclaredNames(Statement) of the loop body.
    let mut body_var_names: HashSet<String> = HashSet::new();
    for stmt in &body.body {
      self.collect_var_declared_names_in_stmt(stmt, &mut body_var_names)?;
    }

    // BoundNames(ForDeclaration) must not collide with VarDeclaredNames(Statement).
    for (name, loc) in bound_names {
      self.step()?;
      if body_var_names.contains(&name) {
        self.push_error(loc, "Identifier has already been declared")?;
      }
    }

    Ok(())
  }

  fn insert_lexical_name(
    &mut self,
    ctx: &ControlContext,
    name: &str,
    kind: LexicalNameKind,
    loc: Loc,
    seen: &mut HashMap<String, LexicalNameKind>,
    var_names: &HashSet<String>,
  ) -> Result<(), VmError> {
    // `LexicallyDeclaredNames` must not collide with `VarDeclaredNames`.
    if var_names.contains(name) {
      self.push_error(loc, "Identifier has already been declared")?;
    }

    match seen.get(name) {
      None => {
        seen.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
        seen.insert(try_clone_string(name)?, kind);
      }
      Some(prev) => {
        let allow_duplicate = !ctx.strict
          && *prev == LexicalNameKind::OrdinaryFunction
          && kind == LexicalNameKind::OrdinaryFunction;
        if !allow_duplicate {
          self.push_error(loc, "Identifier has already been declared")?;
        }
      }
    }

    Ok(())
  }

  fn collect_var_names(&mut self, stmt: &Stmt, out: &mut HashSet<String>) -> Result<(), VmError> {
    // `VarDeclaredNames` can traverse large statement trees (e.g. nested blocks/ifs with no `var`
    // declarations). Budget it so strict-mode scripts can't bypass fuel/interrupt checks during
    // hoisting by forcing an `O(N)` scan that performs no statement/expression evaluation.
    self.step()?;
    match stmt {
      Stmt::VarDecl(var) => {
        if var.stx.mode != VarDeclMode::Var {
          return Ok(());
        }
        for decl in &var.stx.declarators {
          self.step()?;
          self.collect_var_names_from_pat_decl(&decl.pattern.stx, out)?;
        }
        Ok(())
      }
      Stmt::Block(block) => {
        for stmt in &block.stx.body {
          self.collect_var_names(&stmt.stx, out)?;
        }
        Ok(())
      }
      Stmt::If(stmt) => {
        self.collect_var_names(&stmt.stx.consequent.stx, out)?;
        if let Some(alt) = &stmt.stx.alternate {
          self.collect_var_names(&alt.stx, out)?;
        }
        Ok(())
      }
      Stmt::Try(stmt) => {
        for s in &stmt.stx.wrapped.stx.body {
          self.collect_var_names(&s.stx, out)?;
        }
        if let Some(catch) = &stmt.stx.catch {
          for s in &catch.stx.body {
            self.collect_var_names(&s.stx, out)?;
          }
        }
        if let Some(finally) = &stmt.stx.finally {
          for s in &finally.stx.body {
            self.collect_var_names(&s.stx, out)?;
          }
        }
        Ok(())
      }
      Stmt::With(stmt) => self.collect_var_names(&stmt.stx.body.stx, out),
      Stmt::While(stmt) => self.collect_var_names(&stmt.stx.body.stx, out),
      Stmt::DoWhile(stmt) => self.collect_var_names(&stmt.stx.body.stx, out),
      Stmt::ForTriple(stmt) => {
        if let ForTripleStmtInit::Decl(decl) = &stmt.stx.init {
          if decl.stx.mode == VarDeclMode::Var {
            for d in &decl.stx.declarators {
              self.step()?;
              self.collect_var_names_from_pat_decl(&d.pattern.stx, out)?;
            }
          }
        }
        for s in &stmt.stx.body.stx.body {
          self.collect_var_names(&s.stx, out)?;
        }
        Ok(())
      }
      Stmt::ForIn(stmt) => {
        if let ForInOfLhs::Decl((mode, pat_decl)) = &stmt.stx.lhs {
          if *mode == VarDeclMode::Var {
            self.collect_var_names_from_pat_decl(&pat_decl.stx, out)?;
          }
        }
        for s in &stmt.stx.body.stx.body {
          self.collect_var_names(&s.stx, out)?;
        }
        Ok(())
      }
      Stmt::ForOf(stmt) => {
        if let ForInOfLhs::Decl((mode, pat_decl)) = &stmt.stx.lhs {
          if *mode == VarDeclMode::Var {
            self.collect_var_names_from_pat_decl(&pat_decl.stx, out)?;
          }
        }
        for s in &stmt.stx.body.stx.body {
          self.collect_var_names(&s.stx, out)?;
        }
        Ok(())
      }
      Stmt::Label(stmt) => self.collect_var_names(&stmt.stx.statement.stx, out),
      Stmt::Switch(stmt) => {
        const BRANCH_STEP_EVERY: usize = 32;
        for (i, branch) in stmt.stx.branches.iter().enumerate() {
          if i % BRANCH_STEP_EVERY == 0 {
            self.step()?;
          }
          for s in &branch.stx.body {
            self.collect_var_names(&s.stx, out)?;
          }
        }
        Ok(())
      }
      _ => Ok(()),
    }
  }

  fn collect_var_names_from_pat_decl(
    &mut self,
    pat_decl: &PatDecl,
    out: &mut HashSet<String>,
  ) -> Result<(), VmError> {
    self.collect_var_names_from_pat(&pat_decl.pat.stx, out)
  }

  fn collect_var_names_from_pat(
    &mut self,
    pat: &Pat,
    out: &mut HashSet<String>,
  ) -> Result<(), VmError> {
    match pat {
      Pat::Id(id) => {
        let name_str = id.stx.name.as_str();
        if !out.contains(name_str) {
          out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
          out.insert(try_clone_string(name_str)?);
        }
        Ok(())
      }
      Pat::Obj(obj) => {
        for prop in &obj.stx.properties {
          self.step()?;
          self.collect_var_names_from_pat(&prop.stx.target.stx, out)?;
        }
        if let Some(rest) = &obj.stx.rest {
          self.step()?;
          self.collect_var_names_from_pat(&rest.stx, out)?;
        }
        Ok(())
      }
      Pat::Arr(arr) => {
        for elem in &arr.stx.elements {
          self.step()?;
          if let Some(elem) = elem {
            self.collect_var_names_from_pat(&elem.target.stx, out)?;
          }
        }
        if let Some(rest) = &arr.stx.rest {
          self.step()?;
          self.collect_var_names_from_pat(&rest.stx, out)?;
        }
        Ok(())
      }
      Pat::AssignTarget(_) => Ok(()),
    }
  }

  fn collect_sloppy_function_decl_names(
    &mut self,
    stmt: &Stmt,
    out: &mut HashSet<String>,
  ) -> Result<(), VmError> {
    self.collect_sloppy_function_decl_names_in_stmt(stmt, out, /* in_stmt_list */ true)
  }

  fn collect_sloppy_function_decl_names_in_stmt(
    &mut self,
    stmt: &Stmt,
    out: &mut HashSet<String>,
    in_stmt_list: bool,
  ) -> Result<(), VmError> {
    self.step()?;
    match stmt {
      Stmt::FunctionDecl(decl) => {
        let Some(name) = &decl.stx.name else {
          if decl.stx.export_default {
            return Ok(());
          }
          self.push_error(decl.loc, "anonymous function declaration")?;
          return Ok(());
        };

        // In non-strict mode, Annex B var-scoping only applies to *ordinary* function declarations
        // in nested blocks. Async/generator function declarations are always block-scoped.
        let func = &decl.stx.function.stx;
        let annex_b_eligible = !func.async_ && !func.generator;
        if !in_stmt_list && !annex_b_eligible {
          return Ok(());
        }

        let name_str = name.stx.name.as_str();
        if !out.contains(name_str) {
          out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
          out.insert(try_clone_string(name_str)?);
        }
        Ok(())
      }
      Stmt::Block(block) => {
        for stmt in &block.stx.body {
          self.collect_sloppy_function_decl_names_in_stmt(&stmt.stx, out, /* in_stmt_list */ false)?;
        }
        Ok(())
      }
      Stmt::If(stmt) => {
        self.collect_sloppy_function_decl_names_in_stmt(&stmt.stx.consequent.stx, out, false)?;
        if let Some(alt) = &stmt.stx.alternate {
          self.collect_sloppy_function_decl_names_in_stmt(&alt.stx, out, false)?;
        }
        Ok(())
      }
      Stmt::Try(stmt) => {
        for s in &stmt.stx.wrapped.stx.body {
          self.collect_sloppy_function_decl_names_in_stmt(&s.stx, out, false)?;
        }
        if let Some(catch) = &stmt.stx.catch {
          for s in &catch.stx.body {
            self.collect_sloppy_function_decl_names_in_stmt(&s.stx, out, false)?;
          }
        }
        if let Some(finally) = &stmt.stx.finally {
          for s in &finally.stx.body {
            self.collect_sloppy_function_decl_names_in_stmt(&s.stx, out, false)?;
          }
        }
        Ok(())
      }
      Stmt::With(stmt) => self.collect_sloppy_function_decl_names_in_stmt(&stmt.stx.body.stx, out, false),
      Stmt::While(stmt) => self.collect_sloppy_function_decl_names_in_stmt(&stmt.stx.body.stx, out, false),
      Stmt::DoWhile(stmt) => self.collect_sloppy_function_decl_names_in_stmt(&stmt.stx.body.stx, out, false),
      Stmt::ForTriple(stmt) => {
        for s in &stmt.stx.body.stx.body {
          self.collect_sloppy_function_decl_names_in_stmt(&s.stx, out, false)?;
        }
        Ok(())
      }
      Stmt::ForIn(stmt) => {
        for s in &stmt.stx.body.stx.body {
          self.collect_sloppy_function_decl_names_in_stmt(&s.stx, out, false)?;
        }
        Ok(())
      }
      Stmt::ForOf(stmt) => {
        for s in &stmt.stx.body.stx.body {
          self.collect_sloppy_function_decl_names_in_stmt(&s.stx, out, false)?;
        }
        Ok(())
      }
      Stmt::Label(stmt) => self.collect_sloppy_function_decl_names_in_stmt(&stmt.stx.statement.stx, out, false),
      Stmt::Switch(stmt) => {
        const BRANCH_STEP_EVERY: usize = 32;
        for (i, branch) in stmt.stx.branches.iter().enumerate() {
          if i % BRANCH_STEP_EVERY == 0 {
            self.step()?;
          }
          for s in &branch.stx.body {
            self.collect_sloppy_function_decl_names_in_stmt(&s.stx, out, false)?;
          }
        }
        Ok(())
      }
      _ => Ok(()),
    }
  }

  fn collect_lexical_decl_names_from_pat(
    &mut self,
    ctx: &ControlContext,
    pat: &Node<Pat>,
    loc: Loc,
    seen: &mut HashMap<String, LexicalNameKind>,
    var_names: &HashSet<String>,
  ) -> Result<(), VmError> {
    match &*pat.stx {
      Pat::Id(id) => {
        self.insert_lexical_name(
          ctx,
          &id.stx.name,
          LexicalNameKind::Other,
          loc,
          seen,
          var_names,
        )?;
        Ok(())
      }
      Pat::Obj(obj) => {
        for prop in &obj.stx.properties {
          self.step()?;
          self.collect_lexical_decl_names_from_pat(ctx, &prop.stx.target, loc, seen, var_names)?;
        }
        if let Some(rest) = &obj.stx.rest {
          self.step()?;
          self.collect_lexical_decl_names_from_pat(ctx, rest, loc, seen, var_names)?;
        }
        Ok(())
      }
      Pat::Arr(arr) => {
        for elem in &arr.stx.elements {
          self.step()?;
          let Some(elem) = elem else { continue };
          self.collect_lexical_decl_names_from_pat(ctx, &elem.target, loc, seen, var_names)?;
        }
        if let Some(rest) = &arr.stx.rest {
          self.step()?;
          self.collect_lexical_decl_names_from_pat(ctx, rest, loc, seen, var_names)?;
        }
        Ok(())
      }
      Pat::AssignTarget(_) => Ok(()),
    }
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

      // Module-only statement forms.
      //
      // These are rejected in Script goal contexts (`opts.is_module == false`). Most parse entry
      // points rely on parse-js to reject them syntactically for scripts, but vm-js can also parse
      // Script source using the module grammar as a fallback for "async scripts" (top-level
      // `await` / `for await...of`). In that case we must still enforce Script restrictions.
      Stmt::ExportDefaultExpr(export) => {
        if !ctx.is_module {
          self.push_error(stmt.loc, "export not allowed in scripts")?;
        }
        self.visit_expr(ctx, &export.stx.expression)
      }
      Stmt::ExportList(export) => {
        if !ctx.is_module {
          self.push_error(stmt.loc, "export not allowed in scripts")?;
        }
        if let Some(attrs) = &export.stx.attributes {
          self.visit_expr(ctx, attrs)?;
        }
        Ok(())
      }
      Stmt::Import(import) => {
        if !ctx.is_module {
          self.push_error(stmt.loc, "import not allowed in scripts")?;
        }
        // Import declarations require local binding identifiers, not binding patterns.
        if let Some(default) = &import.stx.default {
          if !matches!(&*default.stx.pat.stx, Pat::Id(_)) {
            self.step()?;
            self.push_error(default.loc, "invalid import binding")?;
          } else {
            self.visit_pat(ctx, &default.stx.pat, PatRole::Binding)?;
          }
        }
        if let Some(names) = import.stx.names.as_ref() {
          match names {
            ImportNames::All(pat_decl) => {
              if !matches!(&*pat_decl.stx.pat.stx, Pat::Id(_)) {
                self.step()?;
                self.push_error(pat_decl.loc, "invalid import binding")?;
              } else {
                self.visit_pat(ctx, &pat_decl.stx.pat, PatRole::Binding)?;
              }
            }
            ImportNames::Specific(list) => {
              for name in list {
                if !matches!(&*name.stx.alias.stx.pat.stx, Pat::Id(_)) {
                  self.step()?;
                  self.push_error(name.stx.alias.loc, "invalid import binding")?;
                } else {
                  self.visit_pat(ctx, &name.stx.alias.stx.pat, PatRole::Binding)?;
                }
              }
            }
          }
        }
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
    self.visit_stmt_list(ctx, StmtListKind::BlockLike, &block.body)
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
    self.visit_stmt_list(ctx, StmtListKind::BlockLike, &stmt.wrapped.stx.body)?;
    if let Some(catch) = &stmt.catch {
      self.visit_catch(ctx, &catch.stx)?;
    }
    if let Some(finally) = &stmt.finally {
      self.visit_stmt_list(ctx, StmtListKind::BlockLike, &finally.stx.body)?;
    }
    Ok(())
  }

  fn visit_catch(&mut self, ctx: &mut ControlContext, catch: &CatchBlock) -> Result<(), VmError> {
    // --- TryStatement early errors (ECMA-262) ---
    //
    // It is a Syntax Error if:
    // - BoundNames(CatchParameter) contains any duplicate elements.
    // - Any element of BoundNames(CatchParameter) occurs in LexicallyDeclaredNames(CatchBlock).
    //
    // Note: LexicallyDeclaredNames includes hoistable declarations (function declarations) even in
    // non-strict mode; this is independent of Annex B runtime scoping.
    if let Some(param) = &catch.parameter {
      // BoundNames(CatchParameter)
      let mut bound_names: Vec<(String, Loc)> = Vec::new();
      Self::collect_bound_names_from_pat(&param.stx.pat, &mut bound_names)?;

      // Duplicate BoundNames(CatchParameter).
      //
      // Use borrowed `&str` keys to avoid allocating/cloning binding names again.
      let mut seen: HashSet<&str> = HashSet::new();
      for (name, loc) in &bound_names {
        self.step()?;
        if seen.contains(name.as_str()) {
          self.push_error(*loc, "duplicate binding name")?;
        } else {
          seen.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
          seen.insert(name.as_str());
        }
      }

      // Collect LexicallyDeclaredNames(CatchBlock) from the top-level statement list.
      let mut lexical_names = HashSet::<String>::new();
      for stmt in &catch.body {
        self.step()?;
        match &*stmt.stx {
          Stmt::VarDecl(var)
            if matches!(
              var.stx.mode,
              VarDeclMode::Let | VarDeclMode::Const | VarDeclMode::Using | VarDeclMode::AwaitUsing
            ) =>
          {
            for declarator in &var.stx.declarators {
              self.step()?;
              self
                .collect_var_names_from_pat(&declarator.pattern.stx.pat.stx, &mut lexical_names)?;
            }
          }
          Stmt::ClassDecl(class) => {
            if let Some(name) = class.stx.name.as_ref() {
              let name_str = name.stx.name.as_str();
              if !lexical_names.contains(name_str) {
                lexical_names
                  .try_reserve(1)
                  .map_err(|_| VmError::OutOfMemory)?;
                lexical_names.insert(try_clone_string(name_str)?);
              }
            }
          }
          Stmt::FunctionDecl(decl) => {
            if let Some(name) = &decl.stx.name {
              let name_str = name.stx.name.as_str();
              if !lexical_names.contains(name_str) {
                lexical_names
                  .try_reserve(1)
                  .map_err(|_| VmError::OutOfMemory)?;
                lexical_names.insert(try_clone_string(name_str)?);
              }
            }
          }
          _ => {}
        }
      }

      // Report collisions between the catch parameter bindings and catch block lexical names.
      for (name, loc) in &bound_names {
        self.step()?;
        if lexical_names.contains(name.as_str()) {
          self.push_error(*loc, "Identifier has already been declared")?;
        }
      }
    }

    // Catch binding patterns can contain identifier bindings that are restricted in strict mode
    // (e.g. `catch (eval) {}`) and destructuring defaults that may contain invalid `await`.
    if let Some(param) = &catch.parameter {
      self.visit_pat(ctx, &param.stx.pat, PatRole::Binding)?;
    }
    self.visit_stmt_list(ctx, StmtListKind::BlockLike, &catch.body)
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
    // ForStatement early error (ECMA-262 14.7.4 `for (LexicalDeclaration; ... ) Statement`):
    // It is a Syntax Error if any element of BoundNames(LexicalDeclaration) also occurs in
    // VarDeclaredNames(Statement).
    if let ForTripleStmtInit::Decl(decl) = &stmt.init {
      if matches!(
        decl.stx.mode,
        VarDeclMode::Let | VarDeclMode::Const | VarDeclMode::Using | VarDeclMode::AwaitUsing
      ) {
        // BoundNames(LexicalDeclaration).
        let mut bound_names: Vec<(String, Loc)> = Vec::new();
        for declarator in &decl.stx.declarators {
          self.collect_bound_names_from_pat_budgeted(
            &declarator.pattern.stx.pat,
            &mut bound_names,
          )?;
        }

        // VarDeclaredNames(Statement): only `var` (and var-scoped function declarations, where
        // applicable) should be included; lexical declarations do not participate.
        let mut var_names: HashSet<String> = HashSet::new();
        for stmt in &stmt.body.stx.body {
          self.collect_var_declared_names_in_stmt(stmt, &mut var_names)?;
        }
        for (name, loc) in &bound_names {
          if var_names.contains(name) {
            self.push_error(*loc, "Identifier has already been declared")?;
          }
        }
      }
    }

    let saved_using_allowed = ctx.using_allowed;
    // `ForStatement` is a valid `using` container (for classic Script goal symbol restrictions).
    ctx.using_allowed = true;
    let result = (|| {
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
    })();
    ctx.using_allowed = saved_using_allowed;
    result
  }

  fn visit_for_in(&mut self, ctx: &mut ControlContext, stmt: &ForInStmt) -> Result<(), VmError> {
    let saved_using_allowed = ctx.using_allowed;
    // `ForInOfStatement` is a valid `using` container (for classic Script goal symbol restrictions).
    ctx.using_allowed = true;
    let result = (|| {
      // `using` / `await using` declarations are syntactically invalid in `for-in` heads.
      if let ForInOfLhs::Decl((mode, pat)) = &stmt.lhs {
        if matches!(*mode, VarDeclMode::Using | VarDeclMode::AwaitUsing) {
          self.push_error(pat.loc, "using declarations are not allowed in for-in heads")?;
        }
      }

      self.for_in_of_decl_header_early_errors(ctx, &stmt.lhs, &stmt.body.stx)?;
      self.visit_for_in_of_lhs(ctx, &stmt.lhs)?;
      self.visit_expr(ctx, &stmt.rhs)?;
      self.check_for_in_of_decl_head_early_errors(ctx, &stmt.lhs, &stmt.body.stx)?;
      ctx.loop_depth = ctx.loop_depth.saturating_add(1);
      ctx.breakable_depth = ctx.breakable_depth.saturating_add(1);
      let result = self.visit_for_body(ctx, &stmt.body.stx);
      ctx.loop_depth = ctx.loop_depth.saturating_sub(1);
      ctx.breakable_depth = ctx.breakable_depth.saturating_sub(1);
      result
    })();
    ctx.using_allowed = saved_using_allowed;
    result
  }

  fn visit_for_of(
    &mut self,
    ctx: &mut ControlContext,
    loc: Loc,
    stmt: &ForOfStmt,
  ) -> Result<(), VmError> {
    let saved_using_allowed = ctx.using_allowed;
    // `ForInOfStatement` is a valid `using` container (for classic Script goal symbol restrictions).
    ctx.using_allowed = true;
    let result = (|| {
      if stmt.await_ && !ctx.await_allowed {
        self.push_error(
          loc,
          "for-await-of is only valid in async functions and modules",
        )?;
      }
      self.for_in_of_decl_header_early_errors(ctx, &stmt.lhs, &stmt.body.stx)?;
      self.visit_for_in_of_lhs(ctx, &stmt.lhs)?;
      self.visit_expr(ctx, &stmt.rhs)?;
      self.check_for_in_of_decl_head_early_errors(ctx, &stmt.lhs, &stmt.body.stx)?;
      ctx.loop_depth = ctx.loop_depth.saturating_add(1);
      ctx.breakable_depth = ctx.breakable_depth.saturating_add(1);
      let result = self.visit_for_body(ctx, &stmt.body.stx);
      ctx.loop_depth = ctx.loop_depth.saturating_sub(1);
      ctx.breakable_depth = ctx.breakable_depth.saturating_sub(1);
      result
    })();
    ctx.using_allowed = saved_using_allowed;
    result
  }

  fn visit_for_body(&mut self, ctx: &mut ControlContext, body: &ForBody) -> Result<(), VmError> {
    self.visit_stmt_list(ctx, StmtListKind::BlockLike, &body.body)
  }

  fn visit_for_in_of_lhs(
    &mut self,
    ctx: &mut ControlContext,
    lhs: &ForInOfLhs,
  ) -> Result<(), VmError> {
    match lhs {
      ForInOfLhs::Assign(pat) => self.visit_assignment_target_pat(ctx, pat),
      ForInOfLhs::Decl((_mode, pat)) => self.visit_pat(ctx, &pat.stx.pat, PatRole::Binding),
    }
  }

  fn for_in_of_decl_header_early_errors(
    &mut self,
    ctx: &ControlContext,
    lhs: &ForInOfLhs,
    body: &ForBody,
  ) -> Result<(), VmError> {
    let ForInOfLhs::Decl((mode, pat_decl)) = lhs else {
      return Ok(());
    };
    if !matches!(
      *mode,
      VarDeclMode::Let | VarDeclMode::Const | VarDeclMode::Using | VarDeclMode::AwaitUsing
    ) {
      return Ok(());
    }

    // `BoundNames` of the ForDeclaration.
    let mut bound_names: Vec<(String, Loc)> = Vec::new();
    self.collect_bound_names_from_pat_budgeted(&pat_decl.stx.pat, &mut bound_names)?;

    // Duplicate bound name early error (ECMA-262 `IsSimpleParameterList`-style validation).
    let mut seen = HashSet::<&str>::new();
    for (name, loc) in &bound_names {
      let name_str = name.as_str();
      if seen.contains(name_str) {
        self.push_error(*loc, "Identifier has already been declared")?;
        continue;
      }
      seen.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      seen.insert(name_str);
    }

    // In non-strict mode, the binding name "let" is disallowed in ForDeclarations.
    if !ctx.strict {
      for (name, loc) in &bound_names {
        if name == "let" {
          self.push_error(*loc, "Invalid binding identifier 'let'")?;
        }
      }
    }

    // VarDeclaredNames(Statement) collision early error: the loop body may not `var`-declare any
    // name bound by the head ForDeclaration.
    let mut var_names: HashSet<String> = HashSet::new();
    for stmt in &body.body {
      self.collect_var_declared_names_in_stmt(stmt, &mut var_names)?;
    }
    for (name, loc) in &bound_names {
      if var_names.contains(name) {
        self.push_error(*loc, "Identifier has already been declared")?;
      }
    }

    Ok(())
  }

  fn collect_bound_names_from_pat_budgeted(
    &mut self,
    pat: &Node<Pat>,
    out: &mut Vec<(String, Loc)>,
  ) -> Result<(), VmError> {
    // Budget `BoundNames` collection: patterns can be deeply nested and/or large, and early errors
    // must not bypass fuel/interrupt checks by performing unbudgeted recursive traversals.
    self.step()?;
    match &*pat.stx {
      Pat::Id(id) => {
        out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
        out.push((try_clone_string(&id.stx.name)?, pat.loc));
        Ok(())
      }
      Pat::Obj(obj) => {
        for prop in &obj.stx.properties {
          self.collect_bound_names_from_pat_budgeted(&prop.stx.target, out)?;
        }
        if let Some(rest) = &obj.stx.rest {
          self.collect_bound_names_from_pat_budgeted(rest, out)?;
        }
        Ok(())
      }
      Pat::Arr(arr) => {
        for elem in &arr.stx.elements {
          let Some(elem) = elem else { continue };
          self.collect_bound_names_from_pat_budgeted(&elem.target, out)?;
        }
        if let Some(rest) = &arr.stx.rest {
          self.collect_bound_names_from_pat_budgeted(rest, out)?;
        }
        Ok(())
      }
      Pat::AssignTarget(_) => Ok(()),
    }
  }

  fn visit_switch(&mut self, ctx: &mut ControlContext, stmt: &SwitchStmt) -> Result<(), VmError> {
    self.visit_expr(ctx, &stmt.test)?;
    self.check_declaration_early_errors_in_switch_case_block(ctx, &stmt.branches)?;
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
    self.visit_stmt_list(ctx, StmtListKind::SwitchClause, &branch.body)
  }

  fn visit_label(
    &mut self,
    ctx: &mut ControlContext,
    loc: Loc,
    stmt: &LabelStmt,
  ) -> Result<(), VmError> {
    self.validate_reserved_identifier(ctx, loc, stmt.name.as_str())?;
    if stmt.name == "arguments" && !ctx.arguments_allowed {
      self.push_error(
        loc,
        "'arguments' is not allowed in class field initializer or static initialization block",
      )?;
    }
    let is_iteration = Self::is_iteration_statement(&stmt.statement);
    if ctx.labels.iter().any(|l| l.name == stmt.name) {
      let message = try_format_error_message("duplicate label '", &stmt.name, "'")?;
      self.push_error(loc, message)?;
    }
    ctx
      .labels
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    ctx.labels.push(LabelInfo {
      name: try_clone_string(&stmt.name)?,
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

  fn visit_break(
    &mut self,
    ctx: &mut ControlContext,
    loc: Loc,
    stmt: &parse_js::ast::stmt::BreakStmt,
  ) -> Result<(), VmError> {
    match stmt.label.as_ref() {
      Some(label) => {
        if !ctx.labels.iter().any(|l| l.name == *label) {
          let message = try_format_error_message("undefined label '", label, "'")?;
          self.push_error(loc, message)?;
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
          let message = try_format_error_message("undefined loop label '", label, "'")?;
          self.push_error(loc, message)?;
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

  fn visit_with(
    &mut self,
    ctx: &mut ControlContext,
    loc: Loc,
    stmt: &WithStmt,
  ) -> Result<(), VmError> {
    if ctx.strict {
      self.push_error(loc, "with statements are not allowed in strict mode")?;
    }
    self.visit_expr(ctx, &stmt.object)?;
    self.visit_stmt(ctx, &stmt.body)
  }

  fn visit_class_decl(
    &mut self,
    ctx: &mut ControlContext,
    decl: &ClassDecl,
  ) -> Result<(), VmError> {
    // Per ECMA-262, class definitions are always strict mode code, regardless of whether they
    // appear in a sloppy script/function body.
    //
    // This affects early errors not just within the class body, but also for:
    // - the class binding identifier itself, and
    // - the `extends` (heritage) expression.
    let saved = self.save_scope_flags(ctx);
    ctx.strict = true;

    if let Some(name) = &decl.name {
      self.validate_reserved_identifier(ctx, name.loc, name.stx.name.as_str())?;
      if is_restricted_identifier(&name.stx.name) {
        let message = try_format_error_message(
          "restricted identifier '",
          name.stx.name.as_str(),
          "' is not allowed in strict mode",
        )?;
        self.push_error(name.loc, message)?;
      }
    }
    if let Some(extends) = &decl.extends {
      self.visit_expr(ctx, extends)?;
    }
    let res = self.visit_class_members(ctx, &decl.members, decl.extends.is_some());
    self.restore_scope_flags(ctx, saved);
    res
  }

  fn visit_class_expr(
    &mut self,
    ctx: &mut ControlContext,
    expr: &ClassExpr,
  ) -> Result<(), VmError> {
    // Per ECMA-262, class expressions are always strict mode code.
    let saved = self.save_scope_flags(ctx);
    ctx.strict = true;

    if let Some(name) = &expr.name {
      self.validate_reserved_identifier(ctx, name.loc, name.stx.name.as_str())?;
      if is_restricted_identifier(&name.stx.name) {
        let message = try_format_error_message(
          "restricted identifier '",
          name.stx.name.as_str(),
          "' is not allowed in strict mode",
        )?;
        self.push_error(name.loc, message)?;
      }
    }
    if let Some(extends) = &expr.extends {
      self.visit_expr(ctx, extends)?;
    }
    let res = self.visit_class_members(ctx, &expr.members, expr.extends.is_some());
    self.restore_scope_flags(ctx, saved);
    res
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
    // Likewise, super property references are bound to the current method/initializer/static block;
    // they must not leak into nested class bodies.
    ctx.super_property_allowed = false;

    // Class constructor early errors:
    // - A class may only have one `constructor` method.
    // - Class constructors may not be generators.
    //
    // These must be caught during `validate_top_level` so `eval(...)` and dynamic function
    // constructors throw catchable `SyntaxError` exceptions at parse time (rather than surfacing as
    // a non-catchable `VmError::Syntax` when the class is evaluated).
    let mut seen_ctor = false;
    for member in members {
      self.step()?;
      if member.stx.static_ {
        continue;
      }
      let is_ctor = matches!(
        &member.stx.key,
        ClassOrObjKey::Direct(key)
          if key.stx.key == "constructor" && key.stx.tt == TT::KeywordConstructor
      );
      let ClassOrObjVal::Method(method) = &member.stx.val else {
        continue;
      };
      if !is_ctor {
        continue;
      }
      if seen_ctor {
        self.push_error(member.loc, "A class may only have one constructor")?;
      } else {
        seen_ctor = true;
      }
      if method.stx.func.stx.generator {
        self.push_error(member.loc, "Class constructor may not be a generator")?;
      }
    }

    // Collect declared private names in this class body so `AllPrivateNamesValid` can validate
    // private-name MemberExpressions and private identifiers.
    let mut declared_private_names: HashSet<String> = HashSet::new();
    let mut private_name_decl_state: HashMap<String, PrivateNameDeclState> = HashMap::new();
    for member in members {
      if let ClassOrObjKey::Direct(key) = &member.stx.key {
        if key.stx.tt == TT::PrivateMember {
          let name = key.stx.key.as_str();

          if !declared_private_names.contains(name) {
            declared_private_names
              .try_reserve(1)
              .map_err(|_| VmError::OutOfMemory)?;
            declared_private_names.insert(try_clone_string(name)?);
          }

          // Track private name declarations to detect illegal duplicates:
          // - A private name cannot be used for both static and instance elements.
          // - Getter/setter pairs are allowed, but other duplicate declarations are early errors.
          let entry = match private_name_decl_state.get_mut(name) {
            Some(entry) => entry,
            None => {
              private_name_decl_state
                .try_reserve(1)
                .map_err(|_| VmError::OutOfMemory)?;
              private_name_decl_state
                .insert(try_clone_string(name)?, PrivateNameDeclState::default());
              // Safe: just inserted.
              private_name_decl_state.get_mut(name).ok_or(VmError::InvariantViolation(
                "private name state entry missing after insert",
              ))?
            }
          };

          if let Some(prev_static) = entry.is_static {
            if prev_static != member.stx.static_ {
              let message = try_format_error_message("duplicate private name '", name, "'")?;
              self.push_error(key.loc, message)?;
              continue;
            }
          } else {
            entry.is_static = Some(member.stx.static_);
          }

          match &member.stx.val {
            ClassOrObjVal::Getter(_) => {
              if entry.has_other || entry.has_getter {
                let message = try_format_error_message("duplicate private name '", name, "'")?;
                self.push_error(key.loc, message)?;
              } else {
                entry.has_getter = true;
              }
            }
            ClassOrObjVal::Setter(_) => {
              if entry.has_other || entry.has_setter {
                let message = try_format_error_message("duplicate private name '", name, "'")?;
                self.push_error(key.loc, message)?;
              } else {
                entry.has_setter = true;
              }
            }
            // Methods/fields/accessors are all private name declarations; getters/setters are the
            // only duplication that's allowed as a pair.
            _ => {
              if entry.has_other || entry.has_getter || entry.has_setter {
                let message = try_format_error_message("duplicate private name '", name, "'")?;
                self.push_error(key.loc, message)?;
              } else {
                entry.has_other = true;
              }
            }
          }
        }
      }
    }
    ctx
      .private_names
      .try_reserve(1)
      .map_err(|_| VmError::OutOfMemory)?;
    ctx.private_names.push(declared_private_names);

    let res = (|| {
      for member in members {
        self.visit_class_member(ctx, &member.stx, derived)?;
      }
      Ok(())
    })();

    ctx.private_names.pop();
    self.restore_scope_flags(ctx, saved);
    res
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
        /* super_property_allowed */ true,
        FuncNameKind::Expr,
      ),
      ClassOrObjVal::Setter(setter) => self.visit_func(
        ctx,
        None,
        &setter.stx.func,
        /* unique */ true,
        /* super_call_allowed */ false,
        /* super_property_allowed */ true,
        FuncNameKind::Expr,
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
          /* super_property_allowed */ true,
          FuncNameKind::Expr,
        )
      }
      ClassOrObjVal::Prop(Some(expr)) => {
        // Class field initializers introduce early-error boundaries:
        // - `await`/`yield` expressions are always invalid, regardless of whether the surrounding
        //   function is async/generator (ECMA-262 `ContainsAwait` / `ContainsYieldExpression`).
        // - `arguments` identifier references are always invalid (ECMA-262 `ContainsArguments`).
        // - super property access is permitted (fields are evaluated in a method-like environment
        //   with a `[[HomeObject]]`), but `super()` is still disallowed (`ContainsSuperCall`).
        let saved_await_allowed = ctx.await_allowed;
        let saved_yield_allowed = ctx.yield_allowed;
        let saved_arguments_allowed = ctx.arguments_allowed;
        let saved_super_property_allowed = ctx.super_property_allowed;
        ctx.await_allowed = false;
        ctx.yield_allowed = false;
        ctx.arguments_allowed = false;
        ctx.super_property_allowed = true;
        let res = self.visit_expr(ctx, expr);
        ctx.await_allowed = saved_await_allowed;
        ctx.yield_allowed = saved_yield_allowed;
        ctx.arguments_allowed = saved_arguments_allowed;
        ctx.super_property_allowed = saved_super_property_allowed;
        res
      }
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
    // - `await` expressions are always invalid (ClassStaticBlockStatementList is `~Await`),
    // - `await` is still a reserved identifier regardless (even in scripts),
    // - `yield` expressions are always invalid (ClassStaticBlockStatementList is `~Yield`),
    // - `arguments` identifier references are always invalid (ECMA-262 `ContainsArguments`),
    // - `break`/`continue` target resolution must not cross static-block boundaries.
    let saved = self.save_and_enter_function(
      ctx,
      /* strict */ true,
      /* await_allowed */ false,
      /* yield_allowed */ false,
      /* await_is_reserved */ true,
      /* yield_is_reserved */ ctx.yield_is_reserved,
      /* super_call_allowed */ false,
      /* super_property_allowed */ true,
      /* arguments_allowed */ false,
    );
    ctx.return_allowed = false;
    self.static_block_declared_name_early_errors(stmts)?;
    let res = self.visit_stmt_list(ctx, StmtListKind::BlockLike, stmts);
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
      /* super_property_allowed */ false,
      FuncNameKind::Decl,
    )
  }

  fn visit_var_decl(&mut self, ctx: &mut ControlContext, decl: &VarDecl) -> Result<(), VmError> {
    // ES early error (ECMA-262 13.3.1.1): It is a Syntax Error if the BoundNames of a
    // LexicalDeclaration contains "let".
    //
    // This applies even in non-strict scripts where `let` is otherwise a valid IdentifierName.
    let lexical_mode = matches!(
      decl.mode,
      VarDeclMode::Let | VarDeclMode::Const | VarDeclMode::Using | VarDeclMode::AwaitUsing
    );
    let using_mode = matches!(decl.mode, VarDeclMode::Using | VarDeclMode::AwaitUsing);
    if decl.mode == VarDeclMode::AwaitUsing {
      // Explicit Resource Management early error:
      // `await using` declarations are only valid in async contexts.
      //
      // In `vm-js`, this includes:
      // - async functions,
      // - modules (top-level await), and
      // - "async classic scripts" (top-level await in scripts).
      //
      // Note: in Script goal, `using` / `await using` remain restricted to specific syntactic
      // containers; this is enforced separately via `ctx.using_allowed`.
      let await_using_allowed = ctx.await_allowed;
      if !await_using_allowed {
        if let Some(first) = decl.declarators.first() {
          self.push_error(
            first.pattern.loc,
            "await using declarations are only valid in async functions and modules",
          )?;
        }
      }
    }
    if using_mode && !ctx.using_allowed {
      // Explicit Resource Management early error (tc39/proposal-explicit-resource-management):
      // - In Script goal, `using` declarations are only permitted within specific syntactic containers.
      // - Additionally, `using` / `await using` declarations are disallowed directly within
      //   CaseClause/DefaultClause statement lists.
      //
      // We model this via `ctx.using_allowed`, which is toggled by the walker when entering/exiting
      // those containers.
      if let Some(first) = decl.declarators.first() {
        self.push_error(first.pattern.loc, "using declarations are not allowed in this context")?;
      }
    }
    for declarator in &decl.declarators {
      // Explicit Resource Management early error:
      // `using` / `await using` declarations require a BindingIdentifier (no destructuring).
      if using_mode && !matches!(&*declarator.pattern.stx.pat.stx, Pat::Id(_)) {
        self.push_error(
          declarator.pattern.loc,
          "using declarations may not use destructuring patterns",
        )?;
      }
      if lexical_mode {
        let mut names: Vec<(String, Loc)> = Vec::new();
        Self::collect_bound_names_from_pat(&declarator.pattern.stx.pat, &mut names)?;
        for (name, loc) in names {
          if name == "let" {
            self.push_error(loc, "lexical declarations may not declare a binding named 'let'")?;
          }
        }
      }
      if declarator.initializer.is_none() {
        if decl.mode == VarDeclMode::Const {
          self.push_error(
            declarator.pattern.loc,
            "Missing initializer in const declaration",
          )?;
        } else if using_mode {
          self.push_error(
            declarator.pattern.loc,
            "Missing initializer in using declaration",
          )?;
        } else {
          // Destructuring `var`/`let` declarations require an initializer (early error).
          //
          // Note: `for (var {x} in obj)` / `for (let {x} of iter)` are valid because the binding
          // pattern is parsed as `ForInOfLhs::Decl` (not a `VarDecl` with an omitted initializer).
          if !matches!(&*declarator.pattern.stx.pat.stx, Pat::Id(_)) {
            self.push_error(
              declarator.pattern.loc,
              "Missing initializer in destructuring declaration",
            )?;
          }
        }
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

  fn collect_bound_names_from_pat(
    pat: &Node<Pat>,
    out: &mut Vec<(String, Loc)>,
  ) -> Result<(), VmError> {
    match &*pat.stx {
      Pat::Id(id) => {
        out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
        out.push((try_clone_string(id.stx.name.as_str())?, pat.loc));
        Ok(())
      }
      Pat::Arr(arr) => {
        for elem in &arr.stx.elements {
          let Some(elem) = elem else { continue };
          Self::collect_bound_names_from_pat(&elem.target, out)?;
        }
        if let Some(rest) = &arr.stx.rest {
          Self::collect_bound_names_from_pat(rest, out)?;
        }
        Ok(())
      }
      Pat::Obj(obj) => {
        for prop in &obj.stx.properties {
          Self::collect_bound_names_from_pat(&prop.stx.target, out)?;
        }
        if let Some(rest) = &obj.stx.rest {
          Self::collect_bound_names_from_pat(rest, out)?;
        }
        Ok(())
      }
      Pat::AssignTarget(_) => {
        // Assignment targets are not binding patterns; ignore.
        Ok(())
      }
    }
  }

  fn collect_var_declared_names_in_stmt(
    &mut self,
    stmt: &Node<Stmt>,
    out: &mut HashSet<String>,
  ) -> Result<(), VmError> {
    self.step()?;

    match &*stmt.stx {
      Stmt::VarDecl(decl) => {
        if decl.stx.mode != VarDeclMode::Var {
          return Ok(());
        }
        for declarator in &decl.stx.declarators {
          self.step()?;
          let mut names: Vec<(String, Loc)> = Vec::new();
          Self::collect_bound_names_from_pat(&declarator.pattern.stx.pat, &mut names)?;
          for (name, _) in names {
            out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
            out.insert(name);
          }
        }
        Ok(())
      }
      Stmt::Block(block) => {
        for s in &block.stx.body {
          self.collect_var_declared_names_in_stmt(s, out)?;
        }
        Ok(())
      }
      Stmt::If(stmt) => {
        self.collect_var_declared_names_in_stmt(&stmt.stx.consequent, out)?;
        if let Some(alt) = &stmt.stx.alternate {
          self.collect_var_declared_names_in_stmt(alt, out)?;
        }
        Ok(())
      }
      Stmt::While(stmt) => self.collect_var_declared_names_in_stmt(&stmt.stx.body, out),
      Stmt::DoWhile(stmt) => self.collect_var_declared_names_in_stmt(&stmt.stx.body, out),
      Stmt::ForTriple(stmt) => {
        if let ForTripleStmtInit::Decl(decl) = &stmt.stx.init {
          if decl.stx.mode == VarDeclMode::Var {
            for declarator in &decl.stx.declarators {
              self.step()?;
              let mut names: Vec<(String, Loc)> = Vec::new();
              Self::collect_bound_names_from_pat(&declarator.pattern.stx.pat, &mut names)?;
              for (name, _) in names {
                out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
                out.insert(name);
              }
            }
          }
        }
        for s in &stmt.stx.body.stx.body {
          self.collect_var_declared_names_in_stmt(s, out)?;
        }
        Ok(())
      }
      Stmt::ForIn(stmt) => {
        if let ForInOfLhs::Decl((mode, pat)) = &stmt.stx.lhs {
          if *mode == VarDeclMode::Var {
            let mut names: Vec<(String, Loc)> = Vec::new();
            Self::collect_bound_names_from_pat(&pat.stx.pat, &mut names)?;
            for (name, _) in names {
              out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
              out.insert(name);
            }
          }
        }
        for s in &stmt.stx.body.stx.body {
          self.collect_var_declared_names_in_stmt(s, out)?;
        }
        Ok(())
      }
      Stmt::ForOf(stmt) => {
        if let ForInOfLhs::Decl((mode, pat)) = &stmt.stx.lhs {
          if *mode == VarDeclMode::Var {
            let mut names: Vec<(String, Loc)> = Vec::new();
            Self::collect_bound_names_from_pat(&pat.stx.pat, &mut names)?;
            for (name, _) in names {
              out.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
              out.insert(name);
            }
          }
        }
        for s in &stmt.stx.body.stx.body {
          self.collect_var_declared_names_in_stmt(s, out)?;
        }
        Ok(())
      }
      Stmt::Try(stmt) => {
        for s in &stmt.stx.wrapped.stx.body {
          self.collect_var_declared_names_in_stmt(s, out)?;
        }
        if let Some(catch) = &stmt.stx.catch {
          for s in &catch.stx.body {
            self.collect_var_declared_names_in_stmt(s, out)?;
          }
        }
        if let Some(finally) = &stmt.stx.finally {
          for s in &finally.stx.body {
            self.collect_var_declared_names_in_stmt(s, out)?;
          }
        }
        Ok(())
      }
      Stmt::Switch(stmt) => {
        for branch in &stmt.stx.branches {
          for s in &branch.stx.body {
            self.collect_var_declared_names_in_stmt(s, out)?;
          }
        }
        Ok(())
      }
      Stmt::Label(stmt) => self.collect_var_declared_names_in_stmt(&stmt.stx.statement, out),
      Stmt::With(stmt) => self.collect_var_declared_names_in_stmt(&stmt.stx.body, out),

      // Do not descend into nested functions/classes: var declarations inside them are scoped to
      // that nested code.
      Stmt::ClassDecl(_) | Stmt::FunctionDecl(_) => Ok(()),

      _ => Ok(()),
    }
  }

  fn static_block_declared_name_early_errors(
    &mut self,
    stmts: &[Node<Stmt>],
  ) -> Result<(), VmError> {
    // `VarDeclaredNames` can traverse large statement trees. Budget the traversal.
    let mut var_names: HashSet<String> = HashSet::new();
    for stmt in stmts {
      self.collect_var_declared_names_in_stmt(stmt, &mut var_names)?;
    }

    // `LexicallyDeclaredNames` of a statement list includes only declarations that are direct
    // children of that statement list (it does not include lexical declarations nested inside other
    // statements like inner `{ ... }` blocks).
    let mut lexical_names: HashSet<String> = HashSet::new();
    for stmt in stmts {
      self.step()?;
      match &*stmt.stx {
        Stmt::VarDecl(decl)
          if matches!(
            decl.stx.mode,
            VarDeclMode::Let | VarDeclMode::Const | VarDeclMode::Using | VarDeclMode::AwaitUsing
          ) =>
        {
          for declarator in &decl.stx.declarators {
            self.step()?;
            let mut names: Vec<(String, Loc)> = Vec::new();
            Self::collect_bound_names_from_pat(&declarator.pattern.stx.pat, &mut names)?;
            for (name, loc) in names {
              let collides_var = var_names.contains(name.as_str());
              lexical_names
                .try_reserve(1)
                .map_err(|_| VmError::OutOfMemory)?;
              let inserted = lexical_names.insert(name);
              if !inserted || collides_var {
                self.push_error(loc, "Identifier has already been declared")?;
              }
            }
          }
        }
        Stmt::ClassDecl(decl) => {
          if let Some(name) = &decl.stx.name {
            let name_str = name.stx.name.as_str();
            let duplicate = lexical_names.contains(name_str);
            let collides_var = var_names.contains(name_str);
            if !duplicate {
              lexical_names
                .try_reserve(1)
                .map_err(|_| VmError::OutOfMemory)?;
              lexical_names.insert(try_clone_string(name_str)?);
            }
            if duplicate || collides_var {
              self.push_error(name.loc, "Identifier has already been declared")?;
            }
          }
        }
        Stmt::FunctionDecl(decl) => {
          if let Some(name) = &decl.stx.name {
            let name_str = name.stx.name.as_str();
            let duplicate = lexical_names.contains(name_str);
            let collides_var = var_names.contains(name_str);
            if !duplicate {
              lexical_names
                .try_reserve(1)
                .map_err(|_| VmError::OutOfMemory)?;
              lexical_names.insert(try_clone_string(name_str)?);
            }
            if duplicate || collides_var {
              self.push_error(name.loc, "Identifier has already been declared")?;
            }
          }
        }
        _ => {}
      }
    }

    Ok(())
  }

  fn visit_func(
    &mut self,
    ctx: &mut ControlContext,
    name: Option<&Node<ClassOrFuncName>>,
    func: &Node<Func>,
    unique_formals: bool,
    super_call_allowed: bool,
    super_property_allowed: bool,
    name_kind: FuncNameKind,
  ) -> Result<(), VmError> {
    self.step()?;

    let params = &func.stx.parameters;
    for (idx, param) in params.iter().enumerate() {
      if param.stx.rest && idx + 1 != params.len() {
        self.push_error(param.loc, "rest parameter must be last")?;
      }
    }

    // ECMA-262 early error: It is a Syntax Error if any element of BoundNames(FormalParameters)
    // also occurs in LexicallyDeclaredNames(FunctionBody).
    //
    // Note: `LexicallyDeclaredNames` for a statement list includes only declarations that are
    // *direct* children of that list (e.g. `function f(x) { { let x; } }` is allowed).
    if let Some(FuncBody::Block(stmts)) = &func.stx.body {
      let mut param_names: HashSet<String> = HashSet::new();
      for param in params {
        self.step()?;
        let mut names: Vec<(String, Loc)> = Vec::new();
        Self::collect_bound_names_from_pat(&param.stx.pattern.stx.pat, &mut names)?;
        for (name, _) in names {
          if !param_names.contains(name.as_str()) {
            param_names
              .try_reserve(1)
              .map_err(|_| VmError::OutOfMemory)?;
            param_names.insert(name);
          }
        }
      }

      if !param_names.is_empty() {
        // Collect `LexicallyDeclaredNames` from the top-level statement list (direct children
        // only).
        let mut body_lex_names: HashMap<String, Loc> = HashMap::new();
        for stmt in stmts {
          self.step()?;
          match &*stmt.stx {
            Stmt::VarDecl(decl)
              if matches!(
                decl.stx.mode,
                VarDeclMode::Let | VarDeclMode::Const | VarDeclMode::Using | VarDeclMode::AwaitUsing
              ) =>
            {
              for declarator in &decl.stx.declarators {
                self.step()?;
                let mut names: Vec<(String, Loc)> = Vec::new();
                Self::collect_bound_names_from_pat(&declarator.pattern.stx.pat, &mut names)?;
                for (name, loc) in names {
                  if !body_lex_names.contains_key(name.as_str()) {
                    body_lex_names
                      .try_reserve(1)
                      .map_err(|_| VmError::OutOfMemory)?;
                    body_lex_names.insert(name, loc);
                  }
                }
              }
            }
            Stmt::ClassDecl(decl) => {
              if let Some(name) = &decl.stx.name {
                let name_str = name.stx.name.as_str();
                if !body_lex_names.contains_key(name_str) {
                  body_lex_names
                    .try_reserve(1)
                    .map_err(|_| VmError::OutOfMemory)?;
                  body_lex_names.insert(try_clone_string(name_str)?, name.loc);
                }
              }
            }
            _ => {}
          }
        }

        for (name, loc) in body_lex_names {
          if param_names.contains(name.as_str()) {
            self.push_error(loc, "Identifier has already been declared")?;
            return Err(VmError::Syntax(std::mem::take(&mut self.diags)));
          }
        }
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
          let message = try_format_error_message(
            "restricted identifier '",
            name.stx.name.as_str(),
            "' is not allowed in strict mode",
          )?;
          self.push_error(name.loc, message)?;
        }
      }
    }
    // `await` and `yield` are reserved identifiers in certain contexts (modules/async/generators).
    //
    // Note: function *declarations* and *expressions* differ here:
    // - Declarations introduce a binding in the surrounding scope (so they inherit outer
    //   `await`/`yield` reservation).
    // - Named function expressions bind the name inside the function itself, so they are not
    //   affected by outer async/generator contexts (but are still affected by module parsing, and
    //   by the function being async/generator itself).
    if let Some(name) = name {
      match name_kind {
        FuncNameKind::Decl => self.validate_reserved_identifier_flags(
          /* strict */ func_strict,
          /* is_module */ ctx.is_module,
          /* await_is_reserved */ ctx.await_is_reserved,
          /* yield_is_reserved */ ctx.yield_is_reserved,
          name.loc,
          name.stx.name.as_str(),
        )?,
        FuncNameKind::Expr => self.validate_reserved_identifier_flags(
          /* strict */ func_strict,
          /* is_module */ ctx.is_module,
          /* await_is_reserved */ func.stx.async_,
          /* yield_is_reserved */ func.stx.generator,
          name.loc,
          name.stx.name.as_str(),
        )?,
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
        Self::collect_bound_names_from_pat(&param.stx.pattern.stx.pat, &mut names)?;
        for (name, loc) in names {
          if let Some(_first) = seen.get(&name) {
            let message = try_format_error_message("duplicate parameter name '", &name, "'")?;
            self.push_error(loc, message)?;
          } else {
            seen.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
            seen.insert(name, loc);
          }
        }
      }
    }

    // Enter the function context when traversing parameter initializers and the function body.
    let arguments_allowed = if func.stx.arrow {
      ctx.arguments_allowed
    } else {
      true
    };
    let saved = self.save_and_enter_function(
      ctx,
      func_strict,
      func.stx.async_,
      func.stx.generator,
      func.stx.async_,
      func.stx.generator,
      super_call_allowed,
      super_property_allowed,
      arguments_allowed,
    );

    // `await`/`yield` expressions are early errors in formal parameter initializers, even for
    // async/generator functions (ECMA-262 `ContainsAwaitExpression` / `ContainsYieldExpression`).
    //
    // Example:
    // - `async function f(a = await 0) {}` is a Syntax Error.
    // - `function* g(a = yield 0) {}` is a Syntax Error.
    //
    // This restriction applies only to the parameter list; `await`/`yield` remain valid in the
    // function body for async/generator functions.
    let saved_param_await_allowed = ctx.await_allowed;
    let saved_param_yield_allowed = ctx.yield_allowed;
    ctx.await_allowed = false;
    ctx.yield_allowed = false;
    for param in params {
      self.visit_pat(ctx, &param.stx.pattern.stx.pat, PatRole::Binding)?;
      if let Some(default_value) = &param.stx.default_value {
        self.visit_expr(ctx, default_value)?;
      }
    }
    ctx.await_allowed = saved_param_await_allowed;
    ctx.yield_allowed = saved_param_yield_allowed;

    // ES early error: `LexicallyDeclaredNames(FunctionBody)` must not collide with parameter
    // bound names.
    //
    // Note: this checks only the *direct* statement list items in the function body (nested blocks
    // may legally shadow parameter names).
    if let Some(FuncBody::Block(stmts)) = &func.stx.body {
      let mut param_names: HashSet<String> = HashSet::new();
      for param in params {
        let mut names: Vec<(String, Loc)> = Vec::new();
        self.collect_bound_names_from_pat_budgeted(&param.stx.pattern.stx.pat, &mut names)?;
        for (name, _loc) in names {
          param_names
            .try_reserve(1)
            .map_err(|_| VmError::OutOfMemory)?;
          param_names.insert(name);
        }
      }

      if !param_names.is_empty() {
        for stmt in stmts {
          self.step()?;
          match &*stmt.stx {
            Stmt::VarDecl(var)
              if matches!(
                var.stx.mode,
                VarDeclMode::Let
                  | VarDeclMode::Const
                  | VarDeclMode::Using
                  | VarDeclMode::AwaitUsing
              ) =>
            {
              for declarator in &var.stx.declarators {
                self.step()?;
                let mut names: Vec<(String, Loc)> = Vec::new();
                self.collect_bound_names_from_pat_budgeted(
                  &declarator.pattern.stx.pat,
                  &mut names,
                )?;
                for (name, loc) in names {
                  if param_names.contains(name.as_str()) {
                    self.push_error(loc, "Identifier has already been declared")?;
                  }
                }
              }
            }
            Stmt::ClassDecl(class) => {
              if let Some(name) = class.stx.name.as_ref() {
                if param_names.contains(name.stx.name.as_str()) {
                  self.push_error(name.loc, "Identifier has already been declared")?;
                }
              }
            }
            _ => {}
          }
        }
      }
    }

    match &func.stx.body {
      Some(FuncBody::Block(stmts)) => self.visit_stmt_list(ctx, StmtListKind::VarScope, stmts)?,
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
      Expr::ComputedMember(member) => self.visit_computed_member(ctx, expr.loc, &member.stx),
      Expr::Cond(cond) => self.visit_cond(ctx, &cond.stx),
      Expr::Func(func) => self.visit_func_expr(ctx, &func.stx),
      Expr::Id(id) => {
        self.validate_reserved_identifier(ctx, expr.loc, id.stx.name.as_str())?;
        if id.stx.name == "arguments" && !ctx.arguments_allowed {
          self.push_error(
            expr.loc,
            "'arguments' is not allowed in class field initializer or static initialization block",
          )?;
        }
        // `yield` is a strict mode reserved word, and is also reserved in generator function
        // bodies (where it introduces `YieldExpression`).
        if (ctx.strict || ctx.yield_allowed) && id.stx.name == "yield" {
          self.push_error(expr.loc, "yield is not allowed as an identifier in this context")?;
        }
        if id.stx.name.starts_with('#') {
          // parse-js parses `PrivateIdentifier` tokens as identifier expressions (e.g. `#x` is an
          // `Expr::Id`). In ECMAScript syntax, a private identifier may only appear in:
          // - `#x in obj` (private brand check), and
          // - `obj.#x` (private member access).
          self.push_error(expr.loc, "invalid private identifier")?;
        }
        Ok(())
      }
      Expr::Import(import) => self.visit_import(ctx, &import.stx),
      Expr::ImportMeta(_) => {
        if !ctx.is_module {
          self.push_error(expr.loc, "Cannot use 'import.meta' outside a module")?;
        }
        Ok(())
      }
      Expr::Member(member) => self.visit_member(ctx, expr.loc, &member.stx),
      Expr::TaggedTemplate(tagged) => self.visit_tagged_template(ctx, &tagged.stx),
      Expr::Unary(unary) => self.visit_unary(ctx, &unary.stx, expr.loc),
      Expr::UnaryPostfix(unary) => self.visit_unary_postfix(ctx, &unary.stx, expr.loc),

      // Literals/patterns that contain nested expressions.
      Expr::LitArr(arr) => self.visit_lit_arr(ctx, &arr.stx),
      Expr::LitObj(obj) => self.visit_lit_obj(ctx, &obj.stx.members),
      Expr::LitTemplate(template) => self.visit_lit_template(ctx, &template.stx.parts),
      Expr::ObjPat(obj) => self.visit_obj_pat(ctx, &obj.stx, PatRole::Assignment),
      Expr::ArrPat(arr) => self.visit_arr_pat(ctx, &arr.stx, PatRole::Assignment),

      Expr::IdPat(id) => {
        self.validate_reserved_identifier(ctx, expr.loc, id.stx.name.as_str())?;
        if id.stx.name == "arguments" && !ctx.arguments_allowed {
          self.push_error(
            expr.loc,
            "'arguments' is not allowed in class field initializer or static initialization block",
          )?;
        }
        if id.stx.name.starts_with('#') {
          self.push_error(expr.loc, "invalid private identifier")?;
        }
        Ok(())
      }

      // Leaves (or TS-only nodes) are ignored for this early error set.
      _ => Ok(()),
    }
  }

  fn visit_arrow_func_expr(
    &mut self,
    ctx: &mut ControlContext,
    expr: &ArrowFuncExpr,
  ) -> Result<(), VmError> {
    self.visit_func(
      ctx,
      None,
      &expr.func,
      /* unique */ true,
      /* super_call_allowed */ ctx.super_call_allowed,
      /* super_property_allowed */ ctx.super_property_allowed,
      FuncNameKind::Expr,
    )
  }

  fn visit_func_expr(&mut self, ctx: &mut ControlContext, expr: &FuncExpr) -> Result<(), VmError> {
    self.visit_func(
      ctx,
      expr.name.as_ref(),
      &expr.func,
      /* unique */ false,
      /* super_call_allowed */ false,
      /* super_property_allowed */ false,
      FuncNameKind::Expr,
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

  fn visit_member(
    &mut self,
    ctx: &mut ControlContext,
    loc: Loc,
    expr: &MemberExpr,
  ) -> Result<(), VmError> {
    let is_super = matches!(&*expr.left.stx, Expr::Super(_));
    if is_super {
      if expr.optional_chaining {
        self.push_error(loc, "optional chaining cannot be used on super")?;
      }
      if !ctx.super_property_allowed {
        self.push_error(
          loc,
          "super property access is only valid in methods and class initializers",
        )?;
      }
    }
    self.visit_expr(ctx, &expr.left)?;
    if expr.right.starts_with('#') {
      if matches!(&*expr.left.stx, Expr::Super(_)) {
        self.push_error(loc, "super.#<name> is not a valid private member access")?;
      } else {
        self.validate_declared_private_name(ctx, loc, &expr.right)?;
      }
    }
    Ok(())
  }

  fn visit_computed_member(
    &mut self,
    ctx: &mut ControlContext,
    loc: Loc,
    expr: &ComputedMemberExpr,
  ) -> Result<(), VmError> {
    let is_super = matches!(&*expr.object.stx, Expr::Super(_));
    if is_super {
      if expr.optional_chaining {
        self.push_error(loc, "optional chaining cannot be used on super")?;
      }
      if !ctx.super_property_allowed {
        self.push_error(
          loc,
          "super property access is only valid in methods and class initializers",
        )?;
      }
    }
    self.visit_expr(ctx, &expr.object)?;
    self.visit_expr(ctx, &expr.member)
  }

  fn visit_call(&mut self, ctx: &mut ControlContext, expr: &CallExpr) -> Result<(), VmError> {
    if matches!(&*expr.callee.stx, Expr::Super(_)) {
      if expr.optional_chaining {
        self.push_error(expr.callee.loc, "optional chaining cannot be used on super")?;
      } else if !ctx.super_call_allowed {
        self.push_error(
          expr.callee.loc,
          "super() is only valid in derived class constructors",
        )?;
      }
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
          // Parenthesized expressions break optional-chain propagation:
          // `(a?.b).c` is a valid assignment/update target even though `a?.b` contains optional
          // chaining. See `Evaluator::eval_chain_base`.
          if member.stx.left.assoc.get::<ParenthesizedExpr>().is_some() {
            None
          } else {
            Self::optional_chain_in_assignment_target_expr(&member.stx.left)
          }
        }
      }
      Expr::ComputedMember(member) => {
        if member.stx.optional_chaining {
          Some(expr.loc)
        } else {
          if member.stx.object.assoc.get::<ParenthesizedExpr>().is_some() {
            None
          } else {
            Self::optional_chain_in_assignment_target_expr(&member.stx.object)
          }
        }
      }
      Expr::Call(call) => {
        if call.stx.optional_chaining {
          Some(expr.loc)
        } else {
          if call.stx.callee.assoc.get::<ParenthesizedExpr>().is_some() {
            None
          } else {
            Self::optional_chain_in_assignment_target_expr(&call.stx.callee)
          }
        }
      }
      _ => None,
    }
  }

  fn is_valid_simple_assignment_target_expr(expr: &Node<Expr>) -> bool {
    match &*expr.stx {
      Expr::Id(_) | Expr::IdPat(_) => true,
      Expr::Member(member) => !member.stx.optional_chaining,
      Expr::ComputedMember(member) => !member.stx.optional_chaining,
      _ => false,
    }
  }

  fn visit_binary(&mut self, ctx: &mut ControlContext, expr: &BinaryExpr) -> Result<(), VmError> {
    if expr.operator.is_assignment() {
      // `eval = ...` and `arguments = ...` are strict-mode early errors.
      if ctx.strict {
        match &*expr.left.stx {
          Expr::Id(id) if is_restricted_identifier(&id.stx.name) => {
            let message = try_format_error_message(
              "cannot assign to '",
              id.stx.name.as_str(),
              "' in strict mode",
            )?;
            self.push_error(expr.left.loc, message)?;
          }
          Expr::IdPat(id) if is_restricted_identifier(&id.stx.name) => {
            let message = try_format_error_message(
              "cannot assign to '",
              id.stx.name.as_str(),
              "' in strict mode",
            )?;
            self.push_error(expr.left.loc, message)?;
          }
          _ => {}
        }
      }

      // Destructuring patterns are only valid for plain `=` assignment.
      if matches!(&*expr.left.stx, Expr::ArrPat(_) | Expr::ObjPat(_))
        && expr.operator != OperatorName::Assignment
      {
        self.push_error(expr.left.loc, "Invalid left-hand side in assignment")?;
      }

      // Optional chaining is a static early error in assignment targets.
      if matches!(&*expr.left.stx, Expr::ArrPat(_) | Expr::ObjPat(_)) {
        // Destructuring patterns handle optional chaining only in `Pat::AssignTarget` positions.
      } else if let Some(loc) = Self::optional_chain_in_assignment_target_expr(&expr.left) {
        self.push_error(loc, "optional chaining cannot appear in assignment targets")?;
      } else if !Self::is_valid_simple_assignment_target_expr(&expr.left) {
        // Non-pattern assignment targets must be simple assignment targets.
        self.push_error(expr.left.loc, "Invalid left-hand side in assignment")?;
      }
    }

    if expr.operator == OperatorName::In {
      match &*expr.left.stx {
        Expr::Id(id) if id.stx.name.starts_with('#') => {
          if expr.left.assoc.get::<ParenthesizedExpr>().is_some() {
            self.push_error(expr.left.loc, "invalid private identifier")?;
          } else {
            self.validate_declared_private_name(ctx, expr.left.loc, &id.stx.name)?;
          }
          return self.visit_expr(ctx, &expr.right);
        }
        Expr::IdPat(id) if id.stx.name.starts_with('#') => {
          if expr.left.assoc.get::<ParenthesizedExpr>().is_some() {
            self.push_error(expr.left.loc, "invalid private identifier")?;
          } else {
            self.validate_declared_private_name(ctx, expr.left.loc, &id.stx.name)?;
          }
          return self.visit_expr(ctx, &expr.right);
        }
        _ => {}
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
        // `delete PrivateReference` is an early error (ECMA-262 14.5.1 / 13.5.1.1).
        //
        // Example:
        // - `delete obj.#x`
        // - `delete obj?.#x`
        //
        // Note: this is independent of strict mode.
        //
        // V8/Node reports: "Private fields can not be deleted".
        //
        // When not inside any class body, we instead defer to `AllPrivateNamesValid` checks, which
        // report invalid private-name usage (e.g. `({}).#x`) rather than a delete-specific error.
        if ctx.private_names.last().is_some() {
          if let Expr::Member(member) = &*expr.argument.stx {
            if member.stx.right.starts_with('#') {
              self.push_error(expr.argument.loc, "Private fields can not be deleted")?;
              // Still traverse the base expression for nested early errors.
              self.visit_expr(ctx, &member.stx.left)?;
              return Ok(());
            }
          }
        }

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
              let message = try_format_error_message(
                "cannot assign to '",
                id.stx.name.as_str(),
                "' in strict mode",
              )?;
              self.push_error(expr.argument.loc, message)?;
            }
            Expr::IdPat(id) if is_restricted_identifier(&id.stx.name) => {
              let message = try_format_error_message(
                "cannot assign to '",
                id.stx.name.as_str(),
                "' in strict mode",
              )?;
              self.push_error(expr.argument.loc, message)?;
            }
            _ => {}
          }
        }
        if let Some(loc) = Self::optional_chain_in_assignment_target_expr(&expr.argument) {
          self.push_error(loc, "optional chaining cannot appear in assignment targets")?;
        } else if !Self::is_valid_simple_assignment_target_expr(&expr.argument) {
          self.push_error(expr.argument.loc, "Invalid left-hand side in assignment")?;
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
              let message = try_format_error_message(
                "cannot assign to '",
                id.stx.name.as_str(),
                "' in strict mode",
              )?;
              self.push_error(expr.argument.loc, message)?;
            }
            Expr::IdPat(id) if is_restricted_identifier(&id.stx.name) => {
              let message = try_format_error_message(
                "cannot assign to '",
                id.stx.name.as_str(),
                "' in strict mode",
              )?;
              self.push_error(expr.argument.loc, message)?;
            }
            _ => {}
          }
        }
        if let Some(loc) = Self::optional_chain_in_assignment_target_expr(&expr.argument) {
          self.push_error(loc, "optional chaining cannot appear in assignment targets")?;
        } else if !Self::is_valid_simple_assignment_target_expr(&expr.argument) {
          self.push_error(expr.argument.loc, "Invalid left-hand side in assignment")?;
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

  fn visit_lit_arr(
    &mut self,
    ctx: &mut ControlContext,
    arr: &parse_js::ast::expr::lit::LitArrExpr,
  ) -> Result<(), VmError> {
    for elem in &arr.elements {
      match elem {
        LitArrElem::Single(expr) | LitArrElem::Rest(expr) => self.visit_expr(ctx, expr)?,
        LitArrElem::Empty => {}
      }
    }
    Ok(())
  }

  fn visit_lit_obj(
    &mut self,
    ctx: &mut ControlContext,
    members: &[Node<ObjMember>],
  ) -> Result<(), VmError> {
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
              /* super_property_allowed */ true,
              FuncNameKind::Expr,
            )?,
            ClassOrObjVal::Setter(setter) => self.visit_func(
              ctx,
              None,
              &setter.stx.func,
              /* unique */ true,
              /* super_call_allowed */ false,
              /* super_property_allowed */ true,
              FuncNameKind::Expr,
            )?,
            ClassOrObjVal::Method(method) => self.visit_func(
              ctx,
              None,
              &method.stx.func,
              /* unique */ true,
              /* super_call_allowed */ false,
              /* super_property_allowed */ true,
              FuncNameKind::Expr,
            )?,
            ClassOrObjVal::Prop(Some(expr)) => self.visit_expr(ctx, expr)?,
            ClassOrObjVal::Prop(None) => {}
            // Static blocks not valid in object literals; ignore others.
            _ => {}
          }
        }
        ObjMemberType::Shorthand { id } => {
          self.step()?;
          self.validate_reserved_identifier(ctx, id.loc, id.stx.name.as_str())?;
          if id.stx.name == "arguments" && !ctx.arguments_allowed {
            self.push_error(
              id.loc,
              "'arguments' is not allowed in class field initializer or static initialization block",
            )?;
          }
          if id.stx.name.starts_with('#') {
            self.push_error(id.loc, "invalid private identifier")?;
          }
        }
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

  fn visit_pat(
    &mut self,
    ctx: &mut ControlContext,
    pat: &Node<Pat>,
    role: PatRole,
  ) -> Result<(), VmError> {
    self.step()?;
    match &*pat.stx {
      Pat::Arr(arr) => self.visit_arr_pat(ctx, &arr.stx, role),
      Pat::Id(id) => {
        self.validate_reserved_identifier(ctx, pat.loc, id.stx.name.as_str())?;
        if ctx.strict && is_restricted_identifier(&id.stx.name) {
          let message = try_format_error_message(
            "restricted identifier '",
            id.stx.name.as_str(),
            "' is not allowed in strict mode",
          )?;
          self.push_error(pat.loc, message)?;
        }
        if (ctx.strict || ctx.yield_allowed) && id.stx.name == "yield" {
          self.push_error(pat.loc, "yield is not allowed as an identifier in this context")?;
        }
        if id.stx.name.starts_with('#') {
          // `parse-js` represents `#x` tokens as identifier patterns. In ECMAScript syntax, private
          // identifiers may only appear in:
          // - `obj.#x` (private member access), and
          // - `#x in obj` (private brand check).
          //
          // They are **not** valid assignment targets (e.g. `for (#x in y) {}`), nor valid binding
          // identifiers.
          self.push_error(pat.loc, "invalid private identifier")?;
        }
        Ok(())
      }
      Pat::Obj(obj) => self.visit_obj_pat(ctx, &obj.stx, role),
      Pat::AssignTarget(expr) => {
        if matches!(role, PatRole::AssignmentTarget | PatRole::Assignment) {
          if let Some(loc) = Self::optional_chain_in_assignment_target_expr(expr) {
            self.push_error(loc, "optional chaining cannot appear in assignment targets")?;
          } else if !Self::is_valid_simple_assignment_target_expr(expr) {
            self.push_error(expr.loc, "Invalid left-hand side in assignment")?;
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

#[cfg(test)]
mod tests {
  use super::{validate_top_level, EarlyErrorOptions};
  use crate::VmError;
  use diagnostics::Diagnostic;

  fn assert_syntax_error(source: &str, opts: EarlyErrorOptions) {
    // Use the permissive TS+module parser to ensure we can validate early errors even for
    // context-sensitive keywords like `yield`/`await`. vm-js enforces the spec restrictions in
    // early error validation rather than relying exclusively on parse-time keyword classification.
    let program = parse_js::parse(source).expect("parse source");
    let mut tick = || Ok(());
    let res = validate_top_level(&program.stx.body, opts, Some(source), &mut tick);
    match res {
      Err(VmError::Syntax(diags)) => {
        assert!(
          !diags.is_empty(),
          "expected at least one early error diagnostic, got empty list"
        );
        for d in diags {
          let Diagnostic { .. } = d;
        }
      }
      other => panic!("expected VmError::Syntax, got {other:?}"),
    }
  }

  #[test]
  fn private_name_decl_state_insert_returns_syntax_error_instead_of_panicking() {
    // Exercise the private-name declaration state insertion path in `visit_class_body` by
    // introducing an illegal duplicate private name (static vs instance).
    let source = "class C { #x; static #x; }";
    let program = parse_js::parse(source).expect("parse class with private names");
    let mut tick = || Ok(());
    let res = validate_top_level(
      &program.stx.body,
      EarlyErrorOptions::script(false),
      Some(source),
      &mut tick,
    );
    match res {
      Err(VmError::Syntax(diags)) => {
        assert!(
          !diags.is_empty(),
          "expected at least one early error diagnostic for duplicate private name"
        );
        // Ensure diagnostics are well-formed (the exact message is not important here).
        for d in diags {
          let Diagnostic { .. } = d;
        }
      }
      other => panic!("expected VmError::Syntax, got {other:?}"),
    }
  }

  #[test]
  fn strict_mode_disallows_yield_as_binding_identifier_in_patterns() {
    assert_syntax_error(
      "\"use strict\"; let { yield } = {};",
      EarlyErrorOptions::script(true),
    );
  }

  #[test]
  fn generator_disallows_yield_as_binding_identifier_in_patterns() {
    assert_syntax_error(
      "function* g() { let { yield } = {}; }",
      EarlyErrorOptions::script(false),
    );
  }

  #[test]
  fn strict_mode_disallows_yield_in_assignment_patterns() {
    assert_syntax_error(
      "\"use strict\"; for ({ yield } in [{}]) ;",
      EarlyErrorOptions::script(true),
    );
  }

  #[test]
  fn async_function_disallows_await_as_binding_identifier_in_patterns() {
    assert_syntax_error(
      "async function f() { let { await } = {}; }",
      EarlyErrorOptions::script(false),
    );
  }

  #[test]
  fn async_function_disallows_await_in_assignment_patterns() {
    assert_syntax_error(
      "async function f() { for ({ await } in [{}]) ; }",
      EarlyErrorOptions::script(false),
    );
  }

  #[test]
  fn module_disallows_await_as_binding_identifier_in_patterns() {
    assert_syntax_error("let { await } = {};", EarlyErrorOptions::module());
  }

  #[test]
  fn class_extends_expression_is_strict_mode_code() {
    // Strict-mode restrictions apply to the `extends` expression as well.
    assert_syntax_error(
      "class C extends (yield = 1) {}",
      EarlyErrorOptions::script(false),
    );
  }

  // Note: parse-js currently rejects some reserved words in certain binding-identifier positions
  // during parsing (e.g. function/class names, some import bindings). Those cases are still covered
  // by test262 negative-parse suites; we focus these unit tests on contexts where vm-js relies on
  // early-error validation after parsing (binding patterns and assignment patterns).
}
