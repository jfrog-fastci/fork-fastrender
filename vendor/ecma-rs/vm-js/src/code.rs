//! Stable storage for compiled JavaScript source + lowered HIR.
//!
//! A user-defined [`crate::JsFunction`] stores a [`CompiledFunctionRef`] in its `[[Call]]` handler.
//! Since `CompiledFunctionRef` contains an `Arc<CompiledScript>`, function objects keep their
//! underlying compiled source/HIR alive even after the original compilation API returns.
//!
//! Note that [`CompiledScript`] lives **outside** the GC heap. To ensure compiled code is included
//! in [`crate::HeapLimits`], compilation charges estimated off-heap bytes via
//! [`crate::Heap::charge_external`].

use crate::heap::ExternalMemoryToken;
use crate::fallible_alloc::arc_try_new_vm;
use crate::source::SourceText;
use crate::SourceTextInput;
use crate::Heap;
use crate::VmError;
use crate::Vm;
use crate::function::EcmaFunctionId;
use diagnostics::FileId;
use derive_visitor::{Drive, Event, Visitor};
use parse_js::ast::class_or_object::{ClassOrObjKey, ClassOrObjVal, ObjMemberType};
use parse_js::ast::expr::lit::{LitArrElem, LitTemplatePart};
use parse_js::ast::expr::pat::{ArrPat, IdPat, ObjPat, Pat};
use parse_js::ast::expr::{Expr, IdExpr, MemberExpr};
use parse_js::ast::func::Func;
use parse_js::ast::node::{Node, ParenthesizedExpr};
use parse_js::ast::stmt::decl::VarDeclMode;
use parse_js::ast::stmt::{ForInOfLhs, ForOfStmt, ForTripleStmt, ForTripleStmtInit, Stmt};
use parse_js::operator::OperatorName;
use parse_js::token::TT;
use parse_js::{parse_with_options, parse_with_options_cancellable_by_with_init, Dialect, ParseOptions, SourceType};
use std::collections::BTreeSet;
use std::sync::Arc;

/// A compiled JavaScript source file (source text + lowered HIR).
///
/// Despite the name, this type can represent both classic scripts and modules; the compilation
/// entry points choose the parser's `SourceType`.
#[derive(Debug)]
pub struct CompiledScript {
  pub source: Arc<SourceText>,
  pub hir: Arc<hir_js::LowerResult>,
  pub contains_async_generators: bool,
  /// True if the source contains any generator function (`function*` or `async function*`).
  pub contains_generators: bool,
  /// True if the source contains any async function (`async function` / `async () =>` / `async function*`).
  pub contains_async_functions: bool,
  /// Async function bodies that must fall back to the AST interpreter at call-time.
  ///
  /// The compiled (HIR) async executor intentionally supports only a conservative subset of `await`
  /// forms (see [`crate::hir_exec::run_compiled_async_function`]). Async functions that use any
  /// unsupported `await` pattern are tagged as [`crate::function::FunctionData::AsyncEcmaFallback`]
  /// when allocated during compiled execution so call-time evaluation can delegate to the AST
  /// interpreter without partially executing compiled HIR.
  pub async_function_body_requires_ast_fallback: BTreeSet<hir_js::BodyId>,
  /// True if the compiled (HIR) execution path must fall back to the AST interpreter.
  ///
  /// This is used by high-level entry points like [`crate::JsRuntime::exec_compiled_script`] to
  /// conservatively fall back to the AST interpreter when the compiled executor cannot model the
  /// program.
  ///
  /// Notes:
  /// - **Sync** generator bodies (`yield` / `yield*`) are not supported in the compiled executor;
  ///   sync generator functions are allocated as interpreter-backed ECMAScript functions so their
  ///   bodies execute via the AST evaluator at call-time.
  /// - Async generator bodies are supported by a conservative compiled executor for a limited
  ///   subset of `yield` patterns; unsupported async generators fall back at allocation time.
  /// - Private-name syntax (`#x`, `#m`, ...) is not supported in the compiled executor.
  /// - Async (non-generator) function bodies execute via the compiled async executor when
  ///   supported, falling back to the AST interpreter at call-time for unsupported `await`
  ///   patterns (see [`CompiledScript::async_function_body_requires_ast_fallback`] and
  ///   [`crate::Vm::call_user_function`]).
  ///
  /// Top-level await (classic scripts and modules) is handled by the compiled async executor for a
  /// limited subset of patterns; unsupported forms are tracked separately in
  /// [`CompiledScript::top_level_await_requires_ast_fallback`].
  pub requires_ast_fallback: bool,
  /// Whether this script/module contains a top-level `await` (or `for await..of`) that requires
  /// async evaluation.
  pub contains_top_level_await: bool,
  /// True if top-level await evaluation must fall back to the AST evaluator.
  ///
  /// The compiled async executors are intentionally conservative. For classic scripts, the compiled
  /// executor supports only a small subset of top-level await patterns:
  /// - expression statements of the form `await <expr>;`
  /// - expression statements of the form `x = await <expr>;` (for supported assignment targets)
  /// - `var`/`let`/`const` declarator initializers of the form `= await <expr>`
  /// - simple top-level `for await (<lhs> of <expr>) { ... }` loops, as long as the loop head and
  ///   body contain no other `await`, and the RHS contains no `await` other than an optional
  ///   outer `await <expr>` (with no nested `await` inside `<expr>`)
  ///
  /// Any other top-level await usage (e.g. `await` inside nested blocks, nested `await` inside the
  /// awaited subexpression, or additional `await` inside a `for await..of` loop body) must be
  /// executed via the AST evaluator to avoid partially executing
  /// compiled HIR before discovering an unsupported construct.
  ///
  /// This flag allows callers (notably [`crate::JsRuntime::exec_compiled_script`] for classic
  /// scripts and [`crate::ModuleGraph`] for modules) to choose the AST async evaluator *before
  /// executing any HIR*, avoiding partially-executed side effects.
  pub top_level_await_requires_ast_fallback: bool,
  #[allow(dead_code)]
  source_type: SourceType,
  #[allow(dead_code)]
  external_memory: ExternalMemoryToken,
}

impl CompiledScript {
  /// Parse and lower a classic script (ECMAScript dialect, `SourceType::Script`).
  pub fn compile_script<'a>(
    heap: &mut Heap,
    name: impl Into<SourceTextInput<'a>>,
    text: impl Into<SourceTextInput<'a>>,
  ) -> Result<Arc<CompiledScript>, VmError> {
    let source = arc_try_new_vm(SourceText::new_charged(heap, name, text)?)?;
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    };
 
    let parsed = {
      let parsed_script = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parse_with_options(source.text.as_ref(), opts)
      }))
      .map_err(|_| VmError::InvariantViolation("parse-js panicked while compiling a script"))?;

      match parsed_script {
        Ok(parsed) => parsed,
        Err(script_err) => {
          // Retry classic scripts with top-level `await` enabled ("async classic scripts") without
          // switching to module grammar. This keeps `await` available as an identifier in scripts
          // while still allowing embedders to opt into top-level await.
          let retry = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parse_with_options_cancellable_by_with_init(
              source.text.as_ref(),
              opts,
              || false,
              |p| {
                p.set_allow_top_level_await_in_script(true);
              },
            )
          }))
          .map_err(|_| VmError::InvariantViolation("parse-js panicked while compiling a script"))?;

          match retry {
            Ok(parsed) => parsed,
            Err(_) => {
              let diag =
                crate::parse_diagnostics::parse_js_error_to_diagnostic(&script_err, FileId(0));
              return Err(VmError::Syntax(vec![diag]));
            }
          }
        }
      }
    };

    let contains_top_level_await = parsed.stx.body.iter().any(stmt_contains_await);
    {
      let mut tick = || Ok(());
      let strict = detect_use_strict_directive(source.text.as_ref(), &parsed.stx.body, &mut tick)?;
      crate::early_errors::validate_top_level(
        &parsed.stx.body,
        crate::early_errors::EarlyErrorOptions::script_with_top_level_await(strict, contains_top_level_await),
        Some(source.text.as_ref()),
        &mut tick,
      )?;
    }

    let top_level_await_requires_ast_fallback = contains_top_level_await
      && parsed
        .stx
        .body
        .iter()
        .any(stmt_contains_unsupported_await_for_hir_async_scripts);

    let feature_flags = ast_feature_flags(&parsed);
    let contains_async_generators = feature_flags.contains_async_generators;
    let contains_generators = feature_flags.contains_generators;
    let contains_async_functions = feature_flags.contains_async_functions;
    let contains_private_names = feature_flags.contains_private_names;
    // The compiled (HIR) executor does not yet support private names or all top-level await forms.
    //
    // Generator function bodies execute via the AST evaluator at call-time, so classic scripts that
    // merely define/call generators can still run via the compiled executor.
    //
    // Fall back to the AST interpreter when the script uses unsupported top-level await forms (for
    // example `await` inside nested blocks, or `for await..of` bodies that themselves contain
    // `await`).
    let requires_ast_fallback =
      contains_private_names || top_level_await_requires_ast_fallback;

    let hir = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      hir_js::lower_file(FileId(0), hir_js::FileKind::Js, &parsed)
    }))
    .map_err(|_| VmError::InvariantViolation("hir-js panicked while lowering a script"))?;
    let async_function_body_requires_ast_fallback = async_function_body_requires_ast_fallback(&hir);
    // HIR can be significantly larger than the source text; use a conservative estimate to ensure
    // heap limits apply to compiled code.
    let estimated_hir_bytes = source.text.len().saturating_mul(8);
    let external_memory = heap.charge_external(estimated_hir_bytes)?;
    let hir = arc_try_new_vm(hir)?;
    Ok(arc_try_new_vm(Self {
      source,
      hir,
      contains_async_generators,
      contains_generators,
      contains_async_functions,
      async_function_body_requires_ast_fallback,
      requires_ast_fallback,
      contains_top_level_await,
      top_level_await_requires_ast_fallback,
      source_type: SourceType::Script,
      external_memory,
    })?)
  }

  /// Parse and lower a source text module (ECMAScript dialect, `SourceType::Module`).
  pub fn compile_module<'a>(
    heap: &mut Heap,
    name: impl Into<SourceTextInput<'a>>,
    text: impl Into<SourceTextInput<'a>>,
  ) -> Result<Arc<CompiledScript>, VmError> {
    let source = arc_try_new_vm(SourceText::new_charged(heap, name, text)?)?;
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Module,
    };
    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      parse_with_options(source.text.as_ref(), opts)
    }))
    .map_err(|_| VmError::InvariantViolation("parse-js panicked while compiling a module"))?
    .map_err(|err| {
      let diag = crate::parse_diagnostics::parse_js_error_to_diagnostic(&err, FileId(0));
      VmError::Syntax(vec![diag])
    })?;

    let contains_top_level_await = parsed.stx.body.iter().any(stmt_contains_await);
    let top_level_await_requires_ast_fallback =
      contains_top_level_await && top_level_await_requires_ast_fallback(&parsed.stx.body);

    {
      let mut tick = || Ok(());
      crate::early_errors::validate_top_level(
        &parsed.stx.body,
        crate::early_errors::EarlyErrorOptions::module(),
        Some(source.text.as_ref()),
        &mut tick,
      )?;
      crate::module_record::validate_module_static_semantics_early_errors(&parsed, &mut tick)?;
    }

    let feature_flags = ast_feature_flags(&parsed);
    let contains_async_generators = feature_flags.contains_async_generators;
    let contains_generators = feature_flags.contains_generators;
    let contains_async_functions = feature_flags.contains_async_functions;
    let contains_private_names = feature_flags.contains_private_names;
    // See `compile_script`: generator function bodies execute via per-function AST evaluation, so
    // generator modules can still instantiate/execute through the compiled executor.
    let requires_ast_fallback = contains_private_names || top_level_await_requires_ast_fallback;

    let hir = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      hir_js::lower_file(FileId(0), hir_js::FileKind::Js, &parsed)
    }))
    .map_err(|_| VmError::InvariantViolation("hir-js panicked while lowering a module"))?;
    let async_function_body_requires_ast_fallback = async_function_body_requires_ast_fallback(&hir);

    let estimated_hir_bytes = source.text.len().saturating_mul(8);
    let external_memory = heap.charge_external(estimated_hir_bytes)?;
    let hir = arc_try_new_vm(hir)?;
    Ok(arc_try_new_vm(Self {
      source,
      hir,
      contains_async_generators,
      contains_generators,
      contains_async_functions,
      async_function_body_requires_ast_fallback,
      requires_ast_fallback,
      contains_top_level_await,
      top_level_await_requires_ast_fallback,
      source_type: SourceType::Module,
      external_memory,
    })?)
  }

  /// Parse and lower a classic script using a VM's budget/interrupt checks.
  ///
  /// This is identical to [`CompiledScript::compile_script`], but parsing is performed through the
  /// VM so fuel/deadline/interrupt budgets can be observed *during compilation*.
  pub fn compile_script_with_budget<'a>(
    heap: &mut Heap,
    vm: &mut Vm,
    name: impl Into<SourceTextInput<'a>>,
    text: impl Into<SourceTextInput<'a>>,
  ) -> Result<Arc<CompiledScript>, VmError> {
    let source = arc_try_new_vm(SourceText::new_charged(heap, name, text)?)?;
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    };

    // `Vm::parse_top_level_with_budget` already retries classic scripts with top-level `await`
    // enabled (async classic scripts). Avoid falling back to module grammar so `await` remains a
    // valid identifier in scripts.
    let parsed = vm.parse_top_level_with_budget(&source.text, opts)?;
    let strict = {
      let mut tick = || vm.tick();
      detect_use_strict_directive(source.text.as_ref(), &parsed.stx.body, &mut tick)?
    };
    let has_top_level_await = parsed.stx.body.iter().any(stmt_contains_await);
    let top_level_await_requires_ast_fallback = has_top_level_await
      && parsed
        .stx
        .body
        .iter()
        .any(stmt_contains_unsupported_await_for_hir_async_scripts);
    {
      let mut tick = || vm.tick();
      crate::early_errors::validate_top_level(
        &parsed.stx.body,
        crate::early_errors::EarlyErrorOptions::script_with_top_level_await(strict, has_top_level_await),
        Some(source.text.as_ref()),
        &mut tick,
      )?;
    }

    let feature_flags = ast_feature_flags(&parsed);
    let contains_async_generators = feature_flags.contains_async_generators;
    let contains_generators = feature_flags.contains_generators;
    let contains_async_functions = feature_flags.contains_async_functions;
    let contains_private_names = feature_flags.contains_private_names;
    // See `compile_script`: generator function bodies execute via per-function AST evaluation, so
    // classic scripts that contain generators do not require full-script AST fallback.
    let requires_ast_fallback = contains_private_names || top_level_await_requires_ast_fallback;

    let hir = hir_js::lower_file(FileId(0), hir_js::FileKind::Js, &parsed);
    let async_function_body_requires_ast_fallback = async_function_body_requires_ast_fallback(&hir);
    let estimated_hir_bytes = source.text.len().saturating_mul(8);
    let external_memory = heap.charge_external(estimated_hir_bytes)?;
    let hir = arc_try_new_vm(hir)?;
    Ok(arc_try_new_vm(Self {
      source,
      hir,
      contains_async_generators,
      contains_generators,
      contains_async_functions,
      async_function_body_requires_ast_fallback,
      requires_ast_fallback,
      contains_top_level_await: has_top_level_await,
      top_level_await_requires_ast_fallback,
      source_type: SourceType::Script,
      external_memory,
    })?)
  }

  /// Parse and lower a source text module using a VM's budget/interrupt checks.
  pub fn compile_module_with_budget<'a>(
    heap: &mut Heap,
    vm: &mut Vm,
    name: impl Into<SourceTextInput<'a>>,
    text: impl Into<SourceTextInput<'a>>,
  ) -> Result<Arc<CompiledScript>, VmError> {
    let source = arc_try_new_vm(SourceText::new_charged(heap, name, text)?)?;
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Module,
    };

    let parsed = vm.parse_top_level_with_budget(&source.text, opts)?;
    let contains_top_level_await = parsed.stx.body.iter().any(stmt_contains_await);
    let top_level_await_requires_ast_fallback =
      contains_top_level_await && top_level_await_requires_ast_fallback(&parsed.stx.body);
    {
      let mut tick = || vm.tick();
      crate::early_errors::validate_top_level(
        &parsed.stx.body,
        crate::early_errors::EarlyErrorOptions::module(),
        Some(source.text.as_ref()),
        &mut tick,
      )?;
      crate::module_record::validate_module_static_semantics_early_errors(&parsed, &mut tick)?;
    }
    let feature_flags = ast_feature_flags(&parsed);
    let contains_async_generators = feature_flags.contains_async_generators;
    let contains_generators = feature_flags.contains_generators;
    let contains_async_functions = feature_flags.contains_async_functions;
    let contains_private_names = feature_flags.contains_private_names;
    // See `compile_module`: generator function bodies execute via per-function AST evaluation, so
    // modules that contain generators do not require full-module AST fallback.
    let requires_ast_fallback = contains_private_names || top_level_await_requires_ast_fallback;
    let hir = hir_js::lower_file(FileId(0), hir_js::FileKind::Js, &parsed);
    let async_function_body_requires_ast_fallback = async_function_body_requires_ast_fallback(&hir);
    let estimated_hir_bytes = source.text.len().saturating_mul(8);
    let external_memory = heap.charge_external(estimated_hir_bytes)?;
    let hir = arc_try_new_vm(hir)?;
    Ok(arc_try_new_vm(Self {
      source,
      hir,
      contains_async_generators,
      contains_generators,
      contains_async_functions,
      async_function_body_requires_ast_fallback,
      requires_ast_fallback,
      contains_top_level_await,
      top_level_await_requires_ast_fallback,
      source_type: SourceType::Module,
      external_memory,
    })?)
  }

  /// Lowers an already-parsed module to HIR, reusing the provided [`SourceText`].
  ///
  /// This is used by module compilation APIs that need both:
  /// - module-record metadata (requested modules, import/export entries), and
  /// - compiled HIR for execution.
  ///
  /// Callers are expected to have already run module early-error validation on `parsed`.
  pub(crate) fn compile_module_from_parsed(
    heap: &mut Heap,
    source: Arc<SourceText>,
    parsed: &parse_js::ast::node::Node<parse_js::ast::stx::TopLevel>,
  ) -> Result<Arc<CompiledScript>, VmError> {
    let feature_flags = ast_feature_flags(parsed);
    let contains_async_generators = feature_flags.contains_async_generators;
    let contains_generators = feature_flags.contains_generators;
    let contains_async_functions = feature_flags.contains_async_functions;
    let contains_private_names = feature_flags.contains_private_names;
    let contains_top_level_await = parsed.stx.body.iter().any(stmt_contains_await);
    let top_level_await_requires_ast_fallback =
      contains_top_level_await && top_level_await_requires_ast_fallback(&parsed.stx.body);
    let requires_ast_fallback = contains_private_names || top_level_await_requires_ast_fallback;
    let hir = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      hir_js::lower_file(FileId(0), hir_js::FileKind::Js, parsed)
    }))
    .map_err(|_| VmError::InvariantViolation("hir-js panicked while lowering a module"))?;
    let async_function_body_requires_ast_fallback = async_function_body_requires_ast_fallback(&hir);

    // HIR can be significantly larger than the source text; use a conservative estimate to ensure
    // heap limits apply to compiled code.
    let estimated_hir_bytes = source.text.len().saturating_mul(8);
    let external_memory = heap.charge_external(estimated_hir_bytes)?;
    let hir = arc_try_new_vm(hir)?;
    Ok(arc_try_new_vm(Self {
      source,
      contains_async_generators,
      contains_generators,
      contains_async_functions,
      async_function_body_requires_ast_fallback,
      requires_ast_fallback,
      contains_top_level_await,
      top_level_await_requires_ast_fallback,
      source_type: SourceType::Module,
      external_memory,
      hir,
    })?)
  }
}

