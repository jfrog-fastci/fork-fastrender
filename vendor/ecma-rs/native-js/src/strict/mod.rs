//! Strict TypeScript-subset validator for the native compiler.
//!
//! `typecheck-ts` intentionally implements TypeScript's semantics, which include
//! unsafe escape hatches (`any`, `eval`, type assertions, ...). The native
//! compiler uses a stricter dialect, so we run an additional validation pass on
//! the checked HIR and type tables and emit hard errors for forbidden
//! constructs.
//!
//! ## Diagnostic codes
//!
//! This module emits stable `NJS####` diagnostic codes:
//! - `NJS0001`: `any` type (explicit or inferred)
//! - `NJS0002`: type assertions (`x as T`, `<T>x`)
//! - `NJS0003`: non-null assertions (`x!`)
//! - `NJS0004`: `eval()`
//! - `NJS0005`: `Function()` / `new Function()`
//! - `NJS0006`: `with` statements
//! - `NJS0007`: computed property access with non-literal keys (except array/tuple indexing)
//! - `NJS0008`: use of the `arguments` identifier/object
//! - `NJS0108`: entry file must export a `main` function
//! - `NJS0109`: failed to resolve exported `main`
//! - `NJS0110`: exported `main` must be a function with a body
//! - `NJS0111`: exported `main` must have a supported signature (no params, not async/generator)

use crate::codes;
use diagnostics::{Diagnostic, Span, TextRange};
use hir_js::{
  BinaryOp, BodyKind, ExprId, ExprKind, Literal, ObjectKey, PatId, PatKind, StmtKind, TypeExprKind,
};
use std::collections::{HashMap, HashSet};
use typecheck_ts::{BodyCheckResult, BodyId, DefId, FileId, ImportTarget, Program, TypeId, TypeKindSummary};
use types_ts_interned as tti;

/// Validate that the given files use only the native-js strict TypeScript subset.
///
/// The validator is intentionally conservative: it prefers false positives over
/// letting unsound constructs through (until the native backend can model them
/// safely).
pub fn validate(program: &Program, files: &[FileId]) -> Vec<Diagnostic> {
  let mut diagnostics = Vec::new();
  let mut any_checker = AnyChecker::new(program);

  for &file in files {
    let Some(lowered) = program.hir_lowered(file) else {
      continue;
    };

    check_any_in_type_exprs(file, &lowered, &mut diagnostics);

    // `Program::bodies_in_file` is deterministic and includes nested bodies.
    for body in program.bodies_in_file(file) {
      let result = program.check_body(body);
      check_any_in_body(
        program,
        file,
        body,
        &result,
        &lowered,
        &mut any_checker,
        &mut diagnostics,
      );
      check_hir_body(program, file, body, &result, &lowered, &mut diagnostics);
    }

    check_any_in_exported_defs(program, file, &lowered, &mut any_checker, &mut diagnostics);
  }

  diagnostics::sort_diagnostics(&mut diagnostics);
  diagnostics
}

struct AnyChecker<'a> {
  program: &'a Program,
  cache: HashMap<TypeId, bool>,
  visiting: HashSet<TypeId>,
}

impl<'a> AnyChecker<'a> {
  fn new(program: &'a Program) -> Self {
    Self {
      program,
      cache: HashMap::new(),
      visiting: HashSet::new(),
    }
  }

