//! Strict TypeScript-subset validator for the native compiler.
//!
//! `typecheck-ts` intentionally implements TypeScript's semantics, which include
//! unsafe escape hatches (`any`, `eval`, type assertions, ...). The native
//! compiler uses a stricter dialect, so we run an additional validation pass on
//! the checked HIR and type tables and emit hard errors for forbidden
//! constructs.

use diagnostics::{Diagnostic, Span, TextRange};
use hir_js::{ExprId, ExprKind, Literal, ObjectKey, PatId, PatKind, StmtKind, TypeExprKind};
use typecheck_ts::{BodyId, DefId, FileId, Program, TypeKindSummary};

const CODE_ANY: &str = "NJS0001";
const CODE_TYPE_ASSERTION: &str = "NJS0002";
const CODE_NON_NULL_ASSERTION: &str = "NJS0003";
const CODE_EVAL: &str = "NJS0004";
const CODE_NEW_FUNCTION: &str = "NJS0005";
const CODE_WITH_STMT: &str = "NJS0006";
const CODE_DYNAMIC_MEMBER: &str = "NJS0007";
const CODE_ARGUMENTS: &str = "NJS0008";

/// Validate that the given files use only the native-js strict TypeScript subset.
///
/// The validator is intentionally conservative: it prefers false positives over
/// letting unsound constructs through (until the native backend can model them
/// safely).
pub fn validate(program: &Program, files: &[FileId]) -> Vec<Diagnostic> {
  let mut diagnostics = Vec::new();

  for &file in files {
    let Some(lowered) = program.hir_lowered(file) else {
      continue;
    };

    check_any_in_type_exprs(file, &lowered, &mut diagnostics);

    // `Program::bodies_in_file` is deterministic and includes nested bodies.
    for body in program.bodies_in_file(file) {
      check_any_in_body(program, file, body, &lowered, &mut diagnostics);
      check_hir_body(file, body, &lowered, &mut diagnostics);
    }

    check_any_in_exported_defs(program, file, &lowered, &mut diagnostics);
  }

  diagnostics
}

fn check_any_in_type_exprs(file: FileId, lowered: &hir_js::LowerResult, out: &mut Vec<Diagnostic>) {
  for arenas in lowered.types.values() {
    for ty_expr in arenas.type_exprs.iter() {
      if !matches!(ty_expr.kind, TypeExprKind::Any) {
        continue;
      }
      out.push(
        Diagnostic::error(CODE_ANY, "`any` is not allowed in native-js strict mode", Span::new(file, ty_expr.span))
          .with_note("add a precise type annotation or refactor to avoid `any`"),
      );
    }
  }
}

fn check_any_in_body(
  program: &Program,
  file: FileId,
  body: BodyId,
  lowered: &hir_js::LowerResult,
  out: &mut Vec<Diagnostic>,
) {
  let result = program.check_body(body);

  for (idx, ty) in result.expr_types().iter().copied().enumerate() {
    if !matches!(program.type_kind(ty), TypeKindSummary::Any) {
      continue;
    }
    let expr = ExprId(idx as u32);
    let span = program
      .expr_span(body, expr)
      .or_else(|| lowered.body(body).and_then(|b| b.exprs.get(idx)).map(|e| Span::new(file, e.span)));
    let Some(span) = span else { continue };
    out.push(
      Diagnostic::error(
        CODE_ANY,
        "`any` is not allowed in native-js strict mode",
        span,
      )
      .with_note("add a precise type annotation or refactor to avoid `any`"),
    );
  }

  for (idx, ty) in result.pat_types().iter().copied().enumerate() {
    if !matches!(program.type_kind(ty), TypeKindSummary::Any) {
      continue;
    }
    let pat = PatId(idx as u32);
    let span = program
      .pat_span(body, pat)
      .or_else(|| lowered.body(body).and_then(|b| b.pats.get(idx)).map(|p| Span::new(file, p.span)));
    let Some(span) = span else { continue };
    out.push(
      Diagnostic::error(
        CODE_ANY,
        "`any` is not allowed in native-js strict mode",
        span,
      )
      .with_note("add a precise type annotation or refactor to avoid `any`"),
    );
  }
}

