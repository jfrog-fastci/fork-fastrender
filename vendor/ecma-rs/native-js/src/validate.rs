//! Native-js "strict subset" validator for the typechecked (HIR-backed) pipeline.
//!
//! The native compiler backend does **not** implement full JS/TS semantics yet.
//! This pass rejects syntax and types that we cannot lower safely, even if
//! `typecheck-ts` accepts them.
//!
//! This validator is intentionally conservative: if something is not explicitly
//! supported, it is rejected.
//!
//! ## Diagnostic codes
//!
//! Codes emitted by this validator are stable:
//! - `NJS0009`: unsupported syntax in the native-js strict subset
//! - `NJS0010`: unsupported type in the native-js strict subset

use crate::codes;
use crate::resolve::{BindingId, Resolver};
use diagnostics::{Diagnostic, Span, TextRange};
use hir_js::{
  ArrayElement, AssignOp, BinaryOp, Body, BodyId, BodyKind, ExprId, ExprKind, FileKind, ForInit, FunctionBody,
  Literal, NameId, ObjectKey, ObjectProperty, PatKind, StmtId, StmtKind, TypeExprId, TypeExprKind, UnaryOp, VarDecl,
  VarDeclKind,
};
use std::collections::{HashMap, HashSet};
use typecheck_ts::{Program, TypeKindSummary};
use types_ts_interned as tti;
use parse_js::num::JsNumber;

fn number_literal_to_i64(raw: &str) -> Option<i64> {
  let n = JsNumber::from_literal(raw).map(|n| n.0)?;
  if !n.is_finite() || n.fract() != 0.0 {
    return None;
  }
  if n < i64::MIN as f64 || n > i64::MAX as f64 {
    return None;
  }
  Some(n as i64)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TypeKindGroup {
  Any,
  Unknown,
  Never,
  Void,
  Null,
  Undefined,
  Boolean,
  Number,
  String,
  BigInt,
  Symbol,
  Object,
  Tuple,
  Array,
  Union,
  Intersection,
  Callable,
  Ref,
  Other,
}

impl TypeKindGroup {
  fn as_str(self) -> &'static str {
    match self {
      TypeKindGroup::Any => "any",
      TypeKindGroup::Unknown => "unknown",
      TypeKindGroup::Never => "never",
      TypeKindGroup::Void => "void",
      TypeKindGroup::Null => "null",
      TypeKindGroup::Undefined => "undefined",
      TypeKindGroup::Boolean => "boolean",
      TypeKindGroup::Number => "number",
      TypeKindGroup::String => "string",
      TypeKindGroup::BigInt => "bigint",
      TypeKindGroup::Symbol => "symbol",
      TypeKindGroup::Object => "object",
      TypeKindGroup::Tuple => "tuple",
      TypeKindGroup::Array => "array",
      TypeKindGroup::Union => "union",
      TypeKindGroup::Intersection => "intersection",
      TypeKindGroup::Callable => "callable",
      TypeKindGroup::Ref => "ref",
      TypeKindGroup::Other => "other",
    }
  }
}

fn type_kind_group_from_summary(kind: &TypeKindSummary) -> TypeKindGroup {
  match kind {
    TypeKindSummary::Any => TypeKindGroup::Any,
    TypeKindSummary::Unknown => TypeKindGroup::Unknown,
    TypeKindSummary::Never => TypeKindGroup::Never,
    TypeKindSummary::Void => TypeKindGroup::Void,
    TypeKindSummary::Null => TypeKindGroup::Null,
    TypeKindSummary::Undefined => TypeKindGroup::Undefined,
    TypeKindSummary::Boolean | TypeKindSummary::BooleanLiteral(_) => TypeKindGroup::Boolean,
    TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_) => TypeKindGroup::Number,
    TypeKindSummary::String | TypeKindSummary::StringLiteral(_) | TypeKindSummary::TemplateLiteral => TypeKindGroup::String,
    TypeKindSummary::BigInt | TypeKindSummary::BigIntLiteral(_) => TypeKindGroup::BigInt,
    TypeKindSummary::Symbol | TypeKindSummary::UniqueSymbol => TypeKindGroup::Symbol,
    TypeKindSummary::EmptyObject | TypeKindSummary::Object => TypeKindGroup::Object,
    TypeKindSummary::Tuple { .. } => TypeKindGroup::Tuple,
    TypeKindSummary::Array { .. } => TypeKindGroup::Array,
    TypeKindSummary::Union { .. } => TypeKindGroup::Union,
    TypeKindSummary::Intersection { .. } => TypeKindGroup::Intersection,
    TypeKindSummary::Callable { .. } => TypeKindGroup::Callable,
    TypeKindSummary::Ref { .. } => TypeKindGroup::Ref,
    _ => TypeKindGroup::Other,
  }
}

fn type_expr_kind_group(
  arenas: &hir_js::TypeArenas,
  type_expr: TypeExprId,
) -> Option<TypeKindGroup> {
  let expr = arenas.type_exprs.get(type_expr.0 as usize)?;
  Some(match &expr.kind {
    TypeExprKind::Any => TypeKindGroup::Any,
    TypeExprKind::Unknown => TypeKindGroup::Unknown,
    TypeExprKind::Never => TypeKindGroup::Never,
    TypeExprKind::Void => TypeKindGroup::Void,
    TypeExprKind::Null => TypeKindGroup::Null,
    TypeExprKind::Undefined => TypeKindGroup::Undefined,
    TypeExprKind::Boolean => TypeKindGroup::Boolean,
    TypeExprKind::Number => TypeKindGroup::Number,
    TypeExprKind::String => TypeKindGroup::String,
    TypeExprKind::BigInt => TypeKindGroup::BigInt,
    TypeExprKind::Symbol | TypeExprKind::UniqueSymbol => TypeKindGroup::Symbol,
    TypeExprKind::Object | TypeExprKind::TypeLiteral(_) => TypeKindGroup::Object,
    TypeExprKind::Literal(lit) => match lit {
      hir_js::TypeLiteral::String(_) => TypeKindGroup::String,
      hir_js::TypeLiteral::Number(_) => TypeKindGroup::Number,
      hir_js::TypeLiteral::BigInt(_) => TypeKindGroup::BigInt,
      hir_js::TypeLiteral::Boolean(_) => TypeKindGroup::Boolean,
      hir_js::TypeLiteral::Null => TypeKindGroup::Null,
    },
    TypeExprKind::Array(_) => TypeKindGroup::Array,
    TypeExprKind::Tuple(_) => TypeKindGroup::Tuple,
    TypeExprKind::Union(_) => TypeKindGroup::Union,
    TypeExprKind::Intersection(_) => TypeKindGroup::Intersection,
    TypeExprKind::Function(_) | TypeExprKind::Constructor(_) => TypeKindGroup::Callable,
    TypeExprKind::Parenthesized(inner) => return type_expr_kind_group(arenas, *inner),
    // Type references (and other advanced type operators) require resolution/expansion. Fall back
    // to the checker-provided expression type when we can't classify directly.
    TypeExprKind::TypeRef(_)
    | TypeExprKind::Intrinsic
    | TypeExprKind::This
    | TypeExprKind::TypeQuery(_)
    | TypeExprKind::KeyOf(_)
    | TypeExprKind::IndexedAccess { .. }
    | TypeExprKind::Conditional(_)
    | TypeExprKind::Infer(_)
    | TypeExprKind::Mapped(_)
    | TypeExprKind::TemplateLiteral(_)
    | TypeExprKind::TypePredicate(_)
    | TypeExprKind::Import(_) => return None,
  })
}