/// A reference to a user-defined function body within a [`CompiledScript`].
///
/// This is stored inside `JsFunction` call handlers so closures can outlive the compilation API
/// without holding dangling pointers into ephemeral AST arenas.
#[derive(Debug, Clone)]
pub struct CompiledFunctionRef {
  pub script: Arc<CompiledScript>,
  pub body: hir_js::BodyId,
  /// Optional call-time fallback to the AST interpreter for this compiled function body.
  ///
  /// This is a *per-function* escape hatch: the surrounding script/module can still execute in the
  /// compiled (HIR) path, but this specific function body can fall back to the AST interpreter when
  /// invoked (e.g. when the compiled executor does not yet support some construct such as certain
  /// async `await` forms).
  pub ast_fallback: Option<EcmaFunctionId>,
}

#[derive(Clone, Copy, Debug, Default)]
struct AstFeatureFlags {
  contains_generators: bool,
  contains_async_functions: bool,
  contains_async_generators: bool,
  contains_private_names: bool,
}

fn ast_feature_flags<T: Drive>(root: &T) -> AstFeatureFlags {
  struct FeatureVisitor {
    flags: AstFeatureFlags,
  }
  impl Visitor for FeatureVisitor {
    fn visit(&mut self, item: &dyn std::any::Any, event: Event) {
      if !matches!(event, Event::Enter) {
        return;
      }

      if let Some(func) = item.downcast_ref::<Func>() {
        self.flags.contains_generators |= func.generator;
        self.flags.contains_async_functions |= func.async_;
        self.flags.contains_async_generators |= func.async_ && func.generator;
      }

      // Private names (`#x`, `#m`, ...) appear in a few AST shapes:
      // - class body element keys (`class C { #x; }`)
      // - member expressions (`obj.#x`)
      // - `#x in obj` private-brand-check operator (`BinaryExpression` LHS parsed as an identifier-like node)
      //
      // The compiled (HIR) executor does not yet support these, so compiled scripts containing them
      // must fall back to the AST interpreter.
      if let Some(key) = item.downcast_ref::<parse_js::ast::class_or_object::ClassOrObjMemberDirectKey>() {
        if key.tt == TT::PrivateMember {
          self.flags.contains_private_names = true;
        }
      }
      if let Some(id) = item.downcast_ref::<IdExpr>() {
        if id.name.starts_with('#') {
          self.flags.contains_private_names = true;
        }
      }
      if let Some(id) = item.downcast_ref::<IdPat>() {
        if id.name.starts_with('#') {
          self.flags.contains_private_names = true;
        }
      }
      if let Some(mem) = item.downcast_ref::<MemberExpr>() {
        if mem.right.starts_with('#') {
          self.flags.contains_private_names = true;
        }
      }
    }
  }

  let mut visitor = FeatureVisitor {
    flags: AstFeatureFlags::default(),
  };
  root.drive(&mut visitor);
  visitor.flags
}

fn detect_use_strict_directive<F>(
  source: &str,
  stmts: &[Node<Stmt>],
  tick: &mut F,
) -> Result<bool, VmError>
where
  F: FnMut() -> Result<(), VmError>,
{
  const TICK_EVERY: usize = 32;
  for (i, stmt) in stmts.iter().enumerate() {
    if i % TICK_EVERY == 0 {
      tick()?;
    }
    let Stmt::Expr(expr_stmt) = &*stmt.stx else {
      break;
    };
    let expr = &expr_stmt.stx.expr;
    // Parenthesized string literals are not directive prologues.
    if expr.assoc.get::<ParenthesizedExpr>().is_some() {
      break;
    }
    let Expr::LitStr(lit) = &*expr.stx else {
      break;
    };
    if lit.stx.value == "use strict" {
      let start = expr.loc.0.min(source.len());
      let end = expr.loc.1.min(source.len());
      let raw = source.get(start..end).unwrap_or("");
      if raw == "\"use strict\"" || raw == "'use strict'" {
        return Ok(true);
      }
    }
  }
  Ok(false)
}

fn expr_contains_await(expr: &Node<Expr>) -> bool {
  match &*expr.stx {
    Expr::Unary(unary) => {
      matches!(
        unary.stx.operator,
        OperatorName::Await | OperatorName::Yield | OperatorName::YieldDelegated
      ) || expr_contains_await(&unary.stx.argument)
    }
    Expr::UnaryPostfix(unary) => expr_contains_await(&unary.stx.argument),
    Expr::Binary(binary) => expr_contains_await(&binary.stx.left) || expr_contains_await(&binary.stx.right),
    Expr::Cond(cond) => {
      expr_contains_await(&cond.stx.test)
        || expr_contains_await(&cond.stx.consequent)
        || expr_contains_await(&cond.stx.alternate)
    }
    Expr::Member(member) => expr_contains_await(&member.stx.left),
    Expr::ComputedMember(member) => {
      expr_contains_await(&member.stx.object) || expr_contains_await(&member.stx.member)
    }
    Expr::Call(call) => {
      expr_contains_await(&call.stx.callee)
        || call
          .stx
          .arguments
          .iter()
          .any(|arg| expr_contains_await(&arg.stx.value))
    }
    Expr::Import(import) => {
      expr_contains_await(&import.stx.module)
        || import
          .stx
          .attributes
          .as_ref()
          .is_some_and(|attrs| expr_contains_await(attrs))
    }
    Expr::TaggedTemplate(tag) => {
      expr_contains_await(&tag.stx.function)
        || tag.stx.parts.iter().any(|part| match part {
          LitTemplatePart::Substitution(expr) => expr_contains_await(expr),
          LitTemplatePart::String(_) => false,
        })
    }
    Expr::LitArr(arr) => arr.stx.elements.iter().any(|elem| match elem {
      LitArrElem::Single(expr) | LitArrElem::Rest(expr) => expr_contains_await(expr),
      LitArrElem::Empty => false,
    }),
    Expr::LitObj(obj) => obj.stx.members.iter().any(|member| match &member.stx.typ {
      ObjMemberType::Valued { key, val } => {
        let key_has_await = match key {
          ClassOrObjKey::Direct(_) => false,
          ClassOrObjKey::Computed(expr) => expr_contains_await(expr),
        };

        let val_has_await = match val {
          ClassOrObjVal::Prop(Some(expr)) => expr_contains_await(expr),
          ClassOrObjVal::Prop(None) => false,
          // Function-valued members: the function body is not evaluated at object creation time.
          ClassOrObjVal::Getter(_)
          | ClassOrObjVal::Setter(_)
          | ClassOrObjVal::Method(_)
          | ClassOrObjVal::IndexSignature(_)
          | ClassOrObjVal::StaticBlock(_) => false,
        };

        key_has_await || val_has_await
      }
      ObjMemberType::Shorthand { .. } => false,
      ObjMemberType::Rest { val } => expr_contains_await(val),
    }),
    Expr::LitTemplate(tpl) => tpl.stx.parts.iter().any(|part| match part {
      LitTemplatePart::Substitution(expr) => expr_contains_await(expr),
      LitTemplatePart::String(_) => false,
    }),
    Expr::ArrPat(arr) => arr_pat_contains_await(&arr.stx),
    Expr::ObjPat(obj) => obj_pat_contains_await(&obj.stx),

    // Nested functions are not evaluated when the function value is created.
    Expr::Func(_) | Expr::ArrowFunc(_) => false,

    Expr::Class(class) => {
      class.stx.extends.as_ref().is_some_and(expr_contains_await)
        || class.stx.members.iter().any(|member| {
          let key_has_await = match &member.stx.key {
            ClassOrObjKey::Direct(_) => false,
            ClassOrObjKey::Computed(expr) => expr_contains_await(expr),
          };
          if key_has_await {
            return true;
          }
          match &member.stx.val {
            // Class static blocks are parsed in a `~Await` context, so `await` expressions are not
            // permitted.
            ClassOrObjVal::StaticBlock(_) => false,
            _ => false,
          }
        })
    }

    // TypeScript-only nodes: only the wrapped expression is evaluated.
    Expr::Instantiation(inst) => expr_contains_await(&inst.stx.expression),
    Expr::TypeAssertion(expr) => expr_contains_await(&expr.stx.expression),
    Expr::NonNullAssertion(expr) => expr_contains_await(&expr.stx.expression),
    Expr::SatisfiesExpr(expr) => expr_contains_await(&expr.stx.expression),

    _ => false,
  }
}

fn expr_direct_await_arg(expr: &Node<Expr>) -> Option<&Node<Expr>> {
  let Expr::Unary(unary) = &*expr.stx else {
    return None;
  };
  if unary.stx.operator == OperatorName::Await {
    Some(&unary.stx.argument)
  } else {
    None
  }
}

fn expr_is_direct_await_without_nested_await(expr: &Node<Expr>) -> bool {
  expr_direct_await_arg(expr).is_some_and(|arg| !expr_contains_await(arg))
}

fn expr_is_supported_assignment_with_direct_await_rhs_without_nested_await(expr: &Node<Expr>) -> bool {
  let Expr::Binary(binary) = &*expr.stx else {
    return false;
  };
  if !operator_is_supported_assignment_for_hir_async_scripts(binary.stx.operator) {
    return false;
  }
  let Some(arg) = expr_direct_await_arg(&binary.stx.right) else {
    return false;
  };
  if !expr_is_supported_assignment_target_for_hir_async_scripts(&binary.stx.left) {
    return false;
  }
  // The compiled evaluator does not support nested `await` within the assignment target (including
  // computed member keys) or within the awaited operand.
  !expr_contains_await(&binary.stx.left) && !expr_contains_await(arg)
}

