use crate::codes;
use crate::BodyCheckResult;
use diagnostics::{Diagnostic, FileId, Span, TextRange};
use hir_js::{
  Body, ClassMemberKey, ClassMemberKind, ExprKind, Literal, NameInterner, ObjectKey, PatKind,
  StmtKind,
};
use std::collections::{HashMap, HashSet};
use types_ts_interned::{Indexer, ObjectType, RelateCtx, Shape, TypeId, TypeKind, TypeStore};

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
  let object_name = name_interner.intern("Object");
  let reflect_name = name_interner.intern("Reflect");
  let function_name = name_interner.intern("Function");
  let proxy_name = name_interner.intern("Proxy");
  let revocable_name = name_interner.intern("revocable");
  let arguments_name = name_interner.intern("arguments");
  let call_name = name_interner.intern("call");
  let apply_name = name_interner.intern("apply");
  let bind_name = name_interner.intern("bind");
  let construct_name = name_interner.intern("construct");
  let set_prototype_of_name = name_interner.intern("setPrototypeOf");
  let define_property_name = name_interner.intern("defineProperty");
  let define_properties_name = name_interner.intern("defineProperties");
  let assign_name = name_interner.intern("assign");
  let prototype_name = name_interner.intern("prototype");
  let proto_name = name_interner.intern("__proto__");

  fn object_key_is_ident(key: &ObjectKey, name: hir_js::NameId) -> bool {
    matches!(key, ObjectKey::Ident(id) if *id == name)
  }

  fn object_key_is_string(key: &ObjectKey, value: &str) -> bool {
    matches!(key, ObjectKey::String(s) if s == value)
  }

  fn object_key_is_literal_string(body: &Body, key: &ObjectKey, value: &str) -> bool {
    match key {
      ObjectKey::Computed(expr_id) => {
        let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
          return false;
        };
        match &expr.kind {
          ExprKind::Literal(Literal::String(s)) => s.lossy == value,
          // `${...}` template literals are non-constant; only allow the fully
          // literal `\`foo\`` form.
          ExprKind::Template(tpl) => tpl.spans.is_empty() && tpl.head == value,
          _ => false,
        }
      }
      _ => false,
    }
  }

  fn expr_is_const_string(body: &Body, expr_id: hir_js::ExprId, value: &str) -> bool {
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return false;
    };
    match &expr.kind {
      ExprKind::Literal(Literal::String(s)) => s.lossy == value,
      // `${...}` template literals are non-constant; only allow the fully
      // literal `\`foo\`` form.
      ExprKind::Template(tpl) => tpl.spans.is_empty() && tpl.head == value,
      _ => false,
    }
  }

  fn array_literal_exprs(body: &Body, expr_id: hir_js::ExprId) -> Option<Vec<hir_js::ExprId>> {
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return None;
    };
    let ExprKind::Array(arr) = &expr.kind else {
      return None;
    };
    let mut out = Vec::with_capacity(arr.elements.len());
    for elem in &arr.elements {
      match elem {
        hir_js::ArrayElement::Expr(expr) => out.push(*expr),
        hir_js::ArrayElement::Spread(_) | hir_js::ArrayElement::Empty => return None,
      }
    }
    Some(out)
  }

  fn expr_is_object_literal_with_proto_key(
    body: &Body,
    expr_id: hir_js::ExprId,
    prototype_name: hir_js::NameId,
    proto_name: hir_js::NameId,
  ) -> bool {
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return false;
    };
    let ExprKind::Object(obj) = &expr.kind else {
      return false;
    };
    for prop in &obj.properties {
      let key = match prop {
        hir_js::ObjectProperty::KeyValue { key, .. } => key,
        hir_js::ObjectProperty::Getter { key, .. } => key,
        hir_js::ObjectProperty::Setter { key, .. } => key,
        hir_js::ObjectProperty::Spread(_) => continue,
      };
      if object_key_is_ident(key, prototype_name)
        || object_key_is_ident(key, proto_name)
        || object_key_is_string(key, "prototype")
        || object_key_is_string(key, "__proto__")
        || object_key_is_literal_string(body, key, "prototype")
        || object_key_is_literal_string(body, key, "__proto__")
      {
        return true;
      }
    }
    false
  }

  fn expr_is_builtin_member(
    body: &Body,
    expr_id: hir_js::ExprId,
    global_this_name: hir_js::NameId,
    base_name: hir_js::NameId,
    base_str: &str,
    member_name: hir_js::NameId,
    member_str: &str,
  ) -> bool {
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return false;
    };
    let ExprKind::Member(mem) = &expr.kind else {
      return false;
    };
    expr_is_ident_or_global_this_member(body, mem.object, global_this_name, base_name, base_str)
      && (object_key_is_ident(&mem.property, member_name)
        || object_key_is_string(&mem.property, member_str)
        || object_key_is_literal_string(body, &mem.property, member_str))
  }

  fn expr_is_global_this(
    body: &Body,
    expr_id: hir_js::ExprId,
    global_this_name: hir_js::NameId,
  ) -> bool {
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return false;
    };
    match &expr.kind {
      ExprKind::Ident(name) => *name == global_this_name,
      ExprKind::Member(mem) => {
        if !expr_is_global_this(body, mem.object, global_this_name) {
          return false;
        }
        object_key_is_ident(&mem.property, global_this_name)
          || object_key_is_string(&mem.property, "globalThis")
          || object_key_is_literal_string(body, &mem.property, "globalThis")
      }
      _ => false,
    }
  }

  fn expr_is_ident_or_global_this_member(
    body: &Body,
    expr_id: hir_js::ExprId,
    global_this_name: hir_js::NameId,
    target_name: hir_js::NameId,
    target_str: &str,
  ) -> bool {
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return false;
    };
    match &expr.kind {
      ExprKind::Ident(name) => *name == target_name,
      ExprKind::Member(mem) => {
        let obj_is_global_this = expr_is_global_this(body, mem.object, global_this_name);
        obj_is_global_this
          && (object_key_is_ident(&mem.property, target_name)
            || object_key_is_string(&mem.property, target_str)
            || object_key_is_literal_string(body, &mem.property, target_str))
      }
      _ => false,
    }
  }

  fn expr_chain_contains_proto_mutation(
    body: &Body,
    mut id: hir_js::ExprId,
    prototype_name: hir_js::NameId,
    proto_name: hir_js::NameId,
  ) -> bool {
    loop {
      let Some(expr) = body.exprs.get(id.0 as usize) else {
        return false;
      };
      match &expr.kind {
        ExprKind::Member(member) => {
          let key = &member.property;
          if object_key_is_ident(key, prototype_name)
            || object_key_is_ident(key, proto_name)
            || object_key_is_string(key, "prototype")
            || object_key_is_string(key, "__proto__")
            || object_key_is_literal_string(body, key, "prototype")
            || object_key_is_literal_string(body, key, "__proto__")
          {
            return true;
          }
          id = member.object;
        }
        _ => return false,
      }
    }
  }

  fn pat_contains_proto_mutation(
    body: &Body,
    pat: hir_js::PatId,
    prototype_name: hir_js::NameId,
    proto_name: hir_js::NameId,
  ) -> bool {
    let Some(pat) = body.pats.get(pat.0 as usize) else {
      return false;
    };
    match &pat.kind {
      PatKind::AssignTarget(expr) => {
        expr_chain_contains_proto_mutation(body, *expr, prototype_name, proto_name)
      }
      PatKind::Assign { target, .. } => {
        pat_contains_proto_mutation(body, *target, prototype_name, proto_name)
      }
      PatKind::Rest(inner) => pat_contains_proto_mutation(body, **inner, prototype_name, proto_name),
      PatKind::Array(arr) => {
        for elem in &arr.elements {
          let Some(elem) = elem else {
            continue;
          };
          if pat_contains_proto_mutation(body, elem.pat, prototype_name, proto_name) {
            return true;
          }
        }
        arr
          .rest
          .is_some_and(|rest| pat_contains_proto_mutation(body, rest, prototype_name, proto_name))
      }
      PatKind::Object(obj) => {
        for prop in &obj.props {
          if pat_contains_proto_mutation(body, prop.value, prototype_name, proto_name) {
            return true;
          }
        }
        obj
          .rest
          .is_some_and(|rest| pat_contains_proto_mutation(body, rest, prototype_name, proto_name))
      }
      PatKind::Ident(_) => false,
    }
  }

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
  let numeric_indexer_obj_ty = {
    let mut shape = Shape::new();
    shape.indexers.push(Indexer {
      key_type: prim.number,
      value_type: prim.unknown,
      readonly: true,
    });
    let shape_id = store.intern_shape(shape);
    let obj = store.intern_object(ObjectType { shape: shape_id });
    store.intern_type(TypeKind::Object(obj))
  };

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
                  let prop_is_eval =
                    object_key_is_ident(&mem.property, eval_name)
                      || object_key_is_string(&mem.property, "eval")
                      || object_key_is_literal_string(body, &mem.property, "eval");
                  let obj_is_global_this =
                    expr_is_global_this(body, mem.object, global_this_name);
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

            if let ExprKind::Member(member) = &callee.kind {
              let is_call_like =
                (object_key_is_ident(&member.property, call_name)
                  || object_key_is_string(&member.property, "call")
                  || object_key_is_literal_string(body, &member.property, "call"))
                  || (object_key_is_ident(&member.property, apply_name)
                    || object_key_is_string(&member.property, "apply")
                    || object_key_is_literal_string(body, &member.property, "apply"))
                  || (object_key_is_ident(&member.property, bind_name)
                    || object_key_is_string(&member.property, "bind")
                    || object_key_is_literal_string(body, &member.property, "bind"));
              if is_call_like
                && expr_is_ident_or_global_this_member(
                  body,
                  member.object,
                  global_this_name,
                  eval_name,
                  "eval",
                )
              {
                diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                  "`eval` is forbidden when `native_strict` is enabled",
                  Span::new(file, callee_span),
                ));
              }
              if is_call_like
                && expr_is_ident_or_global_this_member(
                  body,
                  member.object,
                  global_this_name,
                  function_name,
                  "Function",
                )
              {
                diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                  "`Function` constructor is forbidden when `native_strict` is enabled",
                  Span::new(file, callee_span),
                ));
              }
              if is_call_like
                && expr_is_builtin_member(
                  body,
                  member.object,
                  global_this_name,
                  proxy_name,
                  "Proxy",
                  revocable_name,
                  "revocable",
                )
              {
                diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                  "`Proxy` is forbidden when `native_strict` is enabled",
                  Span::new(file, callee_span),
                ));
              }

              let is_call_or_apply =
                (object_key_is_ident(&member.property, call_name)
                  || object_key_is_string(&member.property, "call")
                  || object_key_is_literal_string(body, &member.property, "call"))
                  || (object_key_is_ident(&member.property, apply_name)
                    || object_key_is_string(&member.property, "apply")
                    || object_key_is_literal_string(body, &member.property, "apply"));
              if is_call_or_apply
                && (expr_is_builtin_member(
                  body,
                  member.object,
                  global_this_name,
                  object_name,
                  "Object",
                  set_prototype_of_name,
                  "setPrototypeOf",
                ) || expr_is_builtin_member(
                  body,
                  member.object,
                  global_this_name,
                  reflect_name,
                  "Reflect",
                  set_prototype_of_name,
                  "setPrototypeOf",
                ))
              {
                let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                  "prototype mutation is forbidden when `native_strict` is enabled",
                  Span::new(file, span),
                ));
              }

              if is_call_or_apply {
                let prop_is_call =
                  object_key_is_ident(&member.property, call_name)
                    || object_key_is_string(&member.property, "call")
                    || object_key_is_literal_string(body, &member.property, "call");
                let prop_is_apply =
                  object_key_is_ident(&member.property, apply_name)
                    || object_key_is_string(&member.property, "apply")
                    || object_key_is_literal_string(body, &member.property, "apply");

                let obj_is_object_define_property = expr_is_builtin_member(
                  body,
                  member.object,
                  global_this_name,
                  object_name,
                  "Object",
                  define_property_name,
                  "defineProperty",
                );
                let obj_is_reflect_define_property = expr_is_builtin_member(
                  body,
                  member.object,
                  global_this_name,
                  reflect_name,
                  "Reflect",
                  define_property_name,
                  "defineProperty",
                );
                let obj_is_object_define_properties = expr_is_builtin_member(
                  body,
                  member.object,
                  global_this_name,
                  object_name,
                  "Object",
                  define_properties_name,
                  "defineProperties",
                );
                let obj_is_object_assign = expr_is_builtin_member(
                  body,
                  member.object,
                  global_this_name,
                  object_name,
                  "Object",
                  assign_name,
                  "assign",
                );

                let mut mark_proto_mutation = |is_proto_mutation: bool| {
                  if is_proto_mutation {
                    let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                    diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                      "prototype mutation is forbidden when `native_strict` is enabled",
                      Span::new(file, span),
                    ));
                  }
                };

                if prop_is_call {
                  // `.call(thisArg, ...args)`
                  if let Some(target_obj) =
                    call.args.get(1).filter(|arg| !arg.spread).map(|arg| arg.expr)
                  {
                    if obj_is_object_define_property || obj_is_reflect_define_property {
                      let key_arg =
                        call.args.get(2).filter(|arg| !arg.spread).map(|arg| arg.expr);
                      let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                        body,
                        target_obj,
                        prototype_name,
                        proto_name,
                      );
                      if !is_proto_mutation {
                        if let Some(key_arg) = key_arg {
                          if expr_is_const_string(body, key_arg, "prototype")
                            || expr_is_const_string(body, key_arg, "__proto__")
                          {
                            is_proto_mutation = true;
                          }
                        }
                      }
                      mark_proto_mutation(is_proto_mutation);
                    }

                    if obj_is_object_define_properties {
                      if let Some(props_arg) =
                        call.args.get(2).filter(|arg| !arg.spread).map(|arg| arg.expr)
                      {
                        let is_proto_mutation = expr_chain_contains_proto_mutation(
                          body,
                          target_obj,
                          prototype_name,
                          proto_name,
                        ) || expr_is_object_literal_with_proto_key(
                          body,
                          props_arg,
                          prototype_name,
                          proto_name,
                        );
                        mark_proto_mutation(is_proto_mutation);
                      }
                    }

                    if obj_is_object_assign {
                      let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                        body,
                        target_obj,
                        prototype_name,
                        proto_name,
                      );
                      if !is_proto_mutation {
                        for source_arg in call.args.iter().skip(2) {
                          if source_arg.spread {
                            continue;
                          }
                          if expr_is_object_literal_with_proto_key(
                            body,
                            source_arg.expr,
                            prototype_name,
                            proto_name,
                          ) {
                            is_proto_mutation = true;
                            break;
                          }
                        }
                      }
                      mark_proto_mutation(is_proto_mutation);
                    }
                  }
                } else if prop_is_apply {
                  // `.apply(thisArg, argsArray)`
                  if let Some(args_array) =
                    call.args.get(1).filter(|arg| !arg.spread).map(|arg| arg.expr)
                  {
                    if let Some(args_list) = array_literal_exprs(body, args_array) {
                      if let Some(target_obj) = args_list.first().copied() {
                        if obj_is_object_define_property || obj_is_reflect_define_property {
                          let key_arg = args_list.get(1).copied();
                          let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                            body,
                            target_obj,
                            prototype_name,
                            proto_name,
                          );
                          if !is_proto_mutation {
                            if let Some(key_arg) = key_arg {
                              if expr_is_const_string(body, key_arg, "prototype")
                                || expr_is_const_string(body, key_arg, "__proto__")
                              {
                                is_proto_mutation = true;
                              }
                            }
                          }
                          mark_proto_mutation(is_proto_mutation);
                        }

                        if obj_is_object_define_properties {
                          if let Some(props_arg) = args_list.get(1).copied() {
                            let is_proto_mutation = expr_chain_contains_proto_mutation(
                              body,
                              target_obj,
                              prototype_name,
                              proto_name,
                            ) || expr_is_object_literal_with_proto_key(
                              body,
                              props_arg,
                              prototype_name,
                              proto_name,
                            );
                            mark_proto_mutation(is_proto_mutation);
                          }
                        }

                        if obj_is_object_assign {
                          let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                            body,
                            target_obj,
                            prototype_name,
                            proto_name,
                          );
                          if !is_proto_mutation {
                            for source_arg in args_list.iter().skip(1).copied() {
                              if expr_is_object_literal_with_proto_key(
                                body,
                                source_arg,
                                prototype_name,
                                proto_name,
                              ) {
                                is_proto_mutation = true;
                                break;
                              }
                            }
                          }
                          mark_proto_mutation(is_proto_mutation);
                        }
                      }
                    }
                  }
                }
              }
            }

            // `Reflect.apply(eval, ...)` / `Reflect.apply(Function, ...)` etc.
            if let ExprKind::Member(member) = &callee.kind {
              let obj_is_reflect = expr_is_ident_or_global_this_member(
                body,
                member.object,
                global_this_name,
                reflect_name,
                "Reflect",
              );
              let prop_is_apply =
                object_key_is_ident(&member.property, apply_name)
                  || object_key_is_string(&member.property, "apply")
                  || object_key_is_literal_string(body, &member.property, "apply");
              if obj_is_reflect && prop_is_apply {
                if let Some(target_arg) = call.args.first().filter(|arg| !arg.spread).map(|arg| arg.expr) {
                  let target_span = result
                    .expr_spans
                    .get(target_arg.0 as usize)
                    .copied()
                    .or_else(|| body.exprs.get(target_arg.0 as usize).map(|expr| expr.span))
                    .unwrap_or(callee_span);

                  if expr_is_ident_or_global_this_member(
                    body,
                    target_arg,
                    global_this_name,
                    eval_name,
                    "eval",
                  ) {
                    diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                      "`eval` is forbidden when `native_strict` is enabled",
                      Span::new(file, target_span),
                    ));
                  }
                  if expr_is_ident_or_global_this_member(
                    body,
                    target_arg,
                    global_this_name,
                    function_name,
                    "Function",
                  ) {
                    diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                      "`Function` constructor is forbidden when `native_strict` is enabled",
                      Span::new(file, target_span),
                    ));
                  }
                  if expr_is_ident_or_global_this_member(
                    body,
                    target_arg,
                    global_this_name,
                    proxy_name,
                    "Proxy",
                  ) || expr_is_builtin_member(
                    body,
                    target_arg,
                    global_this_name,
                    proxy_name,
                    "Proxy",
                    revocable_name,
                    "revocable",
                  ) {
                    diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                      "`Proxy` is forbidden when `native_strict` is enabled",
                      Span::new(file, target_span),
                    ));
                  }

                  if expr_is_builtin_member(
                    body,
                    target_arg,
                    global_this_name,
                    object_name,
                    "Object",
                    set_prototype_of_name,
                    "setPrototypeOf",
                  ) || expr_is_builtin_member(
                    body,
                    target_arg,
                    global_this_name,
                    reflect_name,
                    "Reflect",
                    set_prototype_of_name,
                    "setPrototypeOf",
                  ) {
                    let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                    diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                      "prototype mutation is forbidden when `native_strict` is enabled",
                      Span::new(file, span),
                    ));
                  }

                  let target_is_object_define_property = expr_is_builtin_member(
                    body,
                    target_arg,
                    global_this_name,
                    object_name,
                    "Object",
                    define_property_name,
                    "defineProperty",
                  );
                  let target_is_reflect_define_property = expr_is_builtin_member(
                    body,
                    target_arg,
                    global_this_name,
                    reflect_name,
                    "Reflect",
                    define_property_name,
                    "defineProperty",
                  );
                  let target_is_object_define_properties = expr_is_builtin_member(
                    body,
                    target_arg,
                    global_this_name,
                    object_name,
                    "Object",
                    define_properties_name,
                    "defineProperties",
                  );
                  let target_is_object_assign = expr_is_builtin_member(
                    body,
                    target_arg,
                    global_this_name,
                    object_name,
                    "Object",
                    assign_name,
                    "assign",
                  );

                  if target_is_object_define_property
                    || target_is_reflect_define_property
                    || target_is_object_define_properties
                    || target_is_object_assign
                  {
                    if let Some(args_list_expr) =
                      call.args.get(2).filter(|arg| !arg.spread).map(|arg| arg.expr)
                    {
                      if let Some(args_list) = array_literal_exprs(body, args_list_expr) {
                        if let Some(target_obj) = args_list.first().copied() {
                          let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                            body,
                            target_obj,
                            prototype_name,
                            proto_name,
                          );

                          if !is_proto_mutation
                            && (target_is_object_define_property || target_is_reflect_define_property)
                          {
                            if let Some(key_arg) = args_list.get(1).copied() {
                              if expr_is_const_string(body, key_arg, "prototype")
                                || expr_is_const_string(body, key_arg, "__proto__")
                              {
                                is_proto_mutation = true;
                              }
                            }
                          }

                          if !is_proto_mutation && target_is_object_define_properties {
                            if let Some(props_arg) = args_list.get(1).copied() {
                              if expr_is_object_literal_with_proto_key(
                                body,
                                props_arg,
                                prototype_name,
                                proto_name,
                              ) {
                                is_proto_mutation = true;
                              }
                            }
                          }
                          if !is_proto_mutation && target_is_object_assign {
                            for source_arg in args_list.iter().skip(1).copied() {
                              if expr_is_object_literal_with_proto_key(
                                body,
                                source_arg,
                                prototype_name,
                                proto_name,
                              ) {
                                is_proto_mutation = true;
                                break;
                              }
                            }
                          }

                          if is_proto_mutation {
                            let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                            diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                              "prototype mutation is forbidden when `native_strict` is enabled",
                              Span::new(file, span),
                            ));
                          }
                        }
                      }
                    }
                  }
                }
              }
            }
          }

          if expr_is_ident_or_global_this_member(
            body,
            call.callee,
            global_this_name,
            function_name,
            "Function",
          ) {
            diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
              "`Function` constructor is forbidden when `native_strict` is enabled",
              Span::new(file, callee_span),
            ));
          }

          if call.is_new
            && expr_is_ident_or_global_this_member(
              body,
              call.callee,
              global_this_name,
              proxy_name,
              "Proxy",
            )
          {
            diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
              "`Proxy` is forbidden when `native_strict` is enabled",
              Span::new(file, callee_span),
            ));
          }

          if let ExprKind::Member(member) = &callee.kind {
            let obj_is_proxy = expr_is_ident_or_global_this_member(
              body,
              member.object,
              global_this_name,
              proxy_name,
              "Proxy",
            );
            let obj_is_object = expr_is_ident_or_global_this_member(
              body,
              member.object,
              global_this_name,
              object_name,
              "Object",
            );
            let obj_is_reflect = expr_is_ident_or_global_this_member(
              body,
              member.object,
              global_this_name,
              reflect_name,
              "Reflect",
            );

            if obj_is_proxy
              && (object_key_is_ident(&member.property, revocable_name)
                || object_key_is_string(&member.property, "revocable")
                || object_key_is_literal_string(body, &member.property, "revocable"))
            {
              diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                "`Proxy` is forbidden when `native_strict` is enabled",
                Span::new(file, callee_span),
              ));
            }

            // `Reflect.construct(Function, ...)` / `Reflect.construct(Proxy, ...)`.
            let prop_is_construct =
              object_key_is_ident(&member.property, construct_name)
                || object_key_is_string(&member.property, "construct")
                || object_key_is_literal_string(body, &member.property, "construct");
            if obj_is_reflect && prop_is_construct {
              if let Some(target_arg) = call.args.first().filter(|arg| !arg.spread).map(|arg| arg.expr) {
                let target_span = result
                  .expr_spans
                  .get(target_arg.0 as usize)
                  .copied()
                  .or_else(|| body.exprs.get(target_arg.0 as usize).map(|expr| expr.span))
                  .unwrap_or(callee_span);
                if expr_is_ident_or_global_this_member(
                  body,
                  target_arg,
                  global_this_name,
                  function_name,
                  "Function",
                ) {
                  diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                    "`Function` constructor is forbidden when `native_strict` is enabled",
                    Span::new(file, target_span),
                  ));
                }
                if expr_is_ident_or_global_this_member(
                  body,
                  target_arg,
                  global_this_name,
                  proxy_name,
                  "Proxy",
                ) {
                  diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                    "`Proxy` is forbidden when `native_strict` is enabled",
                    Span::new(file, target_span),
                  ));
                }
              }
            }

            if (obj_is_object || obj_is_reflect)
              && (object_key_is_ident(&member.property, set_prototype_of_name)
                || object_key_is_string(&member.property, "setPrototypeOf")
                || object_key_is_literal_string(body, &member.property, "setPrototypeOf"))
            {
              let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
              diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                "prototype mutation is forbidden when `native_strict` is enabled",
                Span::new(file, span),
              ));
            }

            if let Some(first_arg) = call.args.first().map(|arg| arg.expr) {
              let is_define_property = object_key_is_ident(&member.property, define_property_name)
                || object_key_is_string(&member.property, "defineProperty")
                || object_key_is_literal_string(body, &member.property, "defineProperty");
              let is_define_properties = object_key_is_ident(&member.property, define_properties_name)
                || object_key_is_string(&member.property, "defineProperties")
                || object_key_is_literal_string(body, &member.property, "defineProperties");
              let is_assign = object_key_is_ident(&member.property, assign_name)
                || object_key_is_string(&member.property, "assign")
                || object_key_is_literal_string(body, &member.property, "assign");

              let is_object_define_property = obj_is_object && is_define_property;
              let is_object_define = obj_is_object && (is_define_property || is_define_properties || is_assign);
              let is_reflect_define_property = obj_is_reflect && is_define_property;
              let is_reflect_define = is_reflect_define_property;

              let mut is_proto_mutation =
                expr_chain_contains_proto_mutation(body, first_arg, prototype_name, proto_name);
              // `Object/Reflect.defineProperty(Foo, "prototype", ...)` is another way to mutate a
              // constructor's prototype after creation. Treat constant `"prototype"` / `"__proto__"`
              // keys as prototype mutation too, even when the first argument isn't already a
              // `.prototype` / `.__proto__` member chain.
              if !is_proto_mutation && (is_object_define_property || is_reflect_define_property) {
                if let Some(key_arg) = call.args.get(1).map(|arg| arg.expr) {
                  if expr_is_const_string(body, key_arg, "prototype")
                    || expr_is_const_string(body, key_arg, "__proto__")
                  {
                    is_proto_mutation = true;
                  }
                }
              }

              // Also cover `Object.defineProperties` / `Object.assign` writing `"prototype"` /
              // `"__proto__"` to an object.
              if !is_proto_mutation && obj_is_object && is_define_properties {
                if let Some(props_arg) = call.args.get(1).map(|arg| arg.expr) {
                  if expr_is_object_literal_with_proto_key(body, props_arg, prototype_name, proto_name)
                  {
                    is_proto_mutation = true;
                  }
                }
              }
              if !is_proto_mutation && obj_is_object && is_assign {
                for source_arg in call.args.iter().skip(1).map(|arg| arg.expr) {
                  if expr_is_object_literal_with_proto_key(body, source_arg, prototype_name, proto_name)
                  {
                    is_proto_mutation = true;
                    break;
                  }
                }
              }

              if (is_object_define || is_reflect_define) && is_proto_mutation {
                let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                  "prototype mutation is forbidden when `native_strict` is enabled",
                  Span::new(file, span),
                ));
              }
            }
          }
        }
      }
      ExprKind::Assignment { target, .. } => {
        if pat_contains_proto_mutation(body, *target, prototype_name, proto_name) {
          let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
          diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
            "prototype mutation is forbidden when `native_strict` is enabled",
            Span::new(file, span),
          ));
        }
      }
      ExprKind::Update { expr: target_expr, .. } => {
        if expr_chain_contains_proto_mutation(body, *target_expr, prototype_name, proto_name) {
          let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
          diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
            "prototype mutation is forbidden when `native_strict` is enabled",
            Span::new(file, span),
          ));
        }
      }
      ExprKind::Unary { op, expr: target_expr } => {
        if *op == hir_js::UnaryOp::Delete
          && expr_chain_contains_proto_mutation(body, *target_expr, prototype_name, proto_name)
        {
          let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
          diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
            "prototype mutation is forbidden when `native_strict` is enabled",
            Span::new(file, span),
          ));
        }
      }
      ExprKind::Object(obj) => {
        for prop in &obj.properties {
          let key = match prop {
            hir_js::ObjectProperty::KeyValue { key, .. } => key,
            hir_js::ObjectProperty::Getter { key, .. } => key,
            hir_js::ObjectProperty::Setter { key, .. } => key,
            hir_js::ObjectProperty::Spread(_) => continue,
          };

          let ObjectKey::Computed(key_expr) = key else {
            continue;
          };
          let Some(key) = body.exprs.get(key_expr.0 as usize) else {
            continue;
          };

          let key_is_const = matches!(
            &key.kind,
            ExprKind::Literal(Literal::String(_) | Literal::Number(_) | Literal::BigInt(_))
          ) || matches!(&key.kind, ExprKind::Template(tpl) if tpl.spans.is_empty());
          if key_is_const {
            continue;
          }

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
          let key_is_const = matches!(
            &key.kind,
            ExprKind::Literal(Literal::String(_) | Literal::Number(_) | Literal::BigInt(_))
          ) || matches!(&key.kind, ExprKind::Template(tpl) if tpl.spans.is_empty());
          if key_is_const {
            continue;
          }

          let obj_ty = result
            .expr_types
            .get(member.object.0 as usize)
            .copied()
            .unwrap_or(prim.unknown);
          let obj_ty = store.canon(obj_ty);
          let obj_has_numeric_indexer = relate.is_assignable(obj_ty, numeric_indexer_obj_ty);
          let key_ty = result
            .expr_types
            .get(key_expr.0 as usize)
            .copied()
            .unwrap_or(prim.unknown);
          let key_is_number = relate.is_assignable(key_ty, prim.number);
          if obj_has_numeric_indexer && key_is_number {
            continue;
          }

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
      ExprKind::Ident(name) => {
        if *name == arguments_name {
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

  if let Some(class) = &body.class {
    for member in &class.members {
      let key = match &member.kind {
        ClassMemberKind::Method { key, .. } | ClassMemberKind::Field { key, .. } => key,
        _ => continue,
      };
      let ClassMemberKey::Computed(key_expr) = key else {
        continue;
      };
      let Some(key) = body.exprs.get(key_expr.0 as usize) else {
        continue;
      };
      let key_is_const = matches!(
        &key.kind,
        ExprKind::Literal(Literal::String(_) | Literal::Number(_) | Literal::BigInt(_))
      ) || matches!(&key.kind, ExprKind::Template(tpl) if tpl.spans.is_empty());
      if key_is_const {
        continue;
      }
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

  for pat in &body.pats {
    match &pat.kind {
      PatKind::Ident(name) => {
        if *name == arguments_name {
          diagnostics.push(codes::NATIVE_STRICT_ARGUMENTS.error(
            "`arguments` is forbidden when `native_strict` is enabled",
            Span::new(file, pat.span),
          ));
        }
      }
      PatKind::Object(obj) => {
        for prop in &obj.props {
          let ObjectKey::Computed(key_expr) = &prop.key else {
            continue;
          };
          let Some(key) = body.exprs.get(key_expr.0 as usize) else {
            continue;
          };
          let key_is_const = matches!(
            &key.kind,
            ExprKind::Literal(Literal::String(_) | Literal::Number(_) | Literal::BigInt(_))
          ) || matches!(&key.kind, ExprKind::Template(tpl) if tpl.spans.is_empty());
          if key_is_const {
            continue;
          }
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