fn type_may_be_nullish(
  program: &Program,
  ty: typecheck_ts::TypeId,
  cache: &mut HashMap<typecheck_ts::TypeId, bool>,
  visiting: &mut HashSet<typecheck_ts::TypeId>,
) -> bool {
  if let Some(hit) = cache.get(&ty) {
    return *hit;
  }
  if !visiting.insert(ty) {
    // Break cycles conservatively (no nullish found along this path).
    return false;
  }

  let result = match program.interned_type_kind(ty) {
    tti::TypeKind::Any | tti::TypeKind::Unknown => true,
    tti::TypeKind::Null | tti::TypeKind::Undefined | tti::TypeKind::Void => true,
    tti::TypeKind::Never => false,
    tti::TypeKind::Infer { constraint, .. } => constraint.is_some_and(|inner| {
      type_may_be_nullish(program, inner, cache, visiting)
    }),
    tti::TypeKind::Tuple(elems) => elems
      .iter()
      .any(|elem| type_may_be_nullish(program, elem.ty, cache, visiting)),
    tti::TypeKind::Array { ty, .. } => type_may_be_nullish(program, ty, cache, visiting),
    tti::TypeKind::Union(members) | tti::TypeKind::Intersection(members) => members
      .iter()
      .any(|member| type_may_be_nullish(program, *member, cache, visiting)),
    tti::TypeKind::Ref { def, args } => {
      if args
        .iter()
        .any(|arg| type_may_be_nullish(program, *arg, cache, visiting))
      {
        true
      } else {
        let declared = program.declared_type_of_def_interned(def);
        type_may_be_nullish(program, declared, cache, visiting)
      }
    }
    tti::TypeKind::Predicate { asserted, .. } => asserted.is_some_and(|inner| {
      type_may_be_nullish(program, inner, cache, visiting)
    }),
    tti::TypeKind::Conditional {
      check,
      extends,
      true_ty,
      false_ty,
      ..
    } => {
      type_may_be_nullish(program, check, cache, visiting)
        || type_may_be_nullish(program, extends, cache, visiting)
        || type_may_be_nullish(program, true_ty, cache, visiting)
        || type_may_be_nullish(program, false_ty, cache, visiting)
    }
    tti::TypeKind::Mapped(mapped) => {
      type_may_be_nullish(program, mapped.source, cache, visiting)
        || type_may_be_nullish(program, mapped.value, cache, visiting)
        || mapped
          .name_type
          .is_some_and(|inner| type_may_be_nullish(program, inner, cache, visiting))
        || mapped
          .as_type
          .is_some_and(|inner| type_may_be_nullish(program, inner, cache, visiting))
    }
    tti::TypeKind::TemplateLiteral(tpl) => tpl
      .spans
      .iter()
      .any(|chunk| type_may_be_nullish(program, chunk.ty, cache, visiting)),
    tti::TypeKind::Intrinsic { ty, .. } => type_may_be_nullish(program, ty, cache, visiting),
    tti::TypeKind::IndexedAccess { obj, index } => {
      type_may_be_nullish(program, obj, cache, visiting)
        || type_may_be_nullish(program, index, cache, visiting)
    }
    tti::TypeKind::KeyOf(inner) => type_may_be_nullish(program, inner, cache, visiting),
    tti::TypeKind::TypeParam(_) => true,
    _ => false,
  };

  visiting.remove(&ty);
  cache.insert(ty, result);
  result
}

/// Validate that all files reachable from `program`'s roots use only the strict
/// subset currently supported by the native compiler backend.
///
/// This is intended to run **after** a clean `program.check()` and **before**
/// native LLVM IR generation.
pub fn validate_strict_subset(program: &Program) -> Result<(), Vec<Diagnostic>> {
  let mut diagnostics = Vec::new();
  let resolver = Resolver::new(program);

  for file in program.reachable_files() {
    let Some(lowered) = program.hir_lowered(file) else {
      continue;
    };

    // Don't validate declaration files; they are type-only and not codegen'd.
    if matches!(lowered.hir.file_kind, FileKind::Dts) {
      continue;
    }

    for body in program.bodies_in_file(file) {
      validate_body(
        program,
        &resolver,
        file,
        body,
        &lowered,
        &mut diagnostics,
      );
    }
  }

  diagnostics::sort_diagnostics(&mut diagnostics);
  if diagnostics.is_empty() {
    Ok(())
  } else {
    Err(diagnostics)
  }
}

fn validate_body(
  program: &Program,
  resolver: &Resolver<'_>,
  file: typecheck_ts::FileId,
  body: BodyId,
  lowered: &hir_js::LowerResult,
  out: &mut Vec<Diagnostic>,
) {
  let Some(body_data) = lowered.body(body) else {
    return;
  };

  // Ensure the body is checked before running syntax validation. Some program queries (like
  // `debug_symbol_occurrences`) are populated lazily by `check_body`, and the strict-subset syntax
  // rules consult identifier resolution (for intrinsic detection and direct-call validation).
  let checked = program.check_body(body);

  let syntax = validate_body_syntax(
    program,
    file,
    body_data,
    lowered,
    resolver,
    out,
  );
  validate_body_types(
    program,
    file,
    checked.as_ref(),
    body_data,
    lowered,
    resolver,
    &syntax,
    out,
  );
}

#[derive(Debug)]
struct BodySyntaxInfo {
  /// Expression ids which are allowed to be an intrinsic `print(x)` call in statement position.
  ///
  /// `print` is lowered specially (to `printf`) by the native HIR backend, but only when it appears as a standalone
  /// statement. `print(...)` is not supported as an expression.
  allowed_print_stmt_call_expr: Vec<bool>,
}

impl BodySyntaxInfo {
  fn new(body: &Body) -> Self {
    Self {
      allowed_print_stmt_call_expr: vec![false; body.exprs.len()],
    }
  }
}