  fn contains_any(&mut self, ty: TypeId) -> bool {
    if let Some(&cached) = self.cache.get(&ty) {
      return cached;
    }
    if !self.visiting.insert(ty) {
      // Break recursive cycles (e.g. self-referential types).
      return false;
    }

    let found = match self.program.interned_type_kind(ty) {
      tti::TypeKind::Any => true,
      tti::TypeKind::Infer { constraint, .. } => constraint.is_some_and(|ty| self.contains_any(ty)),
      tti::TypeKind::Tuple(elems) => elems.iter().any(|elem| self.contains_any(elem.ty)),
      tti::TypeKind::Array { ty, .. } => self.contains_any(ty),
      tti::TypeKind::Union(members) | tti::TypeKind::Intersection(members) => {
        members.iter().any(|member| self.contains_any(*member))
      }
      tti::TypeKind::Callable { .. } => self
        .program
        .call_signatures(ty)
        .iter()
        .any(|sig| self.signature_contains_any(&sig.signature)),
      tti::TypeKind::Object(_) => {
        if self
          .program
          .properties_of(ty)
          .iter()
          .any(|prop| self.contains_any(prop.ty))
        {
          true
        } else if self
          .program
          .indexers(ty)
          .iter()
          .any(|idx| self.contains_any(idx.key_type) || self.contains_any(idx.value_type))
        {
          true
        } else if self
          .program
          .call_signatures(ty)
          .iter()
          .any(|sig| self.signature_contains_any(&sig.signature))
        {
          true
        } else {
          self
            .program
            .construct_signatures(ty)
            .iter()
            .any(|sig| self.signature_contains_any(&sig.signature))
        }
      }
      tti::TypeKind::Ref { def, args } => {
        if args.iter().any(|arg| self.contains_any(*arg)) {
          true
        } else {
          let declared = self.program.declared_type_of_def_interned(def);
          self.contains_any(declared)
        }
      }
      tti::TypeKind::Predicate { asserted, .. } => asserted.is_some_and(|ty| self.contains_any(ty)),
      tti::TypeKind::Conditional {
        check,
        extends,
        true_ty,
        false_ty,
        ..
      } => {
        self.contains_any(check)
          || self.contains_any(extends)
          || self.contains_any(true_ty)
          || self.contains_any(false_ty)
      }
      tti::TypeKind::Mapped(mapped) => {
        self.contains_any(mapped.source)
          || self.contains_any(mapped.value)
          || mapped.name_type.is_some_and(|ty| self.contains_any(ty))
          || mapped.as_type.is_some_and(|ty| self.contains_any(ty))
      }
      tti::TypeKind::Intrinsic { ty, .. } => self.contains_any(ty),
      tti::TypeKind::IndexedAccess { obj, index } => self.contains_any(obj) || self.contains_any(index),
      tti::TypeKind::KeyOf(ty) => self.contains_any(ty),
      _ => false,
    };

    self.visiting.remove(&ty);
    self.cache.insert(ty, found);
    found
  }

  fn signature_contains_any(&mut self, sig: &tti::Signature) -> bool {
    if self.contains_any(sig.ret) {
      return true;
    }
    if sig.this_param.is_some_and(|ty| self.contains_any(ty)) {
      return true;
    }
    if sig.params.iter().any(|param| self.contains_any(param.ty)) {
      return true;
    }
    sig.type_params.iter().any(|param| {
      param
        .constraint
        .is_some_and(|ty| self.contains_any(ty))
        || param.default.is_some_and(|ty| self.contains_any(ty))
    })
  }
}

fn check_any_in_type_exprs(file: FileId, lowered: &hir_js::LowerResult, out: &mut Vec<Diagnostic>) {
  for arenas in lowered.types.values() {
    for ty_expr in arenas.type_exprs.iter() {
      if !matches!(ty_expr.kind, TypeExprKind::Any) {
        continue;
      }
      out.push(
        codes::STRICT_ANY_TYPE.error(
          "`any` is not allowed in native-js strict mode",
          Span::new(file, ty_expr.span),
        )
          .with_note("add a precise type annotation or refactor to avoid `any`"),
      );
    }
  }
}