fn expr_is_logical_assignment_with_direct_await_rhs_without_nested_await(expr: &Node<Expr>) -> bool {
  let Expr::Binary(binary) = &*expr.stx else {
    return false;
  };
  if !matches!(
    binary.stx.operator,
    OperatorName::AssignmentLogicalAnd
      | OperatorName::AssignmentLogicalOr
      | OperatorName::AssignmentNullishCoalescing
  ) {
    return false;
  }
  let Some(arg) = expr_direct_await_arg(&binary.stx.right) else {
    return false;
  };
  if !expr_is_supported_assignment_target_for_hir_async_scripts(&binary.stx.left) {
    return false;
  }
  // The compiled evaluator does not support nested `await` within the assignment target (including
  // computed member keys) or within the awaited operand.
  !expr_contains_await(&binary.stx.left) && !expr_contains_await(arg)
}

fn expr_is_destructuring_assignment_with_direct_await_rhs_without_nested_await(expr: &Node<Expr>) -> bool {
  let Expr::Binary(binary) = &*expr.stx else {
    return false;
  };
  if binary.stx.operator != OperatorName::Assignment {
    return false;
  }
  let Some(arg) = expr_direct_await_arg(&binary.stx.right) else {
    return false;
  };
  // Restrict this fast-path to actual destructuring patterns. `parse-js` represents destructuring
  // assignment targets using dedicated AST nodes (e.g. `ObjPat` / `ArrPat`).
  match &*binary.stx.left.stx {
    Expr::ArrPat(_) | Expr::ObjPat(_) => {}
    _ => return false,
  };
  // The compiled evaluator does not support `await` in the destructuring pattern itself (computed
  // keys / default values) or nested `await` inside the awaited operand.
  !expr_contains_await(&binary.stx.left) && !expr_contains_await(arg)
}

fn expr_is_supported_assignment_target_for_hir_async_scripts(expr: &Node<Expr>) -> bool {
  match &*expr.stx {
    // Note: `parse-js` represents identifier assignment targets using the `IdPat` AST node (because
    // assignment targets can be destructuring patterns). Treat both `Id` and `IdPat` as supported
    // binding assignment targets for the HIR async script executor.
    Expr::Id(_) | Expr::IdPat(_) | Expr::Member(_) | Expr::ComputedMember(_) => true,
    // TypeScript-only wrappers.
    Expr::Instantiation(inst) => expr_is_supported_assignment_target_for_hir_async_scripts(&inst.stx.expression),
    Expr::TypeAssertion(expr) => expr_is_supported_assignment_target_for_hir_async_scripts(&expr.stx.expression),
    Expr::NonNullAssertion(expr) => expr_is_supported_assignment_target_for_hir_async_scripts(&expr.stx.expression),
    Expr::SatisfiesExpr(expr) => expr_is_supported_assignment_target_for_hir_async_scripts(&expr.stx.expression),
    _ => false,
  }
}

fn operator_is_supported_assignment_for_hir_async_scripts(op: OperatorName) -> bool {
  // The compiled async classic-script executor supports only plain (`=`), arithmetic/bitwise
  // compound assignment, and logical assignment forms whose RHS is a direct `await <expr>`.
  matches!(
    op,
    OperatorName::Assignment
      | OperatorName::AssignmentAddition
      | OperatorName::AssignmentSubtraction
      | OperatorName::AssignmentMultiplication
      | OperatorName::AssignmentDivision
      | OperatorName::AssignmentRemainder
      | OperatorName::AssignmentExponentiation
      | OperatorName::AssignmentBitwiseLeftShift
      | OperatorName::AssignmentBitwiseRightShift
      | OperatorName::AssignmentBitwiseUnsignedRightShift
      | OperatorName::AssignmentBitwiseOr
      | OperatorName::AssignmentBitwiseAnd
      | OperatorName::AssignmentBitwiseXor
      | OperatorName::AssignmentLogicalAnd
      | OperatorName::AssignmentLogicalOr
      | OperatorName::AssignmentNullishCoalescing
  )
}

fn stmt_contains_unsupported_await_for_hir_async_scripts(stmt: &Node<Stmt>) -> bool {
  fn for_await_of_contains_unsupported_await_for_hir_async_scripts(for_of: &Node<ForOfStmt>) -> bool {
    if for_in_of_lhs_contains_await(&for_of.stx.lhs) {
      return true;
    }
    // `for await (x of await <expr>)` is supported because the loop state machine can suspend while
    // evaluating the RHS expression.
    if let Some(arg) = expr_direct_await_arg(&for_of.stx.rhs) {
      if expr_contains_await(arg) {
        return true;
      }
    } else if expr_contains_await(&for_of.stx.rhs) {
      return true;
    }
    if for_of.stx.body.stx.body.iter().any(stmt_contains_await) {
      return true;
    }

    if let ForInOfLhs::Decl((mode, _)) = &for_of.stx.lhs {
      if !matches!(mode, VarDeclMode::Var | VarDeclMode::Let | VarDeclMode::Const) {
        return true;
      }
    }

    false
  }

  fn for_triple_head_expr_supported(expr: &Node<Expr>, allow_assignment: bool) -> bool {
    if !expr_contains_await(expr) {
      return true;
    }
    if expr_is_direct_await_without_nested_await(expr) {
      return true;
    }
    allow_assignment
      && (expr_is_supported_assignment_with_direct_await_rhs_without_nested_await(expr)
        || expr_is_destructuring_assignment_with_direct_await_rhs_without_nested_await(expr))
  }

  fn for_triple_contains_unsupported_await_for_hir_async_scripts(for_stmt: &Node<ForTripleStmt>) -> bool {
    // Loop body must not contain any other `await`.
    if for_stmt.stx.body.stx.body.iter().any(stmt_contains_await) {
      return true;
    }

    // Init position.
    match &for_stmt.stx.init {
      ForTripleStmtInit::None => {}
      ForTripleStmtInit::Expr(expr) => {
        if !for_triple_head_expr_supported(expr, /* allow_assignment */ true) {
          return true;
        }
      }
      ForTripleStmtInit::Decl(decl) => {
        if decl.stx.declarators.iter().any(|d| {
          if pat_contains_await(&d.pattern.stx.pat.stx) {
            return true;
          }
          let Some(init) = d.initializer.as_ref() else {
            return false;
          };

          if let Some(arg) = expr_direct_await_arg(init) {
            // Support direct `await <expr>` initializers for `var`/`let`/`const` declarations only.
            if !matches!(decl.stx.mode, VarDeclMode::Var | VarDeclMode::Let | VarDeclMode::Const) {
              return true;
            }
            // Reject nested awaits inside the awaited operand.
            expr_contains_await(arg)
          } else {
            expr_contains_await(init)
          }
        }) {
          return true;
        }
      }
    }

    // Test position.
    if let Some(test) = for_stmt.stx.cond.as_ref() {
      if !for_triple_head_expr_supported(test, /* allow_assignment */ false) {
        return true;
      }
    }

    // Update position.
    if let Some(post) = for_stmt.stx.post.as_ref() {
      if !for_triple_head_expr_supported(post, /* allow_assignment */ true) {
        return true;
      }
    }

    false
  }

  match &*stmt.stx {
    // Supported async classic script forms for the compiled (HIR) executor:
    // - `await <expr>;`
    // - `x = await <expr>;`
    // - `x += await <expr>;` (and other arithmetic/bitwise compound assignment operators)
    // - `x ||= await <expr>;` / `x &&= await <expr>;` / `x ??= await <expr>;`
    // - `({ ... } = await <expr>);` / `[ ... ] = await <expr>;` (destructuring assignment patterns)
    // - `throw await <expr>;`
    // - `const x = await <expr>;` (and `var`/`let`)
    // - `for (init; test; update) { ... }` loops where the head may contain direct `await` (and
    //   assignments with direct `await <expr>` RHS, including destructuring assignments) in the
    //   init/test/update positions, and the loop body contains no other `await`
    // - `for await (<head> of <rhs>) { ... }` where:
      //   - `<rhs>` is either a normal expression with no `await`, or a direct `await <expr>` with no
      //     nested `await` inside `<expr>`, and
    //   - the loop head + body contain no other `await`
    //
    // Any other `await` / `for await..of` form must fall back to the AST interpreter.
    Stmt::Expr(expr_stmt) => {
      let expr = &expr_stmt.stx.expr;
      if let Some(arg) = expr_direct_await_arg(expr) {
        // Nested awaits inside the await argument are not supported by the compiled executor.
        return expr_contains_await(arg);
      }

      if let Expr::Binary(binary) = &*expr.stx {
        if binary.stx.operator.is_assignment() {
          if let Some(arg) = expr_direct_await_arg(&binary.stx.right) {
            // Destructuring assignment patterns evaluate the RHS first (unlike plain assignment
            // targets, which must evaluate the reference before the RHS).
            //
            // Restrict this fast-path to plain `=` destructuring assignments only.
            if matches!(&*binary.stx.left.stx, Expr::ArrPat(_) | Expr::ObjPat(_)) {
              if binary.stx.operator != OperatorName::Assignment {
                return true;
              }
              // Nested awaits inside the awaited operand (or in computed keys/defaults within the
              // pattern) are not supported by the compiled executor.
              return expr_contains_await(&binary.stx.left) || expr_contains_await(arg);
            }

            if !operator_is_supported_assignment_for_hir_async_scripts(binary.stx.operator) {
              return true;
            }
            if !expr_is_supported_assignment_target_for_hir_async_scripts(&binary.stx.left) {
              return true;
            }
            // Nested awaits inside the await argument (or in computed member keys) are not
            // supported by the compiled executor.
            return expr_contains_await(&binary.stx.left) || expr_contains_await(arg);
          }
        }
      }

      expr_contains_await(expr)
    }
    Stmt::Throw(throw_stmt) => {
      let expr = &throw_stmt.stx.value;
      if !expr_contains_await(expr) {
        return false;
      }
      match &*expr.stx {
        Expr::Unary(unary) if unary.stx.operator == OperatorName::Await => {
          // The compiled evaluator does not yet support nested `await` inside the awaited operand.
          expr_contains_await(&unary.stx.argument)
        }
        _ => true,
      }
    }
    Stmt::VarDecl(decl) => decl.stx.declarators.iter().any(|d| {
      if pat_contains_await(&d.pattern.stx.pat.stx) {
        return true;
      }
      let Some(init) = d.initializer.as_ref() else {
        return false;
      };
      if let Some(arg) = expr_direct_await_arg(init) {
        expr_contains_await(arg)
      } else {
        expr_contains_await(init)
      }
    }),
    Stmt::ForOf(for_of) => {
      if !for_of.stx.await_ {
        // `for (x of xs) { await ... }` is not supported in the compiled async script executor.
        return stmt_contains_await(stmt);
      }

      for_await_of_contains_unsupported_await_for_hir_async_scripts(for_of)
    }
    Stmt::ForTriple(for_stmt) => for_triple_contains_unsupported_await_for_hir_async_scripts(for_stmt),
    Stmt::Label(label_stmt) => {
      // Support labelled top-level loops by deferring to the same constraints as an unlabelled
      // loop.
      let mut inner = &label_stmt.stx.statement;
      while let Stmt::Label(nested) = &*inner.stx {
        inner = &nested.stx.statement;
      }
      match &*inner.stx {
        Stmt::ForOf(for_of) => {
          if !for_of.stx.await_ {
            return stmt_contains_await(stmt);
          }
          for_await_of_contains_unsupported_await_for_hir_async_scripts(for_of)
        }
        Stmt::ForTriple(for_stmt) => {
          for_triple_contains_unsupported_await_for_hir_async_scripts(for_stmt)
        }
        _ => stmt_contains_await(stmt),
      }
    }
    // Other statement kinds must not contain `await` / `for await..of`.
    _ => stmt_contains_await(stmt),
  }
}

fn pat_contains_await(pat: &Pat) -> bool {
  match pat {
    Pat::Id(_) => false,
    Pat::Obj(obj) => obj_pat_contains_await(&obj.stx),
    Pat::Arr(arr) => arr_pat_contains_await(&arr.stx),
    Pat::AssignTarget(expr) => expr_contains_await(expr),
  }
}

fn obj_pat_contains_await(pat: &ObjPat) -> bool {
  pat
    .properties
    .iter()
    .any(|prop| {
      let key_has_await = match &prop.stx.key {
        ClassOrObjKey::Direct(_) => false,
        ClassOrObjKey::Computed(expr) => expr_contains_await(expr),
      };
      key_has_await
        || pat_contains_await(&prop.stx.target.stx)
        || prop.stx.default_value.as_ref().is_some_and(expr_contains_await)
    })
    || pat.rest.as_ref().is_some_and(|rest| pat_contains_await(&rest.stx))
}

fn arr_pat_contains_await(pat: &ArrPat) -> bool {
  pat
    .elements
    .iter()
    .any(|elem| match elem {
      Some(elem) => {
        pat_contains_await(&elem.target.stx)
          || elem.default_value.as_ref().is_some_and(expr_contains_await)
      }
      None => false,
    })
    || pat.rest.as_ref().is_some_and(|rest| pat_contains_await(&rest.stx))
}

fn for_in_of_lhs_contains_await(lhs: &ForInOfLhs) -> bool {
  match lhs {
    ForInOfLhs::Decl((mode, pat_decl)) => {
      // `await using` declarations require `await`-permitted (async) evaluation even when the
      // initializer expression itself contains no `AwaitExpression`.
      //
      // Treat them as containing top-level `await` so classic-script compilation enables async
      // evaluation and early errors can validate the `await` context correctly.
      matches!(*mode, VarDeclMode::AwaitUsing) || pat_contains_await(&pat_decl.stx.pat.stx)
    }
    ForInOfLhs::Assign(pat) => pat_contains_await(&pat.stx),
  }
}