fn validate_body_syntax(
  program: &Program,
  file: typecheck_ts::FileId,
  body: &Body,
  lowered: &hir_js::LowerResult,
  resolver: &Resolver<'_>,
  out: &mut Vec<Diagnostic>,
) -> BodySyntaxInfo {
  let mut info = BodySyntaxInfo::new(body);
  let file_resolver = resolver.for_file(file);
  // Body-level constructs.
  match body.kind {
    BodyKind::Class => {
      push_unsupported_syntax(
        out,
        Span::new(file, body.span),
        "class bodies are not supported by native-js yet",
      );
    }
    BodyKind::Unknown => {
      push_unsupported_syntax(
        out,
        Span::new(file, body.span),
        "unsupported body kind in native-js strict subset",
      );
    }
    _ => {}
  }

  if let Some(func) = &body.function {
    if func.async_ {
      push_unsupported_syntax(
        out,
        Span::new(file, body.span),
        "`async` functions are not supported by native-js yet",
      );
    }
    if func.generator {
      push_unsupported_syntax(
        out,
        Span::new(file, body.span),
        "generator functions are not supported by native-js yet",
      );
    }

    for param in func.params.iter() {
      let span = body
        .pats
        .get(param.pat.0 as usize)
        .map(|pat| Span::new(file, pat.span))
        .unwrap_or_else(|| Span::new(file, body.span));

      if param.optional {
        push_unsupported_syntax(out, span, "optional parameters are not supported by native-js yet");
      }
      if param.rest {
        push_unsupported_syntax(out, span, "rest parameters are not supported by native-js yet");
      }
      if param.default.is_some() {
        push_unsupported_syntax(
          out,
          span,
          "default parameter values are not supported by native-js yet",
        );
      }
    }
  }

  // Statement-driven checks that depend on source order (intrinsic call allowance, loop/label validation, scoping).
  let root_stmts: Vec<StmtId> = match body.function.as_ref().map(|f| &f.body) {
    Some(FunctionBody::Block(stmts)) => stmts.to_vec(),
    Some(FunctionBody::Expr(_)) => Vec::new(),
    None => body.root_stmts.clone(),
  };
  let mut state = SyntaxState::new(file, body, lowered, file_resolver, out, &mut info);
  for stmt in root_stmts {
    state.validate_stmt(stmt);
  }

  let file_resolver = resolver.for_file(file);
  for (expr_idx, expr) in body.exprs.iter().enumerate() {
    match &expr.kind {
      ExprKind::Super => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`super` is not supported yet");
      }
      ExprKind::This => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`this` is not supported by native-js yet");
      }
      ExprKind::NewTarget => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`new.target` is not supported yet");
      }
      ExprKind::ImportMeta => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`import.meta` is not supported yet");
      }
      ExprKind::Yield { .. } => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`yield` is not supported yet");
      }
      ExprKind::Await { .. } => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`await` is not supported yet");
      }
      ExprKind::Unary { op: UnaryOp::Await, .. } => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`await` is not supported yet");
      }
      ExprKind::Call(call) => {
        if *info
          .allowed_print_stmt_call_expr
          .get(expr_idx)
          .unwrap_or(&false)
        {
          continue;
        }
        if call.optional {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "optional calls are not supported by native-js yet",
          );
        }
        if call.args.iter().any(|arg| arg.spread) {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "spread call arguments are not supported by native-js yet",
          );
        }

        // Only allow the simplest call shape: `foo(a, b)` where `foo` is an identifier.
        // (Calls through member access / indirect expressions aren't lowered yet.)
        let callee_is_ident_expr = matches!(
          body
            .exprs
            .get(call.callee.0 as usize)
            .map(|e| &e.kind),
          Some(ExprKind::Ident(_))
        );
        if !callee_is_ident_expr {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "only direct identifier calls are supported by native-js yet",
          );
        }
        // Detect `eval(...)` and `Function(...)` / `new Function(...)` by callee identifier.
        if callee_is_ident(body, lowered, call.callee, "eval") {
          push_unsupported_syntax(out, Span::new(file, expr.span), "`eval()` is not supported");
        }
        if callee_is_ident(body, lowered, call.callee, "Function") {
          if call.is_new {
            push_unsupported_syntax(out, Span::new(file, expr.span), "`new Function()` is not supported");
          } else {
            push_unsupported_syntax(out, Span::new(file, expr.span), "`Function()` is not supported");
          }
        } else if call.is_new {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "`new` expressions are not supported by native-js yet",
          );
        }

        // `print(...)` is a codegen intrinsic, but only in statement position.
        let is_global_print_intrinsic =
          callee_checked_intrinsic(&file_resolver, body, lowered, call.callee)
            == Some(crate::builtins::NativeJsIntrinsic::Print);
        if is_global_print_intrinsic {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "the `print` intrinsic is only supported as a statement (`print(x)` with one argument and no spread)",
          );
        } else if !call.optional && !call.is_new && !call.args.iter().any(|arg| arg.spread) && callee_is_ident_expr {
          let resolved = (|| {
            let BindingId::Def(def) = file_resolver.resolve_expr_ident(body, call.callee)? else {
              return None;
            };
            let resolved = resolve_import_def(program, def)?;
            let expected = codegen_callable_function_param_count(program, resolved)?;
            Some((resolved, expected))
          })();

          if let Some((_resolved, expected)) = resolved {
            if expected != call.args.len() {
              let callee_name = body
                .exprs
                .get(call.callee.0 as usize)
                .and_then(|e| match &e.kind {
                  ExprKind::Ident(name) => lowered.names.resolve(*name),
                  _ => None,
                })
                .unwrap_or("<callee>");
              push_unsupported_syntax(
                out,
                Span::new(file, expr.span),
                format!(
                  "call to `{callee_name}` must pass exactly {expected} arguments (got {got}) in native-js strict subset",
                  got = call.args.len(),
                ),
              );
            }
          } else {
            push_unsupported_syntax(
              out,
              Span::new(file, expr.span),
              "call callee must resolve to a top-level function definition",
            );
          }
        }
      }
      ExprKind::Literal(Literal::Null) => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`null` literals are not supported by native-js yet");
      }
      ExprKind::Literal(Literal::Undefined) => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "`undefined` values are not supported by native-js yet",
        );
      }
      ExprKind::Literal(Literal::BigInt(_)) => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`bigint` literals are not supported by native-js yet");
      }
      ExprKind::Literal(Literal::Regex(_)) => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "regex literals are not supported by native-js yet");
      }
      ExprKind::Unary { op, .. } => {
        // `await` is handled above.
        if !matches!(op, UnaryOp::Plus | UnaryOp::Minus | UnaryOp::Not | UnaryOp::BitNot) {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            format!("unsupported unary operator `{op:?}` in native-js strict subset"),
          );
        }
      }
      ExprKind::Binary { op, .. } => {
        if !matches!(
          op,
          BinaryOp::Add
            | BinaryOp::Subtract
            | BinaryOp::Multiply
            | BinaryOp::Divide
            | BinaryOp::Remainder
            | BinaryOp::BitAnd
            | BinaryOp::BitOr
            | BinaryOp::BitXor
            | BinaryOp::ShiftLeft
            | BinaryOp::ShiftRight
            | BinaryOp::ShiftRightUnsigned
            | BinaryOp::LessThan
            | BinaryOp::LessEqual
            | BinaryOp::GreaterThan
            | BinaryOp::GreaterEqual
            | BinaryOp::Equality
            | BinaryOp::Inequality
            | BinaryOp::StrictEquality
            | BinaryOp::StrictInequality
            | BinaryOp::LogicalOr
            | BinaryOp::LogicalAnd
            | BinaryOp::Comma
        ) {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            format!("unsupported binary operator `{op:?}` in native-js strict subset"),
          );
        }
      }
      ExprKind::Assignment { op, .. } => {
        if !matches!(
          op,
          AssignOp::Assign
            | AssignOp::AddAssign
            | AssignOp::SubAssign
            | AssignOp::MulAssign
            | AssignOp::DivAssign
            | AssignOp::RemAssign
        ) {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            format!("unsupported assignment operator `{op:?}` in native-js strict subset"),
          );
        }
      }
      ExprKind::Ident(name) => {
        if lowered.names.resolve(*name) == Some("arguments") {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "the `arguments` object is not supported by native-js yet",
          );
        }
      }
      ExprKind::Member(member) => {
        if member.optional {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "optional chaining is not supported by native-js yet",
          );
          continue;
        }

        // Reject `__proto__` access (prototype mutation).
        // Note: even if the receiver is a plain object in our subset, `__proto__` has special
        // semantics in JS and is not a normal data property.
        let is_proto = match &member.property {
          ObjectKey::Ident(name) => lowered.names.resolve(*name) == Some("__proto__"),
          ObjectKey::Computed(key_expr) => body
            .exprs
            .get(key_expr.0 as usize)
            .is_some_and(|e| matches!(&e.kind, ExprKind::Literal(Literal::String(s)) if s.lossy == "__proto__")),
          ObjectKey::String(s) => s == "__proto__",
          _ => false,
        };
        if is_proto {
          push_unsupported_syntax(out, Span::new(file, expr.span), "`__proto__` is not supported by native-js");
          continue;
        }

        // Only allow member-expression forms we can lower:
        // - `arr[i]` (array/tuple indexing)
        // - `arr.length`
        // - `obj.foo` / `obj['foo']` / `obj[0]` for supported plain object types
        match &member.property {
          ObjectKey::Ident(_) => {}
          ObjectKey::Computed(_) => {}
          _ => {
            push_unsupported_syntax(
              out,
              Span::new(file, expr.span),
              "unsupported member expression property syntax in native-js strict subset",
            );
          }
        }
      }
      ExprKind::Object(obj) => {
        // Allow a strict subset of object literal syntax:
        // - no spread properties
        // - no computed keys
        // - no getters/setters/methods
        // - reject `__proto__` (prototype mutation)
        for prop in obj.properties.iter() {
          match prop {
            ObjectProperty::Spread(_) => {
              push_unsupported_syntax(
                out,
                Span::new(file, expr.span),
                "object literals with spread properties are not supported by native-js yet",
              );
            }
            ObjectProperty::Getter { .. } | ObjectProperty::Setter { .. } => {
              push_unsupported_syntax(
                out,
                Span::new(file, expr.span),
                "object literals with getters/setters are not supported by native-js yet",
              );
            }
            ObjectProperty::KeyValue {
              key,
              method,
              value: _,
              shorthand: _,
            } => {
              if *method {
                push_unsupported_syntax(
                  out,
                  Span::new(file, expr.span),
                  "object literals with methods are not supported by native-js yet",
                );
                continue;
              }

              match key {
                ObjectKey::Ident(name) => {
                  if lowered.names.resolve(*name) == Some("__proto__") {
                    push_unsupported_syntax(out, Span::new(file, expr.span), "`__proto__` is not supported by native-js");
                  }
                }
                ObjectKey::String(s) => {
                  if s == "__proto__" {
                    push_unsupported_syntax(out, Span::new(file, expr.span), "`__proto__` is not supported by native-js");
                  }
                }
                ObjectKey::Number(raw) => {
                  if number_literal_to_i64(raw).is_none() {
                    push_unsupported_syntax(
                      out,
                      Span::new(file, expr.span),
                      format!("invalid numeric object property key literal `{raw}`"),
                    );
                  }
                }
                ObjectKey::Computed(_) => {
                  push_unsupported_syntax(
                    out,
                    Span::new(file, expr.span),
                    "computed property keys in object literals are not supported by native-js yet",
                  );
                }
              }
            }
          }
        }
      }
      ExprKind::Array(arr) => {
        // Array literals are supported as the backing representation for both `T[]` and tuple
        // types, but only in the simplest form (no spreads / holes).
        if arr
          .elements
          .iter()
          .any(|el| !matches!(el, ArrayElement::Expr(_)))
        {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "array literals with spreads or holes are not supported by native-js yet",
          );
        }
      }
      ExprKind::ClassExpr { .. } => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "classes are not supported by native-js yet");
      }
      ExprKind::Template(_) | ExprKind::TaggedTemplate { .. } => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "template literals are not supported by native-js yet",
        );
      }
      ExprKind::ImportCall { .. } => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "`import()` is not supported by native-js yet",
        );
      }
      ExprKind::Jsx(_) => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "JSX is not supported by native-js yet");
      }
      ExprKind::Conditional { .. } => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "conditional (`?:`) expressions are not supported by native-js yet",
        );
      }
      ExprKind::FunctionExpr { .. } => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "function expressions are not supported by native-js yet");
      }
      ExprKind::Missing => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "unsupported expression in native-js strict subset",
        );
      }
      _ => {}
    }
  }

  for pat in body.pats.iter() {
    match &pat.kind {
      PatKind::Ident(name) if lowered.names.resolve(*name) == Some("arguments") => {
        push_unsupported_syntax(
          out,
          Span::new(file, pat.span),
          "the `arguments` identifier is not supported by native-js yet",
        );
      }
      // Destructuring patterns imply object/array operations; reject until lowering exists.
      PatKind::Array(_) => {
        push_unsupported_syntax(
          out,
          Span::new(file, pat.span),
          "array destructuring patterns are not supported by native-js yet",
        );
      }
      PatKind::Object(_) => {
        push_unsupported_syntax(
          out,
          Span::new(file, pat.span),
          "object destructuring patterns are not supported by native-js yet",
        );
      }
      _ => {}
    }
  }

  info
}

