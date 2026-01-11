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
use diagnostics::{Diagnostic, Span};
use hir_js::{
  Body, BodyId, BodyKind, ExprKind, FileKind, ForInit, PatKind, StmtKind, UnaryOp, VarDeclKind,
};
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

  validate_body_syntax(program, file, body_data, lowered, resolver, out);
  validate_body_types(program, file, body, body_data, lowered, resolver, out);
}

fn validate_body_syntax(
  program: &Program,
  file: typecheck_ts::FileId,
  body: &Body,
  lowered: &hir_js::LowerResult,
  resolver: &Resolver<'_>,
  out: &mut Vec<Diagnostic>,
) {
  let file_resolver = resolver.for_file(file);
  let expr_stmt_roots: std::collections::HashSet<hir_js::ExprId> = body
    .stmts
    .iter()
    .filter_map(|stmt| match stmt.kind {
      StmtKind::Expr(expr) => Some(expr),
      _ => None,
    })
    .collect();
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
  }

  for (idx, expr) in body.exprs.iter().enumerate() {
    let expr_id = hir_js::ExprId(idx as u32);
    match &expr.kind {
      ExprKind::Super => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`super` is not supported yet");
      }
      ExprKind::NewTarget => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`new.target` is not supported yet");
      }
      ExprKind::ImportMeta => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`import.meta` is not supported yet");
      }
      ExprKind::This => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`this` is not supported yet");
      }
      ExprKind::Literal(lit) => match lit {
        hir_js::Literal::Number(raw) => {
          if !is_supported_i32_number_literal(raw) {
            push_unsupported_syntax(
              out,
              Span::new(file, expr.span),
              "numeric literals must be 32-bit signed integers in native-js strict subset",
            );
          }
        }
        hir_js::Literal::Boolean(_) => {}
        hir_js::Literal::String(_) => {
          push_unsupported_syntax(out, Span::new(file, expr.span), "string literals are not supported yet");
        }
        hir_js::Literal::Null => {
          push_unsupported_syntax(out, Span::new(file, expr.span), "`null` is not supported yet");
        }
        hir_js::Literal::Undefined => {
          push_unsupported_syntax(out, Span::new(file, expr.span), "`undefined` is not supported yet");
        }
        hir_js::Literal::BigInt(_) => {
          push_unsupported_syntax(out, Span::new(file, expr.span), "`bigint` is not supported yet");
        }
        hir_js::Literal::Regex(_) => {
          push_unsupported_syntax(out, Span::new(file, expr.span), "regex literals are not supported yet");
        }
      },
      ExprKind::Yield { .. } => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`yield` is not supported yet");
      }
      ExprKind::Await { .. } => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`await` is not supported yet");
      }
      ExprKind::Unary { op: UnaryOp::Await, .. } => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "`await` is not supported yet");
      }
      ExprKind::Unary {
        op: UnaryOp::Typeof | UnaryOp::Void | UnaryOp::Delete,
        ..
      } => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "unsupported unary operator in native-js strict subset",
        );
      }
      ExprKind::Binary {
        op:
          hir_js::BinaryOp::Exponent
          | hir_js::BinaryOp::ShiftRightUnsigned
          | hir_js::BinaryOp::LogicalOr
          | hir_js::BinaryOp::LogicalAnd
          | hir_js::BinaryOp::NullishCoalescing
          | hir_js::BinaryOp::In
          | hir_js::BinaryOp::Instanceof
          | hir_js::BinaryOp::Comma,
        ..
      } => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "unsupported binary operator in native-js strict subset",
        );
      }
      ExprKind::Conditional { .. } => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "conditional expressions are not supported yet");
      }
      ExprKind::FunctionExpr { .. } => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "function expressions are not supported yet");
      }
      ExprKind::Assignment {
        op:
          hir_js::AssignOp::ShiftLeftAssign
          | hir_js::AssignOp::ShiftRightAssign
          | hir_js::AssignOp::ShiftRightUnsignedAssign
          | hir_js::AssignOp::BitOrAssign
          | hir_js::AssignOp::BitAndAssign
          | hir_js::AssignOp::BitXorAssign
          | hir_js::AssignOp::LogicalOrAssign
          | hir_js::AssignOp::LogicalAndAssign
          | hir_js::AssignOp::NullishAssign
          | hir_js::AssignOp::ExponentAssign,
        ..
      } => {
        push_unsupported_syntax(
          out,
          Span::new(file, expr.span),
          "unsupported assignment operator in native-js strict subset",
        );
      }
      ExprKind::Call(call) => {
        let is_print_intrinsic = callee_is_ident(body, lowered, call.callee, "print");
        if is_print_intrinsic && !expr_stmt_roots.contains(&expr_id) {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "`print(...)` is only supported as a standalone statement in native-js strict subset",
          );
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
        if !matches!(
          body
            .exprs
            .get(call.callee.0 as usize)
            .map(|e| &e.kind),
          Some(ExprKind::Ident(_))
        ) {
          push_unsupported_syntax(
            out,
            Span::new(file, expr.span),
            "only direct identifier calls are supported by native-js yet",
          );
        }

        // `print(...)` is a codegen intrinsic, so allow it even when it comes from `.d.ts` libs.
        if !is_print_intrinsic {
          let ok = (|| {
            let BindingId::Def(def) = file_resolver.resolve_expr_ident(body, call.callee)? else {
              return None;
            };
            let resolved = resolve_import_def(program, def)?;
            is_codegen_callable_function_def(program, resolved).then_some(())
          })()
          .is_some();

          if !ok {
            push_unsupported_syntax(
              out,
              Span::new(file, expr.span),
              "call callee must resolve to a top-level function definition",
            );
          }
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
      ExprKind::Missing => {
        push_unsupported_syntax(out, Span::new(file, expr.span), "unsupported expression in native-js strict subset");
      }
      _ => {}
    }
  }

  for stmt in body.stmts.iter() {
    match &stmt.kind {
      StmtKind::Decl(_) if !matches!(body.kind, BodyKind::TopLevel) => {
        push_unsupported_syntax(
          out,
          Span::new(file, stmt.span),
          "nested declarations are not supported by native-js yet",
        );
      }
      StmtKind::With { .. } => {
        push_unsupported_syntax(out, Span::new(file, stmt.span), "`with` statements are not supported yet");
      }
      StmtKind::Try { .. } => {
        push_unsupported_syntax(out, Span::new(file, stmt.span), "`try` is not supported by native-js yet");
      }
      StmtKind::Throw(_) => {
        push_unsupported_syntax(out, Span::new(file, stmt.span), "`throw` is not supported by native-js yet");
      }
      StmtKind::ForIn { .. } => {
        push_unsupported_syntax(
          out,
          Span::new(file, stmt.span),
          "`for..in`/`for..of` statements are not supported by native-js yet",
        );
      }
      StmtKind::Switch { .. } => {
        push_unsupported_syntax(out, Span::new(file, stmt.span), "`switch` is not supported by native-js yet");
      }
      StmtKind::For {
        init: Some(ForInit::Var(decl)),
        ..
      } => {
        if decl.declarators.iter().any(|d| d.init.is_none()) {
          push_unsupported_syntax(
            out,
            Span::new(file, stmt.span),
            "variable declarations must have an initializer in native-js strict subset",
          );
        }
      }
      StmtKind::Var(decl) => match decl.kind {
        VarDeclKind::Using | VarDeclKind::AwaitUsing => {
          push_unsupported_syntax(
            out,
            Span::new(file, stmt.span),
            "`using` declarations are not supported by native-js yet",
          );
        }
        _ => {
          if decl.declarators.iter().any(|d| d.init.is_none()) {
            push_unsupported_syntax(
              out,
              Span::new(file, stmt.span),
              "variable declarations must have an initializer in native-js strict subset",
            );
          }
        }
      },
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
}

fn validate_body_types(
  program: &Program,
  file: typecheck_ts::FileId,
  body: BodyId,
  hir: &Body,
  lowered: &hir_js::LowerResult,
  resolver: &Resolver<'_>,
  out: &mut Vec<Diagnostic>,
) {
  let result = program.check_body(body);
  let file_resolver = resolver.for_file(file);

  // Direct calls such as `foo(1)` require the callee identifier to have a callable type.
  //
  // The strict subset validator generally rejects callable/reference types, but in the direct-call
  // lowering path we never materialize the function value; codegen resolves the callee as a symbol.
  // Skip validating the callee identifier's type in this specific position so programs can call
  // declared functions (and small host-provided intrinsics like `print(...)`) without enabling
  // first-class function values.
  let mut skip_expr_type_check = vec![false; result.expr_types().len()];
  for expr in hir.exprs.iter() {
    if let ExprKind::Call(call) = &expr.kind {
      if call.optional || call.is_new || call.args.iter().any(|arg| arg.spread) {
        continue;
      }
      let idx = call.callee.0 as usize;
      if idx >= skip_expr_type_check.len() {
        continue;
      }
      if !matches!(hir.exprs.get(idx).map(|e| &e.kind), Some(ExprKind::Ident(_))) {
        continue;
      }
      // `print(...)` is a codegen intrinsic (lowered to `printf`), so allow it even though it is
      // declared in a `.d.ts` and does not resolve to a `DefId`.
      let is_print_intrinsic = callee_is_ident(hir, lowered, call.callee, "print");
      if !is_print_intrinsic {
        let ok = (|| {
          let BindingId::Def(def) = file_resolver.resolve_expr_ident(hir, call.callee)? else {
            return None;
          };
          let resolved = resolve_import_def(program, def)?;
          is_codegen_callable_function_def(program, resolved).then_some(())
        })()
        .is_some();

        if !ok {
          continue;
        }
      }

      skip_expr_type_check[idx] = true;
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
      .with_note("supported types are currently limited to: number, boolean, string, void, null, undefined"),
  );
}

fn is_supported_type_kind(kind: &TypeKindSummary) -> bool {
  matches!(
    kind,
    TypeKindSummary::Never
      | TypeKindSummary::Void
      | TypeKindSummary::Null
      | TypeKindSummary::Undefined
      | TypeKindSummary::Boolean
      | TypeKindSummary::BooleanLiteral(_)
      | TypeKindSummary::Number
      | TypeKindSummary::NumberLiteral(_)
      | TypeKindSummary::String
      | TypeKindSummary::StringLiteral(_)
  )
}

fn unsupported_type_message(kind: &TypeKindSummary) -> String {
  match kind {
    TypeKindSummary::Any => "`any` is not supported by native-js strict subset".to_string(),
    TypeKindSummary::Unknown => "`unknown` is not supported by native-js strict subset".to_string(),
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

fn is_supported_i32_number_literal(raw: &str) -> bool {
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
        cur = program
          .exports_of(target_file)
          .get(import.original.as_str())
          .and_then(|entry| entry.def)?;
      }
      _ => return None,
    }
  }
}

fn is_codegen_callable_function_def(program: &Program, def: typecheck_ts::DefId) -> bool {
  // Match `native-js` codegen: only top-level function definitions in non-`.d.ts` files are
  // compiled and callable via the direct-call lowering path.
  let Some(lowered) = program.hir_lowered(def.file()) else {
    return false;
  };
  if matches!(lowered.hir.file_kind, FileKind::Dts) {
    return false;
  }
  let Some(def_data) = lowered.def(def) else {
    return false;
  };
  if def_data.parent.is_some() {
    return false;
  }
  if def_data.path.kind != hir_js::DefKind::Function {
    return false;
  }
  def_data.body.is_some()
}