fn check_any_in_body(
  program: &Program,
  file: FileId,
  body: BodyId,
  result: &BodyCheckResult,
  lowered: &hir_js::LowerResult,
  any_checker: &mut AnyChecker<'_>,
  out: &mut Vec<Diagnostic>,
) {
  for (idx, ty) in result.expr_types().iter().copied().enumerate() {
    if !any_checker.contains_any(ty) {
      continue;
    }
    let expr = ExprId(idx as u32);
    let span = program
      .expr_span(body, expr)
      .or_else(|| lowered.body(body).and_then(|b| b.exprs.get(idx)).map(|e| Span::new(file, e.span)));
    let Some(span) = span else { continue };
    out.push(
      codes::STRICT_ANY_TYPE
        .error("`any` is not allowed in native-js strict mode", span)
      .with_note("add a precise type annotation or refactor to avoid `any`"),
    );
  }

  for (idx, ty) in result.pat_types().iter().copied().enumerate() {
    if !any_checker.contains_any(ty) {
      continue;
    }
    let pat = PatId(idx as u32);
    let span = program
      .pat_span(body, pat)
      .or_else(|| lowered.body(body).and_then(|b| b.pats.get(idx)).map(|p| Span::new(file, p.span)));
    let Some(span) = span else { continue };
    out.push(
      codes::STRICT_ANY_TYPE
        .error("`any` is not allowed in native-js strict mode", span)
      .with_note("add a precise type annotation or refactor to avoid `any`"),
    );
  }
}