fn validate_body_types(
  program: &Program,
  file: typecheck_ts::FileId,
  result: &typecheck_ts::BodyCheckResult,
  hir: &Body,
  lowered: &hir_js::LowerResult,
  resolver: &Resolver<'_>,
  syntax: &BodySyntaxInfo,
  out: &mut Vec<Diagnostic>,
) {
  let file_resolver = resolver.for_file(file);
  let type_arenas = lowered.type_arenas(hir.owner);
  let mut nullish_cache: HashMap<typecheck_ts::TypeId, bool> = HashMap::new();
  let mut nullish_visiting: HashSet<typecheck_ts::TypeId> = HashSet::new();

  // Module globals of type `void`/`undefined`/`never` are not representable as storage locations in
  // the current native-js codegen (they have no value), but TypeScript can still infer/allow such
  // bindings (e.g. `const x = f();` where `f(): void`).
  //
  // Reject them here so the validator stays in sync with codegen's global handling.
  for stmt in hir.stmts.iter() {
    let StmtKind::Var(decl) = &stmt.kind else {
      continue;
    };
    for declarator in decl.declarators.iter() {
      let Some(binding) = file_resolver.resolve_pat_ident(hir, declarator.pat) else {
        continue;
      };
      let BindingId::Def(def) = binding else {
        continue;
      };
      let ty = program.type_of_def_interned(def);
      let kind = program.type_kind(ty);
       if matches!(kind, TypeKindSummary::Void | TypeKindSummary::Undefined | TypeKindSummary::Never) {
         let pat_span = hir
           .pats
           .get(declarator.pat.0 as usize)
           .map(|p| p.span)
           .unwrap_or(stmt.span);
         out.push(
           codes::STRICT_SUBSET_UNSUPPORTED_TYPE.error(
             "module-level variables must not have type `void`/`undefined`/`never` in the native-js strict subset",
             Span::new(file, pat_span),
           ),
         );
       }
    }
  }

  // The strict subset validator generally rejects callable/reference types. However, direct calls such as `foo(1)`
  // require the callee identifier to have a callable type, and in the direct-call lowering path we never materialize
  // the function value (codegen resolves the callee as a symbol).
  //
  // Skip validating the callee identifier's type only when it appears as the direct callee of an allowed call:
  // - intrinsic `print(x)` in statement position
  // - direct calls to top-level function definitions (the only functions codegen can call)
  let mut skip_expr_type_check = vec![false; result.expr_types().len()];
  let mut supported_cache: HashMap<typecheck_ts::TypeId, Option<SupportedAbiKind>> = HashMap::new();
  let mut supported_visiting: HashSet<typecheck_ts::TypeId> = HashSet::new();

  for (expr_idx, expr) in hir.exprs.iter().enumerate() {
    let ExprKind::Call(call) = &expr.kind else {
      continue;
    };
    if call.optional || call.is_new || call.args.iter().any(|arg| arg.spread) {
      continue;
    }
    let callee_idx = call.callee.0 as usize;
    if callee_idx >= skip_expr_type_check.len() {
      continue;
    }
    if !matches!(
      hir.exprs.get(callee_idx).map(|e| &e.kind),
      Some(ExprKind::Ident(_))
    ) {
      continue;
    }

    if *syntax
      .allowed_print_stmt_call_expr
      .get(expr_idx)
      .unwrap_or(&false)
    {
      skip_expr_type_check[callee_idx] = true;
      continue;
    }

    // Don't treat the `print` intrinsic as a normal callable; it's only supported in statement position.
    let is_builtin_print_intrinsic =
      callee_checked_intrinsic(&file_resolver, hir, lowered, call.callee)
        == Some(crate::builtins::NativeJsIntrinsic::Print);
    if is_builtin_print_intrinsic {
      continue;
    }

    let ok = (|| {
      let BindingId::Def(def) = file_resolver.resolve_expr_ident(hir, call.callee)? else {
        return None;
      };
      let resolved = resolve_import_def(program, def)?;
      codegen_callable_function_param_count(program, resolved).map(|_| ())
    })()
    .is_some();

    if ok {
      skip_expr_type_check[callee_idx] = true;
    }
  }

  // Enforce soundness of TypeScript-only "no-op" wrappers. These are erased by codegen, so the
  // types flowing out of them must not claim a different runtime representation.
  for (idx, expr) in hir.exprs.iter().enumerate() {
    let expr_id = ExprId(idx as u32);
    match &expr.kind {
      ExprKind::TypeAssertion {
        expr: inner,
        const_assertion,
        type_annotation,
      } => {
        if *const_assertion {
          continue;
        }
        let Some(actual_ty) = result.expr_type(*inner) else {
          continue;
        };
        let Some(asserted_ty) = result.expr_type(expr_id) else {
          continue;
        };

        let actual_group = type_kind_group_from_summary(&program.type_kind(actual_ty));
        let fallback_asserted_group = type_kind_group_from_summary(&program.type_kind(asserted_ty));
        let asserted_group = type_annotation
          .and_then(|ann| type_arenas.and_then(|arenas| type_expr_kind_group(arenas, ann)))
          .unwrap_or(fallback_asserted_group);

        if actual_group != asserted_group {
          out.push(
            codes::STRICT_SUBSET_UNSAFE_TYPE_ASSERTION
              .error(
                format!(
                  "unsafe type assertion changes runtime type category ({} → {})",
                  actual_group.as_str(),
                  asserted_group.as_str()
                ),
                Span::new(file, expr.span),
              )
              .with_note("type assertions are erased by native-js codegen; asserted types must match the expression's runtime representation"),
          );
          continue;
        }

        // Arrays/tuples are both lowered as GC-managed arrays; their *element* ABI is still part of
        // the runtime representation (stride + pointer-ness). Ensure type assertions do not change
        // element ABI within the array/tuple category.
        if matches!(actual_group, TypeKindGroup::Array | TypeKindGroup::Tuple) {
          let actual_elem_kind =
            array_or_tuple_elem_kind(program, actual_ty, &mut supported_cache, &mut supported_visiting);
          let asserted_elem_kind =
            array_or_tuple_elem_kind(program, asserted_ty, &mut supported_cache, &mut supported_visiting);
          if actual_elem_kind.is_some() && asserted_elem_kind.is_some() && actual_elem_kind != asserted_elem_kind {
              out.push(
              codes::STRICT_SUBSET_UNSAFE_TYPE_ASSERTION
                .error(
                  "unsafe type assertion changes array/tuple element ABI in native-js strict subset",
                  Span::new(file, expr.span),
                )
                .with_note("type assertions are erased by native-js codegen; array/tuple assertions must preserve element ABI (number vs boolean vs string vs GC pointer)"),
            );
          }
        }
      }
      ExprKind::NonNull { expr: inner } => {
        let Some(inner_ty) = result.expr_type(*inner) else {
          continue;
        };
        if type_may_be_nullish(program, inner_ty, &mut nullish_cache, &mut nullish_visiting) {
          out.push(
            codes::STRICT_SUBSET_UNSAFE_NON_NULL_ASSERTION
              .error(
                "unsafe non-null assertion on a value that may be null or undefined",
                Span::new(file, expr.span),
              )
              .with_note("add an explicit null/undefined check or refine the type so the value is proven non-nullable here"),
          );
        }
      }
      ExprKind::Satisfies { .. } => {
        // `satisfies` is a type-only construct that does not change runtime semantics; validate the
        // inner expression normally via the per-expression type checks below.
      }
      _ => {}
    }
  }

  // Member expressions are only supported for:
  // - array/tuple indexing (`arr[i]`) and `.length`
  // - plain object property access with statically known keys (`obj.foo`, `obj["foo"]`, `obj[0]`)
  for expr in hir.exprs.iter() {
    let ExprKind::Member(member) = &expr.kind else { continue };
    if member.optional {
      continue;
    }

    let Some(obj_ty) = result.expr_type(member.object) else {
      continue;
    };
    let obj_kind = program.type_kind(obj_ty);
    match obj_kind {
      TypeKindSummary::Array { .. } | TypeKindSummary::Tuple { .. } => match &member.property {
        ObjectKey::Ident(name) if lowered.names.resolve(*name) == Some("length") => {}
        ObjectKey::Computed(key_expr) => {
          let Some(key_ty) = result.expr_type(*key_expr) else {
            continue;
          };
          if !matches!(
            program.type_kind(key_ty),
            TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_)
          ) {
            push_unsupported_syntax(
              out,
              Span::new(file, expr.span),
              "array/tuple index expressions must have type `number` in native-js strict subset",
            );
          }
        }
        _ => {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "property access on arrays/tuples is only supported for indexing (`arr[i]`) and `.length` in native-js strict subset",
          );
        }
      },
      TypeKindSummary::Object | TypeKindSummary::EmptyObject => {
        // Determine the statically known property key.
        let key: Option<(bool, String, i64)> = match &member.property {
          ObjectKey::Ident(name) => lowered
            .names
            .resolve(*name)
            .map(|s| (true, s.to_string(), 0)),
          ObjectKey::Computed(key_expr) => hir
            .exprs
            .get(key_expr.0 as usize)
            .and_then(|e| match &e.kind {
              ExprKind::Literal(Literal::String(s)) => Some((true, s.lossy.clone(), 0)),
              ExprKind::Literal(Literal::Number(raw)) => {
                let n = number_literal_to_i64(raw)?;
                Some((false, String::new(), n))
              }
              _ => None,
            }),
          _ => None,
        };

        let Some((is_string, str_key, num_key)) = key else {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "object property access is only supported with statically known keys (`obj.foo`, `obj[\"foo\"]`, `obj[0]`) in native-js strict subset",
          );
          continue;
        };

        if is_string && str_key == "__proto__" {
          push_unsupported_syntax(out, Span::new(file, expr.span), "`__proto__` is not supported by native-js");
          continue;
        }

        let store = program.interned_type_store();
        let tti::TypeKind::Object(obj_id) = program.interned_type_kind(obj_ty) else {
          // `EmptyObject` (TS `{}`) and other object-like types do not have a fixed runtime shape.
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "unsupported object receiver type for native-js property access",
          );
          continue;
        };
        let obj = store.object(obj_id);
        let shape = store.shape(obj.shape);

        if !shape.call_signatures.is_empty() || !shape.construct_signatures.is_empty() || !shape.indexers.is_empty() {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "only plain object types with a fixed set of properties are supported by native-js yet",
          );
          continue;
        }

        let has_prop = shape.properties.iter().any(|prop| match &prop.key {
          tti::PropKey::String(id) if is_string => store.name(*id) == str_key,
          tti::PropKey::Number(n) if !is_string => *n == num_key,
          _ => false,
        });
        if !has_prop {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "unsupported object property access in native-js strict subset (property not found on the receiver type)",
          );
        }
      }
      _ => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "unsupported member expression receiver type in native-js strict subset",
        );
      }
    }
  }

  // Assignment to a member expression is only supported for:
  // - array/tuple element stores: `arr[i] = value`
  // - plain object property stores with statically known keys: `obj.foo = value`, `obj['foo'] = value`, `obj[0] = value`
  for expr in hir.exprs.iter() {
    let ExprKind::Assignment { op, target, value: _ } = &expr.kind else {
      continue;
    };
    let Some(pat) = hir.pats.get(target.0 as usize) else {
      continue;
    };
    let PatKind::AssignTarget(target_expr) = pat.kind else {
      continue;
    };
    let Some(target_expr_data) = hir.exprs.get(target_expr.0 as usize) else {
      continue;
    };
    let ExprKind::Member(member) = &target_expr_data.kind else {
      continue;
    };
    if member.optional {
      continue;
    }

    if *op != AssignOp::Assign {
      push_unsupported_syntax(
        out,
        Span::new(file, expr.span),
        "only simple `=` assignment is supported for member expression targets in native-js strict subset",
      );
      continue;
    }

    let Some(obj_ty) = result.expr_type(member.object) else {
      continue;
    };
    match program.type_kind(obj_ty) {
      TypeKindSummary::Array { .. } | TypeKindSummary::Tuple { .. } => {
        let ObjectKey::Computed(key_expr) = &member.property else {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "array/tuple assignment targets must be of the form `arr[i]` in native-js strict subset",
          );
          continue;
        };
        let Some(key_ty) = result.expr_type(*key_expr) else {
          continue;
        };
        if !matches!(
          program.type_kind(key_ty),
          TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_)
        ) {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "array/tuple index expressions must have type `number` in native-js strict subset",
          );
        }
      }
      TypeKindSummary::Object | TypeKindSummary::EmptyObject => {
        // Mirror member-load validation above.
        let key: Option<(bool, String, i64)> = match &member.property {
          ObjectKey::Ident(name) => lowered
            .names
            .resolve(*name)
            .map(|s| (true, s.to_string(), 0)),
          ObjectKey::Computed(key_expr) => hir
            .exprs
            .get(key_expr.0 as usize)
            .and_then(|e| match &e.kind {
              ExprKind::Literal(Literal::String(s)) => Some((true, s.lossy.clone(), 0)),
              ExprKind::Literal(Literal::Number(raw)) => {
                let n = number_literal_to_i64(raw)?;
                Some((false, String::new(), n))
              }
              _ => None,
            }),
          _ => None,
        };
        let Some((is_string, str_key, num_key)) = key else {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "object assignment targets must use statically known keys (`obj.foo`, `obj[\"foo\"]`, `obj[0]`) in native-js strict subset",
          );
          continue;
        };
        if is_string && str_key == "__proto__" {
          push_unsupported_syntax(out, Span::new(file, expr.span), "`__proto__` is not supported by native-js");
          continue;
        }

        let store = program.interned_type_store();
        let tti::TypeKind::Object(obj_id) = program.interned_type_kind(obj_ty) else {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "unsupported object receiver type for native-js property assignment",
          );
          continue;
        };
        let obj = store.object(obj_id);
        let shape = store.shape(obj.shape);
        if !shape.call_signatures.is_empty() || !shape.construct_signatures.is_empty() || !shape.indexers.is_empty() {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "only plain object types with a fixed set of properties are supported by native-js yet",
          );
          continue;
        }
        let has_prop = shape.properties.iter().any(|prop| match &prop.key {
          tti::PropKey::String(id) if is_string => store.name(*id) == str_key,
          tti::PropKey::Number(n) if !is_string => *n == num_key,
          _ => false,
        });
        if !has_prop {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "unsupported object property assignment in native-js strict subset (property not found on the receiver type)",
          );
        }
      }
      _ => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "unsupported assignment target receiver type in native-js strict subset",
        );
      }
    }
  }

  // Operator-level validation that depends on operand types.
  //
  // The strict subset validator generally accepts a small set of operators syntactically, but some
  // of those operators change meaning when applied to `string` operands (e.g. `+` becomes string
  // concatenation). The checked backend represents strings as an interned `u32` id, so we must
  // explicitly reject those cases until proper lowering exists.
  for expr in hir.exprs.iter() {
    match &expr.kind {
      ExprKind::Binary { op, left, right } => {
        let lhs_ty = result.expr_types().get(left.0 as usize).copied();
        let rhs_ty = result.expr_types().get(right.0 as usize).copied();
        let lhs_is_string = lhs_ty
          .map(|ty| is_string_type_kind(&program.type_kind(ty)))
          .unwrap_or(false);
        let rhs_is_string = rhs_ty
          .map(|ty| is_string_type_kind(&program.type_kind(ty)))
          .unwrap_or(false);

        // Disallow `+` on strings (string concatenation).
        if *op == BinaryOp::Add && (lhs_is_string || rhs_is_string) {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "string concatenation (`+`) is not supported by native-js yet",
          );
        }

        // For strings, only allow `===` / `!==` when both sides are string-like.
        if matches!(op, BinaryOp::Equality | BinaryOp::Inequality) && (lhs_is_string || rhs_is_string) {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "string equality is only supported via `===` / `!==` in native-js yet",
          );
        }
        if matches!(op, BinaryOp::StrictEquality | BinaryOp::StrictInequality) && (lhs_is_string ^ rhs_is_string) {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "string `===` / `!==` comparisons are only supported when both operands are strings",
          );
        }
      }
      ExprKind::Assignment { op, target, value } => {
        // Disallow `+=` on strings (string concatenation).
        if *op == AssignOp::AddAssign {
          let target_ty = result.pat_types().get(target.0 as usize).copied();
          let value_ty = result.expr_types().get(value.0 as usize).copied();
          let target_is_string = target_ty
            .map(|ty| is_string_type_kind(&program.type_kind(ty)))
            .unwrap_or(false);
          let value_is_string = value_ty
            .map(|ty| is_string_type_kind(&program.type_kind(ty)))
            .unwrap_or(false);
          if target_is_string || value_is_string {
            push_unsupported_syntax(
              out,
              Span::new(file, expr.span),
              "string concatenation (`+=`) is not supported by native-js yet",
            );
          }
        }
      }
      _ => {}
    }
  }

  // `native-js` currently uses JavaScript truthiness semantics for `number`/`boolean` values, but
  // `string` values in the checked backend are represented as interned `u32` IDs. Until we implement
  // string truthiness (`""` is falsy, all other strings truthy), reject `string` values in
  // truthiness contexts.
  let is_string_expr = |expr: ExprId| {
    result
      .expr_types()
      .get(expr.0 as usize)
      .copied()
      .map(|ty| is_string_type_kind(&program.type_kind(ty)))
      .unwrap_or(false)
  };

  for stmt in hir.stmts.iter() {
    match &stmt.kind {
      StmtKind::If { test, .. }
      | StmtKind::While { test, .. }
      | StmtKind::DoWhile { test, .. } => {
        if is_string_expr(*test) {
          push_unsupported_syntax(
            out,
            Span::new(file, stmt.span),
            "using `string` values as conditions is not supported by native-js yet",
          );
        }
      }
      StmtKind::For { test, .. } => {
        if test.is_some_and(is_string_expr) {
          push_unsupported_syntax(
            out,
            Span::new(file, stmt.span),
            "using `string` values as conditions is not supported by native-js yet",
          );
        }
      }
      _ => {}
    }
  }

  for expr in hir.exprs.iter() {
    match &expr.kind {
      ExprKind::Unary {
        op: UnaryOp::Not,
        expr: inner,
      } => {
        if is_string_expr(*inner) {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "string truthiness (e.g. `!s`) is not supported by native-js yet",
          );
        }
      }
      ExprKind::Binary { op, left, right }
        if matches!(op, BinaryOp::LogicalAnd | BinaryOp::LogicalOr)
          && (is_string_expr(*left) || is_string_expr(*right)) =>
      {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "string truthiness (e.g. `s && t`) is not supported by native-js yet",
        );
      }
      _ => {}
    }
  }

  for (idx, ty) in result.expr_types().iter().copied().enumerate() {
    let Some(expr) = hir.exprs.get(idx) else { continue };
    if skip_expr_type_check.get(idx).copied().unwrap_or(false) {
      continue;
    }
    validate_type_kind(
      program,
      Span::new(file, expr.span),
      ty,
      &mut supported_cache,
      &mut supported_visiting,
      out,
    );
  }

  for (idx, ty) in result.pat_types().iter().copied().enumerate() {
    let Some(pat) = hir.pats.get(idx) else { continue };
    validate_type_kind(
      program,
      Span::new(file, pat.span),
      ty,
      &mut supported_cache,
      &mut supported_visiting,
      out,
    );
  }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SupportedAbiKind {
  Number,
  Boolean,
  String,
  Void,
  /// GC-managed pointer (`ptr addrspace(1)`).
  GcPtr,
}