fn stmt_contains_await(stmt: &Node<Stmt>) -> bool {
  match &*stmt.stx {
    Stmt::Empty(_)
    | Stmt::Debugger(_)
    | Stmt::Import(_)
    | Stmt::ExportList(_)
    | Stmt::FunctionDecl(_)
    | Stmt::Break(_)
    | Stmt::Continue(_) => false,
    Stmt::ExportDefaultExpr(stmt) => expr_contains_await(&stmt.stx.expression),
    Stmt::ClassDecl(class) => {
      class.stx.extends.as_ref().is_some_and(expr_contains_await)
        || class.stx.members.iter().any(|member| {
          let key_has_await = match &member.stx.key {
            ClassOrObjKey::Direct(_) => false,
            ClassOrObjKey::Computed(expr) => expr_contains_await(expr),
          };
          if key_has_await {
            return true;
          }
          match &member.stx.val {
            // Class static blocks are parsed in a `~Await` context, so `await` expressions are not
            // permitted.
            ClassOrObjVal::StaticBlock(_) => false,
            _ => false,
          }
        })
    }
    Stmt::Expr(expr_stmt) => expr_contains_await(&expr_stmt.stx.expr),
    Stmt::Return(ret) => ret.stx.value.as_ref().is_some_and(expr_contains_await),
    Stmt::Throw(throw_stmt) => expr_contains_await(&throw_stmt.stx.value),
    Stmt::VarDecl(decl) => {
      // `await using` requires async evaluation even when the initializer has no `await`.
      if decl.stx.mode == VarDeclMode::AwaitUsing {
        return true;
      }
      decl.stx.declarators.iter().any(|d| {
        d.initializer.as_ref().is_some_and(expr_contains_await)
          || pat_contains_await(&d.pattern.stx.pat.stx)
      })
    }
    Stmt::Block(block) => block.stx.body.iter().any(stmt_contains_await),
    Stmt::If(if_stmt) => {
      expr_contains_await(&if_stmt.stx.test)
        || stmt_contains_await(&if_stmt.stx.consequent)
        || if_stmt.stx.alternate.as_ref().is_some_and(stmt_contains_await)
    }
    Stmt::Try(try_stmt) => {
      let catch_has_await = try_stmt.stx.catch.as_ref().is_some_and(|c| {
        c.stx
          .parameter
          .as_ref()
          .is_some_and(|p| pat_contains_await(&p.stx.pat.stx))
          || c.stx.body.iter().any(stmt_contains_await)
      });

      try_stmt.stx.wrapped.stx.body.iter().any(stmt_contains_await)
        || catch_has_await
        || try_stmt
          .stx
          .finally
          .as_ref()
          .is_some_and(|f| f.stx.body.iter().any(stmt_contains_await))
    }
    Stmt::With(with_stmt) => {
      expr_contains_await(&with_stmt.stx.object) || stmt_contains_await(&with_stmt.stx.body)
    }
    Stmt::While(while_stmt) => {
      expr_contains_await(&while_stmt.stx.condition) || stmt_contains_await(&while_stmt.stx.body)
    }
    Stmt::DoWhile(do_while) => {
      expr_contains_await(&do_while.stx.condition) || stmt_contains_await(&do_while.stx.body)
    }
    Stmt::ForTriple(for_stmt) => {
      let init_has_await = match &for_stmt.stx.init {
        ForTripleStmtInit::None => false,
        ForTripleStmtInit::Expr(expr) => expr_contains_await(expr),
        ForTripleStmtInit::Decl(decl) => {
          if decl.stx.mode == VarDeclMode::AwaitUsing {
            true
          } else {
            decl.stx.declarators.iter().any(|d| {
              d.initializer.as_ref().is_some_and(expr_contains_await)
                || pat_contains_await(&d.pattern.stx.pat.stx)
            })
          }
        }
      };

      init_has_await
        || for_stmt.stx.cond.as_ref().is_some_and(expr_contains_await)
        || for_stmt.stx.post.as_ref().is_some_and(expr_contains_await)
        || for_stmt
          .stx
          .body
          .stx
          .body
          .iter()
          .any(stmt_contains_await)
    }
    Stmt::ForIn(for_in) => {
      for_in_of_lhs_contains_await(&for_in.stx.lhs)
        || expr_contains_await(&for_in.stx.rhs)
        || for_in.stx.body.stx.body.iter().any(stmt_contains_await)
    }
    Stmt::ForOf(for_of) => {
      for_of.stx.await_
        || for_in_of_lhs_contains_await(&for_of.stx.lhs)
        || expr_contains_await(&for_of.stx.rhs)
        || for_of.stx.body.stx.body.iter().any(stmt_contains_await)
    }
    Stmt::Switch(switch_stmt) => {
      expr_contains_await(&switch_stmt.stx.test)
        || switch_stmt.stx.branches.iter().any(|branch| {
          branch
            .stx
            .case
            .as_ref()
            .is_some_and(expr_contains_await)
            || branch.stx.body.iter().any(stmt_contains_await)
        })
    }
    Stmt::Label(label) => stmt_contains_await(&label.stx.statement),
    // Conservatively assume unsupported statement kinds do not contain await so we preserve the
    // existing synchronous evaluator behaviour for them.
    _ => false,
  }
}

