use crate::codes;
use crate::BodyCheckResult;
use diagnostics::{Diagnostic, FileId, Span, TextRange};
use hir_js::{Body, BodyKind, ExprKind, Literal, NameInterner, ObjectKey, StmtKind};
use std::collections::{HashMap, HashSet};
use types_ts_interned::{RelateCtx, TypeId, TypeKind, TypeStore};

pub fn validate_native_strict_body(
  body: &Body,
  result: &BodyCheckResult,
  store: &TypeStore,
  relate: &RelateCtx,
  file: FileId,
) -> Vec<Diagnostic> {
  let prim = store.primitive_ids();
  let mut diagnostics = Vec::new();

  let mut name_interner = NameInterner::default();
  let eval_name = name_interner.intern("eval");
  let global_this_name = name_interner.intern("globalThis");
  let function_name = name_interner.intern("Function");
  let proxy_name = name_interner.intern("Proxy");
  let arguments_name = name_interner.intern("arguments");

  fn is_effective_any(store: &TypeStore, relate: &RelateCtx, ty: TypeId) -> bool {
    let ty = store.canon(ty);
    match store.type_kind(ty) {
      TypeKind::Any => true,
      // `TypeKind::Ref` nodes may expand to `any` (e.g. type aliases). Use the
      // relation engine (which has access to a reference expander) to detect
      // those cases without needing a full evaluator here.
      TypeKind::Ref { .. } => {
        let prim = store.primitive_ids();
        // `unknown` is only assignable to `unknown` and `any`. If `unknown` is
        // assignable to `ty`, then `ty` is either `unknown` or `any` (after
        // expansion). Distinguish `any` from `unknown` by checking assignability
        // to a concrete type.
        relate.is_assignable(prim.unknown, ty) && relate.is_assignable(ty, prim.number)
      }
      _ => false,
    }
  }

  fn type_contains_any(
    store: &TypeStore,
    relate: &RelateCtx,
    ty: TypeId,
    cache: &mut HashMap<TypeId, bool>,
    visiting: &mut HashSet<TypeId>,
  ) -> bool {
    if let Some(hit) = cache.get(&ty) {
      return *hit;
    }

    // Break cycles conservatively (no `any` found along this path).
    if !visiting.insert(ty) {
      return false;
    }

    let result = if is_effective_any(store, relate, ty) {
      true
    } else {
      match store.type_kind(ty) {
        TypeKind::Infer { constraint, .. } => constraint
          .is_some_and(|inner| type_contains_any(store, relate, inner, cache, visiting)),
        TypeKind::Tuple(elems) => elems.into_iter().any(|elem| {
          type_contains_any(store, relate, elem.ty, cache, visiting)
        }),
        TypeKind::Array { ty, .. } => type_contains_any(store, relate, ty, cache, visiting),
        TypeKind::Union(members) | TypeKind::Intersection(members) => members
          .into_iter()
          .any(|member| type_contains_any(store, relate, member, cache, visiting)),
        TypeKind::Ref { args, .. } => args
          .into_iter()
          .any(|arg| type_contains_any(store, relate, arg, cache, visiting)),
        TypeKind::Predicate { asserted, .. } => asserted
          .is_some_and(|inner| type_contains_any(store, relate, inner, cache, visiting)),
        TypeKind::Conditional {
          check,
          extends,
          true_ty,
          false_ty,
          ..
        } => {
          type_contains_any(store, relate, check, cache, visiting)
            || type_contains_any(store, relate, extends, cache, visiting)
            || type_contains_any(store, relate, true_ty, cache, visiting)
            || type_contains_any(store, relate, false_ty, cache, visiting)
        }
        TypeKind::Mapped(mapped) => {
          type_contains_any(store, relate, mapped.source, cache, visiting)
            || type_contains_any(store, relate, mapped.value, cache, visiting)
            || mapped.name_type.is_some_and(|inner| {
              type_contains_any(store, relate, inner, cache, visiting)
            })
            || mapped.as_type.is_some_and(|inner| {
              type_contains_any(store, relate, inner, cache, visiting)
            })
        }
        TypeKind::TemplateLiteral(tpl) => tpl
          .spans
          .into_iter()
          .any(|chunk| type_contains_any(store, relate, chunk.ty, cache, visiting)),
        TypeKind::Intrinsic { ty, .. } => type_contains_any(store, relate, ty, cache, visiting),
        TypeKind::IndexedAccess { obj, index } => {
          type_contains_any(store, relate, obj, cache, visiting)
            || type_contains_any(store, relate, index, cache, visiting)
        }
        TypeKind::KeyOf(inner) => type_contains_any(store, relate, inner, cache, visiting),
        _ => false,
      }
    };

    visiting.remove(&ty);
    cache.insert(ty, result);
    result
  }

  let mut any_cache = HashMap::new();
  for (idx, ty) in result.expr_types.iter().enumerate() {
    let span = result
      .expr_spans
      .get(idx)
      .copied()
      .or_else(|| body.exprs.get(idx).map(|expr| expr.span))
      .unwrap_or(TextRange::new(0, 0));
    let mut visiting = HashSet::new();
    if type_contains_any(store, relate, *ty, &mut any_cache, &mut visiting) {
      diagnostics.push(codes::NATIVE_STRICT_ANY.error(
        "`any` is forbidden when `native_strict` is enabled",
        Span::new(file, span),
      ));
    }
  }
  for (idx, ty) in result.pat_types.iter().enumerate() {
    let span = result
      .pat_spans
      .get(idx)
      .copied()
      .unwrap_or(TextRange::new(0, 0));
    let mut visiting = HashSet::new();
    if type_contains_any(store, relate, *ty, &mut any_cache, &mut visiting) {
      diagnostics.push(codes::NATIVE_STRICT_ANY.error(
        "`any` is forbidden when `native_strict` is enabled",
        Span::new(file, span),
      ));
    }
  }

  let strict_null_checks = relate.options.strict_null_checks;
  let body_is_non_arrow_function = matches!(body.kind, BodyKind::Function)
    && body
      .function
      .as_ref()
      .is_some_and(|function| !function.is_arrow);

  for (idx, expr) in body.exprs.iter().enumerate() {
    match &expr.kind {
      ExprKind::Call(call) => {
        let callee = body.exprs.get(call.callee.0 as usize);
        if let Some(callee) = callee {
          let callee_span = result
            .expr_spans
            .get(call.callee.0 as usize)
            .copied()
            .unwrap_or(callee.span);

          if !call.is_new {
            let direct_eval =
              matches!(&callee.kind, ExprKind::Ident(name) if *name == eval_name);
            let global_eval = match &callee.kind {
              ExprKind::Member(mem) => {
                let prop_is_eval = matches!(&mem.property, ObjectKey::Ident(name) if *name == eval_name)
                  || matches!(&mem.property, ObjectKey::String(name) if name == "eval");
                let obj_is_global_this = matches!(
                  body.exprs.get(mem.object.0 as usize).map(|e| &e.kind),
                  Some(ExprKind::Ident(name)) if *name == global_this_name
                );
                prop_is_eval && obj_is_global_this
              }
              _ => false,
            };

            if direct_eval || global_eval {
              diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                "`eval` is forbidden when `native_strict` is enabled",
                Span::new(file, callee_span),
              ));
            }
          }

          if call.is_new {
            if matches!(&callee.kind, ExprKind::Ident(name) if *name == function_name) {
              diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                "`new Function` is forbidden when `native_strict` is enabled",
                Span::new(file, callee_span),
              ));
            }
            if matches!(&callee.kind, ExprKind::Ident(name) if *name == proxy_name) {
              diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                "`new Proxy` is forbidden when `native_strict` is enabled",
                Span::new(file, callee_span),
              ));
            }
          }
        }
      }
      ExprKind::TypeAssertion {
        expr: inner,
        const_assertion,
        ..
      } => {
        if !*const_assertion {
          let Some(inner_ty) = result.expr_types.get(inner.0 as usize).copied() else {
            continue;
          };
          let Some(asserted_ty) = result.expr_types.get(idx).copied() else {
            continue;
          };
          if !relate.is_assignable(inner_ty, asserted_ty) {
            let span = result
              .expr_spans
              .get(idx)
              .copied()
              .unwrap_or(expr.span);
            diagnostics.push(codes::NATIVE_STRICT_UNSAFE_ASSERTION.error(
              "unsafe type assertion: expression type is not assignable to the asserted type",
              Span::new(file, span),
            ));
          }
        }
      }
      ExprKind::NonNull { expr: inner } => {
        if strict_null_checks {
          let Some(inner_ty) = result.expr_types.get(inner.0 as usize).copied() else {
            continue;
          };
          let nullish = relate.is_assignable(prim.null, inner_ty)
            || relate.is_assignable(prim.undefined, inner_ty);
          if nullish {
            let span = result
              .expr_spans
              .get(idx)
              .copied()
              .unwrap_or(expr.span);
            diagnostics.push(codes::NATIVE_STRICT_NONNULL_ASSERTION.error(
              "non-null assertion on a maybe-nullish value is forbidden when `native_strict` is enabled",
              Span::new(file, span),
            ));
          }
        }
      }
      ExprKind::Member(member) => {
        if let ObjectKey::Computed(key_expr) = &member.property {
          let Some(key) = body.exprs.get(key_expr.0 as usize) else {
            continue;
          };
          let key_is_literal = matches!(
            &key.kind,
            ExprKind::Literal(Literal::String(_) | Literal::Number(_) | Literal::BigInt(_))
          );
          if !key_is_literal {
            let span = result
              .expr_spans
              .get(key_expr.0 as usize)
              .copied()
              .unwrap_or(key.span);
            diagnostics.push(codes::NATIVE_STRICT_COMPUTED_PROPERTY_KEY.error(
              "computed property access requires a constant key when `native_strict` is enabled",
              Span::new(file, span),
            ));
          }
        }
      }
      ExprKind::Ident(name) => {
        if body_is_non_arrow_function && *name == arguments_name {
          let span = result
            .expr_spans
            .get(idx)
            .copied()
            .unwrap_or(expr.span);
          diagnostics.push(codes::NATIVE_STRICT_ARGUMENTS.error(
            "`arguments` is forbidden when `native_strict` is enabled",
            Span::new(file, span),
          ));
        }
      }
      _ => {}
    }
  }

  for stmt in &body.stmts {
    if matches!(&stmt.kind, StmtKind::With { .. }) {
      let start = stmt.span.start;
      let end = start.saturating_add(4).min(stmt.span.end);
      diagnostics.push(codes::NATIVE_STRICT_WITH.error(
        "`with` is forbidden when `native_strict` is enabled",
        Span::new(file, TextRange::new(start, end)),
      ));
    }
  }

  codes::normalize_diagnostics(&mut diagnostics);
  diagnostics
}