fn validate_type_kind(
  program: &Program,
  span: Span,
  ty: typecheck_ts::TypeId,
  cache: &mut HashMap<typecheck_ts::TypeId, Option<SupportedAbiKind>>,
  visiting: &mut HashSet<typecheck_ts::TypeId>,
  out: &mut Vec<Diagnostic>,
) {
  if supported_abi_kind(program, ty, cache, visiting).is_some() {
    return;
  }

  let kind = program.type_kind(ty);
  let message = unsupported_type_message(&kind);
  out.push(
    codes::STRICT_SUBSET_UNSUPPORTED_TYPE
      .error(message, span)
      .with_note("supported types are currently limited to: number, boolean, string, void/undefined, never, arrays, tuples, and plain objects"),
  );
}

fn supported_abi_kind(
  program: &Program,
  ty: typecheck_ts::TypeId,
  cache: &mut HashMap<typecheck_ts::TypeId, Option<SupportedAbiKind>>,
  visiting: &mut HashSet<typecheck_ts::TypeId>,
) -> Option<SupportedAbiKind> {
  if let Some(hit) = cache.get(&ty) {
    return *hit;
  }
  if !visiting.insert(ty) {
    // Break cycles conservatively.
    return None;
  }

  let result = match program.interned_type_kind(ty) {
    tti::TypeKind::Never | tti::TypeKind::Void | tti::TypeKind::Undefined => Some(SupportedAbiKind::Void),
    tti::TypeKind::Boolean | tti::TypeKind::BooleanLiteral(_) => Some(SupportedAbiKind::Boolean),
    tti::TypeKind::Number | tti::TypeKind::NumberLiteral(_) => Some(SupportedAbiKind::Number),
    tti::TypeKind::String | tti::TypeKind::StringLiteral(_) => Some(SupportedAbiKind::String),
    tti::TypeKind::EmptyObject => Some(SupportedAbiKind::GcPtr),

    // Arrays: GC pointer + uniform element stride. Element type must be representable.
    tti::TypeKind::Array { ty: elem_ty, .. } => match supported_abi_kind(program, elem_ty, cache, visiting) {
      Some(SupportedAbiKind::Number | SupportedAbiKind::Boolean | SupportedAbiKind::String | SupportedAbiKind::GcPtr) => {
        Some(SupportedAbiKind::GcPtr)
      }
      _ => None,
    },

    // Tuples: lowered as fixed-length arrays with a uniform element ABI.
    tti::TypeKind::Tuple(elems) => {
      let mut elem_kind: Option<SupportedAbiKind> = None;
      for elem in elems.iter() {
        if elem.optional || elem.rest {
          elem_kind = None;
          break;
        }
        let Some(k) = supported_abi_kind(program, elem.ty, cache, visiting) else {
          elem_kind = None;
          break;
        };
        if matches!(k, SupportedAbiKind::Void) {
          elem_kind = None;
          break;
        }
        if let Some(prev) = elem_kind {
          if prev != k {
            elem_kind = None;
            break;
          }
        } else {
          elem_kind = Some(k);
        }
      }
      elem_kind.map(|_| SupportedAbiKind::GcPtr).or_else(|| {
        // The empty tuple `[]` is treated as a valid fixed-length array (length 0).
        if elems.is_empty() {
          Some(SupportedAbiKind::GcPtr)
        } else {
          None
        }
      })
    }

    // Type wrappers: recurse through the underlying type.
    tti::TypeKind::Infer { constraint, .. } => constraint.and_then(|inner| supported_abi_kind(program, inner, cache, visiting)),
    tti::TypeKind::Intrinsic { ty, .. } => supported_abi_kind(program, ty, cache, visiting),
    tti::TypeKind::Object(obj_id) => {
      let store = program.interned_type_store();
      let obj = store.object(obj_id);
      let shape = store.shape(obj.shape);

      // Support only plain data objects with a fixed set of fields.
      if !shape.call_signatures.is_empty() || !shape.construct_signatures.is_empty() || !shape.indexers.is_empty() {
        None
      } else if shape.properties.iter().any(|prop| prop.data.optional || prop.data.is_method) {
        None
      } else if shape
        .properties
        .iter()
        .any(|prop| matches!(prop.key, tti::PropKey::Symbol(_)))
      {
        None
      } else {
        let mut ok = true;
        for prop in shape.properties.iter() {
          let Some(k) = supported_abi_kind(program, prop.data.ty, cache, visiting) else {
            ok = false;
            break;
          };
          if matches!(k, SupportedAbiKind::Void) {
            ok = false;
            break;
          }
        }
        ok.then_some(SupportedAbiKind::GcPtr)
      }
    }

    _ => None,
  };

  visiting.remove(&ty);
  cache.insert(ty, result);
  result
}