/// Returns true if the compiled/HIR async evaluator must fall back to the AST evaluator for this
/// top-level statement list.
///
/// The compiled async evaluator is intentionally conservative: it only supports suspension at
/// specific, well-scoped boundaries that can be resumed without retaining `parse-js` AST pointers.
/// Any top-level await outside of these supported shapes requires an AST fallback to ensure
/// correctness (before executing any HIR).
fn top_level_await_requires_ast_fallback(stmts: &[Node<Stmt>]) -> bool {
  fn for_triple_head_expr_supported(expr: &Node<Expr>, allow_assignment: bool) -> bool {
    if !expr_contains_await(expr) {
      return true;
    }
    if expr_is_direct_await_without_nested_await(expr) {
      return true;
    }
    allow_assignment
      && (expr_is_supported_assignment_with_direct_await_rhs_without_nested_await(expr)
        || expr_is_logical_assignment_with_direct_await_rhs_without_nested_await(expr)
        || expr_is_destructuring_assignment_with_direct_await_rhs_without_nested_await(expr))
  }

  fn for_triple_stmt_supported(for_stmt: &Node<ForTripleStmt>) -> bool {
    // Loop body must not contain any other `await`.
    if for_stmt.stx.body.stx.body.iter().any(stmt_contains_await) {
      return false;
    }

    let init_supported = match &for_stmt.stx.init {
      ForTripleStmtInit::None => true,
      ForTripleStmtInit::Expr(expr) => for_triple_head_expr_supported(expr, /* allow_assignment */ true),
      ForTripleStmtInit::Decl(decl) => decl.stx.declarators.iter().all(|d| {
        if pat_contains_await(&d.pattern.stx.pat.stx) {
          return false;
        }
        let Some(init) = d.initializer.as_ref() else {
          return true;
        };

        if let Some(arg) = expr_direct_await_arg(init) {
          // Support direct `await <expr>` initializers for `var`/`let`/`const` only.
          if !matches!(decl.stx.mode, VarDeclMode::Var | VarDeclMode::Let | VarDeclMode::Const) {
            return false;
          }
          !expr_contains_await(arg)
        } else {
          !expr_contains_await(init)
        }
      }),
    };
    if !init_supported {
      return false;
    }

    let test_supported = for_stmt
      .stx
      .cond
      .as_ref()
      .is_none_or(|test| for_triple_head_expr_supported(test, /* allow_assignment */ false));
    if !test_supported {
      return false;
    }

    for_stmt
      .stx
      .post
      .as_ref()
      .is_none_or(|post| for_triple_head_expr_supported(post, /* allow_assignment */ true))
  }

  fn for_of_stmt_supported_with_async_head(for_of: &Node<ForOfStmt>) -> bool {
    if for_of.stx.await_ {
      return false;
    }

    // The compiled evaluator's `ForOfState` only supports async suspension while binding the loop
    // head object pattern (computed keys / defaults). It does not support `await` elsewhere in the
    // loop.
    if expr_contains_await(&for_of.stx.rhs) {
      return false;
    }
    if for_of.stx.body.stx.body.iter().any(stmt_contains_await) {
      return false;
    }

    let ForInOfLhs::Decl((_mode, pat_decl)) = &for_of.stx.lhs else {
      return false;
    };
    let Pat::Obj(obj_pat) = &*pat_decl.stx.pat.stx else {
      return false;
    };
    if obj_pat.stx.rest.is_some() {
      // `AsyncObjectPatternBindingState` does not support rest patterns.
      return false;
    }

    let mut has_await: bool = false;
    for prop in obj_pat.stx.properties.iter() {
      // Only allow `await` in computed keys and defaults, and only as a direct `await <expr>` with
      // no nested `await` inside `<expr>`.
      if let ClassOrObjKey::Computed(expr) = &prop.stx.key {
        if expr_contains_await(expr) {
          if !expr_is_direct_await_without_nested_await(expr) {
            return false;
          }
          has_await = true;
        }
      }

      // The property binding target pattern must not contain `await` (including nested defaults or
      // computed keys).
      if pat_contains_await(&prop.stx.target.stx) {
        return false;
      }

      if let Some(default) = prop.stx.default_value.as_ref() {
        if expr_contains_await(default) {
          if !expr_is_direct_await_without_nested_await(default) {
            return false;
          }
          has_await = true;
        }
      }
    }

    // Require an actual `await` in the supported head positions (computed key/default). This avoids
    // accepting `await using` loop heads (which require async semantics even without an
    // `AwaitExpression`) since `ForOfState` does not currently handle that case.
    has_await
  }

  fn for_await_of_stmt_supported(for_of: &Node<ForOfStmt>) -> bool {
    if !for_of.stx.await_ {
      return false;
    }

    // The compiled evaluator's `ForAwaitOfState` supports suspension while binding the loop head
    // object pattern (computed keys / defaults), but only for `var`/`let`/`const` loop heads and
    // only for direct `await <expr>` forms in those head positions.
    //
    // Reject `await using` loop heads for now: they add async resource-management semantics that are
    // not modeled by `ForAwaitOfState`.
    if let ForInOfLhs::Decl((mode, pat_decl)) = &for_of.stx.lhs {
      if *mode == VarDeclMode::AwaitUsing {
        return false;
      }

      let head_pat = &pat_decl.stx.pat.stx;
      if pat_contains_await(head_pat) {
        // Only support `await` in object-pattern computed keys / defaults.
        let Pat::Obj(obj_pat) = &**head_pat else {
          return false;
        };
        if obj_pat.stx.rest.is_some() {
          // `AsyncObjectPatternBindingState` does not support rest patterns.
          return false;
        }

        let mut has_await: bool = false;
        for prop in obj_pat.stx.properties.iter() {
          // Only allow `await` in computed keys and defaults, and only as a direct `await <expr>`
          // with no nested `await` inside `<expr>`.
          if let ClassOrObjKey::Computed(expr) = &prop.stx.key {
            if expr_contains_await(expr) {
              if !expr_is_direct_await_without_nested_await(expr) {
                return false;
              }
              has_await = true;
            }
          }

          // Property target patterns are evaluated synchronously and must not contain `await`
          // (including nested defaults or computed keys).
          if pat_contains_await(&prop.stx.target.stx) {
            return false;
          }

          if let Some(default) = prop.stx.default_value.as_ref() {
            if expr_contains_await(default) {
              if !expr_is_direct_await_without_nested_await(default) {
                return false;
              }
              has_await = true;
            }
          }
        }

        if !has_await {
          return false;
        }
      }
    } else {
      // The compiled evaluator does not support `await` in non-declaration loop heads.
      if for_in_of_lhs_contains_await(&for_of.stx.lhs) {
        return false;
      }
    }

    let rhs = &for_of.stx.rhs;
    let rhs_supported = if let Some(arg) = expr_direct_await_arg(rhs) {
      !expr_contains_await(arg)
    } else {
      !expr_contains_await(rhs)
    };
    rhs_supported && !for_of.stx.body.stx.body.iter().any(stmt_contains_await)
  }

  fn var_decl_stmt_supported(decl: &Node<parse_js::ast::stmt::decl::VarDecl>) -> bool {
    fn obj_pat_has_supported_await_in_keys_or_defaults(pat: &Pat) -> bool {
      let Pat::Obj(obj_pat) = pat else {
        return false;
      };
      if obj_pat.stx.rest.is_some() {
        // `AsyncObjectPatternBindingState` does not support object rest patterns.
        return false;
      }
      let mut has_await: bool = false;
      for prop in obj_pat.stx.properties.iter() {
        // Only allow `await` in computed keys and defaults, and only as a direct `await <expr>` with
        // no nested `await` inside `<expr>`.
        if let ClassOrObjKey::Computed(expr) = &prop.stx.key {
          if expr_contains_await(expr) {
            if !expr_is_direct_await_without_nested_await(expr) {
              return false;
            }
            has_await = true;
          }
        }

        // Property target patterns must not contain any `await` (including nested computed keys /
        // defaults).
        if pat_contains_await(&prop.stx.target.stx) {
          return false;
        }

        if let Some(default) = prop.stx.default_value.as_ref() {
          if expr_contains_await(default) {
            if !expr_is_direct_await_without_nested_await(default) {
              return false;
            }
            has_await = true;
          }
        }
      }
      has_await
    }

    decl.stx.declarators.iter().all(|d| {
      let pat = &d.pattern.stx.pat.stx;

      // Support async binding initialization for object patterns with `await` in computed keys or
      // default values (see `AsyncObjectPatternBindingState`).
      if pat_contains_await(pat) {
        // Avoid mixing pattern-`await` support with Explicit Resource Management (using/await using)
        // for now.
        if !matches!(decl.stx.mode, VarDeclMode::Var | VarDeclMode::Let | VarDeclMode::Const) {
          return false;
        }

        if !obj_pat_has_supported_await_in_keys_or_defaults(pat) {
          return false;
        }

        // RHS is evaluated before destructuring and must be synchronous (no await).
        let Some(init) = d.initializer.as_ref() else {
          return false;
        };
        return !expr_contains_await(init);
      }

      let Some(init) = d.initializer.as_ref() else {
        return true;
      };

      if !expr_contains_await(init) {
        return true;
      }

      match &*init.stx {
        Expr::Unary(unary) if unary.stx.operator == OperatorName::Await => {
          !expr_contains_await(&unary.stx.argument)
        }
        _ => false,
      }
    })
  }

  fn class_decl_stmt_supported(decl: &Node<parse_js::ast::stmt::decl::ClassDecl>) -> bool {
    // Support async class declarations with `await` in:
    // - `extends` (heritage), or
    // - computed member keys (method/getter/setter).
    //
    // This matches the compiled async evaluator's `AsyncClassDeclState`, which is intentionally
    // limited: it does not currently support class fields, and it does not support `await` inside
    // static initialization blocks or nested within the awaited operand.

    if !decl.stx.decorators.is_empty() {
      // Decorators are not supported by the compiled class evaluator.
      return false;
    }
    if decl.stx.declare || decl.stx.abstract_ {
      return false;
    }
    if decl.stx.type_parameters.is_some() || !decl.stx.implements.is_empty() {
      // TypeScript-only class syntax (not relevant in strict Ecma parsing, but keep this
      // conservative for other dialects).
      return false;
    }

    let mut has_supported_await: bool = false;

    if let Some(extends) = decl.stx.extends.as_ref() {
      if expr_contains_await(extends) {
        if !expr_is_direct_await_without_nested_await(extends) {
          return false;
        }
        has_supported_await = true;
      }
    }

    for member in decl.stx.members.iter() {
      if member.stx.declare || member.stx.abstract_ {
        return false;
      }
      if !member.stx.decorators.is_empty() {
        return false;
      }

      // Reject class fields: `AsyncClassDeclState` does not support them in the async path.
      match &member.stx.val {
        ClassOrObjVal::Prop(_) | ClassOrObjVal::IndexSignature(_) => return false,
        // Class static blocks are parsed in a `~Await` context, so `await` should not appear here.
        // Treat it as unsupported for the compiled async class evaluator to be safe.
        ClassOrObjVal::StaticBlock(block) => {
          if block.stx.body.iter().any(stmt_contains_await) {
            return false;
          }
        }
        ClassOrObjVal::Getter(_) | ClassOrObjVal::Setter(_) | ClassOrObjVal::Method(_) => {}
      }

      if let ClassOrObjKey::Computed(key_expr) = &member.stx.key {
        if expr_contains_await(key_expr) {
          if !expr_is_direct_await_without_nested_await(key_expr) {
            return false;
          }
          has_supported_await = true;
        }
      }
    }

    // Require an actual `await` in supported class positions. This avoids accidentally accepting a
    // class declaration whose only `await` appears in an unsupported nested position.
    has_supported_await
  }

  fn expr_is_object_destructuring_assignment_with_supported_await(expr: &Node<Expr>) -> bool {
    // Supported shape:
    //   `({ <object-pattern> } = <rhs>);`
    //
    // where any `await` appears only in:
    // - computed keys (`{ [await p]: x }`), or
    // - default values (`{ x = await p }`),
    //
    // and in both cases the `await` must be a direct `await <expr>` with no nested `await` inside
    // `<expr>`.
    //
    // The object pattern must be "simple" (no rest, no nested patterns in property targets) so it
    // can execute via the compiled async evaluator's `AsyncDestructuringAssignState`.
    let Expr::Binary(binary) = &*expr.stx else {
      return false;
    };
    if binary.stx.operator != OperatorName::Assignment {
      return false;
    }
    // RHS is evaluated before destructuring and must be synchronous (no await).
    if expr_contains_await(&binary.stx.right) {
      return false;
    }

    let Expr::ObjPat(obj_pat) = &*binary.stx.left.stx else {
      return false;
    };
    if obj_pat.stx.rest.is_some() {
      // `AsyncDestructuringAssignState` does not support object rest patterns.
      return false;
    }

    for prop in obj_pat.stx.properties.iter() {
      // Computed key: allow either no await, or direct `await <expr>` with no nested await.
      if let ClassOrObjKey::Computed(key_expr) = &prop.stx.key {
        if let Some(arg) = expr_direct_await_arg(key_expr) {
          if expr_contains_await(arg) {
            return false;
          }
        } else if expr_contains_await(key_expr) {
          return false;
        }
      }

      // Property target must be a simple assignment target (identifier / member) without await.
      match &*prop.stx.target.stx {
        Pat::Id(_) => {}
        Pat::AssignTarget(target_expr) => {
          if !expr_is_supported_assignment_target_for_hir_async_scripts(target_expr) {
            return false;
          }
          if expr_contains_await(target_expr) {
            return false;
          }
        }
        _ => return false,
      }

      // Default value: allow either no await, or direct `await <expr>` with no nested await.
      if let Some(default_expr) = prop.stx.default_value.as_ref() {
        if let Some(arg) = expr_direct_await_arg(default_expr) {
          if expr_contains_await(arg) {
            return false;
          }
        } else if expr_contains_await(default_expr) {
          return false;
        }
      }
    }

    true
  }

  for stmt in stmts {
    if !stmt_contains_await(stmt) {
      continue;
    }

    let supported = match &*stmt.stx {
      // Expression statements.
      //
      // Supported shapes:
      // - `await <expr>;`
      // - `x = await <expr>;`
      // - `x += await <expr>;` (and other arithmetic/bitwise compound assignment operators)
      // - `x ||= await <expr>;` / `x &&= await <expr>;` / `x ??= await <expr>;`
      // - `({ ... } = await <expr>);` / `[ ... ] = await <expr>;` (destructuring assignment patterns)
      // - `({ x = await <expr> } = obj);` (object destructuring assignment with await in defaults/computed keys)
      Stmt::Expr(expr_stmt) => {
        let expr = &expr_stmt.stx.expr;
        expr_is_direct_await_without_nested_await(expr)
          || expr_is_supported_assignment_with_direct_await_rhs_without_nested_await(expr)
          || expr_is_logical_assignment_with_direct_await_rhs_without_nested_await(expr)
          || expr_is_destructuring_assignment_with_direct_await_rhs_without_nested_await(expr)
          || expr_is_object_destructuring_assignment_with_supported_await(expr)
      }

      // `export default await <expr>;`
      //
      // Only support the direct-`await` form for now.
      Stmt::ExportDefaultExpr(export_default) => {
        expr_is_direct_await_without_nested_await(&export_default.stx.expression)
      }

      // `throw await <expr>;` as a standalone statement item.
      Stmt::Throw(throw_stmt) => match &*throw_stmt.stx.value.stx {
        Expr::Unary(unary) if unary.stx.operator == OperatorName::Await => {
          // The compiled evaluator does not yet support nested `await` inside the awaited operand.
          !expr_contains_await(&unary.stx.argument)
        }
        _ => false,
      },

      // `var`/`let`/`const` declarations where any `await` in an initializer is a direct `await`
      // expression (`const x = await <expr>;`).
      Stmt::VarDecl(decl) => var_decl_stmt_supported(decl),

      // Class declarations with `await` in `extends` or computed member keys.
      //
      // This is supported by `AsyncClassDeclState` for a limited subset of class forms; reject
      // fields and `await` in nested class static blocks for now.
      Stmt::ClassDecl(decl) => class_decl_stmt_supported(decl),

      // `for (init; test; update) { ... }` loops at top-level.
      //
      // The compiled evaluator supports suspension/resumption at direct `await` boundaries (and
      // simple `x = await <expr>` / `x += await <expr>` assignments) in the loop head, but does not yet support `await`
      // inside the loop body or within initializer declarations.
      Stmt::ForTriple(for_stmt) => {
        for_triple_stmt_supported(for_stmt)
      },

      // `for await (<lhs> of <rhs>) { ... }` at top-level.
      //
      // The compiled evaluator can suspend/resume the implicit `await` boundaries in `for await..of`
      // itself, but does not yet support additional nested `await` within the head or body. The
      // RHS may be a direct `await <expr>`, as long as `<expr>` contains no nested `await`.
      Stmt::ForOf(for_of) if for_of.stx.await_ => {
        for_await_of_stmt_supported(for_of)
      }

      // `for (const { x = await p } of xs) { ... }` at top-level.
      //
      // This is supported by `ForOfState` (async suspension while binding the object pattern head)
      // as long as:
      // - `await` occurs only in computed keys / default values, and only as direct `await <expr>`
      //   with no nested `await`,
      // - the RHS and loop body contain no `await`.
      Stmt::ForOf(for_of) => for_of_stmt_supported_with_async_head(for_of),

      // Support `try { for await (...) { ... } } catch/finally` at top-level.
      //
      // This is intentionally narrow: within the try block, the only supported `await` forms are
      // top-level `for await..of` loops (including label chains around them). The catch parameter,
      // catch body, and finally block must not contain any `await`.
      Stmt::Try(try_stmt) => {
        // Catch + finally must be synchronous.
        let catch_supported = try_stmt.stx.catch.as_ref().is_none_or(|c| {
          c.stx
            .parameter
            .as_ref()
            .is_none_or(|p| !pat_contains_await(&p.stx.pat.stx))
            && !c.stx.body.iter().any(stmt_contains_await)
        });
        if !catch_supported {
          false
        } else if try_stmt
          .stx
          .finally
          .as_ref()
          .is_some_and(|f| f.stx.body.iter().any(stmt_contains_await))
        {
          false
        } else {
          // Try block: any statement containing `await` must be a supported top-level `for await..of`.
          try_stmt.stx.wrapped.stx.body.iter().all(|s| {
            if !stmt_contains_await(s) {
              return true;
            }
            match &*s.stx {
              Stmt::ForOf(for_of) => for_await_of_stmt_supported(for_of),
              Stmt::Label(label) => {
                let mut inner = &label.stx.statement;
                while let Stmt::Label(label) = &*inner.stx {
                  inner = &label.stx.statement;
                }
                match &*inner.stx {
                  Stmt::ForOf(for_of) => for_await_of_stmt_supported(for_of),
                  _ => false,
                }
              }
              _ => false,
            }
          })
        }
      }

      // Support `label: for await (...) { ... }` (including nested label chains like
      // `a: b: for await (...) { ... }`) as long as the labelled statement ultimately labels a
      // supported top-level `for await..of` loop, `for (init; test; update)` loop, or `for..of`
      // loop with `await` in the loop head pattern.
      Stmt::Label(label) => {
        let mut inner = &label.stx.statement;
        while let Stmt::Label(label) = &*inner.stx {
          inner = &label.stx.statement;
        }
        match &*inner.stx {
          Stmt::Expr(expr_stmt) => {
            let expr = &expr_stmt.stx.expr;
            expr_is_direct_await_without_nested_await(expr)
              || expr_is_supported_assignment_with_direct_await_rhs_without_nested_await(expr)
              || expr_is_destructuring_assignment_with_direct_await_rhs_without_nested_await(expr)
              || expr_is_object_destructuring_assignment_with_supported_await(expr)
          }
          Stmt::Throw(throw_stmt) => match &*throw_stmt.stx.value.stx {
            Expr::Unary(unary) if unary.stx.operator == OperatorName::Await => !expr_contains_await(&unary.stx.argument),
            _ => false,
          },
          Stmt::VarDecl(decl) => var_decl_stmt_supported(decl),
          Stmt::ForTriple(for_stmt) => for_triple_stmt_supported(for_stmt),
          Stmt::ForOf(for_of) if !for_of.stx.await_ => for_of_stmt_supported_with_async_head(for_of),
          Stmt::ForOf(for_of) if for_of.stx.await_ => for_await_of_stmt_supported(for_of),
          _ => false,
        }
      }

      // Everything else is unsupported for the compiled async evaluator for now.
      _ => false,
    };

    if !supported {
      return true;
    }
  }

  false
}

fn stmt_is_module_only(stmt: &Node<Stmt>) -> bool {
  match &*stmt.stx {
    Stmt::Import(_) | Stmt::ExportList(_) | Stmt::ExportDefaultExpr(_) => true,
    Stmt::FunctionDecl(decl) => decl.stx.export || decl.stx.export_default,
    Stmt::ClassDecl(decl) => decl.stx.export || decl.stx.export_default,
    Stmt::VarDecl(decl) => decl.stx.export,
    _ => false,
  }
}

fn async_function_body_requires_ast_fallback(hir: &hir_js::LowerResult) -> BTreeSet<hir_js::BodyId> {
  let mut out: BTreeSet<hir_js::BodyId> = BTreeSet::new();

  for (&body_id, _) in hir.body_index.iter() {
    let Some(body) = hir.body(body_id) else {
      continue;
    };
    if body.kind != hir_js::BodyKind::Function {
      continue;
    }
    let Some(func_meta) = body.function.as_ref() else {
      continue;
    };
    // Only async non-generator functions are eligible for execution in the compiled async executor.
    if !func_meta.async_ || func_meta.generator {
      continue;
    }

    if !hir_async_function_body_is_supported(hir, body_id) {
      out.insert(body_id);
    }
  }

  out
}

fn hir_async_function_body_is_supported(hir: &hir_js::LowerResult, body_id: hir_js::BodyId) -> bool {
  let Some(body) = hir.body(body_id) else {
    // If HIR is missing, conservatively require AST fallback.
    return false;
  };
  let Some(func_meta) = body.function.as_ref() else {
    return false;
  };
  if !func_meta.async_ || func_meta.generator {
    return false;
  }

  let mut visited_bodies: BTreeSet<hir_js::BodyId> = BTreeSet::new();

  // Async functions instantiate parameters synchronously before executing the async body. Any `await`
  // that appears in parameter patterns or default initializers is therefore unsupported in the
  // compiled async executor.
  for param in &func_meta.params {
    if hir_pat_contains_await(hir, body, param.pat, &mut visited_bodies) {
      return false;
    }
    if let Some(default) = param.default {
      if hir_expr_contains_await(hir, body, default, &mut visited_bodies) {
        return false;
      }
    }
  }

  match &func_meta.body {
    hir_js::FunctionBody::Expr(expr_id) => {
      let Some(expr) = hir_get_expr(body, *expr_id) else {
        return false;
      };
      match &expr.kind {
        // Expression-bodied async arrow functions are supported only when the body is:
        // - an AwaitExpression with no nested await, or
        // - an expression with no await at all.
        hir_js::ExprKind::Await { expr: awaited_expr } => {
          !hir_expr_contains_await(hir, body, *awaited_expr, &mut visited_bodies)
        }
        _ => !hir_expr_contains_await(hir, body, *expr_id, &mut visited_bodies),
      }
    }
    hir_js::FunctionBody::Block(stmts) => stmts.iter().all(|stmt_id| {
      hir_async_root_stmt_is_supported(hir, body, *stmt_id, &mut visited_bodies)
    }),
  }
}

fn hir_compound_assign_op_supported(op: hir_js::AssignOp) -> bool {
  matches!(
    op,
    hir_js::AssignOp::AddAssign
      | hir_js::AssignOp::SubAssign
      | hir_js::AssignOp::MulAssign
      | hir_js::AssignOp::DivAssign
      | hir_js::AssignOp::RemAssign
      | hir_js::AssignOp::ExponentAssign
      | hir_js::AssignOp::ShiftLeftAssign
      | hir_js::AssignOp::ShiftRightAssign
      | hir_js::AssignOp::ShiftRightUnsignedAssign
      | hir_js::AssignOp::BitOrAssign
      | hir_js::AssignOp::BitAndAssign
      | hir_js::AssignOp::BitXorAssign
  )
}

fn hir_logical_assign_op_supported(op: hir_js::AssignOp) -> bool {
  matches!(
    op,
    hir_js::AssignOp::LogicalAndAssign
      | hir_js::AssignOp::LogicalOrAssign
      | hir_js::AssignOp::NullishAssign
  )
}

