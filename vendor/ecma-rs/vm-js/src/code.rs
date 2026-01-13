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
use diagnostics::FileId;
use derive_visitor::visitor_enter_fn;
use derive_visitor::Drive;
use parse_js::ast::class_or_object::{ClassOrObjKey, ClassOrObjVal, ObjMemberType};
use parse_js::ast::expr::lit::{LitArrElem, LitTemplatePart};
use parse_js::ast::expr::pat::{ArrPat, ObjPat, Pat};
use parse_js::ast::expr::Expr;
use parse_js::ast::func::Func;
use parse_js::ast::node::{Node, ParenthesizedExpr};
use parse_js::ast::stmt::{ForInOfLhs, ForTripleStmtInit, Stmt};
use parse_js::operator::OperatorName;
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use std::cell::Cell;
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
  /// True if the compiled (HIR) execution path must fall back to the AST interpreter.
  ///
  /// The HIR executor does not currently support async / generator / async-generator function
  /// bodies (`await`, `yield`, `yield*`), so any script containing them must be re-run from source
  /// to avoid mid-execution `VmError::Unimplemented` failures.
  pub requires_ast_fallback: bool,
  /// Whether this script/module contains a top-level `await` (or `for await..of`) that requires
  /// async evaluation.
  pub contains_top_level_await: bool,
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
          // `parse-js` only enables `AwaitExpression` parsing at top-level in module mode. Classic
          // scripts that use top-level await are parsed using the module grammar as a best-effort
          // fallback, then validated/evaluated with Script semantics.
          let opts = ParseOptions {
            dialect: Dialect::Ecma,
            source_type: SourceType::Module,
          };
          let parsed_module = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parse_with_options(source.text.as_ref(), opts)
          }))
          .map_err(|_| VmError::InvariantViolation("parse-js panicked while compiling a script"))?;

          match parsed_module {
            Ok(parsed) => {
              // Only accept the module parse as a script when it actually contains top-level await
              // and does not contain module-only syntax (import/export).
              let has_await = parsed.stx.body.iter().any(stmt_contains_await);
              let has_module_syntax = parsed.stx.body.iter().any(stmt_is_module_only);
              if has_await && !has_module_syntax {
                parsed
              } else {
                return Err(VmError::Syntax(vec![script_err.to_diagnostic(FileId(0))]));
              }
            }
            Err(_) => return Err(VmError::Syntax(vec![script_err.to_diagnostic(FileId(0))])),
          }
        }
      }
    };

    let contains_top_level_await = {
      let mut tick = || Ok(());
      let strict = detect_use_strict_directive(&parsed.stx.body, &mut tick)?;
      let has_await = parsed.stx.body.iter().any(stmt_contains_await);
      crate::early_errors::validate_top_level(
        &parsed.stx.body,
        crate::early_errors::EarlyErrorOptions {
          strict,
          allow_top_level_await: has_await,
          is_module: false,
          allow_super_call: false,
        },
        &mut tick,
      )?;
      has_await
    };

    let feature_flags = ast_feature_flags(&parsed);
    let contains_async_generators = feature_flags.contains_async_generators;
    let contains_generators = feature_flags.contains_generators;
    let contains_async_functions = feature_flags.contains_async_functions;
    let requires_ast_fallback = contains_generators || contains_async_functions;

    let hir = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      hir_js::lower_file(FileId(0), hir_js::FileKind::Js, &parsed)
    }))
    .map_err(|_| VmError::InvariantViolation("hir-js panicked while lowering a script"))?;
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
      requires_ast_fallback,
      contains_top_level_await,
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
    .map_err(|err| VmError::Syntax(vec![err.to_diagnostic(FileId(0))]))?;

    let contains_top_level_await = parsed.stx.body.iter().any(stmt_contains_await);

    {
      let mut tick = || Ok(());
      crate::early_errors::validate_top_level(
        &parsed.stx.body,
        crate::early_errors::EarlyErrorOptions::module(),
        &mut tick,
      )?;
      crate::module_record::validate_module_static_semantics_early_errors(&parsed, &mut tick)?;
    }

    let feature_flags = ast_feature_flags(&parsed);
    let contains_async_generators = feature_flags.contains_async_generators;
    let contains_generators = feature_flags.contains_generators;
    let contains_async_functions = feature_flags.contains_async_functions;
    let requires_ast_fallback = contains_generators || contains_async_functions;

    let hir = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      hir_js::lower_file(FileId(0), hir_js::FileKind::Js, &parsed)
    }))
    .map_err(|_| VmError::InvariantViolation("hir-js panicked while lowering a module"))?;

    let estimated_hir_bytes = source.text.len().saturating_mul(8);
    let external_memory = heap.charge_external(estimated_hir_bytes)?;
    let hir = arc_try_new_vm(hir)?;
    Ok(arc_try_new_vm(Self {
      source,
      hir,
      contains_async_generators,
      contains_generators,
      contains_async_functions,
      requires_ast_fallback,
      contains_top_level_await,
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

    let parsed = match vm.parse_top_level_with_budget(&source.text, opts) {
      Ok(parsed) => parsed,
      Err(VmError::Syntax(script_diags)) => {
        // See `compile_script`: top-level await scripts are parsed using the module grammar as a
        // best-effort fallback.
        let opts = ParseOptions {
          dialect: Dialect::Ecma,
          source_type: SourceType::Module,
        };
        match vm.parse_top_level_with_budget(&source.text, opts) {
          Ok(parsed) => {
            let has_await = parsed.stx.body.iter().any(stmt_contains_await);
            let has_module_syntax = parsed.stx.body.iter().any(stmt_is_module_only);
            if has_await && !has_module_syntax {
              parsed
            } else {
              return Err(VmError::Syntax(script_diags));
            }
          }
          Err(VmError::Syntax(_)) => return Err(VmError::Syntax(script_diags)),
          Err(err) => return Err(err),
        }
      }
      Err(err) => return Err(err),
    };
    let strict = {
      let mut tick = || vm.tick();
      detect_use_strict_directive(&parsed.stx.body, &mut tick)?
    };
    let has_top_level_await = parsed.stx.body.iter().any(stmt_contains_await);
    {
      let mut tick = || vm.tick();
      crate::early_errors::validate_top_level(
        &parsed.stx.body,
        crate::early_errors::EarlyErrorOptions {
          strict,
          allow_top_level_await: has_top_level_await,
          is_module: false,
          allow_super_call: false,
        },
        &mut tick,
      )?;
    }

    let feature_flags = ast_feature_flags(&parsed);
    let contains_async_generators = feature_flags.contains_async_generators;
    let contains_generators = feature_flags.contains_generators;
    let contains_async_functions = feature_flags.contains_async_functions;
    let requires_ast_fallback = contains_generators || contains_async_functions;

    let hir = hir_js::lower_file(FileId(0), hir_js::FileKind::Js, &parsed);
    let estimated_hir_bytes = source.text.len().saturating_mul(8);
    let external_memory = heap.charge_external(estimated_hir_bytes)?;
    let hir = arc_try_new_vm(hir)?;
    Ok(arc_try_new_vm(Self {
      source,
      hir,
      contains_async_generators,
      contains_generators,
      contains_async_functions,
      requires_ast_fallback,
      contains_top_level_await: has_top_level_await,
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
    {
      let mut tick = || vm.tick();
      crate::early_errors::validate_top_level(
        &parsed.stx.body,
        crate::early_errors::EarlyErrorOptions::module(),
        &mut tick,
      )?;
      crate::module_record::validate_module_static_semantics_early_errors(&parsed, &mut tick)?;
    }
    let feature_flags = ast_feature_flags(&parsed);
    let contains_async_generators = feature_flags.contains_async_generators;
    let contains_generators = feature_flags.contains_generators;
    let contains_async_functions = feature_flags.contains_async_functions;
    let requires_ast_fallback = contains_generators || contains_async_functions;
    let hir = hir_js::lower_file(FileId(0), hir_js::FileKind::Js, &parsed);
    let estimated_hir_bytes = source.text.len().saturating_mul(8);
    let external_memory = heap.charge_external(estimated_hir_bytes)?;
    let hir = arc_try_new_vm(hir)?;
    Ok(arc_try_new_vm(Self {
      source,
      hir,
      contains_async_generators,
      contains_generators,
      contains_async_functions,
      requires_ast_fallback,
      contains_top_level_await,
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
    let requires_ast_fallback = contains_generators || contains_async_functions;
    let contains_top_level_await = parsed.stx.body.iter().any(stmt_contains_await);
    let hir = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      hir_js::lower_file(FileId(0), hir_js::FileKind::Js, parsed)
    }))
    .map_err(|_| VmError::InvariantViolation("hir-js panicked while lowering a module"))?;

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
      requires_ast_fallback,
      contains_top_level_await,
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
}

#[derive(Clone, Copy, Debug, Default)]
struct AstFeatureFlags {
  contains_generators: bool,
  contains_async_functions: bool,
  contains_async_generators: bool,
}

fn ast_feature_flags<T: Drive>(root: &T) -> AstFeatureFlags {
  let found = Cell::new(AstFeatureFlags::default());
  let mut visitor = visitor_enter_fn(|func: &Func| {
    let mut flags = found.get();
    flags.contains_generators |= func.generator;
    flags.contains_async_functions |= func.async_;
    flags.contains_async_generators |= func.async_ && func.generator;
    found.set(flags);
  });
  root.drive(&mut visitor);
  found.get()
}

fn detect_use_strict_directive<F>(stmts: &[Node<Stmt>], tick: &mut F) -> Result<bool, VmError>
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
      return Ok(true);
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
            ClassOrObjVal::StaticBlock(block) => block.stx.body.iter().any(stmt_contains_await),
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
    ForInOfLhs::Decl((_, pat_decl)) => pat_contains_await(&pat_decl.stx.pat.stx),
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
            ClassOrObjVal::StaticBlock(block) => block.stx.body.iter().any(stmt_contains_await),
            _ => false,
          }
        })
    }
    Stmt::Expr(expr_stmt) => expr_contains_await(&expr_stmt.stx.expr),
    Stmt::Return(ret) => ret.stx.value.as_ref().is_some_and(expr_contains_await),
    Stmt::Throw(throw_stmt) => expr_contains_await(&throw_stmt.stx.value),
    Stmt::VarDecl(decl) => decl
      .stx
      .declarators
      .iter()
      .any(|d| {
        d.initializer.as_ref().is_some_and(expr_contains_await)
          || pat_contains_await(&d.pattern.stx.pat.stx)
      }),
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
        ForTripleStmtInit::Decl(decl) => decl
          .stx
          .declarators
          .iter()
          .any(|d| {
            d.initializer.as_ref().is_some_and(expr_contains_await)
              || pat_contains_await(&d.pattern.stx.pat.stx)
          }),
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

fn stmt_is_module_only(stmt: &Node<Stmt>) -> bool {
  match &*stmt.stx {
    Stmt::Import(_) | Stmt::ExportList(_) | Stmt::ExportDefaultExpr(_) => true,
    Stmt::FunctionDecl(decl) => decl.stx.export || decl.stx.export_default,
    Stmt::ClassDecl(decl) => decl.stx.export || decl.stx.export_default,
    Stmt::VarDecl(decl) => decl.stx.export,
    _ => false,
  }
}