fn array_or_tuple_elem_kind(
  program: &Program,
  ty: typecheck_ts::TypeId,
  cache: &mut HashMap<typecheck_ts::TypeId, Option<SupportedAbiKind>>,
  visiting: &mut HashSet<typecheck_ts::TypeId>,
) -> Option<SupportedAbiKind> {
  match program.interned_type_kind(ty) {
    tti::TypeKind::Array { ty: elem_ty, .. } => {
      let k = supported_abi_kind(program, elem_ty, cache, visiting)?;
      (!matches!(k, SupportedAbiKind::Void)).then_some(k)
    }
    tti::TypeKind::Tuple(elems) => {
      let mut elem_kind: Option<SupportedAbiKind> = None;
      for elem in elems.iter() {
        if elem.optional || elem.rest {
          return None;
        }
        let k = supported_abi_kind(program, elem.ty, cache, visiting)?;
        if matches!(k, SupportedAbiKind::Void) {
          return None;
        }
        if let Some(prev) = elem_kind {
          if prev != k {
            return None;
          }
        } else {
          elem_kind = Some(k);
        }
      }
      elem_kind
    }
    tti::TypeKind::Infer { constraint, .. } => constraint.and_then(|inner| array_or_tuple_elem_kind(program, inner, cache, visiting)),
    tti::TypeKind::Intrinsic { ty, .. } => array_or_tuple_elem_kind(program, ty, cache, visiting),
    _ => None,
  }
}