fn hir_assignment_target_is_supported(body: &hir_js::Body, target: hir_js::PatId) -> bool {
  let Some(pat) = hir_get_pat(body, target) else {
    return false;
  };
  match &pat.kind {
    hir_js::PatKind::Ident(_) => true,
    hir_js::PatKind::AssignTarget(expr_id) => {
      let Some(expr) = hir_get_expr(body, *expr_id) else {
        return false;
      };
      matches!(expr.kind, hir_js::ExprKind::Ident(_) | hir_js::ExprKind::Member(_))
    }
    _ => false,
  }
}

fn hir_object_pat_allows_direct_await_in_keys_and_defaults(
  hir: &hir_js::LowerResult,
  body: &hir_js::Body,
  pat_id: hir_js::PatId,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
  value_is_assignment_target: bool,
) -> bool {
  let Some(pat) = hir_get_pat(body, pat_id) else {
    return false;
  };
  let hir_js::PatKind::Object(obj_pat) = &pat.kind else {
    return false;
  };

  // The compiled async object-pattern evaluators do not yet support rest.
  if obj_pat.rest.is_some() {
    return false;
  }

  for prop in obj_pat.props.iter() {
    // Property key: allow a direct `await <expr>` in computed keys.
    if let hir_js::ObjectKey::Computed(expr_id) = &prop.key {
      let Some(key_expr) = hir_get_expr(body, *expr_id) else {
        return false;
      };
      if let hir_js::ExprKind::Await { expr: awaited_expr } = key_expr.kind {
        if hir_expr_contains_await(hir, body, awaited_expr, visited_bodies) {
          return false;
        }
      } else if hir_expr_contains_await(hir, body, *expr_id, visited_bodies) {
        return false;
      }
    }

    // Property default: allow a direct `await <expr>` default value.
    if let Some(default_expr_id) = prop.default_value {
      let Some(default_expr) = hir_get_expr(body, default_expr_id) else {
        return false;
      };
      if let hir_js::ExprKind::Await { expr: awaited_expr } = default_expr.kind {
        if hir_expr_contains_await(hir, body, awaited_expr, visited_bodies) {
          return false;
        }
      } else if hir_expr_contains_await(hir, body, default_expr_id, visited_bodies) {
        return false;
      }
    }

    // Property value pattern: must not contain any `await`.
    if value_is_assignment_target {
      if !hir_assignment_target_is_supported(body, prop.value) {
        return false;
      }
      if hir_pat_contains_await(hir, body, prop.value, visited_bodies) {
        return false;
      }
    } else if hir_pat_contains_await(hir, body, prop.value, visited_bodies) {
      return false;
    }
  }

  true
}

fn hir_for_head_allows_direct_object_pat_awaits(
  hir: &hir_js::LowerResult,
  body: &hir_js::Body,
  head: &hir_js::ForHead,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
) -> bool {
  match head {
    hir_js::ForHead::Pat(pat_id) => {
      // The compiled async evaluator does not support `await` in non-var loop heads.
      !hir_pat_contains_await(hir, body, *pat_id, visited_bodies)
    }
    hir_js::ForHead::Var(var_decl) => {
      // If there is no await in any head pattern, the synchronous binder can handle the head.
      // If there *is* an await, only support the subset handled by `AsyncObjectPatternBindingState`
      // (single declarator, no init, object pattern, direct await in keys/defaults).
      let mut head_pat_contains_await = false;
      for decl in var_decl.declarators.iter() {
        if hir_pat_contains_await(hir, body, decl.pat, visited_bodies) {
          head_pat_contains_await = true;
          break;
        }
        if let Some(init) = decl.init {
          if hir_expr_contains_await(hir, body, init, visited_bodies) {
            return false;
          }
        }
      }

      if !head_pat_contains_await {
        return true;
      }

      if var_decl.declarators.len() != 1 {
        return false;
      }
      let decl = &var_decl.declarators[0];
      if decl.init.is_some() {
        return false;
      }
      hir_object_pat_allows_direct_await_in_keys_and_defaults(
        hir,
        body,
        decl.pat,
        visited_bodies,
        /* value_is_assignment_target */ false,
      )
    }
  }
}

fn hir_for_await_of_loop_is_supported(
  hir: &hir_js::LowerResult,
  body: &hir_js::Body,
  left: &hir_js::ForHead,
  right: hir_js::ExprId,
  inner: hir_js::StmtId,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
) -> bool {
  if !hir_for_head_allows_direct_object_pat_awaits(hir, body, left, visited_bodies) {
    return false;
  }

  let Some(rhs_expr) = hir_get_expr(body, right) else {
    return false;
  };
  if let hir_js::ExprKind::Await { expr: awaited_expr } = rhs_expr.kind {
    if hir_expr_contains_await(hir, body, awaited_expr, visited_bodies) {
      return false;
    }
  } else if hir_expr_contains_await(hir, body, right, visited_bodies) {
    return false;
  }

  // The compiled async evaluator currently evaluates the loop body synchronously.
  // Ensure the body contains no nested awaits.
  !hir_stmt_contains_await(hir, body, inner, visited_bodies)
}

fn hir_for_of_loop_is_supported(
  hir: &hir_js::LowerResult,
  body: &hir_js::Body,
  left: &hir_js::ForHead,
  right: hir_js::ExprId,
  inner: hir_js::StmtId,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
) -> bool {
  if !hir_for_head_allows_direct_object_pat_awaits(hir, body, left, visited_bodies) {
    return false;
  }
  // RHS + body are evaluated synchronously.
  if hir_expr_contains_await(hir, body, right, visited_bodies) {
    return false;
  }
  if hir_stmt_contains_await(hir, body, inner, visited_bodies) {
    return false;
  }
  true
}

fn hir_for_triple_expr_is_supported(
  hir: &hir_js::LowerResult,
  body: &hir_js::Body,
  expr_id: hir_js::ExprId,
  allow_assignment_await: bool,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
) -> bool {
  let Some(expr) = hir_get_expr(body, expr_id) else {
    return false;
  };
  if let hir_js::ExprKind::Await { expr: awaited_expr } = expr.kind {
    return !hir_expr_contains_await(hir, body, awaited_expr, visited_bodies);
  }

  if allow_assignment_await {
    if let hir_js::ExprKind::Assignment { op, target, value } = &expr.kind {
      let Some(rhs) = hir_get_expr(body, *value) else {
        return false;
      };
      if let hir_js::ExprKind::Await { expr: awaited_expr } = rhs.kind {
        // Support destructuring assignment with a direct await RHS in `for` init/update expressions:
        // - `({x} = await <expr>)`
        // - `[x] = await <expr>`
        //
        // The awaited operand is evaluated before pattern assignment, and the pattern binding after
        // the await boundary is synchronous, so the pattern itself must be await-free.
        if *op == hir_js::AssignOp::Assign {
          let Some(target_pat) = hir_get_pat(body, *target) else {
            return false;
          };
          if !matches!(
            target_pat.kind,
            hir_js::PatKind::Ident(_) | hir_js::PatKind::AssignTarget(_)
          ) {
            if hir_pat_contains_await(hir, body, *target, visited_bodies) {
              return false;
            }
            return !hir_expr_contains_await(hir, body, awaited_expr, visited_bodies);
          }
        }

        if *op != hir_js::AssignOp::Assign
          && !hir_compound_assign_op_supported(*op)
          && !hir_logical_assign_op_supported(*op)
        {
          return false;
        }
        if !hir_assignment_target_is_supported(body, *target) {
          return false;
        }
        if hir_pat_contains_await(hir, body, *target, visited_bodies) {
          return false;
        }
        return !hir_expr_contains_await(hir, body, awaited_expr, visited_bodies);
      }
    }
  }

  !hir_expr_contains_await(hir, body, expr_id, visited_bodies)
}