fn check_hir_body(
  program: &Program,
  file: FileId,
  body: BodyId,
  result: &BodyCheckResult,
  lowered: &hir_js::LowerResult,
  out: &mut Vec<Diagnostic>,
) {
  let Some(body_data) = lowered.body(body) else {
    return;
  };

  for expr in body_data.exprs.iter() {
    match &expr.kind {
      ExprKind::TypeAssertion { .. } => {
        out.push(codes::STRICT_TYPE_ASSERTION.error(
          "type assertions are not allowed in native-js strict mode",
          Span::new(file, expr.span),
        ));
      }
      ExprKind::NonNull { .. } => {
        out.push(codes::STRICT_NON_NULL_ASSERTION.error(
          "non-null assertions (`!`) are not allowed in native-js strict mode",
          Span::new(file, expr.span),
        ));
      }
      ExprKind::Call(call) => {
        if !call.is_new && call_targets_name(body_data, lowered, call.callee, "eval") {
          let span = span_of_expr(body_data, file, call.callee).unwrap_or(Span::new(file, expr.span));
          out.push(codes::STRICT_EVAL.error("`eval()` is not allowed in native-js strict mode", span));
        }
        if call_targets_name(body_data, lowered, call.callee, "Function") {
          let span = span_of_expr(body_data, file, call.callee).unwrap_or(Span::new(file, expr.span));
          out.push(codes::STRICT_FUNCTION_CTOR.error(
            if call.is_new {
              "`new Function()` is not allowed in native-js strict mode"
            } else {
              "`Function()` is not allowed in native-js strict mode"
            },
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

          // Allow `arr[i]` / `tuple[i]` when the receiver is array-like and the key expression is
          // typed as a number. This is a common and safe pattern even in strict native-js mode;
          // rejecting it would make basic loops and indexing impractical.
          let is_array_index = !is_literal
            && matches!(
              result
                .expr_types()
                .get(member.object.0 as usize)
                .copied()
                .map(|ty| program.type_kind(ty)),
              Some(TypeKindSummary::Array { .. } | TypeKindSummary::Tuple { .. })
            )
            && matches!(
              result
                .expr_types()
                .get(key_expr.0 as usize)
                .copied()
                .map(|ty| program.type_kind(ty)),
              Some(TypeKindSummary::Number | TypeKindSummary::NumberLiteral(_))
            );

           if !is_literal && !is_array_index {
             let span = span_of_expr(body_data, file, key_expr).unwrap_or(Span::new(file, expr.span));
             out.push(
              codes::STRICT_DYNAMIC_MEMBER
                .error(
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
          out.push(codes::STRICT_ARGUMENTS.error(
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
      out.push(codes::STRICT_WITH_STMT.error(
        "`with` statements are not allowed in native-js strict mode",
        Span::new(file, stmt.span),
      ));
    }
  }

  // Also reject `arguments` as a binding target (conservative initial pass).
  for pat in body_data.pats.iter() {
    if let PatKind::Ident(name) = &pat.kind {
      if lowered.names.resolve(*name) == Some("arguments") {
        out.push(codes::STRICT_ARGUMENTS.error(
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
  any_checker: &mut AnyChecker<'_>,
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

    if any_checker.contains_any(ty) {
      out.push(
        codes::STRICT_ANY_TYPE
          .error(
            "exported definition uses `any`, which is not allowed in native-js strict mode",
            def_span,
          )
        .with_note("add a precise exported type to keep native codegen sound"),
      );
    }
  }
}

fn callee_matches_name(
  body: &hir_js::Body,
  lowered: &hir_js::LowerResult,
  expr: ExprId,
  target: &str,
) -> bool {
  let Some(kind) = body.exprs.get(expr.0 as usize).map(|e| &e.kind) else {
    return false;
  };
  match kind {
    ExprKind::Ident(name) => lowered.names.resolve(*name) == Some(target),
    ExprKind::Member(member) => member_property_matches_name(body, lowered, &member.property, target),
    _ => false,
  }
}

fn call_targets_name(
  body: &hir_js::Body,
  lowered: &hir_js::LowerResult,
  callee: ExprId,
  target: &str,
) -> bool {
  if callee_matches_name(body, lowered, callee, target) {
    return true;
  }
  let Some(kind) = body.exprs.get(callee.0 as usize).map(|e| &e.kind) else {
    return false;
  };
  match kind {
    ExprKind::Member(member) => {
      // Catch indirect uses like:
      // - `eval.call(...)`, `eval.apply(...)`
      // - `eval.bind(...)` (bind still produces an eval-capable callable)
      // - `obj.eval.call(...)`, `obj["eval"].apply(...)`, etc.
      //
      // This is intentionally conservative: we do not attempt to track aliasing
      // (e.g. `const e = eval; e("code")`), but we *do* want to reject the common
      // `.call`/`.apply`/`.bind` escape hatches.
      let is_call_apply_or_bind =
        member_property_matches_name(body, lowered, &member.property, "call")
          || member_property_matches_name(body, lowered, &member.property, "apply")
          || member_property_matches_name(body, lowered, &member.property, "bind");
      is_call_apply_or_bind && callee_matches_name(body, lowered, member.object, target)
    }
    ExprKind::Binary {
      op: BinaryOp::Comma,
      right,
      ..
    } => call_targets_name(body, lowered, *right, target),
    _ => false,
  }
}

fn member_property_matches_name(
  body: &hir_js::Body,
  lowered: &hir_js::LowerResult,
  property: &ObjectKey,
  target: &str,
) -> bool {
  match property {
    ObjectKey::Ident(name) => lowered.names.resolve(*name) == Some(target),
    ObjectKey::String(s) => s == target,
    ObjectKey::Computed(expr) => match body.exprs.get(expr.0 as usize).map(|e| &e.kind) {
      Some(ExprKind::Literal(Literal::String(lit))) => lit.lossy == target,
      _ => false,
    },
    _ => false,
  }
}

fn span_of_expr(body: &hir_js::Body, file: FileId, expr: ExprId) -> Option<Span> {
  body
    .exprs
    .get(expr.0 as usize)
    .map(|expr| Span::new(file, expr.span))
}

/// `native-js` currently expects an exported `main` function in the entry file.
///
/// This helper locates that function and validates basic requirements needed by
/// the current HIR-based code generator.
#[derive(Debug, Clone, Copy)]
pub struct Entrypoint {
  pub main_def: DefId,
  pub main_body: BodyId,
}

fn resolve_export_def(program: &Program, def: DefId) -> Option<DefId> {
  // Follow `import { x } from "./dep"` through the module graph to find the
  // underlying defining `DefId`.
  //
  // Note: `typecheck-ts` represents re-exported bindings (`export { x } from ...`
  // and `export * from ...`) with `ExportEntry::def == None`, so this helper also
  // falls back to `Program::symbol_info(export.symbol).def` when traversing
  // exports.
  let mut cur = def;
  let mut seen = HashSet::<DefId>::new();
  loop {
    if !seen.insert(cur) {
      return None;
    }
    let kind = program.def_kind(cur)?;
    let typecheck_ts::DefKind::Import(import) = kind else {
      return Some(cur);
    };
    match import.target {
      ImportTarget::File(target_file) => {
        let (symbol, local_def) = {
          let exports = program.exports_of(target_file);
          let entry = exports.get(import.original.as_str())?;
          (entry.symbol, entry.def)
        };

        cur = program
          .symbol_info(symbol)
          .and_then(|info| info.def)
          .or(local_def)?;
      }
      _ => return None,
    }
  }
}

pub fn entrypoint(program: &Program, entry_file: FileId) -> Result<Entrypoint, Vec<Diagnostic>> {
  let exports = program.exports_of(entry_file);
  let Some(entry) = exports.get("main") else {
    return Err(vec![codes::ENTRYPOINT_MISSING_MAIN_EXPORT.error(
      "entry file must export a `main` function",
      Span::new(entry_file, TextRange::new(0, 0)),
    )]);
  };

  let def = program
    .symbol_info(entry.symbol)
    .and_then(|info| info.def)
    .or(entry.def)
    .and_then(|def| resolve_export_def(program, def))
    .ok_or_else(|| {
      vec![codes::ENTRYPOINT_UNRESOLVED_MAIN.error(
        "failed to resolve exported `main` definition",
        Span::new(entry_file, TextRange::new(0, 0)),
      )]
    })?;

  let span = program
    .span_of_def(def)
    .unwrap_or_else(|| Span::new(entry_file, TextRange::new(0, 0)));

  let Some(def_kind) = program.def_kind(def) else {
    return Err(vec![codes::ENTRYPOINT_MAIN_NOT_FUNCTION.error(
      "failed to resolve exported `main` definition",
      span,
    )]);
  };
  if !matches!(def_kind, typecheck_ts::DefKind::Function(_)) {
    return Err(vec![codes::ENTRYPOINT_MAIN_NOT_FUNCTION.error(
      "exported `main` must be a function",
      span,
    )]);
  }

  let Some(body) = program.body_of_def(def) else {
    return Err(vec![codes::ENTRYPOINT_MAIN_NOT_FUNCTION.error(
      "exported `main` must have a body",
      span,
    )]);
  };

  let Some(lowered) = program.hir_lowered(def.file()) else {
    return Err(vec![codes::ENTRYPOINT_MAIN_NOT_FUNCTION.error(
      "failed to access lowered HIR for `main` file",
      span,
    )]);
  };
  let Some(hir_body) = lowered.body(body) else {
    return Err(vec![codes::ENTRYPOINT_MAIN_NOT_FUNCTION.error(
      "failed to access lowered HIR for `main` body",
      span,
    )]);
  };
  if hir_body.kind != BodyKind::Function {
    return Err(vec![codes::ENTRYPOINT_MAIN_NOT_FUNCTION.error(
      "exported `main` must be a function body",
      span,
    )]);
  }
  let Some(function) = hir_body.function.as_ref() else {
    return Err(vec![codes::ENTRYPOINT_MAIN_NOT_FUNCTION.error(
      "missing function metadata for `main` body",
      span,
    )]);
  };
  if !function.params.is_empty() {
    return Err(vec![codes::ENTRYPOINT_MAIN_BAD_SIGNATURE.error(
      "`main` must not accept parameters in native-js strict mode",
      span,
    )]);
  }
  if function.async_ || function.generator {
    return Err(vec![codes::ENTRYPOINT_MAIN_BAD_SIGNATURE.error(
      "`main` must not be async or a generator in native-js strict mode",
      span,
    )]);
  }

  Ok(Entrypoint {
    main_def: def,
    main_body: body,
  })
}