fn is_string_type_kind(kind: &TypeKindSummary) -> bool {
  matches!(kind, TypeKindSummary::String | TypeKindSummary::StringLiteral(_))
}

fn unsupported_type_message(kind: &TypeKindSummary) -> String {
  match kind {
    TypeKindSummary::Any => "`any` is not supported by native-js strict subset".to_string(),
    TypeKindSummary::Unknown => "`unknown` is not supported by native-js strict subset".to_string(),
    TypeKindSummary::Null => "`null` is not supported by native-js strict subset".to_string(),
    TypeKindSummary::Union { .. } => "union types are not supported by native-js strict subset yet".to_string(),
    TypeKindSummary::Intersection { .. } => {
      "intersection types are not supported by native-js strict subset yet".to_string()
    }
    TypeKindSummary::Object | TypeKindSummary::EmptyObject => "unsupported object type in native-js strict subset".to_string(),
    TypeKindSummary::Callable { .. } => "function types are not supported by native-js strict subset yet".to_string(),
    TypeKindSummary::Ref { .. } => "reference/nominal types are not supported by native-js strict subset yet".to_string(),
    TypeKindSummary::BigInt | TypeKindSummary::BigIntLiteral(_) => {
      "`bigint` is not supported by native-js strict subset yet".to_string()
    }
    TypeKindSummary::Symbol | TypeKindSummary::UniqueSymbol => {
      "`symbol` is not supported by native-js strict subset yet".to_string()
    }
    TypeKindSummary::TemplateLiteral => "template literal types are not supported by native-js strict subset yet".to_string(),
    other => format!("unsupported type in native-js strict subset: {other:?}"),
  }
}

fn push_unsupported_syntax(out: &mut Vec<Diagnostic>, span: Span, message: impl Into<String>) {
  out.push(
    codes::STRICT_SUBSET_UNSUPPORTED_SYNTAX
      .error(message, span)
      .with_note("native-js currently only supports a small, statically-analyzable subset of TypeScript"),
  );
}

fn callee_is_ident(body: &Body, lowered: &hir_js::LowerResult, expr: hir_js::ExprId, target: &str) -> bool {
  let Some(expr) = body.exprs.get(expr.0 as usize) else {
    return false;
  };
  match &expr.kind {
    ExprKind::Ident(name) => lowered.names.resolve(*name) == Some(target),
    _ => false,
  }
}

fn callee_native_js_intrinsic(
  body: &Body,
  lowered: &hir_js::LowerResult,
  expr: hir_js::ExprId,
) -> Option<crate::builtins::NativeJsIntrinsic> {
  let Some(expr) = body.exprs.get(expr.0 as usize) else {
    return None;
  };
  let ExprKind::Ident(name) = &expr.kind else {
    return None;
  };
  let resolved = lowered.names.resolve(*name)?;
  crate::builtins::intrinsic_by_name(resolved)
}

fn resolve_import_def(program: &Program, def: typecheck_ts::DefId) -> Option<typecheck_ts::DefId> {
  let mut cur = def;
  let mut seen = std::collections::HashSet::<typecheck_ts::DefId>::new();
  loop {
    if !seen.insert(cur) {
      return None;
    }
    let kind = program.def_kind(cur)?;
    let typecheck_ts::DefKind::Import(import) = kind else {
      return Some(cur);
    };
    match import.target {
      typecheck_ts::ImportTarget::File(target_file) => {
        let (symbol, local_def) = {
          let exports = program.exports_of(target_file);
          let entry = exports.get(import.original.as_str())?;
          (entry.symbol, entry.def)
        };
        cur = local_def.or_else(|| program.symbol_info(symbol).and_then(|info| info.def))?;
      }
      _ => return None,
    }
  }
}

fn codegen_callable_function_param_count(program: &Program, def: typecheck_ts::DefId) -> Option<usize> {
  // Match `native-js` codegen: only top-level function definitions in non-`.d.ts` files are
  // compiled and callable via the direct-call lowering path.
  let lowered = program.hir_lowered(def.file())?;
  if matches!(lowered.hir.file_kind, FileKind::Dts) {
    return None;
  }
  let def_data = lowered.def(def)?;
  if def_data.parent.is_some() {
    return None;
  }
  if def_data.path.kind != hir_js::DefKind::Function {
    return None;
  }
  let body_id = def_data.body?;
  let body = lowered.body(body_id)?;
  let meta = body.function.as_ref()?;
  Some(meta.params.len())
}