fn hir_async_root_stmt_is_supported(
  hir: &hir_js::LowerResult,
  body: &hir_js::Body,
  stmt_id: hir_js::StmtId,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
) -> bool {
  let Some(stmt) = hir_get_stmt(body, stmt_id) else {
    return false;
  };
  match &stmt.kind {
    hir_js::StmtKind::Expr(expr_id) => {
      let Some(expr) = hir_get_expr(body, *expr_id) else {
        return false;
      };
      match &expr.kind {
        hir_js::ExprKind::Await { expr: awaited_expr } => {
          return !hir_expr_contains_await(hir, body, *awaited_expr, visited_bodies);
        }
        hir_js::ExprKind::Assignment { op, target, value } => {
          let Some(rhs) = hir_get_expr(body, *value) else {
            return false;
          };
          if let hir_js::ExprKind::Await { expr: awaited_expr } = rhs.kind {
            // Support assignment expression statements with a direct `await` RHS:
            // - `x = await expr;`
            // - `x <op>= await expr;` (supported compound ops only)
            // - `x &&= await expr;` / `x ||= await expr;` / `x ??= await expr;`
            // - `({x} = await expr);` / `[x] = await expr;` (destructuring assignment; pattern must be await-free)
            //
            // Ensure:
            // - assignment target contains no await, and
            // - the awaited operand contains no await.
            if *op == hir_js::AssignOp::Assign
              || hir_compound_assign_op_supported(*op)
              || hir_logical_assign_op_supported(*op)
            {
              if hir_assignment_target_is_supported(body, *target) {
                if hir_pat_contains_await(hir, body, *target, visited_bodies) {
                  return false;
                }
                if hir_expr_contains_await(hir, body, awaited_expr, visited_bodies) {
                  return false;
                }
                return true;
              }

              // Destructuring assignment statements with a direct await RHS are supported so long as
              // the pattern itself is await-free (pattern evaluation happens after the await
              // boundary via the synchronous evaluator).
              if *op == hir_js::AssignOp::Assign {
                let Some(target_pat) = hir_get_pat(body, *target) else {
                  return false;
                };
                if !matches!(
                  target_pat.kind,
                  hir_js::PatKind::Ident(_) | hir_js::PatKind::AssignTarget(_)
                ) {
                  if hir_pat_contains_await(hir, body, *target, visited_bodies) {
                    return false;
                  }
                  if hir_expr_contains_await(hir, body, awaited_expr, visited_bodies) {
                    return false;
                  }
                  return true;
                }
              }
            }
            return false;
          }

          // Support object destructuring assignment statements where the pattern contains `await` in
          // a computed key or default value. The RHS is evaluated synchronously before the pattern
          // state machine begins, so it must not contain any await.
          if *op == hir_js::AssignOp::Assign {
            let Some(target_pat) = hir_get_pat(body, *target) else {
              return false;
            };
            if matches!(&target_pat.kind, hir_js::PatKind::Object(_))
              && hir_pat_contains_await(hir, body, *target, visited_bodies)
            {
              if hir_expr_contains_await(hir, body, *value, visited_bodies) {
                return false;
              }
              return hir_object_pat_allows_direct_await_in_keys_and_defaults(
                hir,
                body,
                *target,
                visited_bodies,
                /* value_is_assignment_target */ true,
              );
            }
          }
        }
        _ => {}
      }

      !hir_expr_contains_await(hir, body, *expr_id, visited_bodies)
    }

    hir_js::StmtKind::Return(opt_expr) => match opt_expr {
      None => true,
      Some(expr_id) => {
        let Some(expr) = hir_get_expr(body, *expr_id) else {
          return false;
        };
        match &expr.kind {
          hir_js::ExprKind::Await { expr: awaited_expr } => {
            !hir_expr_contains_await(hir, body, *awaited_expr, visited_bodies)
          }
          _ => !hir_expr_contains_await(hir, body, *expr_id, visited_bodies),
        }
      }
    },

    hir_js::StmtKind::Throw(expr_id) => {
      let Some(expr) = hir_get_expr(body, *expr_id) else {
        return false;
      };
      match &expr.kind {
        hir_js::ExprKind::Await { expr: awaited_expr } => {
          !hir_expr_contains_await(hir, body, *awaited_expr, visited_bodies)
        }
        _ => !hir_expr_contains_await(hir, body, *expr_id, visited_bodies),
      }
    }

    // Support `var`/`let`/`const` declarations with direct `await` initializers.
    hir_js::StmtKind::Var(var_decl) => {
      for declarator in &var_decl.declarators {
        let pat_contains_await = hir_pat_contains_await(hir, body, declarator.pat, visited_bodies);
        if pat_contains_await {
          // Support `var`/`let`/`const` object destructuring patterns where computed keys or default
          // values contain a *direct* `await <expr>`.
          //
          // The initializer must be evaluated synchronously before the pattern-binding state machine
          // begins. This means the initializer cannot contain any `await` (including a direct `=
          // await <expr>` initializer).
          if !matches!(
            var_decl.kind,
            hir_js::VarDeclKind::Var | hir_js::VarDeclKind::Let | hir_js::VarDeclKind::Const
          ) {
            return false;
          }
          if !hir_object_pat_allows_direct_await_in_keys_and_defaults(
            hir,
            body,
            declarator.pat,
            visited_bodies,
            /* value_is_assignment_target */ false,
          ) {
            return false;
          }
          if let Some(init_id) = declarator.init {
            if hir_expr_contains_await(hir, body, init_id, visited_bodies) {
              return false;
            }
          }
          continue;
        }
        let Some(init) = declarator.init else {
          continue;
        };
        let Some(init_expr) = hir_get_expr(body, init) else {
          return false;
        };
        match &init_expr.kind {
          hir_js::ExprKind::Await { expr: awaited_expr } => {
            if hir_expr_contains_await(hir, body, *awaited_expr, visited_bodies) {
              return false;
            }
          }
          _ => {
            if hir_expr_contains_await(hir, body, init, visited_bodies) {
              return false;
            }
          }
        }
      }
      true
    }

    // Support `for await..of` loops, as long as the loop head, RHS, and body contain no other `await`.
    hir_js::StmtKind::ForIn {
      left,
      right,
      body: inner,
      is_for_of: true,
      await_: true,
    } => hir_for_await_of_loop_is_supported(hir, body, left, *right, *inner, visited_bodies),

    hir_js::StmtKind::ForIn {
      left,
      right,
      body: inner,
      is_for_of: true,
      await_: false,
      ..
    } => hir_for_of_loop_is_supported(hir, body, left, *right, *inner, visited_bodies),

    hir_js::StmtKind::Labeled { body: inner, .. } => {
      // Support top-level label chains around async-aware loop forms handled by the compiled async
      // evaluator:
      // - `for await..of`
      // - `for..of` with await-in-pattern
      // - `for (...)` with await in init/test/update
      //
      // Also support label chains around direct-await statement forms (`expr`/`var`/`throw`/
      // `return`). These statements cannot produce `break`/`continue` completions, so label
      // semantics are irrelevant and the compiled async evaluator can safely "see through" the
      // labels.
      //
      // Any other labeled statement containing `await` remains unsupported because the compiled
      // async evaluator does not preserve label semantics for arbitrary awaiting statements that may
      // produce break/continue completions.
      let mut current_stmt_id: hir_js::StmtId = *inner;
      loop {
        let Some(inner_stmt) = hir_get_stmt(body, current_stmt_id) else {
          return false;
        };
        match &inner_stmt.kind {
          hir_js::StmtKind::Labeled { body: next, .. } => {
            current_stmt_id = *next;
            continue;
          }
          hir_js::StmtKind::ForIn {
            left,
            right,
            body: inner,
            is_for_of: true,
            await_: true,
          } => {
            return hir_for_await_of_loop_is_supported(hir, body, left, *right, *inner, visited_bodies);
          }
          hir_js::StmtKind::ForIn {
            left,
            right,
            body: inner,
            is_for_of: true,
            await_: false,
            ..
          } => {
            return hir_for_of_loop_is_supported(hir, body, left, *right, *inner, visited_bodies);
          }
          hir_js::StmtKind::For {
            init,
            test,
            update,
            body: inner,
          } => {
            // Mirror the `StmtKind::For` logic below for labeled loops.
            if hir_stmt_contains_await(hir, body, *inner, visited_bodies) {
              return false;
            }

            if let Some(init) = init {
              match init {
                hir_js::ForInit::Expr(expr_id) => {
                  if !hir_for_triple_expr_is_supported(
                    hir,
                    body,
                    *expr_id,
                    /* allow_assignment_await */ true,
                    visited_bodies,
                  ) {
                    return false;
                  }
                }
                hir_js::ForInit::Var(var_decl) => {
                  for declarator in var_decl.declarators.iter() {
                    if hir_pat_contains_await(hir, body, declarator.pat, visited_bodies) {
                      return false;
                    }
                    if let Some(init_id) = declarator.init {
                      let Some(init_expr) = hir_get_expr(body, init_id) else {
                        return false;
                      };
                      if let hir_js::ExprKind::Await { expr: awaited_expr } = init_expr.kind {
                        if !matches!(
                          var_decl.kind,
                          hir_js::VarDeclKind::Var | hir_js::VarDeclKind::Let | hir_js::VarDeclKind::Const
                        ) {
                          return false;
                        }
                        if hir_expr_contains_await(hir, body, awaited_expr, visited_bodies) {
                          return false;
                        }
                      } else if hir_expr_contains_await(hir, body, init_id, visited_bodies) {
                        return false;
                      }
                    }
                  }
                }
              }
            }

            if let Some(test_id) = test {
              if !hir_for_triple_expr_is_supported(
                hir,
                body,
                *test_id,
                /* allow_assignment_await */ false,
                visited_bodies,
              ) {
                return false;
              }
            }

            if let Some(update_id) = update {
              if !hir_for_triple_expr_is_supported(
                hir,
                body,
                *update_id,
                /* allow_assignment_await */ true,
                visited_bodies,
              ) {
                return false;
              }
            }

            return true;
          }
          hir_js::StmtKind::Expr(_)
          | hir_js::StmtKind::Var(_)
          | hir_js::StmtKind::Return(_)
          | hir_js::StmtKind::Throw(_) => {
            return hir_async_root_stmt_is_supported(hir, body, current_stmt_id, visited_bodies);
          }
          _ => {
            return !hir_stmt_contains_await(hir, body, stmt_id, visited_bodies);
          }
        }
      }
    }

    // Support `for ( ... ) { ... }` loops that may suspend in init/test/update, as long as the loop
    // body contains no await.
    hir_js::StmtKind::For {
      init,
      test,
      update,
      body: inner,
    } => {
      if hir_stmt_contains_await(hir, body, *inner, visited_bodies) {
        return false;
      }

      if let Some(init) = init {
        match init {
          hir_js::ForInit::Expr(expr_id) => {
            if !hir_for_triple_expr_is_supported(
              hir,
              body,
              *expr_id,
              /* allow_assignment_await */ true,
              visited_bodies,
            ) {
              return false;
            }
          }
          hir_js::ForInit::Var(decl) => {
            for declarator in decl.declarators.iter() {
              if hir_pat_contains_await(hir, body, declarator.pat, visited_bodies) {
                return false;
              }
              if let Some(init_id) = declarator.init {
                let Some(init_expr) = hir_get_expr(body, init_id) else {
                  return false;
                };
                if let hir_js::ExprKind::Await { expr: awaited_expr } = init_expr.kind {
                  // Support direct `await <expr>` initializers for `var`/`let`/`const` declarations
                  // in the init position.
                  if !matches!(
                    decl.kind,
                    hir_js::VarDeclKind::Var | hir_js::VarDeclKind::Let | hir_js::VarDeclKind::Const
                  ) {
                    return false;
                  }
                  if hir_expr_contains_await(hir, body, awaited_expr, visited_bodies) {
                    return false;
                  }
                } else if hir_expr_contains_await(hir, body, init_id, visited_bodies) {
                  return false;
                }
              }
            }
          }
        }
      }

      if let Some(test_id) = test {
        if !hir_for_triple_expr_is_supported(
          hir,
          body,
          *test_id,
          /* allow_assignment_await */ false,
          visited_bodies,
        ) {
          return false;
        }
      }

      if let Some(update_id) = update {
        if !hir_for_triple_expr_is_supported(
          hir,
          body,
          *update_id,
          /* allow_assignment_await */ true,
          visited_bodies,
        ) {
          return false;
        }
      }

      true
    }

    hir_js::StmtKind::Try {
      block,
      catch,
      finally_block,
    } => {
      // Async-aware `try` support is currently limited to suspensions caused by:
      // - `for await..of` loops directly in the try-block statement list, and
      // - direct statement-level `return await <expr>;` / `throw await <expr>;` forms.
      //
      // Conservatively require all other statements within try/catch/finally to be await-free.
      let Some(try_block_stmt) = hir_get_stmt(body, *block) else {
        return false;
      };
      let hir_js::StmtKind::Block(try_stmts) = &try_block_stmt.kind else {
        return false;
      };

      for inner_stmt_id in try_stmts.iter() {
        let Some(inner_stmt) = hir_get_stmt(body, *inner_stmt_id) else {
          return false;
        };
        match &inner_stmt.kind {
          hir_js::StmtKind::ForIn {
            left,
            right,
            body: inner,
            is_for_of: true,
            await_: true,
          } => {
            if !hir_for_await_of_loop_is_supported(hir, body, left, *right, *inner, visited_bodies) {
              return false;
            }
          }
          hir_js::StmtKind::Return(Some(expr_id)) => {
            let Some(expr) = hir_get_expr(body, *expr_id) else {
              return false;
            };
            if let hir_js::ExprKind::Await { expr: awaited_expr } = expr.kind {
              if hir_expr_contains_await(hir, body, awaited_expr, visited_bodies) {
                return false;
              }
              continue;
            }
            if hir_stmt_contains_await(hir, body, *inner_stmt_id, visited_bodies) {
              return false;
            }
          }
          hir_js::StmtKind::Throw(expr_id) => {
            let Some(expr) = hir_get_expr(body, *expr_id) else {
              return false;
            };
            if let hir_js::ExprKind::Await { expr: awaited_expr } = expr.kind {
              if hir_expr_contains_await(hir, body, awaited_expr, visited_bodies) {
                return false;
              }
              continue;
            }
            if hir_stmt_contains_await(hir, body, *inner_stmt_id, visited_bodies) {
              return false;
            }
          }
          hir_js::StmtKind::Labeled { body: first_body, .. } => {
            // Support label chains ending in `for await..of` inside the try block.
            let mut current_stmt_id: hir_js::StmtId = *first_body;
            loop {
              let Some(s) = hir_get_stmt(body, current_stmt_id) else {
                return false;
              };
              match &s.kind {
                hir_js::StmtKind::Labeled { body: next, .. } => current_stmt_id = *next,
                _ => {
                  if let hir_js::StmtKind::ForIn {
                    left,
                    right,
                    body: inner,
                    is_for_of: true,
                    await_: true,
                  } = &s.kind
                  {
                    if !hir_for_await_of_loop_is_supported(
                      hir,
                      body,
                      left,
                      *right,
                      *inner,
                      visited_bodies,
                    ) {
                      return false;
                    }
                  } else if hir_stmt_contains_await(hir, body, *inner_stmt_id, visited_bodies) {
                    return false;
                  }
                  break;
                }
              }
            }
          }
          _ => {
            if hir_stmt_contains_await(hir, body, *inner_stmt_id, visited_bodies) {
              return false;
            }
          }
        }
      }

      if let Some(catch_clause) = catch {
        if let Some(param_pat_id) = catch_clause.param {
          if hir_pat_contains_await(hir, body, param_pat_id, visited_bodies) {
            return false;
          }
        }
        if hir_stmt_contains_await(hir, body, catch_clause.body, visited_bodies) {
          return false;
        }
      }

      if let Some(finally_stmt) = finally_block {
        if hir_stmt_contains_await(hir, body, *finally_stmt, visited_bodies) {
          return false;
        }
      }

      true
    }

    hir_js::StmtKind::Decl(def_id) => {
      // Async class declarations may suspend while evaluating:
      // - `extends` expressions, and/or
      // - computed member keys.
      //
      // Support a narrow subset where the suspension point is a *direct* await:
      // - `class C extends (await <expr>) {}`
      // - `class C { [await <expr>]() {} }`
      //
      // Note: `await` is not allowed in class static blocks in this engine (VMJS0004).
      let Some(def) = hir.def(*def_id) else {
        return false;
      };
      let Some(decl_body_id) = def.body else {
        return true;
      };
      let Some(decl_body) = hir.body(decl_body_id) else {
        return false;
      };
      if decl_body.kind != hir_js::BodyKind::Class {
        // Nested functions do not execute their body when creating the function value.
        return true;
      }
      let Some(class_meta) = decl_body.class.as_ref() else {
        return false;
      };

      // Class-level root statements represent evaluation that occurs when defining the class (e.g.
      // decorators). These are not yet supported by the compiled async executor if they contain
      // `await`.
      for stmt_id in decl_body.root_stmts.as_slice() {
        if hir_stmt_contains_await(hir, decl_body, *stmt_id, visited_bodies) {
          return false;
        }
      }

      let mut has_await_in_class_eval: bool = false;
      let mut has_fields: bool = false;

      // `extends`
      if let Some(extends_expr_id) = class_meta.extends {
        let Some(expr) = hir_get_expr(decl_body, extends_expr_id) else {
          return false;
        };
        if let hir_js::ExprKind::Await { expr: awaited_expr } = expr.kind {
          has_await_in_class_eval = true;
          if hir_expr_contains_await(hir, decl_body, awaited_expr, visited_bodies) {
            return false;
          }
        } else if hir_expr_contains_await(hir, decl_body, extends_expr_id, visited_bodies) {
          return false;
        }
      }

      // Members.
      for member in class_meta.members.iter() {
        match &member.kind {
          hir_js::ClassMemberKind::Constructor { .. } => {}
          hir_js::ClassMemberKind::Method { key, .. } => {
            if let hir_js::ClassMemberKey::Computed(expr_id) = key {
              let Some(expr) = hir_get_expr(decl_body, *expr_id) else {
                return false;
              };
              if let hir_js::ExprKind::Await { expr: awaited_expr } = expr.kind {
                has_await_in_class_eval = true;
                if hir_expr_contains_await(hir, decl_body, awaited_expr, visited_bodies) {
                  return false;
                }
              } else if hir_expr_contains_await(hir, decl_body, *expr_id, visited_bodies) {
                return false;
              }
            }
          }
          hir_js::ClassMemberKind::Field {
            key,
            initializer,
            ..
          } => {
            has_fields = true;
            if let hir_js::ClassMemberKey::Computed(expr_id) = key {
              if hir_expr_contains_await(hir, decl_body, *expr_id, visited_bodies) {
                return false;
              }
            }
            if let Some(init_body) = initializer {
              if hir_body_contains_await(hir, *init_body, visited_bodies) {
                return false;
              }
            }
          }
          hir_js::ClassMemberKind::StaticBlock { body: static_body, .. } => {
            if hir_body_contains_await(hir, *static_body, visited_bodies) {
              return false;
            }
          }
        }
      }

      // Fields are not yet supported by the compiled async class evaluator.
      if has_await_in_class_eval && has_fields {
        return false;
      }

      true
    }

    // For all other statement kinds, async/await support is limited to the direct root statement
    // forms above. If any await occurs in these statements (including nested blocks), conservatively
    // fall back to the AST interpreter.
    _ => !hir_stmt_contains_await(hir, body, stmt_id, visited_bodies),
  }
}

fn hir_for_head_contains_await(
  hir: &hir_js::LowerResult,
  body: &hir_js::Body,
  head: &hir_js::ForHead,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
) -> bool {
  match head {
    hir_js::ForHead::Pat(pat_id) => hir_pat_contains_await(hir, body, *pat_id, visited_bodies),
    hir_js::ForHead::Var(decl) => hir_var_decl_contains_await(hir, body, decl, visited_bodies),
  }
}

fn hir_var_decl_contains_await(
  hir: &hir_js::LowerResult,
  body: &hir_js::Body,
  decl: &hir_js::VarDecl,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
) -> bool {
  for declarator in &decl.declarators {
    if hir_pat_contains_await(hir, body, declarator.pat, visited_bodies) {
      return true;
    }
    if let Some(init) = declarator.init {
      if hir_expr_contains_await(hir, body, init, visited_bodies) {
        return true;
      }
    }
  }
  false
}