fn check_hir_body(
  file: FileId,
  body: BodyId,
  lowered: &hir_js::LowerResult,
  out: &mut Vec<Diagnostic>,
) {
  let Some(body_data) = lowered.body(body) else {
    return;
  };

  for expr in body_data.exprs.iter() {
    match &expr.kind {
      ExprKind::TypeAssertion { .. } => {
        out.push(Diagnostic::error(
          CODE_TYPE_ASSERTION,
          "type assertions are not allowed in native-js strict mode",
          Span::new(file, expr.span),
        ));
      }
      ExprKind::NonNull { .. } => {
        out.push(Diagnostic::error(
          CODE_NON_NULL_ASSERTION,
          "non-null assertions (`!`) are not allowed in native-js strict mode",
          Span::new(file, expr.span),
        ));
      }
      ExprKind::Call(call) => {
        let callee_name = ident_name(body_data, lowered, call.callee);
        if !call.is_new && callee_name == Some("eval") {
          let span = span_of_expr(body_data, file, call.callee).unwrap_or(Span::new(file, expr.span));
          out.push(Diagnostic::error(
            CODE_EVAL,
            "`eval()` is not allowed in native-js strict mode",
            span,
          ));
        }
        if call.is_new && callee_name == Some("Function") {
          let span = span_of_expr(body_data, file, call.callee).unwrap_or(Span::new(file, expr.span));
          out.push(Diagnostic::error(
            CODE_NEW_FUNCTION,
            "`new Function()` is not allowed in native-js strict mode",
            span,
          ));
        }
      }
      ExprKind::Member(member) => {
        if let ObjectKey::Computed(key_expr) = &member.property {
          let key_expr = *key_expr;
          let is_literal = matches!(
            body_data
              .exprs
              .get(key_expr.0 as usize)
              .map(|e| &e.kind),
            Some(ExprKind::Literal(Literal::String(_)) | ExprKind::Literal(Literal::Number(_)))
          );

          if !is_literal {
            let span = span_of_expr(body_data, file, key_expr).unwrap_or(Span::new(file, expr.span));
            out.push(
              Diagnostic::error(
                CODE_DYNAMIC_MEMBER,
                "computed property access requires a literal string/number key in native-js strict mode",
                span,
              )
              .with_note("rewrite as `obj[\"prop\"]`/`obj[0]` or use a safer typed API"),
            );
          }
        }
      }
      ExprKind::Ident(name) => {
        if lowered.names.resolve(*name) == Some("arguments") {
          out.push(Diagnostic::error(
            CODE_ARGUMENTS,
            "the `arguments` object is not allowed in native-js strict mode",
            Span::new(file, expr.span),
          ));
        }
      }
      _ => {}
    }
  }

  for stmt in body_data.stmts.iter() {
    if matches!(stmt.kind, StmtKind::With { .. }) {
      out.push(Diagnostic::error(
        CODE_WITH_STMT,
        "`with` statements are not allowed in native-js strict mode",
        Span::new(file, stmt.span),
      ));
    }
  }

  // Also reject `arguments` as a binding target (conservative initial pass).
  for pat in body_data.pats.iter() {
    if let PatKind::Ident(name) = &pat.kind {
      if lowered.names.resolve(*name) == Some("arguments") {
        out.push(Diagnostic::error(
          CODE_ARGUMENTS,
          "the `arguments` identifier is not allowed in native-js strict mode",
          Span::new(file, pat.span),
        ));
      }
    }
  }
}

fn check_any_in_exported_defs(
  program: &Program,
  file: FileId,
  lowered: &hir_js::LowerResult,
  out: &mut Vec<Diagnostic>,
) {
  let exported: Vec<DefId> = lowered
    .defs
    .iter()
    .filter(|def| def.is_exported || def.is_default_export)
    .map(|def| def.id)
    .collect();

  for def in exported {
    let Some(def_kind) = program.def_kind(def) else {
      continue;
    };

    // Skip type-only definitions; they are erased before codegen.
    if matches!(
      def_kind,
      typecheck_ts::DefKind::Interface(_) | typecheck_ts::DefKind::TypeAlias(_)
    ) {
      continue;
    }

    let def_span = program
      .span_of_def(def)
      .or_else(|| lowered.def(def).map(|d| Span::new(file, d.span)))
      .unwrap_or_else(|| Span::new(file, TextRange::new(0, 0)));
    let ty = program.type_of_def_interned(def);

    if matches!(program.type_kind(ty), TypeKindSummary::Any) {
      out.push(
        Diagnostic::error(
          CODE_ANY,
          "exported definition has type `any`, which is not allowed in native-js strict mode",
          def_span,
        )
        .with_note("add a precise exported type to keep native codegen sound"),
      );
      continue;
    }

    // For callable exports, also forbid `any` in signature positions (e.g. `(): any`).
    for sig in program.call_signatures(ty) {
      if matches!(program.type_kind(sig.signature.ret), TypeKindSummary::Any) {
        out.push(Diagnostic::error(
          CODE_ANY,
          "exported function has return type `any`, which is not allowed in native-js strict mode",
          def_span,
        ));
        break;
      }
      if sig
        .signature
        .params
        .iter()
        .any(|param| matches!(program.type_kind(param.ty), TypeKindSummary::Any))
      {
        out.push(Diagnostic::error(
          CODE_ANY,
          "exported function has an `any` parameter type, which is not allowed in native-js strict mode",
          def_span,
        ));
        break;
      }
      if sig
        .signature
        .this_param
        .is_some_and(|this_ty| matches!(program.type_kind(this_ty), TypeKindSummary::Any))
      {
        out.push(Diagnostic::error(
          CODE_ANY,
          "exported function has `this: any`, which is not allowed in native-js strict mode",
          def_span,
        ));
        break;
      }
    }

    for sig in program.construct_signatures(ty) {
      if matches!(program.type_kind(sig.signature.ret), TypeKindSummary::Any) {
        out.push(Diagnostic::error(
          CODE_ANY,
          "exported constructor has return type `any`, which is not allowed in native-js strict mode",
          def_span,
        ));
        break;
      }
    }
  }
}

fn ident_name<'a>(
  body: &'a hir_js::Body,
  lowered: &'a hir_js::LowerResult,
  expr: ExprId,
) -> Option<&'a str> {
  let kind = body.exprs.get(expr.0 as usize).map(|e| &e.kind)?;
  match kind {
    ExprKind::Ident(name) => lowered.names.resolve(*name),
    _ => None,
  }
}

fn span_of_expr(body: &hir_js::Body, file: FileId, expr: ExprId) -> Option<Span> {
  body
    .exprs
    .get(expr.0 as usize)
    .map(|expr| Span::new(file, expr.span))
}