struct SyntaxState<'a, 'b, 'p> {
  file: typecheck_ts::FileId,
  body: &'a Body,
  lowered: &'a hir_js::LowerResult,
  file_resolver: crate::resolve::FileResolver<'a, 'p>,
  out: &'b mut Vec<Diagnostic>,
  info: &'b mut BodySyntaxInfo,
  loop_stack: Vec<Option<NameId>>,
}

impl<'a, 'b, 'p> SyntaxState<'a, 'b, 'p> {
  fn new(
    file: typecheck_ts::FileId,
    body: &'a Body,
    lowered: &'a hir_js::LowerResult,
    file_resolver: crate::resolve::FileResolver<'a, 'p>,
    out: &'b mut Vec<Diagnostic>,
    info: &'b mut BodySyntaxInfo,
  ) -> Self {
    Self {
      file,
      body,
      lowered,
      file_resolver,
      out,
      info,
      loop_stack: Vec::new(),
    }
  }

  fn validate_stmt(&mut self, stmt_id: StmtId) {
    let Some(stmt) = self.body.stmts.get(stmt_id.0 as usize) else {
      return;
    };
    match &stmt.kind {
      StmtKind::Empty | StmtKind::Debugger => {}
      StmtKind::Expr(expr) => {
        self.maybe_allow_intrinsic_call_stmt(*expr);
      }
      StmtKind::ExportDefaultExpr(expr) => {
        self.maybe_allow_intrinsic_call_stmt(*expr);
      }
      StmtKind::Return(_) => {}
      StmtKind::Block(stmts) => {
        for &s in stmts {
          self.validate_stmt(s);
        }
      }
      StmtKind::If {
        consequent,
        alternate,
        ..
      } => {
        self.validate_stmt(*consequent);
        if let Some(alt) = alternate {
          self.validate_stmt(*alt);
        }
      }
      StmtKind::While { body, .. } => self.validate_loop(None, *body),
      StmtKind::DoWhile { body, .. } => self.validate_loop(None, *body),
      StmtKind::For { init, body, .. } => self.validate_for_loop(None, init.as_ref(), *body, stmt.span),
      StmtKind::ForIn { .. } => {
        push_unsupported_syntax(
          self.out,
          Span::new(self.file, stmt.span),
          "`for-in` / `for-of` loops are not supported by native-js yet",
        );
      }
      StmtKind::Switch { .. } => {
        push_unsupported_syntax(
          self.out,
          Span::new(self.file, stmt.span),
          "`switch` statements are not supported by native-js yet",
        );
      }
      StmtKind::Try { .. } => {
        push_unsupported_syntax(
          self.out,
          Span::new(self.file, stmt.span),
          "`try` is not supported by native-js yet",
        );
      }
      StmtKind::Throw(_) => {
        push_unsupported_syntax(
          self.out,
          Span::new(self.file, stmt.span),
          "`throw` is not supported by native-js yet",
        );
      }
      StmtKind::With { .. } => {
        push_unsupported_syntax(
          self.out,
          Span::new(self.file, stmt.span),
          "`with` statements are not supported by native-js yet",
        );
      }
      StmtKind::Decl(_) if self.body.function.is_some() => {
        push_unsupported_syntax(
          self.out,
          Span::new(self.file, stmt.span),
          "nested declarations are not supported by native-js yet",
        );
      }
      StmtKind::Var(decl) => self.validate_var_decl(decl, stmt.span),
      StmtKind::Break(label) => self.validate_break_continue("break", *label, stmt.span),
      StmtKind::Continue(label) => self.validate_break_continue("continue", *label, stmt.span),
      StmtKind::Labeled { label, body } => self.validate_labeled(*label, *body, stmt.span),
      _ => {}
    }
  }

  fn validate_loop(&mut self, label: Option<NameId>, body: StmtId) {
    self.loop_stack.push(label);
    self.validate_stmt(body);
    self.loop_stack.pop();
  }

  fn validate_for_loop(&mut self, label: Option<NameId>, init: Option<&ForInit>, body: StmtId, span: TextRange) {
    if let Some(init) = init {
      if let ForInit::Var(decl) = init {
        self.validate_var_decl(decl, span);
      }
    }
    self.loop_stack.push(label);
    self.validate_stmt(body);
    self.loop_stack.pop();
  }

  fn validate_break_continue(&mut self, keyword: &str, label: Option<NameId>, span: TextRange) {
    if self.loop_stack.is_empty() {
      push_unsupported_syntax(
        self.out,
        Span::new(self.file, span),
        format!("`{keyword}` is only supported inside loops in native-js yet"),
      );
      return;
    }
    if let Some(label) = label {
      if !self.loop_stack.iter().rev().any(|l| *l == Some(label)) {
        let lbl = self.lowered.names.resolve(label).unwrap_or("<label>");
        push_unsupported_syntax(
          self.out,
          Span::new(self.file, span),
          format!("unknown loop label `{lbl}` for `{keyword}`"),
        );
      }
    }
  }

  fn validate_labeled(&mut self, label: NameId, body: StmtId, span: TextRange) {
    let Some(stmt) = self.body.stmts.get(body.0 as usize) else {
      return;
    };
    match &stmt.kind {
      StmtKind::While { body, .. } => self.validate_loop(Some(label), *body),
      StmtKind::DoWhile { body, .. } => self.validate_loop(Some(label), *body),
      StmtKind::For { init, body, .. } => self.validate_for_loop(Some(label), init.as_ref(), *body, span),
      _ => push_unsupported_syntax(
        self.out,
        Span::new(self.file, span),
        "only labeled loops are supported by native-js yet",
      ),
    }
  }

  fn validate_var_decl(&mut self, decl: &VarDecl, span: TextRange) {
    match decl.kind {
      VarDeclKind::Var | VarDeclKind::Let | VarDeclKind::Const => {}
      VarDeclKind::Using | VarDeclKind::AwaitUsing => {
        push_unsupported_syntax(
          self.out,
          Span::new(self.file, span),
          "`using` declarations are not supported by native-js yet",
        );
        return;
      }
    }

    for declarator in decl.declarators.iter() {
      if declarator.init.is_none() {
        let pat_span = self
          .body
          .pats
          .get(declarator.pat.0 as usize)
          .map(|p| p.span)
          .unwrap_or(span);
        push_unsupported_syntax(
          self.out,
          Span::new(self.file, pat_span),
          "variable declarations must have an initializer in native-js strict subset",
        );
      }
    }
  }

  fn maybe_allow_intrinsic_call_stmt(&mut self, expr_id: ExprId) {
    let Some(expr) = self.body.exprs.get(expr_id.0 as usize) else {
      return;
    };
    let ExprKind::Call(call) = &expr.kind else {
      return;
    };
    if call.optional || call.is_new {
      return;
    }
    if call.args.len() != 1 {
      return;
    }
    let Some(arg) = call.args.first() else {
      return;
    };
    if arg.spread {
      return;
    }
    if callee_checked_intrinsic(
      &self.file_resolver,
      self.body,
      self.lowered,
      call.callee,
    ) != Some(crate::builtins::NativeJsIntrinsic::Print)
    {
      return;
    }

    // This is an allowed intrinsic `print(x)` statement call.
    if let Some(slot) = self
      .info
      .allowed_print_stmt_call_expr
      .get_mut(expr_id.0 as usize)
    {
      *slot = true;
    }
  }
}

fn callee_checked_intrinsic(
  file_resolver: &crate::resolve::FileResolver<'_, '_>,
  body: &Body,
  lowered: &hir_js::LowerResult,
  expr: hir_js::ExprId,
) -> Option<crate::builtins::NativeJsIntrinsic> {
  let intrinsic = callee_native_js_intrinsic(body, lowered, expr)?;
  // `typecheck-ts` only records symbol occurrences for file-local (declared) bindings. Global
  // names coming from injected `.d.ts` libs (like native-js intrinsics) generally resolve to
  // `None` here.
  file_resolver
    .resolve_expr_ident(body, expr)
    .is_none()
    .then_some(intrinsic)
}
