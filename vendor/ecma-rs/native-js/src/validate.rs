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
  AssignOp, BinaryOp, Body, BodyId, BodyKind, ExprId, ExprKind, FileKind, ForInit, FunctionBody, Literal, NameId,
  PatId, PatKind, StmtId, StmtKind, UnaryOp, VarDecl, VarDeclKind,
};
use std::collections::HashSet;
use typecheck_ts::{Program, TypeKindSummary};

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
      validate_body(program, &resolver, file, body, &lowered, &mut diagnostics);
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

  let syntax = validate_body_syntax(program, file, body_data, lowered, resolver, out);
  validate_body_types(program, file, body, body_data, lowered, resolver, &syntax, out);
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
  let mut state = SyntaxState::new(file, body, lowered, out, &mut info);
  for stmt in root_stmts {
    state.validate_stmt(stmt);
  }

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
          matches!(
            callee_native_js_intrinsic(body, lowered, call.callee),
            Some(crate::builtins::NativeJsIntrinsic::Print)
          )
          && matches!(
            file_resolver.resolve_expr_ident(body, call.callee),
            Some(BindingId::Def(_))
          );
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
      ExprKind::Literal(Literal::String(_)) => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "string literals are not supported by native-js yet");
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
      ExprKind::Literal(Literal::Number(raw)) => {
        if !numeric_literal_is_i32(raw) {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            format!("unsupported numeric literal `{raw}` (expected 32-bit integer)"),
          );
        }
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
      ExprKind::Member(_) => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "property access is not supported by native-js yet",
        );
      }
      ExprKind::Object(_) => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "object literals are not supported by native-js yet",
        );
      }
      ExprKind::Array(_) => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "array literals are not supported by native-js yet",
        );
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
  body: BodyId,
  hir: &Body,
  lowered: &hir_js::LowerResult,
  resolver: &Resolver<'_>,
  syntax: &BodySyntaxInfo,
  out: &mut Vec<Diagnostic>,
) {
  let result = program.check_body(body);
  let file_resolver = resolver.for_file(file);

  // The strict subset validator generally rejects callable/reference types. However, direct calls such as `foo(1)`
  // require the callee identifier to have a callable type, and in the direct-call lowering path we never materialize
  // the function value (codegen resolves the callee as a symbol).
  //
  // Skip validating the callee identifier's type only when it appears as the direct callee of an allowed call:
  // - intrinsic `print(x)` in statement position
  // - direct calls to top-level function definitions (the only functions codegen can call)
  let mut skip_expr_type_check = vec![false; result.expr_types().len()];

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
    if matches!(
      callee_native_js_intrinsic(hir, lowered, call.callee),
      Some(crate::builtins::NativeJsIntrinsic::Print)
    ) {
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

  for (idx, ty) in result.expr_types().iter().copied().enumerate() {
    let Some(expr) = hir.exprs.get(idx) else { continue };
    if skip_expr_type_check.get(idx).copied().unwrap_or(false) {
      continue;
    }
    validate_type_kind(program, Span::new(file, expr.span), ty, out);
  }

  for (idx, ty) in result.pat_types().iter().copied().enumerate() {
    let Some(pat) = hir.pats.get(idx) else { continue };
    validate_type_kind(program, Span::new(file, pat.span), ty, out);
  }
}

fn validate_type_kind(program: &Program, span: Span, ty: typecheck_ts::TypeId, out: &mut Vec<Diagnostic>) {
  let kind = program.type_kind(ty);
  if is_supported_type_kind(&kind) {
    return;
  }

  let message = unsupported_type_message(&kind);
  out.push(
    codes::STRICT_SUBSET_UNSUPPORTED_TYPE
      .error(message, span)
      .with_note("supported types are currently limited to: number (32-bit integer), boolean, void/undefined, never"),
  );
}

fn is_supported_type_kind(kind: &TypeKindSummary) -> bool {
  matches!(
    kind,
    TypeKindSummary::Never
      | TypeKindSummary::Void
      | TypeKindSummary::Undefined
      | TypeKindSummary::Boolean
      | TypeKindSummary::BooleanLiteral(_)
      | TypeKindSummary::Number
      | TypeKindSummary::NumberLiteral(_)
  )
}

fn unsupported_type_message(kind: &TypeKindSummary) -> String {
  match kind {
    TypeKindSummary::Any => "`any` is not supported by native-js strict subset".to_string(),
    TypeKindSummary::Unknown => "`unknown` is not supported by native-js strict subset".to_string(),
    TypeKindSummary::Null => "`null` is not supported by native-js strict subset".to_string(),
    TypeKindSummary::String | TypeKindSummary::StringLiteral(_) => {
      "`string` is not supported by native-js strict subset".to_string()
    }
    TypeKindSummary::Union { .. } => "union types are not supported by native-js strict subset yet".to_string(),
    TypeKindSummary::Intersection { .. } => {
      "intersection types are not supported by native-js strict subset yet".to_string()
    }
    TypeKindSummary::Object | TypeKindSummary::EmptyObject => {
      "object types are not supported by native-js strict subset yet".to_string()
    }
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

fn numeric_literal_is_i32(raw: &str) -> bool {
  // Keep this logic in sync with `native_js::codegen::parse_i32_const`.
  let raw = raw.trim();
  if raw.is_empty() {
    return false;
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
      return false;
    }
    (10, normalized.as_str())
  };

  let Ok(value) = i64::from_str_radix(digits, radix) else {
    return false;
  };
  i32::try_from(value).is_ok()
}

struct SyntaxState<'a, 'b> {
  file: typecheck_ts::FileId,
  body: &'a Body,
  lowered: &'a hir_js::LowerResult,
  out: &'b mut Vec<Diagnostic>,
  info: &'b mut BodySyntaxInfo,
  scopes: Vec<HashSet<NameId>>,
  loop_stack: Vec<Option<NameId>>,
}

impl<'a, 'b> SyntaxState<'a, 'b> {
  fn new(
    file: typecheck_ts::FileId,
    body: &'a Body,
    lowered: &'a hir_js::LowerResult,
    out: &'b mut Vec<Diagnostic>,
    info: &'b mut BodySyntaxInfo,
  ) -> Self {
    let mut state = Self {
      file,
      body,
      lowered,
      out,
      info,
      scopes: vec![HashSet::new()],
      loop_stack: Vec::new(),
    };
    if let Some(func) = &state.body.function {
      for param in func.params.iter() {
        state.declare_pat(param.pat);
      }
    }
    state
  }

  fn push_scope(&mut self) {
    self.scopes.push(HashSet::new());
  }

  fn pop_scope(&mut self) {
    self.scopes.pop();
    if self.scopes.is_empty() {
      // Internal invariant: there is always a root scope.
      self.scopes.push(HashSet::new());
    }
  }

  fn is_shadowed(&self, name: NameId) -> bool {
    self.scopes.iter().rev().any(|scope| scope.contains(&name))
  }

  fn declare_pat(&mut self, pat: PatId) {
    let Some(pat) = self.body.pats.get(pat.0 as usize) else {
      return;
    };
    let PatKind::Ident(name) = &pat.kind else {
      return;
    };
    if let Some(scope) = self.scopes.last_mut() {
      scope.insert(*name);
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
      StmtKind::Return(_) => {}
      StmtKind::Block(stmts) => {
        self.push_scope();
        for &s in stmts {
          self.validate_stmt(s);
        }
        self.pop_scope();
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
    // Keep loop scoping in sync with `native_js::codegen::FnCodegen::codegen_for`.
    //
    // `for (let/const ...)` introduces a lexical scope that does not leak outside the loop. `var` does *not* introduce a
    // new scope in this subset.
    let needs_loop_scope = matches!(
      init,
      Some(ForInit::Var(decl)) if matches!(decl.kind, VarDeclKind::Let | VarDeclKind::Const)
    );
    if needs_loop_scope {
      self.push_scope();
    }
    if let Some(init) = init {
      if let ForInit::Var(decl) = init {
        self.validate_var_decl(decl, span);
      }
    }
    self.loop_stack.push(label);
    self.validate_stmt(body);
    self.loop_stack.pop();
    if needs_loop_scope {
      self.pop_scope();
    }
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

    // Track shadowing for intrinsics like `print`.
    //
    // Match codegen's behaviour: `LocalEnv` only contains locals. Module-level
    // `let`/`const`/`var` (defs without a function-like ancestor) are stored in
    // globals and do not shadow intrinsics.
    let shadows_intrinsics = self.body.function.is_some()
      || (self.scopes.len() > 1 && matches!(decl.kind, VarDeclKind::Let | VarDeclKind::Const));

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
      if shadows_intrinsics {
        self.declare_pat(declarator.pat);
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

    let Some(callee_expr) = self.body.exprs.get(call.callee.0 as usize) else {
      return;
    };
    let ExprKind::Ident(name) = &callee_expr.kind else {
      return;
    };
    let Some(resolved) = self.lowered.names.resolve(*name) else {
      return;
    };
    if crate::builtins::intrinsic_by_name(resolved) != Some(crate::builtins::NativeJsIntrinsic::Print) {
      return;
    }
    if self.is_shadowed(*name) {
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
