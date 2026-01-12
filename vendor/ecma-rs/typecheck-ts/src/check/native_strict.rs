use crate::codes;
use crate::BodyCheckResult;
use diagnostics::{Diagnostic, FileId, Span, TextRange};
use hir_js::{
  Body, ClassMemberKey, ClassMemberKind, ExprKind, Literal, NameInterner, ObjectKey, PatKind,
  StmtKind,
};
use std::collections::{HashMap, HashSet};
use types_ts_interned::{
  Indexer, ObjectType, PropKey, RelateCtx, RelateTypeExpander, Shape, TypeId, TypeKind, TypeStore,
};

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
  let constructor_name = name_interner.intern("constructor");

  // Used for type-based detection of "function-like" values when validating
  // `.constructor` access. (Do not confuse with `hir_js::NameId` above.)
  let type_call_name = store.intern_name("call");
  let type_apply_name = store.intern_name("apply");
  let type_bind_name = store.intern_name("bind");
  let type_expander = relate.expander();

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

  fn object_key_is_constructor(
    body: &Body,
    key: &ObjectKey,
    constructor_name: hir_js::NameId,
  ) -> bool {
    object_key_is_ident(key, constructor_name)
      || object_key_is_string(key, "constructor")
      || object_key_is_literal_string(body, key, "constructor")
  }

  fn expr_unwrap_comma(body: &Body, mut expr_id: hir_js::ExprId) -> hir_js::ExprId {
    loop {
      let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
        return expr_id;
      };
      let ExprKind::Binary { op, right, .. } = &expr.kind else {
        return expr_id;
      };
      if *op != hir_js::BinaryOp::Comma {
        return expr_id;
      }
      expr_id = *right;
    }
  }

  fn expr_unwrap_comma_and_alias(
    body: &Body,
    mut expr_id: hir_js::ExprId,
    aliases: &HashMap<hir_js::NameId, hir_js::ExprId>,
  ) -> hir_js::ExprId {
    // Keep this intentionally conservative: follow only simple `const x = <expr>` aliases recorded
    // in `aliases`. This is enough to prevent common "store builtins / prototype objects in locals"
    // bypasses without requiring full scope-aware binding resolution.
    let mut visited = Vec::new();
    loop {
      expr_id = expr_unwrap_comma(body, expr_id);

      // Unwrap TypeScript-only "no-op" wrappers that do not affect runtime behavior. These can
      // otherwise be used to hide banned call targets (e.g. `(eval as typeof eval)("1")`,
      // `(eval!)("1")`).
      loop {
        let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
          return expr_id;
        };
        match &expr.kind {
          ExprKind::TypeAssertion { expr: inner, .. }
          | ExprKind::Instantiation { expr: inner, .. }
          | ExprKind::NonNull { expr: inner }
          | ExprKind::Satisfies { expr: inner, .. } => {
            expr_id = *inner;
          }
          _ => break,
        }
      }

      let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
        return expr_id;
      };
      let ExprKind::Ident(name) = &expr.kind else {
        return expr_id;
      };
      if visited.contains(name) {
        return expr_id;
      }
      let Some(next) = aliases.get(name).copied() else {
        return expr_id;
      };
      visited.push(*name);
      expr_id = next;
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
    aliases: &HashMap<hir_js::NameId, hir_js::ExprId>,
    global_this_name: hir_js::NameId,
    base_name: hir_js::NameId,
    base_str: &str,
    member_name: hir_js::NameId,
    member_str: &str,
  ) -> bool {
    let expr_id = expr_unwrap_comma_and_alias(body, expr_id, aliases);
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return false;
    };
    let ExprKind::Member(mem) = &expr.kind else {
      return false;
    };
    expr_is_ident_or_global_this_member(
      body,
      mem.object,
      aliases,
      global_this_name,
      base_name,
      base_str,
    )
      && (object_key_is_ident(&mem.property, member_name)
        || object_key_is_string(&mem.property, member_str)
        || object_key_is_literal_string(body, &mem.property, member_str))
  }

  fn expr_is_function_prototype_member(
    body: &Body,
    expr_id: hir_js::ExprId,
    aliases: &HashMap<hir_js::NameId, hir_js::ExprId>,
    global_this_name: hir_js::NameId,
    function_name: hir_js::NameId,
    prototype_name: hir_js::NameId,
    member_name: hir_js::NameId,
    member_str: &str,
  ) -> bool {
    let expr_id = expr_unwrap_comma_and_alias(body, expr_id, aliases);
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return false;
    };
    let ExprKind::Member(mem) = &expr.kind else {
      return false;
    };
    expr_is_builtin_member(
      body,
      mem.object,
      aliases,
      global_this_name,
      function_name,
      "Function",
      prototype_name,
      "prototype",
    ) && (object_key_is_ident(&mem.property, member_name)
      || object_key_is_string(&mem.property, member_str)
      || object_key_is_literal_string(body, &mem.property, member_str))
  }

  fn expr_is_global_this(
    body: &Body,
    expr_id: hir_js::ExprId,
    aliases: &HashMap<hir_js::NameId, hir_js::ExprId>,
    global_this_name: hir_js::NameId,
  ) -> bool {
    let expr_id = expr_unwrap_comma_and_alias(body, expr_id, aliases);
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return false;
    };
    match &expr.kind {
      ExprKind::Ident(name) => *name == global_this_name,
      ExprKind::Member(mem) => {
        if !expr_is_global_this(body, mem.object, aliases, global_this_name) {
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
    aliases: &HashMap<hir_js::NameId, hir_js::ExprId>,
    global_this_name: hir_js::NameId,
    target_name: hir_js::NameId,
    target_str: &str,
  ) -> bool {
    let expr_id = expr_unwrap_comma_and_alias(body, expr_id, aliases);
    let Some(expr) = body.exprs.get(expr_id.0 as usize) else {
      return false;
    };
    match &expr.kind {
      ExprKind::Ident(name) => *name == target_name,
      ExprKind::Member(mem) => {
        let obj_is_global_this = expr_is_global_this(body, mem.object, aliases, global_this_name);
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
    aliases: &HashMap<hir_js::NameId, hir_js::ExprId>,
  ) -> bool {
    loop {
      id = expr_unwrap_comma_and_alias(body, id, aliases);
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
    aliases: &HashMap<hir_js::NameId, hir_js::ExprId>,
  ) -> bool {
    let Some(pat) = body.pats.get(pat.0 as usize) else {
      return false;
    };
    match &pat.kind {
      PatKind::AssignTarget(expr) => {
        expr_chain_contains_proto_mutation(body, *expr, prototype_name, proto_name, aliases)
      }
      PatKind::Assign { target, .. } => {
        pat_contains_proto_mutation(body, *target, prototype_name, proto_name, aliases)
      }
      PatKind::Rest(inner) => pat_contains_proto_mutation(body, **inner, prototype_name, proto_name, aliases),
      PatKind::Array(arr) => {
        for elem in &arr.elements {
          let Some(elem) = elem else {
            continue;
          };
          if pat_contains_proto_mutation(body, elem.pat, prototype_name, proto_name, aliases) {
            return true;
          }
        }
        arr
          .rest
          .is_some_and(|rest| pat_contains_proto_mutation(body, rest, prototype_name, proto_name, aliases))
      }
      PatKind::Object(obj) => {
        for prop in &obj.props {
          if pat_contains_proto_mutation(body, prop.value, prototype_name, proto_name, aliases) {
            return true;
          }
        }
        obj
          .rest
          .is_some_and(|rest| pat_contains_proto_mutation(body, rest, prototype_name, proto_name, aliases))
      }
      PatKind::Ident(_) => false,
    }
  }

  fn type_is_function_like(
    store: &TypeStore,
    ty: TypeId,
    expander: Option<&dyn RelateTypeExpander>,
    call_name: types_ts_interned::NameId,
    apply_name: types_ts_interned::NameId,
    bind_name: types_ts_interned::NameId,
    cache: &mut HashMap<TypeId, bool>,
    visiting: &mut HashSet<TypeId>,
  ) -> bool {
    let ty = store.canon(ty);
    if let Some(hit) = cache.get(&ty) {
      return *hit;
    }

    // Break cycles conservatively (treat as not function-like on this path).
    if !visiting.insert(ty) {
      return false;
    }

    let result = match store.type_kind(ty) {
      TypeKind::Any => true,
      TypeKind::Callable { .. } => true,
      TypeKind::Object(obj) => {
        let shape = store.shape(store.object(obj).shape);
        if !shape.call_signatures.is_empty() || !shape.construct_signatures.is_empty() {
          true
        } else {
          // `lib.es5.d.ts` models `Function` as a non-callable interface but it is still a
          // function object at runtime (and is used as the type of `Object.prototype.constructor`).
          // Treat values with the standard `Function` methods as function-like to avoid missing
          // `.constructor.constructor` bypasses (e.g. `({}).constructor.constructor(...)`).
          let mut has_call = false;
          let mut has_apply = false;
          let mut has_bind = false;
          for prop in &shape.properties {
            if let PropKey::String(name) = prop.key {
              if name == call_name {
                has_call = true;
              } else if name == apply_name {
                has_apply = true;
              } else if name == bind_name {
                has_bind = true;
              }
            }
          }
          has_call && has_apply && has_bind
        }
      }
      TypeKind::Union(members) | TypeKind::Intersection(members) => members
        .into_iter()
        .any(|member| type_is_function_like(
          store,
          member,
          expander,
          call_name,
          apply_name,
          bind_name,
          cache,
          visiting,
        )),
      TypeKind::Intrinsic { ty, .. } => type_is_function_like(
        store,
        ty,
        expander,
        call_name,
        apply_name,
        bind_name,
        cache,
        visiting,
      ),
      TypeKind::Ref { def, args } => expander
        .and_then(|expander| expander.expand_ref(store, def, &args))
        .is_some_and(|expanded| type_is_function_like(
          store,
          expanded,
          expander,
          call_name,
          apply_name,
          bind_name,
          cache,
          visiting,
        )),
      _ => false,
    };

    visiting.remove(&ty);
    cache.insert(ty, result);
    result
  }

  fn expr_is_function_constructor_via_constructor_access(
    body: &Body,
    expr_id: hir_js::ExprId,
    aliases: &HashMap<hir_js::NameId, hir_js::ExprId>,
    result: &BodyCheckResult,
    store: &TypeStore,
    expander: Option<&dyn RelateTypeExpander>,
    constructor_name: hir_js::NameId,
    type_call_name: types_ts_interned::NameId,
    type_apply_name: types_ts_interned::NameId,
    type_bind_name: types_ts_interned::NameId,
    cache: &mut HashMap<TypeId, bool>,
  ) -> Option<TextRange> {
    let expr_id = expr_unwrap_comma_and_alias(body, expr_id, aliases);
    let expr = body.exprs.get(expr_id.0 as usize)?;
    let ExprKind::Member(mem) = &expr.kind else {
      return None;
    };
    if !object_key_is_constructor(body, &mem.property, constructor_name) {
      return None;
    }

    // `.constructor` yields the `Function`/`AsyncFunction`/`GeneratorFunction` constructors when
    // accessed on a function object. This can be used to bypass name-based detection of `Function`.
    let receiver_id = expr_unwrap_comma_and_alias(body, mem.object, aliases);
    let receiver_expr_is_function_like = body
      .exprs
      .get(receiver_id.0 as usize)
      .is_some_and(|receiver_expr| match &receiver_expr.kind {
        // Any function/class value has `.constructor === Function` (or a derived constructor in the
        // async/generator cases).
        ExprKind::FunctionExpr { .. } | ExprKind::ClassExpr { .. } => true,
        // `.constructor` on *any* object yields its constructor function, which is itself a
        // function object whose `.constructor` is `Function`. This handles chained forms like:
        // `({}).constructor.constructor.call(...)`.
        ExprKind::Member(mem)
          if object_key_is_constructor(body, &mem.property, constructor_name) =>
        {
          true
        }
        _ => false,
      });
    if !receiver_expr_is_function_like {
      let receiver_ty = result
        .expr_types
        .get(receiver_id.0 as usize)
        .copied()
        .unwrap_or(store.primitive_ids().unknown);
      let mut visiting = HashSet::new();
      if !type_is_function_like(
        store,
        receiver_ty,
        expander,
        type_call_name,
        type_apply_name,
        type_bind_name,
        cache,
        &mut visiting,
      ) {
        return None;
      }
    }

    Some(
      result
        .expr_spans
        .get(expr_id.0 as usize)
        .copied()
        .unwrap_or(expr.span),
    )
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

  // Track simple `const` aliases (`const x = expr;`) so `native_strict` bans cannot be bypassed via
  // indirection (e.g. `const dp = Object.defineProperty; dp(...)`, `const p = Foo.prototype; p.x++`).
  //
  // This is name-based (not scope-aware), which matches how this validator already treats global
  // builtins by name. The rules are intentionally conservative: we only record aliases that are
  // syntactically obvious and immutable (`const`).
  let mut const_aliases: HashMap<hir_js::NameId, hir_js::ExprId> = HashMap::new();
  for stmt in &body.stmts {
    let StmtKind::Var(var) = &stmt.kind else {
      continue;
    };
    if var.kind != hir_js::VarDeclKind::Const {
      continue;
    }
    for decl in &var.declarators {
      let Some(init) = decl.init else {
        continue;
      };
      let Some(pat) = body.pats.get(decl.pat.0 as usize) else {
        continue;
      };
      match &pat.kind {
        PatKind::Ident(name) => {
          const_aliases.insert(*name, init);
        }
        PatKind::Assign { target, .. } => {
          let Some(target) = body.pats.get(target.0 as usize) else {
            continue;
          };
          if let PatKind::Ident(name) = &target.kind {
            const_aliases.insert(*name, init);
          }
        }
        _ => {}
      }
    }
  }

  let mut function_like_cache: HashMap<TypeId, bool> = HashMap::new();

  for (idx, expr) in body.exprs.iter().enumerate() {
    match &expr.kind {
      ExprKind::Call(call) => {
        // For diagnostics, we want to point at the *callsite* callee. For checks, we additionally
        // follow simple `const` aliases so users cannot bypass bans by storing dangerous values in
        // locals.
        let callee_span_id = expr_unwrap_comma(body, call.callee);
        let callee_check_id = expr_unwrap_comma_and_alias(body, call.callee, &const_aliases);
        let callee = body.exprs.get(callee_check_id.0 as usize);
        if let Some(callee) = callee {
          let callee_span = result
            .expr_spans
            .get(callee_span_id.0 as usize)
            .copied()
            .or_else(|| body.exprs.get(callee_span_id.0 as usize).map(|expr| expr.span))
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
                    expr_is_global_this(body, mem.object, &const_aliases, global_this_name);
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
              let prop_is_call =
                object_key_is_ident(&member.property, call_name)
                  || object_key_is_string(&member.property, "call")
                  || object_key_is_literal_string(body, &member.property, "call");
              let prop_is_apply =
                object_key_is_ident(&member.property, apply_name)
                  || object_key_is_string(&member.property, "apply")
                  || object_key_is_literal_string(body, &member.property, "apply");
              let prop_is_bind =
                object_key_is_ident(&member.property, bind_name)
                  || object_key_is_string(&member.property, "bind")
                  || object_key_is_literal_string(body, &member.property, "bind");
              let is_call_like = prop_is_call || prop_is_apply || prop_is_bind;
              let is_call_or_apply = prop_is_call || prop_is_apply;
              if is_call_like
                && expr_is_ident_or_global_this_member(
                  body,
                  member.object,
                  &const_aliases,
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
                  &const_aliases,
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
              if is_call_like {
                if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                  body,
                  member.object,
                  &const_aliases,
                  result,
                  store,
                  type_expander,
                  constructor_name,
                  type_call_name,
                  type_apply_name,
                  type_bind_name,
                  &mut function_like_cache,
                ) {
                  diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                    "`Function` constructor is forbidden when `native_strict` is enabled",
                    Span::new(file, constructor_span),
                  ));
                }
              }
              if is_call_like
                && expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
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

              if is_call_like
                && (expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  object_name,
                  "Object",
                  set_prototype_of_name,
                  "setPrototypeOf",
                ) || expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
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

              // `Reflect.apply.bind(Reflect, target, ...)` / `Reflect.construct.bind(...)` can be
              // used to indirectly call forbidden targets.
              if prop_is_bind {
                let obj_is_reflect_apply = expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  reflect_name,
                  "Reflect",
                  apply_name,
                  "apply",
                );
                let obj_is_reflect_construct = expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  reflect_name,
                  "Reflect",
                  construct_name,
                  "construct",
                );
                if obj_is_reflect_apply || obj_is_reflect_construct {
                  if let Some(target_arg) =
                    call.args.get(1).filter(|arg| !arg.spread).map(|arg| arg.expr)
                  {
                    let target_span = result
                      .expr_spans
                      .get(target_arg.0 as usize)
                      .copied()
                      .or_else(|| body.exprs.get(target_arg.0 as usize).map(|expr| expr.span))
                      .unwrap_or(callee_span);

                    if expr_is_ident_or_global_this_member(
                      body,
                      target_arg,
                      &const_aliases,
                      global_this_name,
                      function_name,
                      "Function",
                    ) {
                      diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                        "`Function` constructor is forbidden when `native_strict` is enabled",
                        Span::new(file, target_span),
                      ));
                    }
                    if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                      body,
                      target_arg,
                      &const_aliases,
                      result,
                      store,
                      type_expander,
                      constructor_name,
                      type_call_name,
                      type_apply_name,
                      type_bind_name,
                      &mut function_like_cache,
                    ) {
                      diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                        "`Function` constructor is forbidden when `native_strict` is enabled",
                        Span::new(file, constructor_span),
                      ));
                    }
                    if obj_is_reflect_apply {
                      if expr_is_ident_or_global_this_member(
                        body,
                        target_arg,
                        &const_aliases,
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
                        &const_aliases,
                        global_this_name,
                        proxy_name,
                        "Proxy",
                      ) || expr_is_builtin_member(
                        body,
                        target_arg,
                        &const_aliases,
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
                    } else if obj_is_reflect_construct {
                      if expr_is_ident_or_global_this_member(
                        body,
                        target_arg,
                        &const_aliases,
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

                    if obj_is_reflect_apply {
                      let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                      if expr_is_builtin_member(
                        body,
                        target_arg,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        set_prototype_of_name,
                        "setPrototypeOf",
                      ) || expr_is_builtin_member(
                        body,
                        target_arg,
                        &const_aliases,
                        global_this_name,
                        reflect_name,
                        "Reflect",
                        set_prototype_of_name,
                        "setPrototypeOf",
                      ) {
                        diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                          "prototype mutation is forbidden when `native_strict` is enabled",
                          Span::new(file, span),
                        ));
                      }

                      let target_is_object_define_property = expr_is_builtin_member(
                        body,
                        target_arg,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        define_property_name,
                        "defineProperty",
                      );
                      let target_is_reflect_define_property = expr_is_builtin_member(
                        body,
                        target_arg,
                        &const_aliases,
                        global_this_name,
                        reflect_name,
                        "Reflect",
                        define_property_name,
                        "defineProperty",
                      );
                      let target_is_object_define_properties = expr_is_builtin_member(
                        body,
                        target_arg,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        define_properties_name,
                        "defineProperties",
                      );
                      let target_is_object_assign = expr_is_builtin_member(
                        body,
                        target_arg,
                        &const_aliases,
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
                          call.args.get(3).filter(|arg| !arg.spread).map(|arg| arg.expr)
                        {
                          if let Some(args_list) = array_literal_exprs(body, args_list_expr) {
                            if let Some(target_obj) = args_list.first().copied() {
                              let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                                body,
                                target_obj,
                                prototype_name,
                                proto_name,
                                &const_aliases,
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

              // `Function.prototype.call.call(...)` and friends are a common way to indirectly
              // invoke (or bind) a function (bypassing direct `eval.call(...)` / `eval.bind(...)`
              // checks etc). Also covers the equivalent `Function.call.*` / `Function.apply.*` /
              // `Function.bind.*` forms.
              if is_call_like {
                let obj_is_call_invoker = expr_is_function_prototype_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  function_name,
                  prototype_name,
                  call_name,
                  "call",
                ) || expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  function_name,
                  "Function",
                  call_name,
                  "call",
                );
                let obj_is_apply_invoker = expr_is_function_prototype_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  function_name,
                  prototype_name,
                  apply_name,
                  "apply",
                ) || expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  function_name,
                  "Function",
                  apply_name,
                  "apply",
                );
                let obj_is_bind_invoker = expr_is_function_prototype_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  function_name,
                  prototype_name,
                  bind_name,
                  "bind",
                ) || expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  function_name,
                  "Function",
                  bind_name,
                  "bind",
                );
                if obj_is_call_invoker || obj_is_apply_invoker || obj_is_bind_invoker {
                  if let Some(target_arg) =
                    call.args.first().filter(|arg| !arg.spread).map(|arg| arg.expr)
                  {
                    let target_span = result
                      .expr_spans
                      .get(target_arg.0 as usize)
                      .copied()
                      .or_else(|| body.exprs.get(target_arg.0 as usize).map(|expr| expr.span))
                      .unwrap_or(callee_span);

                    if expr_is_ident_or_global_this_member(
                      body,
                      target_arg,
                      &const_aliases,
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
                      &const_aliases,
                      global_this_name,
                      function_name,
                      "Function",
                    ) {
                      diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                        "`Function` constructor is forbidden when `native_strict` is enabled",
                        Span::new(file, target_span),
                      ));
                    }
                    if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                      body,
                      target_arg,
                      &const_aliases,
                      result,
                      store,
                      type_expander,
                      constructor_name,
                      type_call_name,
                      type_apply_name,
                      type_bind_name,
                      &mut function_like_cache,
                    ) {
                      diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                        "`Function` constructor is forbidden when `native_strict` is enabled",
                        Span::new(file, constructor_span),
                      ));
                    }
                    if expr_is_ident_or_global_this_member(
                      body,
                      target_arg,
                      &const_aliases,
                      global_this_name,
                      proxy_name,
                      "Proxy",
                    ) || expr_is_builtin_member(
                      body,
                      target_arg,
                      &const_aliases,
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
                      &const_aliases,
                      global_this_name,
                      object_name,
                      "Object",
                      set_prototype_of_name,
                      "setPrototypeOf",
                    ) || expr_is_builtin_member(
                      body,
                      target_arg,
                      &const_aliases,
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

                    // For prototype mutation helpers where the ban depends on arguments, attempt to
                    // extract the argument list when it's statically knowable.
                    let mut args_for_target: Option<Vec<hir_js::ExprId>> = None;
                    if obj_is_call_invoker {
                      if prop_is_call {
                        // `Function.prototype.call.call(target, thisArg, ...args)`
                        let mut out = Vec::new();
                        for arg in call.args.iter().skip(2) {
                          if arg.spread {
                            out.clear();
                            break;
                          }
                          out.push(arg.expr);
                        }
                        if !out.is_empty() || call.args.len() == 2 {
                          args_for_target = Some(out);
                        }
                      } else if prop_is_apply {
                        // `Function.prototype.call.apply(target, [thisArg, ...args])`
                        if let Some(args_array) =
                          call.args.get(1).filter(|arg| !arg.spread).map(|arg| arg.expr)
                        {
                          if let Some(mut out) = array_literal_exprs(body, args_array) {
                            if !out.is_empty() {
                              out.remove(0);
                            }
                            args_for_target = Some(out);
                          }
                        }
                      } else if prop_is_bind && !call.args.iter().any(|arg| arg.spread) {
                        // `Function.prototype.call.bind(target, thisArg, ...args)`
                        args_for_target = Some(call.args.iter().skip(2).map(|arg| arg.expr).collect());
                      }
                    } else if obj_is_apply_invoker {
                      if prop_is_call {
                        // `Function.prototype.apply.call(target, thisArg, argsArray)`
                        if let Some(args_array) =
                          call.args.get(2).filter(|arg| !arg.spread).map(|arg| arg.expr)
                        {
                          if let Some(out) = array_literal_exprs(body, args_array) {
                            args_for_target = Some(out);
                          }
                        }
                      } else if prop_is_apply {
                        // `Function.prototype.apply.apply(target, [thisArg, argsArray])`
                        if let Some(args_array) =
                          call.args.get(1).filter(|arg| !arg.spread).map(|arg| arg.expr)
                        {
                          if let Some(outer_args) = array_literal_exprs(body, args_array) {
                            if let Some(inner_array) = outer_args.get(1).copied() {
                              if let Some(out) = array_literal_exprs(body, inner_array) {
                                args_for_target = Some(out);
                              }
                            }
                          }
                        }
                      } else if prop_is_bind && !call.args.iter().any(|arg| arg.spread) {
                        // `Function.prototype.apply.bind(target, thisArg, argsArray)`
                        if let Some(args_array) = call.args.get(2).map(|arg| arg.expr) {
                          if let Some(out) = array_literal_exprs(body, args_array) {
                            args_for_target = Some(out);
                          }
                        }
                      }
                    } else if obj_is_bind_invoker {
                      if prop_is_call {
                        // `Function.prototype.bind.call(target, thisArg, ...args)`
                        let mut out = Vec::new();
                        for arg in call.args.iter().skip(2) {
                          if arg.spread {
                            out.clear();
                            break;
                          }
                          out.push(arg.expr);
                        }
                        if !out.is_empty() || call.args.len() == 2 {
                          args_for_target = Some(out);
                        }
                      } else if prop_is_apply {
                        // `Function.prototype.bind.apply(target, [thisArg, ...args])`
                        if let Some(args_array) =
                          call.args.get(1).filter(|arg| !arg.spread).map(|arg| arg.expr)
                        {
                          if let Some(mut out) = array_literal_exprs(body, args_array) {
                            if !out.is_empty() {
                              out.remove(0);
                            }
                            args_for_target = Some(out);
                          }
                        }
                      } else if prop_is_bind && !call.args.iter().any(|arg| arg.spread) {
                        // `Function.prototype.bind.bind(target, thisArg, ...args)`
                        args_for_target = Some(call.args.iter().skip(2).map(|arg| arg.expr).collect());
                      }
                    }

                    if let Some(args_for_target) = args_for_target {
                      let mut args_for_target = args_for_target;

                      let mut target_is_object_define_property = expr_is_builtin_member(
                        body,
                        target_arg,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        define_property_name,
                        "defineProperty",
                      );
                      let mut target_is_reflect_define_property = expr_is_builtin_member(
                        body,
                        target_arg,
                        &const_aliases,
                        global_this_name,
                        reflect_name,
                        "Reflect",
                        define_property_name,
                        "defineProperty",
                      );
                      let mut target_is_object_define_properties = expr_is_builtin_member(
                        body,
                        target_arg,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        define_properties_name,
                        "defineProperties",
                      );
                      let mut target_is_object_assign = expr_is_builtin_member(
                        body,
                        target_arg,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        assign_name,
                        "assign",
                      );

                      // `Function.prototype.call.call(Object.defineProperty.bind(...), ...)`
                      // and friends: if the target is a bound builtin, evaluate the effective
                      // arguments (`bound_args + call_args`) and apply the same prototype-mutation
                      // heuristics.
                      if !target_is_object_define_property
                        && !target_is_reflect_define_property
                        && !target_is_object_define_properties
                        && !target_is_object_assign
                      {
                        if let Some(target_expr) = body.exprs.get(target_arg.0 as usize) {
                          if let ExprKind::Call(bound_call) = &target_expr.kind {
                            if !bound_call.is_new && !bound_call.args.iter().any(|arg| arg.spread) {
                              if let Some(bound_callee) =
                                body.exprs.get(bound_call.callee.0 as usize)
                              {
                                if let ExprKind::Member(bound_member) = &bound_callee.kind {
                                  let prop_is_bind = object_key_is_ident(&bound_member.property, bind_name)
                                    || object_key_is_string(&bound_member.property, "bind")
                                    || object_key_is_literal_string(body, &bound_member.property, "bind");
                                  if prop_is_bind {
                                    let bound_is_object_define_property = expr_is_builtin_member(
                                      body,
                                      bound_member.object,
                                      &const_aliases,
                                      global_this_name,
                                      object_name,
                                      "Object",
                                      define_property_name,
                                      "defineProperty",
                                    );
                                    let bound_is_reflect_define_property = expr_is_builtin_member(
                                      body,
                                      bound_member.object,
                                      &const_aliases,
                                      global_this_name,
                                      reflect_name,
                                      "Reflect",
                                      define_property_name,
                                      "defineProperty",
                                    );
                                    let bound_is_object_define_properties = expr_is_builtin_member(
                                      body,
                                      bound_member.object,
                                      &const_aliases,
                                      global_this_name,
                                      object_name,
                                      "Object",
                                      define_properties_name,
                                      "defineProperties",
                                    );
                                    let bound_is_object_assign = expr_is_builtin_member(
                                      body,
                                      bound_member.object,
                                      &const_aliases,
                                      global_this_name,
                                      object_name,
                                      "Object",
                                      assign_name,
                                      "assign",
                                    );
                                    if bound_is_object_define_property
                                      || bound_is_reflect_define_property
                                      || bound_is_object_define_properties
                                      || bound_is_object_assign
                                    {
                                      target_is_object_define_property = bound_is_object_define_property;
                                      target_is_reflect_define_property = bound_is_reflect_define_property;
                                      target_is_object_define_properties = bound_is_object_define_properties;
                                      target_is_object_assign = bound_is_object_assign;

                                      let mut prefix = Vec::new();
                                      for arg in bound_call.args.iter().skip(1) {
                                        // checked above: no spreads
                                        prefix.push(arg.expr);
                                      }
                                      if !prefix.is_empty() {
                                        let mut combined =
                                          Vec::with_capacity(prefix.len() + args_for_target.len());
                                        combined.extend(prefix);
                                        combined.extend(args_for_target);
                                        args_for_target = combined;
                                      }
                                    }
                                  }
                                }
                              }
                            }
                          }
                        }
                      }

                      let mut is_proto_mutation = false;
                      if target_is_object_define_property || target_is_reflect_define_property {
                        if let Some(target_obj) = args_for_target.first().copied() {
                          is_proto_mutation = expr_chain_contains_proto_mutation(
                            body,
                            target_obj,
                            prototype_name,
                            proto_name,
                            &const_aliases,
                          );
                          if !is_proto_mutation {
                            if let Some(key_arg) = args_for_target.get(1).copied() {
                              if expr_is_const_string(body, key_arg, "prototype")
                                || expr_is_const_string(body, key_arg, "__proto__")
                              {
                                is_proto_mutation = true;
                              }
                            }
                          }
                        }
                      }
                      if !is_proto_mutation && target_is_object_define_properties {
                        if let (Some(target_obj), Some(props_arg)) =
                          (args_for_target.first().copied(), args_for_target.get(1).copied())
                        {
                          is_proto_mutation = expr_chain_contains_proto_mutation(
                            body,
                            target_obj,
                            prototype_name,
                            proto_name,
                            &const_aliases,
                          ) || expr_is_object_literal_with_proto_key(
                            body,
                            props_arg,
                            prototype_name,
                            proto_name,
                          );
                        }
                      }
                      if !is_proto_mutation && target_is_object_assign {
                        if let Some(target_obj) = args_for_target.first().copied() {
                          is_proto_mutation = expr_chain_contains_proto_mutation(
                            body,
                            target_obj,
                            prototype_name,
                            proto_name,
                            &const_aliases,
                          );
                          if !is_proto_mutation {
                            for source_arg in args_for_target.iter().skip(1).copied() {
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
                        }
                      }

                      if is_proto_mutation
                        && (target_is_object_define_property
                          || target_is_reflect_define_property
                          || target_is_object_define_properties
                          || target_is_object_assign)
                      {
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

              // `Reflect.apply.call(...)` / `Reflect.apply.apply(...)` and `Reflect.construct.*`
              // can be used to indirectly call banned targets.
              if is_call_or_apply {
                let obj_is_reflect_apply = expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  reflect_name,
                  "Reflect",
                  apply_name,
                  "apply",
                );
                let obj_is_reflect_construct = expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  reflect_name,
                  "Reflect",
                  construct_name,
                  "construct",
                );
                if obj_is_reflect_apply || obj_is_reflect_construct {
                  let reflect_args = if prop_is_call {
                    // `.call(thisArg, ...args)`
                    let mut out = Vec::new();
                    for arg in call.args.iter().skip(1) {
                      if arg.spread {
                        out.clear();
                        break;
                      }
                      out.push(arg.expr);
                    }
                    Some(out)
                  } else if prop_is_apply {
                    // `.apply(thisArg, argsArray)`
                    call
                      .args
                      .get(1)
                      .filter(|arg| !arg.spread)
                      .map(|arg| arg.expr)
                      .and_then(|args_array| array_literal_exprs(body, args_array))
                  } else {
                    None
                  };

                  if let Some(reflect_args) = reflect_args {
                    if obj_is_reflect_apply {
                      if let Some(target_arg) = reflect_args.first().copied() {
                        let target_span = result
                          .expr_spans
                          .get(target_arg.0 as usize)
                          .copied()
                          .or_else(|| body.exprs.get(target_arg.0 as usize).map(|expr| expr.span))
                          .unwrap_or(callee_span);

                        if expr_is_ident_or_global_this_member(
                          body,
                          target_arg,
                          &const_aliases,
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
                          &const_aliases,
                          global_this_name,
                          function_name,
                          "Function",
                        ) {
                          diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                            "`Function` constructor is forbidden when `native_strict` is enabled",
                            Span::new(file, target_span),
                          ));
                        }
                        if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                          body,
                          target_arg,
                          &const_aliases,
                          result,
                          store,
                          type_expander,
                          constructor_name,
                          type_call_name,
                          type_apply_name,
                          type_bind_name,
                          &mut function_like_cache,
                        ) {
                          diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                            "`Function` constructor is forbidden when `native_strict` is enabled",
                            Span::new(file, constructor_span),
                          ));
                        }
                        if expr_is_ident_or_global_this_member(
                          body,
                          target_arg,
                          &const_aliases,
                          global_this_name,
                          proxy_name,
                          "Proxy",
                        ) || expr_is_builtin_member(
                          body,
                          target_arg,
                          &const_aliases,
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

                        // `Reflect.apply(Function.prototype.{call,apply,bind}, target, [...])` via
                        // `Reflect.apply.call` / `Reflect.apply.apply` can be used to indirectly
                        // invoke (or bind) a function.
                        let target_is_call_invoker = expr_is_function_prototype_member(
                          body,
                          target_arg,
                          &const_aliases,
                          global_this_name,
                          function_name,
                          prototype_name,
                          call_name,
                          "call",
                        ) || expr_is_builtin_member(
                          body,
                          target_arg,
                          &const_aliases,
                          global_this_name,
                          function_name,
                          "Function",
                          call_name,
                          "call",
                        );
                        let target_is_apply_invoker = expr_is_function_prototype_member(
                          body,
                          target_arg,
                          &const_aliases,
                          global_this_name,
                          function_name,
                          prototype_name,
                          apply_name,
                          "apply",
                        ) || expr_is_builtin_member(
                          body,
                          target_arg,
                          &const_aliases,
                          global_this_name,
                          function_name,
                          "Function",
                          apply_name,
                          "apply",
                        );
                        let target_is_bind_invoker = expr_is_function_prototype_member(
                          body,
                          target_arg,
                          &const_aliases,
                          global_this_name,
                          function_name,
                          prototype_name,
                          bind_name,
                          "bind",
                        ) || expr_is_builtin_member(
                          body,
                          target_arg,
                          &const_aliases,
                          global_this_name,
                          function_name,
                          "Function",
                          bind_name,
                          "bind",
                        );
                        if target_is_call_invoker || target_is_apply_invoker || target_is_bind_invoker {
                          if let Some(called_target) = reflect_args.get(1).copied() {
                            let called_target_span = result
                              .expr_spans
                              .get(called_target.0 as usize)
                              .copied()
                              .or_else(|| body.exprs.get(called_target.0 as usize).map(|expr| expr.span))
                              .unwrap_or(target_span);
 
                            if expr_is_ident_or_global_this_member(
                              body,
                              called_target,
                              &const_aliases,
                              global_this_name,
                              eval_name,
                              "eval",
                            ) {
                              diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                                "`eval` is forbidden when `native_strict` is enabled",
                                Span::new(file, called_target_span),
                              ));
                            }
                            if expr_is_ident_or_global_this_member(
                              body,
                              called_target,
                              &const_aliases,
                              global_this_name,
                              function_name,
                              "Function",
                            ) {
                              diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                                "`Function` constructor is forbidden when `native_strict` is enabled",
                                Span::new(file, called_target_span),
                              ));
                            }
                            if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                              body,
                              called_target,
                              &const_aliases,
                              result,
                              store,
                              type_expander,
                              constructor_name,
                              type_call_name,
                              type_apply_name,
                              type_bind_name,
                              &mut function_like_cache,
                            ) {
                              diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                                "`Function` constructor is forbidden when `native_strict` is enabled",
                                Span::new(file, constructor_span),
                              ));
                            }
                            if expr_is_ident_or_global_this_member(
                              body,
                              called_target,
                              &const_aliases,
                              global_this_name,
                              proxy_name,
                              "Proxy",
                            ) || expr_is_builtin_member(
                              body,
                              called_target,
                              &const_aliases,
                              global_this_name,
                              proxy_name,
                              "Proxy",
                              revocable_name,
                              "revocable",
                            ) {
                              diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                                "`Proxy` is forbidden when `native_strict` is enabled",
                                Span::new(file, called_target_span),
                              ));
                            }
 
                            if expr_is_builtin_member(
                              body,
                              called_target,
                              &const_aliases,
                              global_this_name,
                              object_name,
                              "Object",
                              set_prototype_of_name,
                              "setPrototypeOf",
                            ) || expr_is_builtin_member(
                              body,
                              called_target,
                              &const_aliases,
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
 
                            let called_target_is_object_define_property = expr_is_builtin_member(
                              body,
                              called_target,
                              &const_aliases,
                              global_this_name,
                              object_name,
                              "Object",
                              define_property_name,
                              "defineProperty",
                            );
                            let called_target_is_reflect_define_property = expr_is_builtin_member(
                              body,
                              called_target,
                              &const_aliases,
                              global_this_name,
                              reflect_name,
                              "Reflect",
                              define_property_name,
                              "defineProperty",
                            );
                            let called_target_is_object_define_properties = expr_is_builtin_member(
                              body,
                              called_target,
                              &const_aliases,
                              global_this_name,
                              object_name,
                              "Object",
                              define_properties_name,
                              "defineProperties",
                            );
                            let called_target_is_object_assign = expr_is_builtin_member(
                              body,
                              called_target,
                              &const_aliases,
                              global_this_name,
                              object_name,
                              "Object",
                              assign_name,
                              "assign",
                            );
                            if called_target_is_object_define_property
                              || called_target_is_reflect_define_property
                              || called_target_is_object_define_properties
                              || called_target_is_object_assign
                            {
                              if let Some(args_list_expr) = reflect_args.get(2).copied() {
                                let args_for_target =
                                  if target_is_call_invoker || target_is_bind_invoker {
                                    array_literal_exprs(body, args_list_expr).map(|mut args_list| {
                                      if !args_list.is_empty() {
                                        args_list.remove(0);
                                      }
                                      args_list
                                    })
                                  } else if target_is_apply_invoker {
                                    array_literal_exprs(body, args_list_expr)
                                      .and_then(|args_list| args_list.get(1).copied())
                                      .and_then(|inner| array_literal_exprs(body, inner))
                                  } else {
                                    None
                                  };
                                if let Some(args_list) = args_for_target {
                                  if let Some(target_obj) = args_list.first().copied() {
                                    let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                                      body,
                                      target_obj,
                                      prototype_name,
                                      proto_name,
                                      &const_aliases,
                                    );
 
                                    if !is_proto_mutation
                                      && (called_target_is_object_define_property
                                        || called_target_is_reflect_define_property)
                                    {
                                      if let Some(key_arg) = args_list.get(1).copied() {
                                        if expr_is_const_string(body, key_arg, "prototype")
                                          || expr_is_const_string(body, key_arg, "__proto__")
                                        {
                                          is_proto_mutation = true;
                                        }
                                      }
                                    }
 
                                    if !is_proto_mutation && called_target_is_object_define_properties {
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
 
                                    if !is_proto_mutation && called_target_is_object_assign {
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
                                      let span =
                                        result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                                      diagnostics.push(
                                        codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                                          "prototype mutation is forbidden when `native_strict` is enabled",
                                          Span::new(file, span),
                                        ),
                                      );
                                    }
                                  }
                                }
                              }
                            }
                          }
                        }

                        if expr_is_builtin_member(
                          body,
                          target_arg,
                          &const_aliases,
                          global_this_name,
                          object_name,
                          "Object",
                          set_prototype_of_name,
                          "setPrototypeOf",
                        ) || expr_is_builtin_member(
                          body,
                          target_arg,
                          &const_aliases,
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
                          &const_aliases,
                          global_this_name,
                          object_name,
                          "Object",
                          define_property_name,
                          "defineProperty",
                        );
                        let target_is_reflect_define_property = expr_is_builtin_member(
                          body,
                          target_arg,
                          &const_aliases,
                          global_this_name,
                          reflect_name,
                          "Reflect",
                          define_property_name,
                          "defineProperty",
                        );
                        let target_is_object_define_properties = expr_is_builtin_member(
                          body,
                          target_arg,
                          &const_aliases,
                          global_this_name,
                          object_name,
                          "Object",
                          define_properties_name,
                          "defineProperties",
                        );
                        let target_is_object_assign = expr_is_builtin_member(
                          body,
                          target_arg,
                          &const_aliases,
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
                          if let Some(args_list_expr) = reflect_args.get(2).copied() {
                            if let Some(args_list) = array_literal_exprs(body, args_list_expr) {
                              if let Some(target_obj) = args_list.first().copied() {
                                let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                                  body,
                                  target_obj,
                                  prototype_name,
                                  proto_name,
                                  &const_aliases,
                                );

                                if !is_proto_mutation
                                  && (target_is_object_define_property
                                    || target_is_reflect_define_property)
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
                    } else if obj_is_reflect_construct {
                      if let Some(target_arg) = reflect_args.first().copied() {
                        let target_span = result
                          .expr_spans
                          .get(target_arg.0 as usize)
                          .copied()
                          .or_else(|| body.exprs.get(target_arg.0 as usize).map(|expr| expr.span))
                          .unwrap_or(callee_span);

                        if expr_is_ident_or_global_this_member(
                          body,
                          target_arg,
                          &const_aliases,
                          global_this_name,
                          function_name,
                          "Function",
                        ) {
                          diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                            "`Function` constructor is forbidden when `native_strict` is enabled",
                            Span::new(file, target_span),
                          ));
                        }
                        if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                          body,
                          target_arg,
                          &const_aliases,
                          result,
                          store,
                          type_expander,
                          constructor_name,
                          type_call_name,
                          type_apply_name,
                          type_bind_name,
                          &mut function_like_cache,
                        ) {
                          diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                            "`Function` constructor is forbidden when `native_strict` is enabled",
                            Span::new(file, constructor_span),
                          ));
                        }
                        if expr_is_ident_or_global_this_member(
                          body,
                          target_arg,
                          &const_aliases,
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
                  }
                }
              }

              if is_call_like {
                let obj_is_object_define_property = expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  object_name,
                  "Object",
                  define_property_name,
                  "defineProperty",
                );
                let obj_is_reflect_define_property = expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  reflect_name,
                  "Reflect",
                  define_property_name,
                  "defineProperty",
                );
                let obj_is_object_define_properties = expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
                  global_this_name,
                  object_name,
                  "Object",
                  define_properties_name,
                  "defineProperties",
                );
                let obj_is_object_assign = expr_is_builtin_member(
                  body,
                  member.object,
                  &const_aliases,
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

                if prop_is_call || prop_is_bind {
                  // `.call(thisArg, ...args)` / `.bind(thisArg, ...args)`
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
                        &const_aliases,
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
                          &const_aliases,
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
                        &const_aliases,
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
                            &const_aliases,
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
                              &const_aliases,
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
                            &const_aliases,
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

            if let ExprKind::Member(member) = &callee.kind {
              let prop_is_call =
                object_key_is_ident(&member.property, call_name)
                  || object_key_is_string(&member.property, "call")
                  || object_key_is_literal_string(body, &member.property, "call");
              let prop_is_apply =
                object_key_is_ident(&member.property, apply_name)
                  || object_key_is_string(&member.property, "apply")
                  || object_key_is_literal_string(body, &member.property, "apply");
              if prop_is_call || prop_is_apply {
                if let Some(bound_expr) = body.exprs.get(member.object.0 as usize) {
                  if let ExprKind::Call(bind_call) = &bound_expr.kind {
                    if !bind_call.is_new && !bind_call.args.iter().any(|arg| arg.spread)
                    {
                      if let Some(bind_callee) = body.exprs.get(bind_call.callee.0 as usize) {
                        if let ExprKind::Member(bind_member) = &bind_callee.kind {
                          let prop_is_bind =
                            object_key_is_ident(&bind_member.property, bind_name)
                              || object_key_is_string(&bind_member.property, "bind")
                              || object_key_is_literal_string(body, &bind_member.property, "bind");
                          if prop_is_bind {
                            let is_object_define_property = expr_is_builtin_member(
                              body,
                              bind_member.object,
                              &const_aliases,
                              global_this_name,
                              object_name,
                              "Object",
                              define_property_name,
                              "defineProperty",
                            );
                            let is_reflect_define_property = expr_is_builtin_member(
                              body,
                              bind_member.object,
                              &const_aliases,
                              global_this_name,
                              reflect_name,
                              "Reflect",
                              define_property_name,
                              "defineProperty",
                            );
                            let is_object_define_properties = expr_is_builtin_member(
                              body,
                              bind_member.object,
                              &const_aliases,
                              global_this_name,
                              object_name,
                              "Object",
                              define_properties_name,
                              "defineProperties",
                            );
                            let is_object_assign = expr_is_builtin_member(
                              body,
                              bind_member.object,
                              &const_aliases,
                              global_this_name,
                              object_name,
                              "Object",
                              assign_name,
                              "assign",
                            );
                            let is_define_property =
                              is_object_define_property || is_reflect_define_property;
                            if is_define_property || is_object_define_properties || is_object_assign
                            {
                              let bound_arity = bind_call.args.len().saturating_sub(1);
                              let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);

                              let mut report_if = |is_proto_mutation: bool| {
                                if is_proto_mutation {
                                  diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                                    "prototype mutation is forbidden when `native_strict` is enabled",
                                    Span::new(file, span),
                                  ));
                                }
                              };

                              if prop_is_call {
                                let effective_arg = |i: usize| -> Option<hir_js::ExprId> {
                                  if i < bound_arity {
                                    bind_call
                                      .args
                                      .get(i + 1)
                                      .filter(|arg| !arg.spread)
                                      .map(|arg| arg.expr)
                                  } else {
                                    call
                                      .args
                                      .get((i - bound_arity) + 1)
                                      .filter(|arg| !arg.spread)
                                      .map(|arg| arg.expr)
                                  }
                                };

                                if let Some(first_arg) = effective_arg(0) {
                                  if is_define_property {
                                    let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                                      body,
                                      first_arg,
                                      prototype_name,
                                      proto_name,
                                      &const_aliases,
                                    );
                                    if !is_proto_mutation {
                                      if let Some(key_arg) = effective_arg(1) {
                                        if expr_is_const_string(body, key_arg, "prototype")
                                          || expr_is_const_string(body, key_arg, "__proto__")
                                        {
                                          is_proto_mutation = true;
                                        }
                                      }
                                    }
                                    report_if(is_proto_mutation);
                                  }
                                  if is_object_define_properties {
                                    if let Some(props_arg) = effective_arg(1) {
                                      let is_proto_mutation = expr_chain_contains_proto_mutation(
                                        body,
                                        first_arg,
                                        prototype_name,
                                        proto_name,
                                        &const_aliases,
                                      ) || expr_is_object_literal_with_proto_key(
                                        body,
                                        props_arg,
                                        prototype_name,
                                        proto_name,
                                      );
                                      report_if(is_proto_mutation);
                                    }
                                  }
                                  if is_object_assign {
                                    let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                                      body,
                                      first_arg,
                                      prototype_name,
                                      proto_name,
                                      &const_aliases,
                                    );
                                    if !is_proto_mutation {
                                      let outer_sources = call
                                        .args
                                        .iter()
                                        .skip(if bound_arity == 0 { 2 } else { 1 });
                                      for source_arg in
                                        bind_call.args.iter().skip(2).chain(outer_sources)
                                      {
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
                                    report_if(is_proto_mutation);
                                  }
                                }
                              } else if prop_is_apply {
                                if let Some(args_array) =
                                  call.args.get(1).filter(|arg| !arg.spread).map(|arg| arg.expr)
                                {
                                  if let Some(args_list) = array_literal_exprs(body, args_array) {
                                    let effective_arg = |i: usize| -> Option<hir_js::ExprId> {
                                      if i < bound_arity {
                                        bind_call
                                          .args
                                          .get(i + 1)
                                          .filter(|arg| !arg.spread)
                                          .map(|arg| arg.expr)
                                      } else {
                                        args_list.get(i - bound_arity).copied()
                                      }
                                    };

                                      if let Some(first_arg) = effective_arg(0) {
                                        if is_define_property {
                                          let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                                            body,
                                            first_arg,
                                            prototype_name,
                                            proto_name,
                                            &const_aliases,
                                          );
                                          if !is_proto_mutation {
                                            if let Some(key_arg) = effective_arg(1) {
                                              if expr_is_const_string(body, key_arg, "prototype")
                                                || expr_is_const_string(body, key_arg, "__proto__")
                                            {
                                              is_proto_mutation = true;
                                            }
                                          }
                                        }
                                        report_if(is_proto_mutation);
                                      }
                                      if is_object_define_properties {
                                        if let Some(props_arg) = effective_arg(1) {
                                          let is_proto_mutation = expr_chain_contains_proto_mutation(
                                            body,
                                            first_arg,
                                            prototype_name,
                                            proto_name,
                                            &const_aliases,
                                          ) || expr_is_object_literal_with_proto_key(
                                            body,
                                            props_arg,
                                            prototype_name,
                                            proto_name,
                                          );
                                          report_if(is_proto_mutation);
                                        }
                                      }
                                      if is_object_assign {
                                        let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                                          body,
                                          first_arg,
                                          prototype_name,
                                          proto_name,
                                          &const_aliases,
                                        );
                                        if !is_proto_mutation {
                                          let call_sources =
                                            args_list.iter().skip(if bound_arity == 0 { 1 } else { 0 });
                                          for source_arg in
                                            bind_call.args.iter().skip(2).map(|arg| arg.expr).chain(call_sources.copied())
                                          {
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
                                        report_if(is_proto_mutation);
                                      }
                                    }
                                  }
                                }
                              }
                            }
                          }
                        }
                      }
                    }
                  }
                }
              }
            }

            if let ExprKind::Call(bound_call) = &callee.kind {
              if !bound_call.is_new && !bound_call.args.iter().any(|arg| arg.spread)
              {
                if let Some(bound_callee) = body.exprs.get(bound_call.callee.0 as usize) {
                  if let ExprKind::Member(bound_member) = &bound_callee.kind {
                    let prop_is_bind =
                      object_key_is_ident(&bound_member.property, bind_name)
                        || object_key_is_string(&bound_member.property, "bind")
                        || object_key_is_literal_string(body, &bound_member.property, "bind");
                    if prop_is_bind {
                      let is_object_define_property = expr_is_builtin_member(
                        body,
                        bound_member.object,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        define_property_name,
                        "defineProperty",
                      );
                      let is_reflect_define_property = expr_is_builtin_member(
                        body,
                        bound_member.object,
                        &const_aliases,
                        global_this_name,
                        reflect_name,
                        "Reflect",
                        define_property_name,
                        "defineProperty",
                      );
                      let is_object_define_properties = expr_is_builtin_member(
                        body,
                        bound_member.object,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        define_properties_name,
                        "defineProperties",
                      );
                      let is_object_assign = expr_is_builtin_member(
                        body,
                        bound_member.object,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        assign_name,
                        "assign",
                      );

                      let is_define_property = is_object_define_property || is_reflect_define_property;
                      if is_define_property || is_object_define_properties || is_object_assign {
                        let bound_arity = bound_call.args.len().saturating_sub(1);
                        let effective_arg = |i: usize| -> Option<hir_js::ExprId> {
                          if i < bound_arity {
                            bound_call
                              .args
                              .get(i + 1)
                              .filter(|arg| !arg.spread)
                              .map(|arg| arg.expr)
                          } else {
                            call
                              .args
                              .get(i - bound_arity)
                              .filter(|arg| !arg.spread)
                              .map(|arg| arg.expr)
                          }
                        };

                        if let Some(first_arg) = effective_arg(0) {
                          if is_define_property {
                            let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                              body,
                              first_arg,
                              prototype_name,
                              proto_name,
                              &const_aliases,
                            );
                            if !is_proto_mutation {
                              if let Some(key_arg) = effective_arg(1) {
                                if expr_is_const_string(body, key_arg, "prototype")
                                  || expr_is_const_string(body, key_arg, "__proto__")
                                {
                                  is_proto_mutation = true;
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

                          if is_object_define_properties {
                            if let Some(props_arg) = effective_arg(1) {
                              let is_proto_mutation = expr_chain_contains_proto_mutation(
                                body,
                                first_arg,
                                prototype_name,
                                proto_name,
                                &const_aliases,
                              ) || expr_is_object_literal_with_proto_key(
                                body,
                                props_arg,
                                prototype_name,
                                proto_name,
                              );
                              if is_proto_mutation {
                                let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                                diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                                  "prototype mutation is forbidden when `native_strict` is enabled",
                                  Span::new(file, span),
                                ));
                              }
                            }
                          }

                          if is_object_assign {
                            let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                              body,
                              first_arg,
                              prototype_name,
                              proto_name,
                              &const_aliases,
                            );
                            if !is_proto_mutation {
                              let outer_sources = call
                                .args
                                .iter()
                                .skip(if bound_arity == 0 { 1 } else { 0 });
                              for source_arg in bound_call.args.iter().skip(2).chain(outer_sources) {
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

            // `Reflect.apply(eval, ...)` / `Reflect.apply(Function, ...)` etc.
            if let ExprKind::Member(member) = &callee.kind {
              let obj_is_reflect = expr_is_ident_or_global_this_member(
                body,
                member.object,
                &const_aliases,
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
                    &const_aliases,
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
                    &const_aliases,
                    global_this_name,
                    function_name,
                    "Function",
                  ) {
                    diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                      "`Function` constructor is forbidden when `native_strict` is enabled",
                      Span::new(file, target_span),
                    ));
                  }
                  if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                    body,
                    target_arg,
                    &const_aliases,
                    result,
                    store,
                    type_expander,
                    constructor_name,
                    type_call_name,
                    type_apply_name,
                    type_bind_name,
                    &mut function_like_cache,
                  ) {
                    diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                      "`Function` constructor is forbidden when `native_strict` is enabled",
                      Span::new(file, constructor_span),
                    ));
                  }
                  if expr_is_ident_or_global_this_member(
                    body,
                    target_arg,
                    &const_aliases,
                    global_this_name,
                    proxy_name,
                    "Proxy",
                  ) || expr_is_builtin_member(
                    body,
                    target_arg,
                    &const_aliases,
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

                  // `Reflect.apply(Function.prototype.call, target, [thisArg, ...args])` and
                  // `Reflect.apply(Function.prototype.apply, target, [thisArg, argsArray])` can be
                  // used to indirectly invoke a function.
                  let target_is_call_invoker = expr_is_function_prototype_member(
                    body,
                    target_arg,
                    &const_aliases,
                    global_this_name,
                    function_name,
                    prototype_name,
                    call_name,
                    "call",
                  ) || expr_is_builtin_member(
                    body,
                    target_arg,
                    &const_aliases,
                    global_this_name,
                    function_name,
                    "Function",
                    call_name,
                    "call",
                  );
                  let target_is_apply_invoker = expr_is_function_prototype_member(
                    body,
                    target_arg,
                    &const_aliases,
                    global_this_name,
                    function_name,
                    prototype_name,
                    apply_name,
                    "apply",
                  ) || expr_is_builtin_member(
                    body,
                    target_arg,
                    &const_aliases,
                    global_this_name,
                    function_name,
                    "Function",
                    apply_name,
                    "apply",
                  );
                  let target_is_bind_invoker = expr_is_function_prototype_member(
                    body,
                    target_arg,
                    &const_aliases,
                    global_this_name,
                    function_name,
                    prototype_name,
                    bind_name,
                    "bind",
                  ) || expr_is_builtin_member(
                    body,
                    target_arg,
                    &const_aliases,
                    global_this_name,
                    function_name,
                    "Function",
                    bind_name,
                    "bind",
                  );
                  if target_is_call_invoker || target_is_apply_invoker || target_is_bind_invoker {
                    if let Some(called_target) =
                      call.args.get(1).filter(|arg| !arg.spread).map(|arg| arg.expr)
                    {
                      let called_target_span = result
                        .expr_spans
                        .get(called_target.0 as usize)
                        .copied()
                        .or_else(|| body.exprs.get(called_target.0 as usize).map(|expr| expr.span))
                        .unwrap_or(target_span);

                      if expr_is_ident_or_global_this_member(
                        body,
                        called_target,
                        &const_aliases,
                        global_this_name,
                        eval_name,
                        "eval",
                      ) {
                        diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                          "`eval` is forbidden when `native_strict` is enabled",
                          Span::new(file, called_target_span),
                        ));
                      }
                      if expr_is_ident_or_global_this_member(
                        body,
                        called_target,
                        &const_aliases,
                        global_this_name,
                        function_name,
                        "Function",
                      ) {
                        diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                          "`Function` constructor is forbidden when `native_strict` is enabled",
                          Span::new(file, called_target_span),
                        ));
                      }
                      if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                        body,
                        called_target,
                        &const_aliases,
                        result,
                        store,
                        type_expander,
                        constructor_name,
                        type_call_name,
                        type_apply_name,
                        type_bind_name,
                        &mut function_like_cache,
                      ) {
                        diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                          "`Function` constructor is forbidden when `native_strict` is enabled",
                          Span::new(file, constructor_span),
                        ));
                      }
                      if expr_is_ident_or_global_this_member(
                        body,
                        called_target,
                        &const_aliases,
                        global_this_name,
                        proxy_name,
                        "Proxy",
                      ) || expr_is_builtin_member(
                        body,
                        called_target,
                        &const_aliases,
                        global_this_name,
                        proxy_name,
                        "Proxy",
                        revocable_name,
                        "revocable",
                      ) {
                        diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                          "`Proxy` is forbidden when `native_strict` is enabled",
                          Span::new(file, called_target_span),
                        ));
                      }

                      if expr_is_builtin_member(
                        body,
                        called_target,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        set_prototype_of_name,
                        "setPrototypeOf",
                      ) || expr_is_builtin_member(
                        body,
                        called_target,
                        &const_aliases,
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

                      let mut called_target_is_object_define_property = expr_is_builtin_member(
                        body,
                        called_target,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        define_property_name,
                        "defineProperty",
                      );
                      let mut called_target_is_reflect_define_property = expr_is_builtin_member(
                        body,
                        called_target,
                        &const_aliases,
                        global_this_name,
                        reflect_name,
                        "Reflect",
                        define_property_name,
                        "defineProperty",
                      );
                      let mut called_target_is_object_define_properties = expr_is_builtin_member(
                        body,
                        called_target,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        define_properties_name,
                        "defineProperties",
                      );
                      let mut called_target_is_object_assign = expr_is_builtin_member(
                        body,
                        called_target,
                        &const_aliases,
                        global_this_name,
                        object_name,
                        "Object",
                        assign_name,
                        "assign",
                      );

                      let mut bound_prefix: Vec<hir_js::ExprId> = Vec::new();
                      if !called_target_is_object_define_property
                        && !called_target_is_reflect_define_property
                        && !called_target_is_object_define_properties
                        && !called_target_is_object_assign
                      {
                        if let Some(called_expr) = body.exprs.get(called_target.0 as usize) {
                          if let ExprKind::Call(bound_call) = &called_expr.kind {
                            if !bound_call.is_new
                              && !bound_call.args.iter().any(|arg| arg.spread)
                            {
                              if let Some(bound_callee) =
                                body.exprs.get(bound_call.callee.0 as usize)
                              {
                                if let ExprKind::Member(bound_member) = &bound_callee.kind {
                                  let prop_is_bind =
                                    object_key_is_ident(&bound_member.property, bind_name)
                                      || object_key_is_string(&bound_member.property, "bind")
                                      || object_key_is_literal_string(body, &bound_member.property, "bind");
                                  if prop_is_bind {
                                    let bound_is_object_define_property = expr_is_builtin_member(
                                      body,
                                      bound_member.object,
                                      &const_aliases,
                                      global_this_name,
                                      object_name,
                                      "Object",
                                      define_property_name,
                                      "defineProperty",
                                    );
                                    let bound_is_reflect_define_property = expr_is_builtin_member(
                                      body,
                                      bound_member.object,
                                      &const_aliases,
                                      global_this_name,
                                      reflect_name,
                                      "Reflect",
                                      define_property_name,
                                      "defineProperty",
                                    );
                                    let bound_is_object_define_properties = expr_is_builtin_member(
                                      body,
                                      bound_member.object,
                                      &const_aliases,
                                      global_this_name,
                                      object_name,
                                      "Object",
                                      define_properties_name,
                                      "defineProperties",
                                    );
                                    let bound_is_object_assign = expr_is_builtin_member(
                                      body,
                                      bound_member.object,
                                      &const_aliases,
                                      global_this_name,
                                      object_name,
                                      "Object",
                                      assign_name,
                                      "assign",
                                    );
                                    if bound_is_object_define_property
                                      || bound_is_reflect_define_property
                                      || bound_is_object_define_properties
                                      || bound_is_object_assign
                                    {
                                      called_target_is_object_define_property =
                                        bound_is_object_define_property;
                                      called_target_is_reflect_define_property =
                                        bound_is_reflect_define_property;
                                      called_target_is_object_define_properties =
                                        bound_is_object_define_properties;
                                      called_target_is_object_assign = bound_is_object_assign;
                                      for arg in bound_call.args.iter().skip(1) {
                                        bound_prefix.push(arg.expr);
                                      }
                                    }
                                  }
                                }
                              }
                            }
                          }
                        }
                      }

                      if called_target_is_object_define_property
                        || called_target_is_reflect_define_property
                        || called_target_is_object_define_properties
                        || called_target_is_object_assign
                      {
                        if let Some(args_list_expr) =
                          call.args.get(2).filter(|arg| !arg.spread).map(|arg| arg.expr)
                        {
                          let args_for_target = if target_is_call_invoker || target_is_bind_invoker {
                            array_literal_exprs(body, args_list_expr).map(|mut args_list| {
                              if !args_list.is_empty() {
                                args_list.remove(0);
                              }
                              args_list
                            })
                          } else if target_is_apply_invoker {
                            array_literal_exprs(body, args_list_expr)
                              .and_then(|args_list| args_list.get(1).copied())
                              .and_then(|inner| array_literal_exprs(body, inner))
                          } else {
                            None
                          };
                          if let Some(mut args_list) = args_for_target {
                            if !bound_prefix.is_empty() {
                              let mut combined =
                                Vec::with_capacity(bound_prefix.len() + args_list.len());
                              combined.extend(bound_prefix.iter().copied());
                              combined.extend(args_list);
                              args_list = combined;
                            }
                            if let Some(target_obj) = args_list.first().copied() {
                              let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                                body,
                                target_obj,
                                prototype_name,
                                proto_name,
                                &const_aliases,
                              );

                              if !is_proto_mutation
                                && (called_target_is_object_define_property
                                  || called_target_is_reflect_define_property)
                              {
                                if let Some(key_arg) = args_list.get(1).copied() {
                                  if expr_is_const_string(body, key_arg, "prototype")
                                    || expr_is_const_string(body, key_arg, "__proto__")
                                  {
                                    is_proto_mutation = true;
                                  }
                                }
                              }

                              if !is_proto_mutation && called_target_is_object_define_properties {
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

                              if !is_proto_mutation && called_target_is_object_assign {
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

                  if expr_is_builtin_member(
                    body,
                    target_arg,
                    &const_aliases,
                    global_this_name,
                    object_name,
                    "Object",
                    set_prototype_of_name,
                    "setPrototypeOf",
                  ) || expr_is_builtin_member(
                    body,
                    target_arg,
                    &const_aliases,
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
                    &const_aliases,
                    global_this_name,
                    object_name,
                    "Object",
                    define_property_name,
                    "defineProperty",
                  );
                  let target_is_reflect_define_property = expr_is_builtin_member(
                    body,
                    target_arg,
                    &const_aliases,
                    global_this_name,
                    reflect_name,
                    "Reflect",
                    define_property_name,
                    "defineProperty",
                  );
                  let target_is_object_define_properties = expr_is_builtin_member(
                    body,
                    target_arg,
                    &const_aliases,
                    global_this_name,
                    object_name,
                    "Object",
                    define_properties_name,
                    "defineProperties",
                  );
                  let target_is_object_assign = expr_is_builtin_member(
                    body,
                    target_arg,
                    &const_aliases,
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
                            &const_aliases,
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

                  if let Some(target_expr) = body.exprs.get(target_arg.0 as usize) {
                    if let ExprKind::Call(bound_call) = &target_expr.kind {
                      if !bound_call.is_new && !bound_call.args.iter().any(|arg| arg.spread)
                      {
                        if let Some(bound_callee) = body.exprs.get(bound_call.callee.0 as usize) {
                          if let ExprKind::Member(bound_member) = &bound_callee.kind {
                            let prop_is_bind =
                              object_key_is_ident(&bound_member.property, bind_name)
                                || object_key_is_string(&bound_member.property, "bind")
                                || object_key_is_literal_string(body, &bound_member.property, "bind");
                            if prop_is_bind {
                              let is_object_define_property = expr_is_builtin_member(
                                body,
                                bound_member.object,
                                &const_aliases,
                                global_this_name,
                                object_name,
                                "Object",
                                define_property_name,
                                "defineProperty",
                              );
                              let is_reflect_define_property = expr_is_builtin_member(
                                body,
                                bound_member.object,
                                &const_aliases,
                                global_this_name,
                                reflect_name,
                                "Reflect",
                                define_property_name,
                                "defineProperty",
                              );
                              let is_object_define_properties = expr_is_builtin_member(
                                body,
                                bound_member.object,
                                &const_aliases,
                                global_this_name,
                                object_name,
                                "Object",
                                define_properties_name,
                                "defineProperties",
                              );
                              let is_object_assign = expr_is_builtin_member(
                                body,
                                bound_member.object,
                                &const_aliases,
                                global_this_name,
                                object_name,
                                "Object",
                                assign_name,
                                "assign",
                              );
                              let is_define_property =
                                is_object_define_property || is_reflect_define_property;
                              if is_define_property || is_object_define_properties || is_object_assign
                              {
                                if let Some(args_list_expr) =
                                  call.args.get(2).filter(|arg| !arg.spread).map(|arg| arg.expr)
                                {
                                  if let Some(args_list) = array_literal_exprs(body, args_list_expr) {
                                    let bound_arity = bound_call.args.len().saturating_sub(1);
                                    let effective_arg = |i: usize| -> Option<hir_js::ExprId> {
                                      if i < bound_arity {
                                        bound_call
                                          .args
                                          .get(i + 1)
                                          .filter(|arg| !arg.spread)
                                          .map(|arg| arg.expr)
                                      } else {
                                        args_list.get(i - bound_arity).copied()
                                      }
                                    };

                                    if let Some(first_arg) = effective_arg(0) {
                                      let mut report_if = |is_proto_mutation: bool| {
                                        if is_proto_mutation {
                                          let span = result
                                            .expr_spans
                                            .get(idx)
                                            .copied()
                                            .unwrap_or(expr.span);
                                          diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                                            "prototype mutation is forbidden when `native_strict` is enabled",
                                            Span::new(file, span),
                                          ));
                                        }
                                      };

                                      if is_define_property {
                                        let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                                          body,
                                          first_arg,
                                          prototype_name,
                                          proto_name,
                                          &const_aliases,
                                        );
                                        if !is_proto_mutation {
                                          if let Some(key_arg) = effective_arg(1) {
                                            if expr_is_const_string(body, key_arg, "prototype")
                                              || expr_is_const_string(body, key_arg, "__proto__")
                                            {
                                              is_proto_mutation = true;
                                            }
                                          }
                                        }
                                        report_if(is_proto_mutation);
                                      }

                                      if is_object_define_properties {
                                        if let Some(props_arg) = effective_arg(1) {
                                          let is_proto_mutation = expr_chain_contains_proto_mutation(
                                            body,
                                            first_arg,
                                            prototype_name,
                                            proto_name,
                                            &const_aliases,
                                          ) || expr_is_object_literal_with_proto_key(
                                            body,
                                            props_arg,
                                            prototype_name,
                                            proto_name,
                                          );
                                          report_if(is_proto_mutation);
                                        }
                                      }

                                      if is_object_assign {
                                        let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                                          body,
                                          first_arg,
                                          prototype_name,
                                          proto_name,
                                          &const_aliases,
                                        );
                                        if !is_proto_mutation {
                                          let call_sources = args_list.iter().skip(if bound_arity == 0 { 1 } else { 0 });
                                          for source_arg in bound_call
                                            .args
                                            .iter()
                                            .skip(2)
                                            .map(|arg| arg.expr)
                                            .chain(call_sources.copied())
                                          {
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
                                        report_if(is_proto_mutation);
                                      }
                                    }
                                  }
                                }
                              }
                            }
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
            &const_aliases,
            global_this_name,
            function_name,
            "Function",
          ) {
            diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
              "`Function` constructor is forbidden when `native_strict` is enabled",
              Span::new(file, callee_span),
            ));
          }
          if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
            body,
            call.callee,
            &const_aliases,
            result,
            store,
            type_expander,
            constructor_name,
            type_call_name,
            type_apply_name,
            type_bind_name,
            &mut function_like_cache,
          ) {
            diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
              "`Function` constructor is forbidden when `native_strict` is enabled",
              Span::new(file, constructor_span),
            ));
          }

          if call.is_new
            && expr_is_ident_or_global_this_member(
              body,
              call.callee,
              &const_aliases,
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
              &const_aliases,
              global_this_name,
              proxy_name,
              "Proxy",
            );
            let obj_is_object = expr_is_ident_or_global_this_member(
              body,
              member.object,
              &const_aliases,
              global_this_name,
              object_name,
              "Object",
            );
            let obj_is_reflect = expr_is_ident_or_global_this_member(
              body,
              member.object,
              &const_aliases,
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
                  &const_aliases,
                  global_this_name,
                  function_name,
                  "Function",
                ) {
                  diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                    "`Function` constructor is forbidden when `native_strict` is enabled",
                    Span::new(file, target_span),
                  ));
                }
                if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                  body,
                  target_arg,
                  &const_aliases,
                  result,
                  store,
                  type_expander,
                  constructor_name,
                  type_call_name,
                  type_apply_name,
                  type_bind_name,
                  &mut function_like_cache,
                ) {
                  diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                    "`Function` constructor is forbidden when `native_strict` is enabled",
                    Span::new(file, constructor_span),
                  ));
                }
                if expr_is_ident_or_global_this_member(
                  body,
                  target_arg,
                  &const_aliases,
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
                expr_chain_contains_proto_mutation(body, first_arg, prototype_name, proto_name, &const_aliases);
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
        if pat_contains_proto_mutation(body, *target, prototype_name, proto_name, &const_aliases) {
          let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
          diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
            "prototype mutation is forbidden when `native_strict` is enabled",
            Span::new(file, span),
          ));
        }
      }
      ExprKind::Update { expr: target_expr, .. } => {
        if expr_chain_contains_proto_mutation(body, *target_expr, prototype_name, proto_name, &const_aliases) {
          let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
          diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
            "prototype mutation is forbidden when `native_strict` is enabled",
            Span::new(file, span),
          ));
        }
      }
      ExprKind::Unary { op, expr: target_expr } => {
        if *op == hir_js::UnaryOp::Delete
          && expr_chain_contains_proto_mutation(body, *target_expr, prototype_name, proto_name, &const_aliases)
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