fn hir_stmt_contains_await(
  hir: &hir_js::LowerResult,
  body: &hir_js::Body,
  stmt_id: hir_js::StmtId,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
) -> bool {
  let Some(stmt) = hir_get_stmt(body, stmt_id) else {
    // Missing HIR node: conservatively assume it may contain await.
    return true;
  };

  match &stmt.kind {
    hir_js::StmtKind::Expr(expr_id) => hir_expr_contains_await(hir, body, *expr_id, visited_bodies),
    hir_js::StmtKind::ExportDefaultExpr(expr_id) => {
      hir_expr_contains_await(hir, body, *expr_id, visited_bodies)
    }
    hir_js::StmtKind::Return(opt_expr) => opt_expr
      .as_ref()
      .is_some_and(|expr_id| hir_expr_contains_await(hir, body, *expr_id, visited_bodies)),
    hir_js::StmtKind::Throw(expr_id) => hir_expr_contains_await(hir, body, *expr_id, visited_bodies),

    hir_js::StmtKind::Var(decl) => hir_var_decl_contains_await(hir, body, decl, visited_bodies),

    hir_js::StmtKind::Block(stmts) => stmts
      .iter()
      .any(|inner| hir_stmt_contains_await(hir, body, *inner, visited_bodies)),

    hir_js::StmtKind::If {
      test,
      consequent,
      alternate,
    } => {
      hir_expr_contains_await(hir, body, *test, visited_bodies)
        || hir_stmt_contains_await(hir, body, *consequent, visited_bodies)
        || alternate
          .as_ref()
          .is_some_and(|alt| hir_stmt_contains_await(hir, body, *alt, visited_bodies))
    }

    hir_js::StmtKind::While { test, body: inner }
    | hir_js::StmtKind::DoWhile { test, body: inner } => {
      hir_expr_contains_await(hir, body, *test, visited_bodies)
        || hir_stmt_contains_await(hir, body, *inner, visited_bodies)
    }

    hir_js::StmtKind::With { object, body: inner } => {
      hir_expr_contains_await(hir, body, *object, visited_bodies)
        || hir_stmt_contains_await(hir, body, *inner, visited_bodies)
    }

    hir_js::StmtKind::Labeled { body: inner, .. } => {
      hir_stmt_contains_await(hir, body, *inner, visited_bodies)
    }

    hir_js::StmtKind::For {
      init,
      test,
      update,
      body: inner,
    } => {
      let init_has_await = init.as_ref().is_some_and(|init| match init {
        hir_js::ForInit::Expr(expr_id) => hir_expr_contains_await(hir, body, *expr_id, visited_bodies),
        hir_js::ForInit::Var(decl) => hir_var_decl_contains_await(hir, body, decl, visited_bodies),
      });
      init_has_await
        || test
          .as_ref()
          .is_some_and(|expr_id| hir_expr_contains_await(hir, body, *expr_id, visited_bodies))
        || update
          .as_ref()
          .is_some_and(|expr_id| hir_expr_contains_await(hir, body, *expr_id, visited_bodies))
        || hir_stmt_contains_await(hir, body, *inner, visited_bodies)
    }

    hir_js::StmtKind::ForIn {
      left,
      right,
      body: inner,
      await_,
      ..
    } => {
      // `for await..of` is itself an await boundary, even if the head/body contain no explicit
      // `await` expression.
      if *await_ {
        return true;
      }

      hir_for_head_contains_await(hir, body, left, visited_bodies)
        || hir_expr_contains_await(hir, body, *right, visited_bodies)
        || hir_stmt_contains_await(hir, body, *inner, visited_bodies)
    }

    hir_js::StmtKind::Switch { discriminant, cases } => {
      hir_expr_contains_await(hir, body, *discriminant, visited_bodies)
        || cases.iter().any(|case| {
          case
            .test
            .as_ref()
            .is_some_and(|expr_id| hir_expr_contains_await(hir, body, *expr_id, visited_bodies))
            || case
              .consequent
              .iter()
              .any(|stmt_id| hir_stmt_contains_await(hir, body, *stmt_id, visited_bodies))
        })
    }

    hir_js::StmtKind::Try {
      block,
      catch,
      finally_block,
    } => {
      hir_stmt_contains_await(hir, body, *block, visited_bodies)
        || catch.as_ref().is_some_and(|c| {
          c.param
            .as_ref()
            .is_some_and(|pat_id| hir_pat_contains_await(hir, body, *pat_id, visited_bodies))
            || hir_stmt_contains_await(hir, body, c.body, visited_bodies)
        })
        || finally_block
          .as_ref()
          .is_some_and(|finally_id| hir_stmt_contains_await(hir, body, *finally_id, visited_bodies))
    }

    hir_js::StmtKind::Decl(def_id) => {
      let def = hir.def(*def_id);
      let Some(def) = def else {
        return true;
      };
      let Some(body_id) = def.body else {
        return false;
      };

      // Class declarations evaluate their body eagerly (static blocks, computed keys, etc). Function
      // declarations do not execute their body.
      match hir.body(body_id).map(|b| b.kind) {
        Some(hir_js::BodyKind::Function) => false,
        // Be conservative for other declaration kinds: treat any await within their executed body as
        // a reason to fall back.
        Some(_) | None => hir_body_contains_await(hir, body_id, visited_bodies),
      }
    }

    hir_js::StmtKind::Break(_)
    | hir_js::StmtKind::Continue(_)
    | hir_js::StmtKind::Debugger
    | hir_js::StmtKind::Empty => false,
  }
}

fn hir_body_contains_await(
  hir: &hir_js::LowerResult,
  body_id: hir_js::BodyId,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
) -> bool {
  // Break accidental cycles conservatively.
  if !visited_bodies.insert(body_id) {
    return false;
  }

  let Some(body) = hir.body(body_id) else {
    return true;
  };

  if body
    .root_stmts
    .iter()
    .any(|stmt_id| hir_stmt_contains_await(hir, body, *stmt_id, visited_bodies))
  {
    return true;
  }

  if body.kind == hir_js::BodyKind::Class {
    if hir_class_metadata_contains_await(hir, body, visited_bodies) {
      return true;
    }
  }

  false
}

fn hir_class_metadata_contains_await(
  hir: &hir_js::LowerResult,
  class_body: &hir_js::Body,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
) -> bool {
  let Some(class_meta) = class_body.class.as_ref() else {
    return false;
  };

  if let Some(extends_expr) = class_meta.extends {
    if hir_expr_contains_await(hir, class_body, extends_expr, visited_bodies) {
      return true;
    }
  }

  for member in &class_meta.members {
    match &member.kind {
      hir_js::ClassMemberKind::Constructor { .. } => {}

      hir_js::ClassMemberKind::Method { key, .. } | hir_js::ClassMemberKind::Field { key, .. } => {
        if let hir_js::ClassMemberKey::Computed(expr_id) = key {
          if hir_expr_contains_await(hir, class_body, *expr_id, visited_bodies) {
            return true;
          }
        }
      }

      hir_js::ClassMemberKind::StaticBlock { body, .. } => {
        if hir_body_contains_await(hir, *body, visited_bodies) {
          return true;
        }
      }
    }
  }

  false
}

fn hir_pat_contains_await(
  hir: &hir_js::LowerResult,
  body: &hir_js::Body,
  pat_id: hir_js::PatId,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
) -> bool {
  let Some(pat) = hir_get_pat(body, pat_id) else {
    return true;
  };
  match &pat.kind {
    hir_js::PatKind::Ident(_) => false,
    hir_js::PatKind::Array(arr) => {
      arr.elements.iter().any(|elem| match elem {
        Some(elem) => {
          hir_pat_contains_await(hir, body, elem.pat, visited_bodies)
            || elem
              .default_value
              .as_ref()
              .is_some_and(|expr_id| hir_expr_contains_await(hir, body, *expr_id, visited_bodies))
        }
        None => false,
      }) || arr
        .rest
        .as_ref()
        .is_some_and(|rest| hir_pat_contains_await(hir, body, *rest, visited_bodies))
    }
    hir_js::PatKind::Object(obj) => {
      obj.props.iter().any(|prop| {
        if let hir_js::ObjectKey::Computed(expr_id) = &prop.key {
          if hir_expr_contains_await(hir, body, *expr_id, visited_bodies) {
            return true;
          }
        }
        hir_pat_contains_await(hir, body, prop.value, visited_bodies)
          || prop.default_value.as_ref().is_some_and(|expr_id| {
            hir_expr_contains_await(hir, body, *expr_id, visited_bodies)
          })
      }) || obj
        .rest
        .as_ref()
        .is_some_and(|rest| hir_pat_contains_await(hir, body, *rest, visited_bodies))
    }
    hir_js::PatKind::Rest(inner) => hir_pat_contains_await(hir, body, **inner, visited_bodies),
    hir_js::PatKind::Assign {
      target,
      default_value,
    } => {
      hir_pat_contains_await(hir, body, *target, visited_bodies)
        || hir_expr_contains_await(hir, body, *default_value, visited_bodies)
    }
    hir_js::PatKind::AssignTarget(expr_id) => hir_expr_contains_await(hir, body, *expr_id, visited_bodies),
  }
}

fn hir_expr_contains_await(
  hir: &hir_js::LowerResult,
  body: &hir_js::Body,
  expr_id: hir_js::ExprId,
  visited_bodies: &mut BTreeSet<hir_js::BodyId>,
) -> bool {
  let Some(expr) = hir_get_expr(body, expr_id) else {
    return true;
  };

  match &expr.kind {
    hir_js::ExprKind::Await { .. } => true,

    hir_js::ExprKind::FunctionExpr { .. } => false,

    hir_js::ExprKind::ClassExpr { body: class_body, .. } => {
      hir_body_contains_await(hir, *class_body, visited_bodies)
    }

    hir_js::ExprKind::Missing
    | hir_js::ExprKind::Ident(_)
    | hir_js::ExprKind::This
    | hir_js::ExprKind::Super
    | hir_js::ExprKind::Literal(_)
    | hir_js::ExprKind::ImportMeta
    | hir_js::ExprKind::NewTarget => false,

    hir_js::ExprKind::Unary { expr, .. } | hir_js::ExprKind::Update { expr, .. } => {
      hir_expr_contains_await(hir, body, *expr, visited_bodies)
    }

    hir_js::ExprKind::Binary { left, right, .. } => {
      hir_expr_contains_await(hir, body, *left, visited_bodies)
        || hir_expr_contains_await(hir, body, *right, visited_bodies)
    }

    hir_js::ExprKind::Assignment { target, value, .. } => {
      hir_pat_contains_await(hir, body, *target, visited_bodies)
        || hir_expr_contains_await(hir, body, *value, visited_bodies)
    }

    hir_js::ExprKind::Call(call) => {
      hir_expr_contains_await(hir, body, call.callee, visited_bodies)
        || call
          .args
          .iter()
          .any(|arg| hir_expr_contains_await(hir, body, arg.expr, visited_bodies))
    }

    hir_js::ExprKind::Member(mem) => {
      hir_expr_contains_await(hir, body, mem.object, visited_bodies)
        || match &mem.property {
          hir_js::ObjectKey::Computed(expr_id) => hir_expr_contains_await(hir, body, *expr_id, visited_bodies),
          _ => false,
        }
    }

    hir_js::ExprKind::Conditional {
      test,
      consequent,
      alternate,
    } => {
      hir_expr_contains_await(hir, body, *test, visited_bodies)
        || hir_expr_contains_await(hir, body, *consequent, visited_bodies)
        || hir_expr_contains_await(hir, body, *alternate, visited_bodies)
    }

    hir_js::ExprKind::Array(arr) => arr.elements.iter().any(|elem| match elem {
      hir_js::ArrayElement::Expr(expr_id) | hir_js::ArrayElement::Spread(expr_id) => {
        hir_expr_contains_await(hir, body, *expr_id, visited_bodies)
      }
      hir_js::ArrayElement::Empty => false,
    }),

    hir_js::ExprKind::Object(obj) => obj.properties.iter().any(|prop| match prop {
      hir_js::ObjectProperty::KeyValue { key, value, .. } => {
        if let hir_js::ObjectKey::Computed(expr_id) = key {
          if hir_expr_contains_await(hir, body, *expr_id, visited_bodies) {
            return true;
          }
        }
        hir_expr_contains_await(hir, body, *value, visited_bodies)
      }
      hir_js::ObjectProperty::Getter { key, .. } | hir_js::ObjectProperty::Setter { key, .. } => {
        matches!(key, hir_js::ObjectKey::Computed(_))
          && match key {
            hir_js::ObjectKey::Computed(expr_id) => hir_expr_contains_await(hir, body, *expr_id, visited_bodies),
            _ => false,
          }
      }
      hir_js::ObjectProperty::Spread(expr_id) => hir_expr_contains_await(hir, body, *expr_id, visited_bodies),
    }),

    hir_js::ExprKind::Template(tpl) => tpl
      .spans
      .iter()
      .any(|span| hir_expr_contains_await(hir, body, span.expr, visited_bodies)),

    hir_js::ExprKind::TaggedTemplate { tag, template } => {
      hir_expr_contains_await(hir, body, *tag, visited_bodies)
        || template
          .spans
          .iter()
          .any(|span| hir_expr_contains_await(hir, body, span.expr, visited_bodies))
    }

    hir_js::ExprKind::Yield { expr, .. } => expr
      .as_ref()
      .is_some_and(|expr_id| hir_expr_contains_await(hir, body, *expr_id, visited_bodies)),

    hir_js::ExprKind::Instantiation { expr, .. }
    | hir_js::ExprKind::TypeAssertion { expr, .. }
    | hir_js::ExprKind::NonNull { expr }
    | hir_js::ExprKind::Satisfies { expr, .. } => {
      hir_expr_contains_await(hir, body, *expr, visited_bodies)
    }

    hir_js::ExprKind::ImportCall { argument, attributes } => {
      hir_expr_contains_await(hir, body, *argument, visited_bodies)
        || attributes
          .as_ref()
          .is_some_and(|expr_id| hir_expr_contains_await(hir, body, *expr_id, visited_bodies))
    }

    hir_js::ExprKind::Jsx(jsx) => {
      let attrs_have_await = jsx.attributes.iter().any(|attr| match attr {
        hir_js::JsxAttr::Named { value, .. } => value.as_ref().is_some_and(|val| match val {
          hir_js::JsxAttrValue::Expression(expr_container) => expr_container
            .expr
            .as_ref()
            .is_some_and(|expr_id| hir_expr_contains_await(hir, body, *expr_id, visited_bodies)),
          hir_js::JsxAttrValue::Element(expr_id) => hir_expr_contains_await(hir, body, *expr_id, visited_bodies),
          hir_js::JsxAttrValue::Text(_) => false,
        }),
        hir_js::JsxAttr::Spread { expr } => hir_expr_contains_await(hir, body, *expr, visited_bodies),
      });
      if attrs_have_await {
        return true;
      }
      jsx.children.iter().any(|child| match child {
        hir_js::JsxChild::Element(expr_id) => hir_expr_contains_await(hir, body, *expr_id, visited_bodies),
        hir_js::JsxChild::Expr(container) => container
          .expr
          .as_ref()
          .is_some_and(|expr_id| hir_expr_contains_await(hir, body, *expr_id, visited_bodies)),
        hir_js::JsxChild::Text(_) => false,
      })
    }
  }
}

fn hir_get_expr(body: &hir_js::Body, id: hir_js::ExprId) -> Option<&hir_js::Expr> {
  body.exprs.get(id.0 as usize)
}

fn hir_get_stmt(body: &hir_js::Body, id: hir_js::StmtId) -> Option<&hir_js::Stmt> {
  body.stmts.get(id.0 as usize)
}

fn hir_get_pat(body: &hir_js::Body, id: hir_js::PatId) -> Option<&hir_js::Pat> {
  body.pats.get(id.0 as usize)
}
