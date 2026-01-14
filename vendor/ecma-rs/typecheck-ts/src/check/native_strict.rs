use crate::codes;
use crate::BodyCheckResult;
use crate::VarInit;
use diagnostics::{Diagnostic, FileId, Span, TextRange};
use hir_js::{
  Body, ClassMemberKey, ClassMemberKind, ExprKind, Literal, NameInterner, ObjectKey, PatKind,
  StmtKind,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use types_ts_interned::{
  Indexer, ObjectType, PropKey, RelateCtx, RelateTypeExpander, Shape, SignatureId, TypeId,
  TypeKind, TypeStore,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ExprRef {
  body: hir_js::BodyId,
  expr: hir_js::ExprId,
}

pub trait NativeStrictResolver {
  fn body(&self, body: hir_js::BodyId) -> Option<&Body>;
  fn resolve_ident(&self, body: hir_js::BodyId, expr: hir_js::ExprId) -> Option<hir_js::DefId>;
  fn var_initializer(&self, def: hir_js::DefId) -> Option<VarInit>;
}

struct ConstAliasResolver<'a> {
  resolver: &'a dyn NativeStrictResolver,
  // Cache `const` aliases by definition so lookups are scope-aware and work across bodies.
  cache: RefCell<HashMap<hir_js::DefId, Option<ExprRef>>>,
}

impl<'a> ConstAliasResolver<'a> {
  fn new(resolver: &'a dyn NativeStrictResolver) -> Self {
    Self {
      resolver,
      cache: RefCell::new(HashMap::new()),
    }
  }

  fn init_is_simple_const_alias(&self, init: VarInit) -> bool {
    // Only treat `const x = <expr>` as an alias. Destructuring bindings like
    // `const { eval: e } = globalThis` must *not* be unwrapped, because `e` is
    // not an alias to `globalThis` at runtime.
    let Some(pat_id) = init.pat else {
      return false;
    };
    let Some(body) = self.resolver.body(init.body) else {
      return false;
    };

    // Ensure the returned `pat` is the *root* declarator pattern, not a nested
    // binding within an object/array destructure.
    let mut is_root_pat = false;
    for stmt in &body.stmts {
      let StmtKind::Var(var) = &stmt.kind else {
        continue;
      };
      for declarator in &var.declarators {
        if declarator.init == Some(init.expr) && declarator.pat == pat_id {
          is_root_pat = true;
          break;
        }
      }
      if is_root_pat {
        break;
      }
    }
    if !is_root_pat {
      return false;
    }

    // Must be a simple identifier binding (`const x = ...`), optionally wrapped
    // in a default assignment pattern.
    let Some(pat) = body.pats.get(pat_id.0 as usize) else {
      return false;
    };
    match &pat.kind {
      PatKind::Ident(_) => true,
      PatKind::Assign { target, .. } => body
        .pats
        .get(target.0 as usize)
        .is_some_and(|target| matches!(&target.kind, PatKind::Ident(_))),
      _ => false,
    }
  }

  fn unwrap(&self, mut expr: ExprRef) -> ExprRef {
    let mut visited: HashSet<hir_js::DefId> = HashSet::new();
    let mut path: Vec<hir_js::DefId> = Vec::new();

    loop {
      expr = self.unwrap_comma_and_ts(expr);

      let Some(body) = self.resolver.body(expr.body) else {
        break;
      };
      let Some(expr_data) = body.exprs.get(expr.expr.0 as usize) else {
        break;
      };
      let ExprKind::Ident(_) = &expr_data.kind else {
        break;
      };

      let Some(def) = self.resolver.resolve_ident(expr.body, expr.expr) else {
        break;
      };

      if let Some(hit) = self.cache.borrow().get(&def).copied() {
        let Some(next) = hit else {
          break;
        };
        expr = next;
        continue;
      }

      if !visited.insert(def) {
        break;
      }

      let Some(init) = self.resolver.var_initializer(def) else {
        self.cache.borrow_mut().insert(def, None);
        break;
      };
      if init.decl_kind != hir_js::VarDeclKind::Const {
        self.cache.borrow_mut().insert(def, None);
        break;
      }

      if !self.init_is_simple_const_alias(init) {
        self.cache.borrow_mut().insert(def, None);
        break;
      }

      path.push(def);
      expr = ExprRef {
        body: init.body,
        expr: init.expr,
      };
    }

    if !path.is_empty() {
      let mut cache = self.cache.borrow_mut();
      for def in path {
        cache.insert(def, Some(expr));
      }
    }

    expr
  }

  fn unwrap_comma_and_ts(&self, mut expr: ExprRef) -> ExprRef {
    let Some(body) = self.resolver.body(expr.body) else {
      return expr;
    };

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

    expr.expr = expr_unwrap_comma(body, expr.expr);

    // Unwrap TypeScript-only "no-op" wrappers that do not affect runtime behavior. These can
    // otherwise be used to hide banned call targets (e.g. `(eval as typeof eval)("1")`,
    // `(eval!)("1")`).
    loop {
      let Some(expr_data) = body.exprs.get(expr.expr.0 as usize) else {
        return expr;
      };
      match &expr_data.kind {
        ExprKind::TypeAssertion { expr: inner, .. }
        | ExprKind::Instantiation { expr: inner, .. }
        | ExprKind::NonNull { expr: inner }
        | ExprKind::Satisfies { expr: inner, .. } => {
          expr.expr = *inner;
        }
        _ => break,
      }
    }

    expr
  }
}

pub fn validate_native_strict_body(
  body: &Body,
  result: &BodyCheckResult,
  store: &TypeStore,
  relate: &RelateCtx,
  expander: Option<&dyn RelateTypeExpander>,
  resolver: &dyn NativeStrictResolver,
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
  let type_call_name = store.intern_name_ref("call");
  let type_apply_name = store.intern_name_ref("apply");
  let type_bind_name = store.intern_name_ref("bind");
  let type_expander = relate.expander();

  fn object_key_is_ident(key: &ObjectKey, name: hir_js::NameId) -> bool {
    matches!(key, ObjectKey::Ident(id) if *id == name)
  }

  fn object_key_is_string(key: &ObjectKey, value: &str) -> bool {
    matches!(key, ObjectKey::String(s) if s == value)
  }

  fn expr_unwrap_ts_noop(body: &Body, mut expr_id: hir_js::ExprId) -> hir_js::ExprId {
    // Unwrap TypeScript-only "no-op" wrappers that do not affect runtime behavior.
    //
    // This matters for strict-native enforcement because these wrappers can:
    // - legitimately appear around constant expressions (e.g. `obj["x" as const]`), and
    // - be used to hide banned constructs if we only inspect the outer node.
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
        _ => return expr_id,
      }
    }
  }

  fn object_key_is_literal_string(body: &Body, key: &ObjectKey, value: &str) -> bool {
    match key {
      ObjectKey::Computed(expr_id) => {
        let expr_id = expr_unwrap_ts_noop(body, *expr_id);
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

  fn expr_unwrap_comma_and_local_alias(
    body: &Body,
    mut expr_id: hir_js::ExprId,
    aliases: &HashMap<hir_js::ExprId, hir_js::ExprId>,
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
      let ExprKind::Ident(_) = &expr.kind else {
        return expr_id;
      };
      if visited.contains(&expr_id) {
        return expr_id;
      }
      let Some(next) = aliases.get(&expr_id).copied() else {
        return expr_id;
      };
      visited.push(expr_id);
      expr_id = next;
    }
  }

  fn expr_unwrap_comma_and_alias(aliases: &ConstAliasResolver, expr: ExprRef) -> ExprRef {
    aliases.unwrap(expr)
  }

  fn expr_is_const_string(body: &Body, expr_id: hir_js::ExprId, value: &str) -> bool {
    let expr_id = expr_unwrap_ts_noop(body, expr_id);
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
    resolver: &dyn NativeStrictResolver,
    aliases: &ConstAliasResolver,
    expr: ExprRef,
    global_this_name: hir_js::NameId,
    base_name: hir_js::NameId,
    base_str: &str,
    member_name: hir_js::NameId,
    member_str: &str,
  ) -> bool {
    let expr = expr_unwrap_comma_and_alias(aliases, expr);
    let Some(body) = resolver.body(expr.body) else {
      return false;
    };
    let Some(expr_data) = body.exprs.get(expr.expr.0 as usize) else {
      return false;
    };
    let ExprKind::Member(mem) = &expr_data.kind else {
      return false;
    };
    expr_is_ident_or_global_this_member(
      resolver,
      aliases,
      ExprRef {
        body: expr.body,
        expr: mem.object,
      },
      global_this_name,
      base_name,
      base_str,
    )
      && (object_key_is_ident(&mem.property, member_name)
        || object_key_is_string(&mem.property, member_str)
        || object_key_is_literal_string(body, &mem.property, member_str))
  }

  fn expr_is_function_prototype_member(
    resolver: &dyn NativeStrictResolver,
    aliases: &ConstAliasResolver,
    expr: ExprRef,
    global_this_name: hir_js::NameId,
    function_name: hir_js::NameId,
    prototype_name: hir_js::NameId,
    member_name: hir_js::NameId,
    member_str: &str,
  ) -> bool {
    let expr = expr_unwrap_comma_and_alias(aliases, expr);
    let Some(body) = resolver.body(expr.body) else {
      return false;
    };
    let Some(expr_data) = body.exprs.get(expr.expr.0 as usize) else {
      return false;
    };
    let ExprKind::Member(mem) = &expr_data.kind else {
      return false;
    };
    expr_is_builtin_member(
      resolver,
      aliases,
      ExprRef {
        body: expr.body,
        expr: mem.object,
      },
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
    resolver: &dyn NativeStrictResolver,
    aliases: &ConstAliasResolver,
    expr: ExprRef,
    global_this_name: hir_js::NameId,
  ) -> bool {
    let expr = expr_unwrap_comma_and_alias(aliases, expr);
    let Some(body) = resolver.body(expr.body) else {
      return false;
    };
    let Some(expr_data) = body.exprs.get(expr.expr.0 as usize) else {
      return false;
    };
    match &expr_data.kind {
      ExprKind::Ident(name) => *name == global_this_name,
      ExprKind::Member(mem) => {
        if !expr_is_global_this(
          resolver,
          aliases,
          ExprRef {
            body: expr.body,
            expr: mem.object,
          },
          global_this_name,
        ) {
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
    resolver: &dyn NativeStrictResolver,
    aliases: &ConstAliasResolver,
    expr: ExprRef,
    global_this_name: hir_js::NameId,
    target_name: hir_js::NameId,
    target_str: &str,
  ) -> bool {
    let expr = expr_unwrap_comma_and_alias(aliases, expr);
    let Some(body) = resolver.body(expr.body) else {
      return false;
    };
    let Some(expr_data) = body.exprs.get(expr.expr.0 as usize) else {
      return false;
    };
    match &expr_data.kind {
      ExprKind::Ident(name) => *name == target_name,
      ExprKind::Member(mem) => {
        let obj_is_global_this = expr_is_global_this(
          resolver,
          aliases,
          ExprRef {
            body: expr.body,
            expr: mem.object,
          },
          global_this_name,
        );
        obj_is_global_this
          && (object_key_is_ident(&mem.property, target_name)
            || object_key_is_string(&mem.property, target_str)
            || object_key_is_literal_string(body, &mem.property, target_str))
      }
      _ => false,
    }
  }

  fn expr_is_function_like_value(
    resolver: &dyn NativeStrictResolver,
    aliases: &ConstAliasResolver,
    expr: ExprRef,
    global_this_name: hir_js::NameId,
    object_name: hir_js::NameId,
    function_name: hir_js::NameId,
    proxy_name: hir_js::NameId,
    constructor_name: hir_js::NameId,
  ) -> bool {
    let expr = expr_unwrap_comma_and_alias(aliases, expr);
    let Some(body) = resolver.body(expr.body) else {
      return false;
    };
    let Some(expr_data) = body.exprs.get(expr.expr.0 as usize) else {
      return false;
    };

    match &expr_data.kind {
      ExprKind::FunctionExpr { .. } | ExprKind::ClassExpr { .. } => true,
      ExprKind::Member(mem) if object_key_is_constructor(body, &mem.property, constructor_name) => true,
      _ => {
        // Recognize a few "obviously function-like" builtins without relying on
        // type information, which is only available for the current body.
        expr_is_ident_or_global_this_member(
          resolver,
          aliases,
          expr,
          global_this_name,
          object_name,
          "Object",
        ) || expr_is_ident_or_global_this_member(
          resolver,
          aliases,
          expr,
          global_this_name,
          function_name,
          "Function",
        ) || expr_is_ident_or_global_this_member(
          resolver,
          aliases,
          expr,
          global_this_name,
          proxy_name,
          "Proxy",
        )
      }
    }
  }

  fn expr_chain_contains_proto_mutation(
    resolver: &dyn NativeStrictResolver,
    aliases: &ConstAliasResolver,
    mut expr: ExprRef,
    prototype_name: hir_js::NameId,
    proto_name: hir_js::NameId,
  ) -> bool {
    loop {
      expr = expr_unwrap_comma_and_alias(aliases, expr);
      let Some(body) = resolver.body(expr.body) else {
        return false;
      };
      let Some(expr_data) = body.exprs.get(expr.expr.0 as usize) else {
        return false;
      };
      match &expr_data.kind {
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
          expr = ExprRef {
            body: expr.body,
            expr: member.object,
          };
        }
        _ => return false,
      }
    }
  }

  fn pat_contains_proto_mutation(
    body: &Body,
    body_id: hir_js::BodyId,
    pat: hir_js::PatId,
    prototype_name: hir_js::NameId,
    proto_name: hir_js::NameId,
    aliases: &ConstAliasResolver,
    resolver: &dyn NativeStrictResolver,
  ) -> bool {
    let Some(pat) = body.pats.get(pat.0 as usize) else {
      return false;
    };
    match &pat.kind {
      PatKind::AssignTarget(expr) => {
        expr_chain_contains_proto_mutation(
          resolver,
          aliases,
          ExprRef {
            body: body_id,
            expr: *expr,
          },
          prototype_name,
          proto_name,
        )
      }
      PatKind::Assign { target, .. } => {
        pat_contains_proto_mutation(
          body,
          body_id,
          *target,
          prototype_name,
          proto_name,
          aliases,
          resolver,
        )
      }
      PatKind::Rest(inner) => pat_contains_proto_mutation(
        body,
        body_id,
        **inner,
        prototype_name,
        proto_name,
        aliases,
        resolver,
      ),
      PatKind::Array(arr) => {
        for elem in &arr.elements {
          let Some(elem) = elem else {
            continue;
          };
          if pat_contains_proto_mutation(
            body,
            body_id,
            elem.pat,
            prototype_name,
            proto_name,
            aliases,
            resolver,
          ) {
            return true;
          }
        }
        arr
          .rest
          .is_some_and(|rest| {
            pat_contains_proto_mutation(
              body,
              body_id,
              rest,
              prototype_name,
              proto_name,
              aliases,
              resolver,
            )
          })
      }
      PatKind::Object(obj) => {
        for prop in &obj.props {
          if pat_contains_proto_mutation(
            body,
            body_id,
            prop.value,
            prototype_name,
            proto_name,
            aliases,
            resolver,
          ) {
            return true;
          }
        }
        obj
          .rest
          .is_some_and(|rest| {
            pat_contains_proto_mutation(
              body,
              body_id,
              rest,
              prototype_name,
              proto_name,
              aliases,
              resolver,
            )
          })
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
    resolver: &dyn NativeStrictResolver,
    aliases: &ConstAliasResolver,
    expr: ExprRef,
    result: &BodyCheckResult,
    store: &TypeStore,
    expander: Option<&dyn RelateTypeExpander>,
    constructor_name: hir_js::NameId,
    type_call_name: types_ts_interned::NameId,
    type_apply_name: types_ts_interned::NameId,
    type_bind_name: types_ts_interned::NameId,
    cache: &mut HashMap<TypeId, bool>,
  ) -> Option<TextRange> {
    let expr = expr_unwrap_comma_and_alias(aliases, expr);
    let body = resolver.body(expr.body)?;
    let expr_data = body.exprs.get(expr.expr.0 as usize)?;
    let ExprKind::Member(mem) = &expr_data.kind else {
      return None;
    };
    if !object_key_is_constructor(body, &mem.property, constructor_name) {
      return None;
    }

    // `.constructor` yields the `Function`/`AsyncFunction`/`GeneratorFunction` constructors when
    // accessed on a function object. This can be used to bypass name-based detection of `Function`.
    let receiver = expr_unwrap_comma_and_alias(
      aliases,
      ExprRef {
        body: expr.body,
        expr: mem.object,
      },
    );
    let receiver_body = resolver.body(receiver.body)?;
    let receiver_expr_data = receiver_body.exprs.get(receiver.expr.0 as usize)?;
    let receiver_expr_is_function_like = match &receiver_expr_data.kind {
      // Any function/class value has `.constructor === Function` (or a derived constructor in the
      // async/generator cases).
      ExprKind::FunctionExpr { .. } | ExprKind::ClassExpr { .. } => true,
      // `.constructor` on *any* object yields its constructor function, which is itself a
      // function object whose `.constructor` is `Function`. This handles chained forms like:
      // `({}).constructor.constructor.call(...)`.
      ExprKind::Member(mem) if object_key_is_constructor(receiver_body, &mem.property, constructor_name) => true,
      _ => false,
    };
    if !receiver_expr_is_function_like {
      // We only have type information for the current body.
      let receiver_ty = if receiver.body == result.body {
        result
          .expr_types
          .get(receiver.expr.0 as usize)
          .copied()
          .unwrap_or(store.primitive_ids().unknown)
      } else {
        store.primitive_ids().unknown
      };
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

    let span = if expr.body == result.body {
      result
        .expr_spans
        .get(expr.expr.0 as usize)
        .copied()
        .unwrap_or(expr_data.span)
    } else {
      expr_data.span
    };
    Some(span)
  }

  fn signature_contains_any(
    store: &TypeStore,
    expander: Option<&dyn RelateTypeExpander>,
    sig_id: SignatureId,
    type_cache: &mut HashMap<TypeId, bool>,
    sig_cache: &mut HashMap<SignatureId, bool>,
    visiting: &mut HashSet<TypeId>,
  ) -> bool {
    if let Some(hit) = sig_cache.get(&sig_id) {
      return *hit;
    }

    let sig = store.signature(sig_id);
    let result = sig
      .params
      .iter()
      .any(|param| type_contains_any(store, expander, param.ty, type_cache, sig_cache, visiting))
      || sig.this_param.is_some_and(|inner| {
        type_contains_any(store, expander, inner, type_cache, sig_cache, visiting)
      })
      || type_contains_any(store, expander, sig.ret, type_cache, sig_cache, visiting)
      || sig.type_params.iter().any(|param| {
        param.constraint.is_some_and(|inner| {
          type_contains_any(store, expander, inner, type_cache, sig_cache, visiting)
        }) || param.default.is_some_and(|inner| {
          type_contains_any(store, expander, inner, type_cache, sig_cache, visiting)
        })
      });

    sig_cache.insert(sig_id, result);
    result
  }

  fn type_contains_any(
    store: &TypeStore,
    expander: Option<&dyn RelateTypeExpander>,
    ty: TypeId,
    cache: &mut HashMap<TypeId, bool>,
    sig_cache: &mut HashMap<SignatureId, bool>,
    visiting: &mut HashSet<TypeId>,
  ) -> bool {
    let ty = store.canon(ty);
    if let Some(hit) = cache.get(&ty) {
      return *hit;
    }

    // Break cycles conservatively (no `any` found along this path).
    if !visiting.insert(ty) {
      return false;
    }

    let result = match store.type_kind(ty) {
      TypeKind::Any => true,
      TypeKind::Infer { constraint, .. } => constraint.is_some_and(|inner| {
        type_contains_any(store, expander, inner, cache, sig_cache, visiting)
      }),
      TypeKind::Tuple(elems) => elems.into_iter().any(|elem| {
        type_contains_any(store, expander, elem.ty, cache, sig_cache, visiting)
      }),
      TypeKind::Array { ty, .. } => {
        type_contains_any(store, expander, ty, cache, sig_cache, visiting)
      }
      TypeKind::Union(members) | TypeKind::Intersection(members) => members
        .into_iter()
        .any(|member| type_contains_any(store, expander, member, cache, sig_cache, visiting)),
      TypeKind::Object(obj) => {
        let shape = store.shape(store.object(obj).shape);
        shape.properties.iter().any(|prop| {
          type_contains_any(store, expander, prop.data.ty, cache, sig_cache, visiting)
        }) || shape.indexers.iter().any(|indexer| {
          type_contains_any(store, expander, indexer.key_type, cache, sig_cache, visiting)
            || type_contains_any(store, expander, indexer.value_type, cache, sig_cache, visiting)
        }) || shape.call_signatures.iter().copied().any(|sig_id| {
          signature_contains_any(store, expander, sig_id, cache, sig_cache, visiting)
        }) || shape.construct_signatures.iter().copied().any(|sig_id| {
          signature_contains_any(store, expander, sig_id, cache, sig_cache, visiting)
        })
      }
      TypeKind::Callable { overloads } => overloads.into_iter().any(|sig_id| {
        signature_contains_any(store, expander, sig_id, cache, sig_cache, visiting)
      }),
      TypeKind::Ref { def, args } => {
        let args_contain_any = args
          .iter()
          .copied()
          .any(|arg| type_contains_any(store, expander, arg, cache, sig_cache, visiting));
        if args_contain_any {
          true
        } else if let Some(expander) = expander {
          expander
            .expand_ref(store, def, &args)
            .is_some_and(|expanded| {
              type_contains_any(store, Some(expander), expanded, cache, sig_cache, visiting)
            })
        } else {
          false
        }
      }
      TypeKind::Predicate { asserted, .. } => asserted.is_some_and(|inner| {
        type_contains_any(store, expander, inner, cache, sig_cache, visiting)
      }),
      TypeKind::Conditional {
        check,
        extends,
        true_ty,
        false_ty,
        ..
      } => {
        type_contains_any(store, expander, check, cache, sig_cache, visiting)
          || type_contains_any(store, expander, extends, cache, sig_cache, visiting)
          || type_contains_any(store, expander, true_ty, cache, sig_cache, visiting)
          || type_contains_any(store, expander, false_ty, cache, sig_cache, visiting)
      }
      TypeKind::Mapped(mapped) => {
        type_contains_any(store, expander, mapped.source, cache, sig_cache, visiting)
          || type_contains_any(store, expander, mapped.value, cache, sig_cache, visiting)
          || mapped.name_type.is_some_and(|inner| {
            type_contains_any(store, expander, inner, cache, sig_cache, visiting)
          })
          || mapped.as_type.is_some_and(|inner| {
            type_contains_any(store, expander, inner, cache, sig_cache, visiting)
          })
      }
      TypeKind::TemplateLiteral(tpl) => tpl.spans.into_iter().any(|chunk| {
        type_contains_any(store, expander, chunk.ty, cache, sig_cache, visiting)
      }),
      TypeKind::Intrinsic { ty, .. } => {
        type_contains_any(store, expander, ty, cache, sig_cache, visiting)
      }
      TypeKind::IndexedAccess { obj, index } => {
        type_contains_any(store, expander, obj, cache, sig_cache, visiting)
          || type_contains_any(store, expander, index, cache, sig_cache, visiting)
      }
      TypeKind::KeyOf(inner) => type_contains_any(store, expander, inner, cache, sig_cache, visiting),
      _ => false,
    };

    visiting.remove(&ty);
    cache.insert(ty, result);
    result
  }

  let mut any_cache = HashMap::new();
  let mut any_sig_cache = HashMap::new();
  for (idx, ty) in result.expr_types.iter().enumerate() {
    let span = result
      .expr_spans
      .get(idx)
      .copied()
      .or_else(|| body.exprs.get(idx).map(|expr| expr.span))
      .unwrap_or(TextRange::new(0, 0));
    let mut visiting = HashSet::new();
    if type_contains_any(
      store,
      expander,
      *ty,
      &mut any_cache,
      &mut any_sig_cache,
      &mut visiting,
    ) {
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
    if type_contains_any(
      store,
      expander,
      *ty,
      &mut any_cache,
      &mut any_sig_cache,
      &mut visiting,
    ) {
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

  // Track `const` aliases and destructured builtin aliases so `native_strict` bans cannot be
  // bypassed via indirection.
  //
  // - Simple aliases: `const x = expr;` (e.g. `const dp = Object.defineProperty; dp(...)`).
  // - Destructured aliases: `const { eval: e } = globalThis; e("...")`.
  //
  // Both are tracked in a scope-aware way by recording alias targets per *identifier expression*,
  // avoiding leaks across shadowing blocks.
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  enum DestructuredAliasKind {
    Eval,
    Function,
    Proxy,
    ProxyRevocable,
    ReflectApply,
    ReflectConstruct,
    FunctionPrototypeCall,
    FunctionPrototypeApply,
    FunctionPrototypeBind,
    ObjectSetPrototypeOf,
    ReflectSetPrototypeOf,
    ObjectDefineProperty,
    ReflectDefineProperty,
    ObjectDefineProperties,
    ObjectAssign,
  }

  #[derive(Clone, Copy, Debug)]
  enum BindingKind {
    ConstExpr(hir_js::ExprId),
    ConstDestructured(DestructuredAliasKind),
    Shadow,
  }

  struct AliasBuilder<'a> {
    body: &'a Body,
    body_id: hir_js::BodyId,
    resolver: &'a dyn NativeStrictResolver,
    semantic_aliases: &'a ConstAliasResolver<'a>,
    result: &'a BodyCheckResult,
    store: &'a TypeStore,
    type_expander: Option<&'a dyn RelateTypeExpander>,
    type_call_name: types_ts_interned::NameId,
    type_apply_name: types_ts_interned::NameId,
    type_bind_name: types_ts_interned::NameId,
    function_like_cache: HashMap<TypeId, bool>,
    scopes: Vec<HashMap<hir_js::NameId, BindingKind>>,
    const_aliases: HashMap<hir_js::ExprId, hir_js::ExprId>,
    destructured_aliases: HashMap<hir_js::ExprId, DestructuredAliasKind>,

    eval_name: hir_js::NameId,
    global_this_name: hir_js::NameId,
    object_name: hir_js::NameId,
    reflect_name: hir_js::NameId,
    function_name: hir_js::NameId,
    proxy_name: hir_js::NameId,
    revocable_name: hir_js::NameId,
    apply_name: hir_js::NameId,
    construct_name: hir_js::NameId,
    set_prototype_of_name: hir_js::NameId,
    define_property_name: hir_js::NameId,
    define_properties_name: hir_js::NameId,
    assign_name: hir_js::NameId,
    constructor_name: hir_js::NameId,
  }

  impl<'a> AliasBuilder<'a> {
    fn new(
      body: &'a Body,
      body_id: hir_js::BodyId,
      resolver: &'a dyn NativeStrictResolver,
      semantic_aliases: &'a ConstAliasResolver<'a>,
      result: &'a BodyCheckResult,
      store: &'a TypeStore,
      type_expander: Option<&'a dyn RelateTypeExpander>,
      type_call_name: types_ts_interned::NameId,
      type_apply_name: types_ts_interned::NameId,
      type_bind_name: types_ts_interned::NameId,
      eval_name: hir_js::NameId,
      global_this_name: hir_js::NameId,
      object_name: hir_js::NameId,
      reflect_name: hir_js::NameId,
      function_name: hir_js::NameId,
      proxy_name: hir_js::NameId,
      revocable_name: hir_js::NameId,
      apply_name: hir_js::NameId,
      construct_name: hir_js::NameId,
      set_prototype_of_name: hir_js::NameId,
      define_property_name: hir_js::NameId,
      define_properties_name: hir_js::NameId,
      assign_name: hir_js::NameId,
      constructor_name: hir_js::NameId,
    ) -> Self {
      Self {
        body,
        body_id,
        resolver,
        semantic_aliases,
        result,
        store,
        type_expander,
        type_call_name,
        type_apply_name,
        type_bind_name,
        function_like_cache: HashMap::new(),
        scopes: vec![HashMap::new()],
        const_aliases: HashMap::new(),
        destructured_aliases: HashMap::new(),
        eval_name,
        global_this_name,
        object_name,
        reflect_name,
        function_name,
        proxy_name,
        revocable_name,
        apply_name,
        construct_name,
        set_prototype_of_name,
        define_property_name,
        define_properties_name,
        assign_name,
        constructor_name,
      }
    }

    fn current_scope_mut(&mut self) -> &mut HashMap<hir_js::NameId, BindingKind> {
      self
        .scopes
        .last_mut()
        .expect("AliasBuilder scope stack should never be empty")
    }

    fn lookup(&self, name: hir_js::NameId) -> Option<BindingKind> {
      self
        .scopes
        .iter()
        .rev()
        .find_map(|scope| scope.get(&name).copied())
    }

    fn record_ident_use(&mut self, expr_id: hir_js::ExprId, name: hir_js::NameId) {
      match self.lookup(name) {
        Some(BindingKind::ConstExpr(target)) => {
          self.const_aliases.insert(expr_id, target);
        }
        Some(BindingKind::ConstDestructured(kind)) => {
          self.destructured_aliases.insert(expr_id, kind);
        }
        Some(BindingKind::Shadow) | None => {}
      }
    }

    fn simple_binding_name(&self, pat: hir_js::PatId) -> Option<hir_js::NameId> {
      let pat = self.body.pats.get(pat.0 as usize)?;
      match &pat.kind {
        PatKind::Ident(name) => Some(*name),
        PatKind::Assign { target, .. } => {
          let target = self.body.pats.get(target.0 as usize)?;
          match &target.kind {
            PatKind::Ident(name) => Some(*name),
            _ => None,
          }
        }
        _ => None,
      }
    }

    fn collect_bound_names(&self, pat: hir_js::PatId, out: &mut Vec<hir_js::NameId>) {
      let Some(pat) = self.body.pats.get(pat.0 as usize) else {
        return;
      };
      match &pat.kind {
        PatKind::Ident(name) => out.push(*name),
        PatKind::Array(arr) => {
          for elem in &arr.elements {
            let Some(elem) = elem else {
              continue;
            };
            self.collect_bound_names(elem.pat, out);
          }
          if let Some(rest) = arr.rest {
            self.collect_bound_names(rest, out);
          }
        }
        PatKind::Object(obj) => {
          for prop in &obj.props {
            self.collect_bound_names(prop.value, out);
          }
          if let Some(rest) = obj.rest {
            self.collect_bound_names(rest, out);
          }
        }
        PatKind::Rest(inner) => self.collect_bound_names(**inner, out),
        PatKind::Assign { target, .. } => self.collect_bound_names(*target, out),
        PatKind::AssignTarget(_) => {}
      }
    }

    fn bind_shadow(&mut self, pat: hir_js::PatId) {
      let mut names = Vec::new();
      self.collect_bound_names(pat, &mut names);
      for name in names {
        self.current_scope_mut().insert(name, BindingKind::Shadow);
      }
    }

    fn bind_const_simple_alias(&mut self, pat: hir_js::PatId, init: hir_js::ExprId) -> bool {
      let Some(name) = self.simple_binding_name(pat) else {
        return false;
      };
      self
        .current_scope_mut()
        .insert(name, BindingKind::ConstExpr(init));
      true
    }

    fn object_key_matches(&self, key: &ObjectKey, name: hir_js::NameId, value: &str) -> bool {
      object_key_is_ident(key, name)
        || object_key_is_string(key, value)
        || object_key_is_literal_string(self.body, key, value)
    }

    fn expr_is_function_like(&mut self, expr_id: hir_js::ExprId) -> bool {
      let expr_ty = self
        .result
        .expr_types
        .get(expr_id.0 as usize)
        .copied()
        .unwrap_or(self.store.primitive_ids().unknown);
      let mut visiting = HashSet::new();
      type_is_function_like(
        self.store,
        expr_ty,
        self.type_expander,
        self.type_call_name,
        self.type_apply_name,
        self.type_bind_name,
        &mut self.function_like_cache,
        &mut visiting,
      )
    }

    fn bind_const_object_pat(&mut self, obj: &hir_js::ObjectPat, init: hir_js::ExprId) {
      let init_ref = ExprRef {
        body: self.body_id,
        expr: init,
      };
      let init_is_global_this = expr_is_global_this(
        self.resolver,
        self.semantic_aliases,
        init_ref,
        self.global_this_name,
      );
      let init_is_object = expr_is_ident_or_global_this_member(
        self.resolver,
        self.semantic_aliases,
        init_ref,
        self.global_this_name,
        self.object_name,
        "Object",
      );
      let init_is_reflect = expr_is_ident_or_global_this_member(
        self.resolver,
        self.semantic_aliases,
        init_ref,
        self.global_this_name,
        self.reflect_name,
        "Reflect",
      );
      let init_is_proxy = expr_is_ident_or_global_this_member(
        self.resolver,
        self.semantic_aliases,
        init_ref,
        self.global_this_name,
        self.proxy_name,
        "Proxy",
      );
      let mut init_is_function_like: Option<bool> = None;

      for prop in &obj.props {
        let mut kind = None;
        if init_is_global_this {
          if self.object_key_matches(&prop.key, self.eval_name, "eval") {
            kind = Some(DestructuredAliasKind::Eval);
          } else if self.object_key_matches(&prop.key, self.function_name, "Function") {
            kind = Some(DestructuredAliasKind::Function);
          } else if self.object_key_matches(&prop.key, self.proxy_name, "Proxy") {
            kind = Some(DestructuredAliasKind::Proxy);
          }
        }
        if init_is_object {
          if self.object_key_matches(&prop.key, self.set_prototype_of_name, "setPrototypeOf") {
            kind = Some(DestructuredAliasKind::ObjectSetPrototypeOf);
          } else if self.object_key_matches(&prop.key, self.define_property_name, "defineProperty") {
            kind = Some(DestructuredAliasKind::ObjectDefineProperty);
          } else if self.object_key_matches(&prop.key, self.define_properties_name, "defineProperties") {
            kind = Some(DestructuredAliasKind::ObjectDefineProperties);
          } else if self.object_key_matches(&prop.key, self.assign_name, "assign") {
            kind = Some(DestructuredAliasKind::ObjectAssign);
          }
        }
        if init_is_reflect {
          if self.object_key_matches(&prop.key, self.apply_name, "apply") {
            kind = Some(DestructuredAliasKind::ReflectApply);
          } else if self.object_key_matches(&prop.key, self.construct_name, "construct") {
            kind = Some(DestructuredAliasKind::ReflectConstruct);
          } else if self.object_key_matches(&prop.key, self.set_prototype_of_name, "setPrototypeOf") {
            kind = Some(DestructuredAliasKind::ReflectSetPrototypeOf);
          } else if self.object_key_matches(&prop.key, self.define_property_name, "defineProperty") {
            kind = Some(DestructuredAliasKind::ReflectDefineProperty);
          }
        }
        if init_is_proxy {
          if self.object_key_matches(&prop.key, self.revocable_name, "revocable") {
            kind = Some(DestructuredAliasKind::ProxyRevocable);
          }
        }
        if kind.is_none() && self.object_key_matches(&prop.key, self.constructor_name, "constructor") {
          let init_is_function_like =
            *init_is_function_like.get_or_insert_with(|| self.expr_is_function_like(init));
          if init_is_function_like {
            kind = Some(DestructuredAliasKind::Function);
          }
        }

        let Some(target_name) = self.simple_binding_name(prop.value) else {
          // Still shadow any bindings this property introduces.
          self.bind_shadow(prop.value);
          continue;
        };

        let binding = kind
          .map(BindingKind::ConstDestructured)
          .unwrap_or(BindingKind::Shadow);
        self.current_scope_mut().insert(target_name, binding);
      }

      if let Some(rest) = obj.rest {
        self.bind_shadow(rest);
      }
    }

    fn bind_var_decl(&mut self, var: &hir_js::VarDecl) {
      for decl in &var.declarators {
        if let Some(init) = decl.init {
          self.visit_expr(init);
        }

        // Visit destructuring defaults / computed keys.
        self.visit_pat(decl.pat);

        match var.kind {
          hir_js::VarDeclKind::Const => {
            let Some(init) = decl.init else {
              self.bind_shadow(decl.pat);
              continue;
            };

            let Some(pat) = self.body.pats.get(decl.pat.0 as usize) else {
              continue;
            };

            match &pat.kind {
              PatKind::Ident(_) | PatKind::Assign { .. } => {
                if !self.bind_const_simple_alias(decl.pat, init) {
                  self.bind_shadow(decl.pat);
                }
              }
              PatKind::Object(obj) => self.bind_const_object_pat(obj, init),
              _ => self.bind_shadow(decl.pat),
            }
          }
          // `let`/`var`/`using` are mutable; we don't treat them as aliases, but they must still
          // shadow any outer aliases.
          _ => self.bind_shadow(decl.pat),
        }
      }
    }

    fn visit_stmt(&mut self, stmt_id: hir_js::StmtId) {
      let Some(stmt) = self.body.stmts.get(stmt_id.0 as usize) else {
        return;
      };
      match &stmt.kind {
        StmtKind::Expr(expr) => self.visit_expr(*expr),
        StmtKind::ExportDefaultExpr(expr) => self.visit_expr(*expr),
        StmtKind::Decl(_) => {}
        StmtKind::Return(expr) => {
          if let Some(expr) = expr {
            self.visit_expr(*expr);
          }
        }
        StmtKind::Block(stmts) => {
          self.scopes.push(HashMap::new());
          for stmt in stmts {
            self.visit_stmt(*stmt);
          }
          self.scopes.pop();
        }
        StmtKind::If {
          test,
          consequent,
          alternate,
        } => {
          self.visit_expr(*test);
          self.visit_stmt(*consequent);
          if let Some(alternate) = alternate {
            self.visit_stmt(*alternate);
          }
        }
        StmtKind::While { test, body } | StmtKind::DoWhile { test, body } => {
          self.visit_expr(*test);
          self.visit_stmt(*body);
        }
        StmtKind::For {
          init,
          test,
          update,
          body,
        } => {
          self.scopes.push(HashMap::new());
          if let Some(init) = init {
            match init {
              hir_js::ForInit::Expr(expr) => self.visit_expr(*expr),
              hir_js::ForInit::Var(var) => self.bind_var_decl(var),
            }
          }
          if let Some(test) = test {
            self.visit_expr(*test);
          }
          if let Some(update) = update {
            self.visit_expr(*update);
          }
          self.visit_stmt(*body);
          self.scopes.pop();
        }
        StmtKind::ForIn {
          left,
          right,
          body,
          ..
        } => {
          self.scopes.push(HashMap::new());
          match left {
            hir_js::ForHead::Pat(pat) => self.visit_pat(*pat),
            hir_js::ForHead::Var(var) => self.bind_var_decl(var),
          }
          self.visit_expr(*right);
          self.visit_stmt(*body);
          self.scopes.pop();
        }
        StmtKind::Switch { discriminant, cases } => {
          self.scopes.push(HashMap::new());
          self.visit_expr(*discriminant);
          for case in cases {
            if let Some(test) = case.test {
              self.visit_expr(test);
            }
            for stmt in &case.consequent {
              self.visit_stmt(*stmt);
            }
          }
          self.scopes.pop();
        }
        StmtKind::Try {
          block,
          catch,
          finally_block,
        } => {
          self.visit_stmt(*block);
          if let Some(catch) = catch {
            self.scopes.push(HashMap::new());
            if let Some(param) = catch.param {
              self.bind_shadow(param);
            }
            self.visit_stmt(catch.body);
            self.scopes.pop();
          }
          if let Some(finally) = finally_block {
            self.visit_stmt(*finally);
          }
        }
        StmtKind::Throw(expr) => self.visit_expr(*expr),
        StmtKind::Break(_) | StmtKind::Continue(_) | StmtKind::Debugger | StmtKind::Empty => {}
        StmtKind::Var(var) => self.bind_var_decl(var),
        StmtKind::Labeled { body, .. } => self.visit_stmt(*body),
        StmtKind::With { object, body } => {
          self.visit_expr(*object);
          self.visit_stmt(*body);
        }
      }
    }

    fn visit_pat(&mut self, pat_id: hir_js::PatId) {
      let Some(pat) = self.body.pats.get(pat_id.0 as usize) else {
        return;
      };
      match &pat.kind {
        PatKind::Ident(_) => {}
        PatKind::Array(arr) => {
          for elem in &arr.elements {
            let Some(elem) = elem else {
              continue;
            };
            self.visit_pat(elem.pat);
            if let Some(default) = elem.default_value {
              self.visit_expr(default);
            }
          }
          if let Some(rest) = arr.rest {
            self.visit_pat(rest);
          }
        }
        PatKind::Object(obj) => {
          for prop in &obj.props {
            if let ObjectKey::Computed(expr) = prop.key {
              self.visit_expr(expr);
            }
            self.visit_pat(prop.value);
            if let Some(default) = prop.default_value {
              self.visit_expr(default);
            }
          }
          if let Some(rest) = obj.rest {
            self.visit_pat(rest);
          }
        }
        PatKind::Rest(inner) => self.visit_pat(**inner),
        PatKind::Assign {
          target,
          default_value,
        } => {
          self.visit_pat(*target);
          self.visit_expr(*default_value);
        }
        PatKind::AssignTarget(expr) => self.visit_expr(*expr),
      }
    }

    fn visit_expr(&mut self, expr_id: hir_js::ExprId) {
      let Some(expr) = self.body.exprs.get(expr_id.0 as usize) else {
        return;
      };

      match &expr.kind {
        ExprKind::Ident(name) => self.record_ident_use(expr_id, *name),
        ExprKind::Unary { expr, .. } => self.visit_expr(*expr),
        ExprKind::Update { expr, .. } => self.visit_expr(*expr),
        ExprKind::Binary { left, right, .. } => {
          self.visit_expr(*left);
          self.visit_expr(*right);
        }
        ExprKind::Assignment { target, value, .. } => {
          self.visit_pat(*target);
          self.visit_expr(*value);
        }
        ExprKind::Call(call) => {
          self.visit_expr(call.callee);
          for arg in &call.args {
            self.visit_expr(arg.expr);
          }
        }
        ExprKind::Member(mem) => {
          self.visit_expr(mem.object);
          if let ObjectKey::Computed(expr) = mem.property {
            self.visit_expr(expr);
          }
        }
        ExprKind::Conditional {
          test,
          consequent,
          alternate,
        } => {
          self.visit_expr(*test);
          self.visit_expr(*consequent);
          self.visit_expr(*alternate);
        }
        ExprKind::Array(arr) => {
          for elem in &arr.elements {
            match elem {
              hir_js::ArrayElement::Expr(expr) | hir_js::ArrayElement::Spread(expr) => {
                self.visit_expr(*expr)
              }
              hir_js::ArrayElement::Empty => {}
            }
          }
        }
        ExprKind::Object(obj) => {
          for prop in &obj.properties {
            match prop {
              hir_js::ObjectProperty::KeyValue { key, value, .. } => {
                if let ObjectKey::Computed(expr) = key {
                  self.visit_expr(*expr);
                }
                self.visit_expr(*value);
              }
              hir_js::ObjectProperty::Getter { key, .. }
              | hir_js::ObjectProperty::Setter { key, .. } => {
                if let ObjectKey::Computed(expr) = key {
                  self.visit_expr(*expr);
                }
              }
              hir_js::ObjectProperty::Spread(expr) => self.visit_expr(*expr),
            }
          }
        }
        ExprKind::Template(tpl) => {
          for span in &tpl.spans {
            self.visit_expr(span.expr);
          }
        }
        ExprKind::TaggedTemplate { tag, template } => {
          self.visit_expr(*tag);
          for span in &template.spans {
            self.visit_expr(span.expr);
          }
        }
        ExprKind::Await { expr } => self.visit_expr(*expr),
        ExprKind::Yield { expr, .. } => {
          if let Some(expr) = expr {
            self.visit_expr(*expr);
          }
        }
        ExprKind::TypeAssertion { expr, .. }
        | ExprKind::Instantiation { expr, .. }
        | ExprKind::NonNull { expr }
        | ExprKind::Satisfies { expr, .. } => self.visit_expr(*expr),
        ExprKind::ImportCall {
          argument,
          attributes,
        } => {
          self.visit_expr(*argument);
          if let Some(attributes) = attributes {
            self.visit_expr(*attributes);
          }
        }
        ExprKind::Jsx(jsx) => {
          for attr in &jsx.attributes {
            match attr {
              hir_js::JsxAttr::Named { value, .. } => {
                if let Some(value) = value {
                  match value {
                    hir_js::JsxAttrValue::Expression(expr) => {
                      if let Some(expr) = expr.expr {
                        self.visit_expr(expr);
                      }
                    }
                    hir_js::JsxAttrValue::Element(expr) => self.visit_expr(*expr),
                    hir_js::JsxAttrValue::Text(_) => {}
                  }
                }
              }
              hir_js::JsxAttr::Spread { expr } => self.visit_expr(*expr),
            }
          }
          for child in &jsx.children {
            match child {
              hir_js::JsxChild::Element(expr) => self.visit_expr(*expr),
              hir_js::JsxChild::Expr(expr) => {
                if let Some(expr) = expr.expr {
                  self.visit_expr(expr);
                }
              }
              hir_js::JsxChild::Text(_) => {}
            }
          }
        }
        ExprKind::Literal(_)
        | ExprKind::This
        | ExprKind::Super
        | ExprKind::Missing
        | ExprKind::FunctionExpr { .. }
        | ExprKind::ClassExpr { .. }
        | ExprKind::ImportMeta
        | ExprKind::NewTarget => {}
        #[cfg(feature = "semantic-ops")]
        ExprKind::ArrayMap { array, callback }
        | ExprKind::ArrayFilter { array, callback }
        | ExprKind::ArrayFind { array, callback }
        | ExprKind::ArrayEvery { array, callback }
        | ExprKind::ArraySome { array, callback } => {
          self.visit_expr(*array);
          self.visit_expr(*callback);
        }
        #[cfg(feature = "semantic-ops")]
        ExprKind::ArrayReduce {
          array,
          callback,
          init,
        } => {
          self.visit_expr(*array);
          self.visit_expr(*callback);
          if let Some(init) = init {
            self.visit_expr(*init);
          }
        }
        #[cfg(feature = "semantic-ops")]
        ExprKind::ArrayChain { array, ops } => {
          self.visit_expr(*array);
          for op in ops {
            match op {
              hir_js::ArrayChainOp::Map(expr)
              | hir_js::ArrayChainOp::Filter(expr)
              | hir_js::ArrayChainOp::Find(expr)
              | hir_js::ArrayChainOp::Every(expr)
              | hir_js::ArrayChainOp::Some(expr) => self.visit_expr(*expr),
              hir_js::ArrayChainOp::Reduce(expr, init) => {
                self.visit_expr(*expr);
                if let Some(init) = init {
                  self.visit_expr(*init);
                }
              }
            }
          }
        }
        #[cfg(feature = "semantic-ops")]
        ExprKind::PromiseAll { promises } | ExprKind::PromiseRace { promises } => {
          for promise in promises {
            self.visit_expr(*promise);
          }
        }
        #[cfg(feature = "semantic-ops")]
        ExprKind::AwaitExpr { value, .. } => self.visit_expr(*value),
        #[cfg(feature = "semantic-ops")]
        ExprKind::KnownApiCall { args, .. } => {
          for arg in args {
            self.visit_expr(*arg);
          }
        }
      }
    }
  }

  struct DestructuredAliasResolver<'a> {
    resolver: &'a dyn NativeStrictResolver,
    const_aliases: &'a ConstAliasResolver<'a>,
    cache: RefCell<HashMap<hir_js::DefId, Option<DestructuredAliasKind>>>,

    eval_name: hir_js::NameId,
    global_this_name: hir_js::NameId,
    object_name: hir_js::NameId,
    reflect_name: hir_js::NameId,
    function_name: hir_js::NameId,
    prototype_name: hir_js::NameId,
    call_name: hir_js::NameId,
    proxy_name: hir_js::NameId,
    revocable_name: hir_js::NameId,
    apply_name: hir_js::NameId,
    bind_name: hir_js::NameId,
    construct_name: hir_js::NameId,
    set_prototype_of_name: hir_js::NameId,
    define_property_name: hir_js::NameId,
    define_properties_name: hir_js::NameId,
    assign_name: hir_js::NameId,
    constructor_name: hir_js::NameId,
  }

  impl<'a> DestructuredAliasResolver<'a> {
    fn new(
      resolver: &'a dyn NativeStrictResolver,
      const_aliases: &'a ConstAliasResolver<'a>,
      eval_name: hir_js::NameId,
      global_this_name: hir_js::NameId,
      object_name: hir_js::NameId,
      reflect_name: hir_js::NameId,
      function_name: hir_js::NameId,
      prototype_name: hir_js::NameId,
      call_name: hir_js::NameId,
      proxy_name: hir_js::NameId,
      revocable_name: hir_js::NameId,
      apply_name: hir_js::NameId,
      bind_name: hir_js::NameId,
      construct_name: hir_js::NameId,
      set_prototype_of_name: hir_js::NameId,
      define_property_name: hir_js::NameId,
      define_properties_name: hir_js::NameId,
      assign_name: hir_js::NameId,
      constructor_name: hir_js::NameId,
    ) -> Self {
      Self {
        resolver,
        const_aliases,
        cache: RefCell::new(HashMap::new()),
        eval_name,
        global_this_name,
        object_name,
        reflect_name,
        function_name,
        prototype_name,
        call_name,
        proxy_name,
        revocable_name,
        apply_name,
        bind_name,
        construct_name,
        set_prototype_of_name,
        define_property_name,
        define_properties_name,
        assign_name,
        constructor_name,
      }
    }

    fn resolve_ident(&self, body_id: hir_js::BodyId, expr_id: hir_js::ExprId) -> Option<DestructuredAliasKind> {
      let body = self.resolver.body(body_id)?;
      let expr = body.exprs.get(expr_id.0 as usize)?;
      if !matches!(&expr.kind, ExprKind::Ident(_)) {
        return None;
      }
      let def = self.resolver.resolve_ident(body_id, expr_id)?;
      self.resolve_def(def)
    }

    fn resolve_expr(&self, expr: ExprRef) -> Option<DestructuredAliasKind> {
      let expr = expr_unwrap_comma_and_alias(self.const_aliases, expr);
      self.resolve_ident(expr.body, expr.expr)
    }

    fn resolve_def(&self, def: hir_js::DefId) -> Option<DestructuredAliasKind> {
      if let Some(hit) = self.cache.borrow().get(&def).copied() {
        return hit;
      }
      let kind = self.compute_def(def);
      self.cache.borrow_mut().insert(def, kind);
      kind
    }

    fn compute_def(&self, def: hir_js::DefId) -> Option<DestructuredAliasKind> {
      let init = self.resolver.var_initializer(def)?;
      if init.decl_kind != hir_js::VarDeclKind::Const {
        return None;
      }
      let binding_pat = init.pat?;
      let body = self.resolver.body(init.body)?;

      let prop_key = self.object_pat_prop_key(body, init.expr, binding_pat)?;
      let init_ref = ExprRef {
        body: init.body,
        expr: init.expr,
      };

      let init_is_global_this = expr_is_global_this(
        self.resolver,
        self.const_aliases,
        init_ref,
        self.global_this_name,
      );
      let init_is_object = expr_is_ident_or_global_this_member(
        self.resolver,
        self.const_aliases,
        init_ref,
        self.global_this_name,
        self.object_name,
        "Object",
      );
      let init_is_reflect = expr_is_ident_or_global_this_member(
        self.resolver,
        self.const_aliases,
        init_ref,
        self.global_this_name,
        self.reflect_name,
        "Reflect",
      );
      let init_is_function_like = expr_is_function_like_value(
        self.resolver,
        self.const_aliases,
        init_ref,
        self.global_this_name,
        self.object_name,
        self.function_name,
        self.proxy_name,
        self.constructor_name,
      );
      let init_is_function_prototype = expr_is_builtin_member(
        self.resolver,
        self.const_aliases,
        init_ref,
        self.global_this_name,
        self.function_name,
        "Function",
        self.prototype_name,
        "prototype",
      );
      let init_is_proxy = expr_is_ident_or_global_this_member(
        self.resolver,
        self.const_aliases,
        init_ref,
        self.global_this_name,
        self.proxy_name,
        "Proxy",
      );

      let mut kind = None;

      if init_is_global_this {
        if self.object_key_matches(body, &prop_key, self.eval_name, "eval") {
          kind = Some(DestructuredAliasKind::Eval);
        } else if self.object_key_matches(body, &prop_key, self.function_name, "Function") {
          kind = Some(DestructuredAliasKind::Function);
        } else if self.object_key_matches(body, &prop_key, self.proxy_name, "Proxy") {
          kind = Some(DestructuredAliasKind::Proxy);
        }
      }

      if init_is_object {
        if self.object_key_matches(body, &prop_key, self.set_prototype_of_name, "setPrototypeOf") {
          kind = Some(DestructuredAliasKind::ObjectSetPrototypeOf);
        } else if self.object_key_matches(body, &prop_key, self.define_property_name, "defineProperty") {
          kind = Some(DestructuredAliasKind::ObjectDefineProperty);
        } else if self.object_key_matches(body, &prop_key, self.define_properties_name, "defineProperties") {
          kind = Some(DestructuredAliasKind::ObjectDefineProperties);
        } else if self.object_key_matches(body, &prop_key, self.assign_name, "assign") {
          kind = Some(DestructuredAliasKind::ObjectAssign);
        }
      }

      if init_is_reflect {
        if self.object_key_matches(body, &prop_key, self.apply_name, "apply") {
          kind = Some(DestructuredAliasKind::ReflectApply);
        } else if self.object_key_matches(body, &prop_key, self.construct_name, "construct") {
          kind = Some(DestructuredAliasKind::ReflectConstruct);
        } else if self.object_key_matches(body, &prop_key, self.set_prototype_of_name, "setPrototypeOf") {
          kind = Some(DestructuredAliasKind::ReflectSetPrototypeOf);
        } else if self.object_key_matches(body, &prop_key, self.define_property_name, "defineProperty") {
          kind = Some(DestructuredAliasKind::ReflectDefineProperty);
        }
      }

      if kind.is_none() && (init_is_function_like || init_is_function_prototype) {
        if self.object_key_matches(body, &prop_key, self.call_name, "call") {
          kind = Some(DestructuredAliasKind::FunctionPrototypeCall);
        } else if self.object_key_matches(body, &prop_key, self.apply_name, "apply") {
          kind = Some(DestructuredAliasKind::FunctionPrototypeApply);
        } else if self.object_key_matches(body, &prop_key, self.bind_name, "bind") {
          kind = Some(DestructuredAliasKind::FunctionPrototypeBind);
        }
      }

      if init_is_proxy && self.object_key_matches(body, &prop_key, self.revocable_name, "revocable") {
        kind = Some(DestructuredAliasKind::ProxyRevocable);
      }

      if kind.is_none()
        && object_key_is_constructor(body, &prop_key, self.constructor_name)
        && expr_is_function_like_value(
          self.resolver,
          self.const_aliases,
          init_ref,
          self.global_this_name,
          self.object_name,
          self.function_name,
          self.proxy_name,
          self.constructor_name,
        )
      {
        kind = Some(DestructuredAliasKind::Function);
      }

      kind
    }

    fn object_key_matches(
      &self,
      body: &Body,
      key: &ObjectKey,
      name: hir_js::NameId,
      value: &str,
    ) -> bool {
      object_key_is_ident(key, name) || object_key_is_string(key, value) || object_key_is_literal_string(body, key, value)
    }

    fn object_pat_prop_key(
      &self,
      body: &Body,
      init_expr: hir_js::ExprId,
      binding_pat: hir_js::PatId,
    ) -> Option<ObjectKey> {
      let binding_pat = self.pat_unwrap_assign_target(body, binding_pat);

      for stmt in &body.stmts {
        let StmtKind::Var(var) = &stmt.kind else {
          continue;
        };
        for declarator in &var.declarators {
          if declarator.init != Some(init_expr) {
            continue;
          }

          let pat = body.pats.get(declarator.pat.0 as usize)?;
          let PatKind::Object(obj) = &pat.kind else {
            return None;
          };

          for prop in &obj.props {
            if self.prop_value_binds_pat(body, prop.value, binding_pat) {
              return Some(prop.key.clone());
            }
          }
          return None;
        }
      }
      None
    }

    fn pat_unwrap_assign_target(&self, body: &Body, pat_id: hir_js::PatId) -> hir_js::PatId {
      let Some(pat) = body.pats.get(pat_id.0 as usize) else {
        return pat_id;
      };
      match &pat.kind {
        PatKind::Assign { target, .. } => *target,
        _ => pat_id,
      }
    }

    fn prop_value_binds_pat(&self, body: &Body, prop_value: hir_js::PatId, binding_pat: hir_js::PatId) -> bool {
      if prop_value == binding_pat {
        return true;
      }
      let Some(pat) = body.pats.get(prop_value.0 as usize) else {
        return false;
      };
      match &pat.kind {
        PatKind::Assign { target, .. } => *target == binding_pat,
        _ => false,
      }
    }
  }

  // Resolve scope-aware `const` aliases via semantic bindings so `native_strict` bans cannot be
  // bypassed via indirection across nested functions/blocks.
  let const_aliases = ConstAliasResolver::new(resolver);
  let semantic_destructured_aliases = DestructuredAliasResolver::new(
    resolver,
    &const_aliases,
    eval_name,
    global_this_name,
    object_name,
    reflect_name,
    function_name,
    prototype_name,
    call_name,
    proxy_name,
    revocable_name,
    apply_name,
    bind_name,
    construct_name,
    set_prototype_of_name,
    define_property_name,
    define_properties_name,
    assign_name,
    constructor_name,
  );
  let body_id = result.body;

  let (local_const_aliases, destructured_aliases) = {
    let mut builder = AliasBuilder::new(
      body,
      body_id,
      resolver,
      &const_aliases,
      result,
      store,
      type_expander,
      type_call_name,
      type_apply_name,
      type_bind_name,
      eval_name,
      global_this_name,
      object_name,
      reflect_name,
      function_name,
      proxy_name,
      revocable_name,
      apply_name,
      construct_name,
      set_prototype_of_name,
      define_property_name,
      define_properties_name,
      assign_name,
      constructor_name,
    );
    for stmt in &body.root_stmts {
      builder.visit_stmt(*stmt);
    }
    (builder.const_aliases, builder.destructured_aliases)
  };
  let mut function_like_cache: HashMap<TypeId, bool> = HashMap::new();

  for (idx, expr) in body.exprs.iter().enumerate() {
    match &expr.kind {
      ExprKind::Call(call) => {
        // For diagnostics, we want to point at the *callsite* callee. For checks, we additionally
        // follow simple `const` aliases so users cannot bypass bans by storing dangerous values in
        // locals.
        let callee_span_id = expr_unwrap_comma(body, call.callee);
        let callee_span = result
          .expr_spans
          .get(callee_span_id.0 as usize)
          .copied()
          .or_else(|| body.exprs.get(callee_span_id.0 as usize).map(|expr| expr.span))
          .unwrap_or(expr.span);

        let callee_check_id =
          expr_unwrap_comma_and_local_alias(body, call.callee, &local_const_aliases);
        let alias_kind = destructured_aliases
          .get(&callee_check_id)
          .copied()
          .or_else(|| {
            semantic_destructured_aliases.resolve_expr(ExprRef {
              body: body_id,
              expr: call.callee,
            })
          });
        if let Some(alias_kind) = alias_kind {
          match alias_kind {
            DestructuredAliasKind::Eval => {
              if !call.is_new {
                diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                  "`eval` is forbidden when `native_strict` is enabled",
                  Span::new(file, callee_span),
                ));
              }
            }
            DestructuredAliasKind::Function => {
              diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                "`Function` constructor is forbidden when `native_strict` is enabled",
                Span::new(file, callee_span),
              ));
            }
            DestructuredAliasKind::Proxy => {
              if call.is_new {
                diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                  "`Proxy` is forbidden when `native_strict` is enabled",
                  Span::new(file, callee_span),
                ));
              }
            }
            DestructuredAliasKind::ProxyRevocable => {
              diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                "`Proxy` is forbidden when `native_strict` is enabled",
                Span::new(file, callee_span),
              ));
            }
            DestructuredAliasKind::ReflectApply => {
              // Minimal direct `Reflect.apply(target, thisArg, argsList)` handling.
              if !call.is_new {
                if let Some(target_arg) =
                  call.args.first().filter(|arg| !arg.spread).map(|arg| arg.expr)
                {
                  let target_alias_kind = semantic_destructured_aliases.resolve_expr(ExprRef {
                    body: body_id,
                    expr: target_arg,
                  });
                  if expr_is_ident_or_global_this_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    eval_name,
                    "eval",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Eval)) {
                    diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                      "`eval` is forbidden when `native_strict` is enabled",
                      Span::new(file, callee_span),
                    ));
                  }
                  if expr_is_ident_or_global_this_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    "Function",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Function)) {
                    diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                      "`Function` constructor is forbidden when `native_strict` is enabled",
                      Span::new(file, callee_span),
                    ));
                  }
                  if expr_is_ident_or_global_this_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    proxy_name,
                    "Proxy",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    proxy_name,
                    "Proxy",
                    revocable_name,
                    "revocable",
                  ) || matches!(
                    target_alias_kind,
                    Some(DestructuredAliasKind::Proxy | DestructuredAliasKind::ProxyRevocable)
                  ) {
                    diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                      "`Proxy` is forbidden when `native_strict` is enabled",
                      Span::new(file, callee_span),
                    ));
                  }

                  if expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    object_name,
                    "Object",
                    set_prototype_of_name,
                    "setPrototypeOf",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    reflect_name,
                    "Reflect",
                    set_prototype_of_name,
                    "setPrototypeOf",
                  ) || matches!(
                    target_alias_kind,
                    Some(
                      DestructuredAliasKind::ObjectSetPrototypeOf | DestructuredAliasKind::ReflectSetPrototypeOf
                    )
                  ) {
                    diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                      "prototype mutation is forbidden when `native_strict` is enabled",
                      Span::new(file, callee_span),
                    ));
                  }

                  // Handle `Reflect.apply(Object.defineProperty, _, [obj, "prototype", ...])`.
                  let target_is_object_define_property = expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    object_name,
                    "Object",
                    define_property_name,
                    "defineProperty",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ObjectDefineProperty));
                  let target_is_reflect_define_property = expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    reflect_name,
                    "Reflect",
                    define_property_name,
                    "defineProperty",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ReflectDefineProperty));
                  let target_is_object_define_properties = expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    object_name,
                    "Object",
                    define_properties_name,
                    "defineProperties",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ObjectDefineProperties));
                  let target_is_object_assign = expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    object_name,
                    "Object",
                    assign_name,
                    "assign",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ObjectAssign));

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
                            resolver,
                            &const_aliases,
                            ExprRef {
                              body: body_id,
                              expr: target_obj,
                            },
                            prototype_name,
                            proto_name,
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
                            diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                              "prototype mutation is forbidden when `native_strict` is enabled",
                              Span::new(file, callee_span),
                            ));
                          }
                        }
                      }
                    }
                  }

                  // `Reflect.apply(Function.prototype.{call,apply,bind}, target, [...])` can be used
                  // to indirectly invoke (or bind) a forbidden target.
                  let target_is_call_invoker = expr_is_function_prototype_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    prototype_name,
                    call_name,
                    "call",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    "Function",
                    call_name,
                    "call",
                  ) || matches!(
                    target_alias_kind,
                    Some(DestructuredAliasKind::FunctionPrototypeCall)
                  );
                  let target_is_apply_invoker = expr_is_function_prototype_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    prototype_name,
                    apply_name,
                    "apply",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    "Function",
                    apply_name,
                    "apply",
                  ) || matches!(
                    target_alias_kind,
                    Some(DestructuredAliasKind::FunctionPrototypeApply)
                  );
                  let target_is_bind_invoker = expr_is_function_prototype_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    prototype_name,
                    bind_name,
                    "bind",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    "Function",
                    bind_name,
                    "bind",
                  ) || matches!(
                    target_alias_kind,
                    Some(DestructuredAliasKind::FunctionPrototypeBind)
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
                        .unwrap_or(callee_span);
                      let called_target_alias_kind =
                        semantic_destructured_aliases.resolve_expr(ExprRef {
                          body: body_id,
                          expr: called_target,
                        });

                      if expr_is_ident_or_global_this_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        eval_name,
                        "eval",
                      ) || matches!(called_target_alias_kind, Some(DestructuredAliasKind::Eval)) {
                        diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                          "`eval` is forbidden when `native_strict` is enabled",
                          Span::new(file, called_target_span),
                        ));
                      }
                      if expr_is_ident_or_global_this_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        function_name,
                        "Function",
                      ) || matches!(called_target_alias_kind, Some(DestructuredAliasKind::Function))
                      {
                        diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                          "`Function` constructor is forbidden when `native_strict` is enabled",
                          Span::new(file, called_target_span),
                        ));
                      }
                      if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
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
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        proxy_name,
                        "Proxy",
                      ) || expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        proxy_name,
                        "Proxy",
                        revocable_name,
                        "revocable",
                      ) || matches!(
                        called_target_alias_kind,
                        Some(DestructuredAliasKind::Proxy | DestructuredAliasKind::ProxyRevocable)
                      ) {
                        diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                          "`Proxy` is forbidden when `native_strict` is enabled",
                          Span::new(file, called_target_span),
                        ));
                      }

                      if expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        set_prototype_of_name,
                        "setPrototypeOf",
                      ) || expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        reflect_name,
                        "Reflect",
                        set_prototype_of_name,
                        "setPrototypeOf",
                      ) || matches!(
                        called_target_alias_kind,
                        Some(
                          DestructuredAliasKind::ObjectSetPrototypeOf | DestructuredAliasKind::ReflectSetPrototypeOf
                        )
                      ) {
                        let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                        diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                          "prototype mutation is forbidden when `native_strict` is enabled",
                          Span::new(file, span),
                        ));
                      }

                      let mut called_target_is_object_define_property = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        define_property_name,
                        "defineProperty",
                      ) || matches!(
                        called_target_alias_kind,
                        Some(DestructuredAliasKind::ObjectDefineProperty)
                      );
                      let mut called_target_is_reflect_define_property = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        reflect_name,
                        "Reflect",
                        define_property_name,
                        "defineProperty",
                      ) || matches!(
                        called_target_alias_kind,
                        Some(DestructuredAliasKind::ReflectDefineProperty)
                      );
                      let mut called_target_is_object_define_properties = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        define_properties_name,
                        "defineProperties",
                      ) || matches!(
                        called_target_alias_kind,
                        Some(DestructuredAliasKind::ObjectDefineProperties)
                      );
                      let mut called_target_is_object_assign = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        assign_name,
                        "assign",
                      ) || matches!(
                        called_target_alias_kind,
                        Some(DestructuredAliasKind::ObjectAssign)
                      );

                      let mut bound_prefix: Vec<hir_js::ExprId> = Vec::new();
                      if !called_target_is_object_define_property
                        && !called_target_is_reflect_define_property
                        && !called_target_is_object_define_properties
                        && !called_target_is_object_assign
                      {
                        if let Some(called_expr) = body.exprs.get(called_target.0 as usize) {
                          if let ExprKind::Call(bound_call) = &called_expr.kind {
                            if !bound_call.is_new && !bound_call.args.iter().any(|arg| arg.spread) {
                              if let Some(bound_callee) = body.exprs.get(bound_call.callee.0 as usize) {
                                if let ExprKind::Member(bound_member) = &bound_callee.kind {
                                  let prop_is_bind =
                                    object_key_is_ident(&bound_member.property, bind_name)
                                      || object_key_is_string(&bound_member.property, "bind")
                                      || object_key_is_literal_string(body, &bound_member.property, "bind");
                                  if prop_is_bind {
                                    let bound_object_alias_kind =
                                      semantic_destructured_aliases.resolve_expr(ExprRef {
                                        body: body_id,
                                        expr: bound_member.object,
                                      });
                                    let bound_is_object_define_property = expr_is_builtin_member(
                                      resolver,
                                      &const_aliases,
                                      ExprRef {
                                        body: body_id,
                                        expr: bound_member.object,
                                      },
                                      global_this_name,
                                      object_name,
                                      "Object",
                                      define_property_name,
                                      "defineProperty",
                                    ) || matches!(
                                      bound_object_alias_kind,
                                      Some(DestructuredAliasKind::ObjectDefineProperty)
                                    );
                                    let bound_is_reflect_define_property = expr_is_builtin_member(
                                      resolver,
                                      &const_aliases,
                                      ExprRef {
                                        body: body_id,
                                        expr: bound_member.object,
                                      },
                                      global_this_name,
                                      reflect_name,
                                      "Reflect",
                                      define_property_name,
                                      "defineProperty",
                                    ) || matches!(
                                      bound_object_alias_kind,
                                      Some(DestructuredAliasKind::ReflectDefineProperty)
                                    );
                                    let bound_is_object_define_properties = expr_is_builtin_member(
                                      resolver,
                                      &const_aliases,
                                      ExprRef {
                                        body: body_id,
                                        expr: bound_member.object,
                                      },
                                      global_this_name,
                                      object_name,
                                      "Object",
                                      define_properties_name,
                                      "defineProperties",
                                    ) || matches!(
                                      bound_object_alias_kind,
                                      Some(DestructuredAliasKind::ObjectDefineProperties)
                                    );
                                    let bound_is_object_assign = expr_is_builtin_member(
                                      resolver,
                                      &const_aliases,
                                      ExprRef {
                                        body: body_id,
                                        expr: bound_member.object,
                                      },
                                      global_this_name,
                                      object_name,
                                      "Object",
                                      assign_name,
                                      "assign",
                                    ) || matches!(
                                      bound_object_alias_kind,
                                      Some(DestructuredAliasKind::ObjectAssign)
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
                                resolver,
                                &const_aliases,
                                ExprRef {
                                  body: body_id,
                                  expr: target_obj,
                                },
                                prototype_name,
                                proto_name,
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
                }
              }
            }
            DestructuredAliasKind::ReflectConstruct => {
              // Minimal direct `Reflect.construct(target, argsList)` handling.
              if !call.is_new {
                if let Some(target_arg) =
                  call.args.first().filter(|arg| !arg.spread).map(|arg| arg.expr)
                {
                  let target_alias_kind = semantic_destructured_aliases.resolve_expr(ExprRef {
                    body: body_id,
                    expr: target_arg,
                  });
                  if expr_is_ident_or_global_this_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    "Function",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Function)) {
                    diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                      "`Function` constructor is forbidden when `native_strict` is enabled",
                      Span::new(file, callee_span),
                    ));
                  }
                  if expr_is_ident_or_global_this_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    proxy_name,
                    "Proxy",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Proxy)) {
                    diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                      "`Proxy` is forbidden when `native_strict` is enabled",
                      Span::new(file, callee_span),
                    ));
                  }
                }
              }
            }
            DestructuredAliasKind::FunctionPrototypeCall
            | DestructuredAliasKind::FunctionPrototypeApply
            | DestructuredAliasKind::FunctionPrototypeBind => {}
            DestructuredAliasKind::ObjectSetPrototypeOf | DestructuredAliasKind::ReflectSetPrototypeOf => {
              diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                "prototype mutation is forbidden when `native_strict` is enabled",
                Span::new(file, callee_span),
              ));
            }
            DestructuredAliasKind::ObjectDefineProperty
            | DestructuredAliasKind::ReflectDefineProperty
            | DestructuredAliasKind::ObjectDefineProperties
            | DestructuredAliasKind::ObjectAssign => {
              if let Some(first_arg) = call.args.first().map(|arg| arg.expr) {
                let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                  resolver,
                  &const_aliases,
                  ExprRef {
                    body: body_id,
                    expr: first_arg,
                  },
                  prototype_name,
                  proto_name,
                );

                if !is_proto_mutation
                  && matches!(
                    alias_kind,
                    DestructuredAliasKind::ObjectDefineProperty
                      | DestructuredAliasKind::ReflectDefineProperty
                  )
                {
                  if let Some(key_arg) = call.args.get(1).map(|arg| arg.expr) {
                    if expr_is_const_string(body, key_arg, "prototype")
                      || expr_is_const_string(body, key_arg, "__proto__")
                    {
                      is_proto_mutation = true;
                    }
                  }
                }

                if !is_proto_mutation && matches!(alias_kind, DestructuredAliasKind::ObjectDefineProperties) {
                  if let Some(props_arg) = call.args.get(1).map(|arg| arg.expr) {
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

                if !is_proto_mutation && matches!(alias_kind, DestructuredAliasKind::ObjectAssign) {
                  for source_arg in call.args.iter().skip(1).map(|arg| arg.expr) {
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
                    Span::new(file, callee_span),
                  ));
                }
              }
            }
          }
        }

        let callee_check = expr_unwrap_comma_and_alias(
          &const_aliases,
          ExprRef {
            body: body_id,
            expr: call.callee,
          },
        );
        let Some(callee_body) = resolver.body(callee_check.body) else {
          continue;
        };
        let Some(callee) = callee_body.exprs.get(callee_check.expr.0 as usize) else {
          continue;
        };

        if !call.is_new {
          let direct_eval = matches!(&callee.kind, ExprKind::Ident(name) if *name == eval_name);
          let global_eval = match &callee.kind {
            ExprKind::Member(mem) => {
              let prop_is_eval = object_key_is_ident(&mem.property, eval_name)
                || object_key_is_string(&mem.property, "eval")
                || object_key_is_literal_string(callee_body, &mem.property, "eval");
              let obj_is_global_this = expr_is_global_this(
                resolver,
                &const_aliases,
                ExprRef {
                  body: callee_check.body,
                  expr: mem.object,
                },
                global_this_name,
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

          if let ExprKind::Member(member) = &callee.kind {
            let prop_is_call = object_key_is_ident(&member.property, call_name)
              || object_key_is_string(&member.property, "call")
              || object_key_is_literal_string(callee_body, &member.property, "call");
            let prop_is_apply = object_key_is_ident(&member.property, apply_name)
              || object_key_is_string(&member.property, "apply")
              || object_key_is_literal_string(callee_body, &member.property, "apply");
            let prop_is_bind = object_key_is_ident(&member.property, bind_name)
              || object_key_is_string(&member.property, "bind")
              || object_key_is_literal_string(callee_body, &member.property, "bind");
            let is_call_like = prop_is_call || prop_is_apply || prop_is_bind;
            let is_call_or_apply = prop_is_call || prop_is_apply;
            let member_object_alias_kind = if is_call_like {
              semantic_destructured_aliases.resolve_expr(ExprRef {
                body: callee_check.body,
                expr: member.object,
              })
            } else {
              None
            };

            if is_call_like
              && (expr_is_ident_or_global_this_member(
                resolver,
                &const_aliases,
                ExprRef {
                  body: callee_check.body,
                  expr: member.object,
                },
                global_this_name,
                eval_name,
                "eval",
              ) || matches!(member_object_alias_kind, Some(DestructuredAliasKind::Eval)))
            {
              diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                "`eval` is forbidden when `native_strict` is enabled",
                Span::new(file, callee_span),
              ));
            }

            if is_call_like
              && (expr_is_ident_or_global_this_member(
                resolver,
                &const_aliases,
                ExprRef {
                  body: callee_check.body,
                  expr: member.object,
                },
                global_this_name,
                function_name,
                "Function",
              ) || matches!(member_object_alias_kind, Some(DestructuredAliasKind::Function)))
            {
              diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                "`Function` constructor is forbidden when `native_strict` is enabled",
                Span::new(file, callee_span),
              ));
            }

            if is_call_like {
              if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                resolver,
                &const_aliases,
                ExprRef {
                  body: callee_check.body,
                  expr: member.object,
                },
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
              && (expr_is_builtin_member(
                resolver,
                &const_aliases,
                ExprRef {
                  body: callee_check.body,
                  expr: member.object,
                },
                global_this_name,
                proxy_name,
                "Proxy",
                revocable_name,
                "revocable",
              ) || matches!(member_object_alias_kind, Some(DestructuredAliasKind::ProxyRevocable)))
            {
              diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                "`Proxy` is forbidden when `native_strict` is enabled",
                Span::new(file, callee_span),
              ));
            }

            if is_call_like
              && (expr_is_builtin_member(
                resolver,
                &const_aliases,
                ExprRef {
                  body: callee_check.body,
                  expr: member.object,
                },
                global_this_name,
                object_name,
                "Object",
                set_prototype_of_name,
                "setPrototypeOf",
              ) || expr_is_builtin_member(
                resolver,
                &const_aliases,
                ExprRef {
                  body: callee_check.body,
                  expr: member.object,
                },
                global_this_name,
                reflect_name,
                "Reflect",
                set_prototype_of_name,
                "setPrototypeOf",
              ) || matches!(
                member_object_alias_kind,
                Some(
                  DestructuredAliasKind::ObjectSetPrototypeOf | DestructuredAliasKind::ReflectSetPrototypeOf
                )
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
                  resolver,
                  &const_aliases,
                  ExprRef {
                    body: callee_check.body,
                    expr: member.object,
                  },
                  global_this_name,
                  reflect_name,
                  "Reflect",
                  apply_name,
                  "apply",
                ) || matches!(member_object_alias_kind, Some(DestructuredAliasKind::ReflectApply));
                let obj_is_reflect_construct = expr_is_builtin_member(
                  resolver,
                  &const_aliases,
                  ExprRef {
                    body: callee_check.body,
                    expr: member.object,
                  },
                  global_this_name,
                  reflect_name,
                  "Reflect",
                  construct_name,
                  "construct",
                ) || matches!(member_object_alias_kind, Some(DestructuredAliasKind::ReflectConstruct));
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
                    let target_alias_kind = semantic_destructured_aliases.resolve_expr(ExprRef {
                      body: body_id,
                      expr: target_arg,
                    });

                    if expr_is_ident_or_global_this_member(
                      resolver,
                      &const_aliases,
                      ExprRef {
                        body: body_id,
                        expr: target_arg,
                      },
                      global_this_name,
                      function_name,
                      "Function",
                    ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Function)) {
                      diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                        "`Function` constructor is forbidden when `native_strict` is enabled",
                        Span::new(file, target_span),
                      ));
                    }
                    if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                      resolver,
                      &const_aliases,
                      ExprRef {
                        body: body_id,
                        expr: target_arg,
                      },
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
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_arg,
                        },
                        global_this_name,
                        eval_name,
                        "eval",
                      ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Eval)) {
                        diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                          "`eval` is forbidden when `native_strict` is enabled",
                          Span::new(file, target_span),
                        ));
                      }
                      if expr_is_ident_or_global_this_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_arg,
                        },
                        global_this_name,
                        proxy_name,
                        "Proxy",
                      ) || expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_arg,
                        },
                        global_this_name,
                        proxy_name,
                        "Proxy",
                        revocable_name,
                        "revocable",
                      ) || matches!(
                        target_alias_kind,
                        Some(DestructuredAliasKind::Proxy | DestructuredAliasKind::ProxyRevocable)
                      ) {
                        diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                          "`Proxy` is forbidden when `native_strict` is enabled",
                          Span::new(file, target_span),
                        ));
                      }
                    } else if obj_is_reflect_construct {
                      if expr_is_ident_or_global_this_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_arg,
                        },
                        global_this_name,
                        proxy_name,
                        "Proxy",
                      ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Proxy)) {
                        diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                          "`Proxy` is forbidden when `native_strict` is enabled",
                          Span::new(file, target_span),
                        ));
                      }
                    }

                      if obj_is_reflect_apply {
                        let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                        if expr_is_builtin_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          object_name,
                          "Object",
                          set_prototype_of_name,
                          "setPrototypeOf",
                        ) || expr_is_builtin_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          reflect_name,
                          "Reflect",
                          set_prototype_of_name,
                          "setPrototypeOf",
                        ) || matches!(
                          target_alias_kind,
                          Some(
                            DestructuredAliasKind::ObjectSetPrototypeOf | DestructuredAliasKind::ReflectSetPrototypeOf
                          )
                        ) {
                          diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                            "prototype mutation is forbidden when `native_strict` is enabled",
                            Span::new(file, span),
                          ));
                        }

                      let target_is_object_define_property = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_arg,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        define_property_name,
                        "defineProperty",
                      ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ObjectDefineProperty));
                      let target_is_reflect_define_property = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_arg,
                        },
                        global_this_name,
                        reflect_name,
                        "Reflect",
                        define_property_name,
                        "defineProperty",
                      ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ReflectDefineProperty));
                      let target_is_object_define_properties = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_arg,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        define_properties_name,
                        "defineProperties",
                      ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ObjectDefineProperties));
                      let target_is_object_assign = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_arg,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        assign_name,
                        "assign",
                      ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ObjectAssign));
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
                                resolver,
                                &const_aliases,
                                ExprRef {
                                  body: body_id,
                                  expr: target_obj,
                                },
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
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: callee_check.body,
                      expr: member.object,
                    },
                    global_this_name,
                    function_name,
                    prototype_name,
                    call_name,
                    "call",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: callee_check.body,
                      expr: member.object,
                    },
                    global_this_name,
                    function_name,
                    "Function",
                    call_name,
                    "call",
                  ) || matches!(
                    member_object_alias_kind,
                    Some(DestructuredAliasKind::FunctionPrototypeCall)
                  );
                  let obj_is_apply_invoker = expr_is_function_prototype_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: callee_check.body,
                      expr: member.object,
                    },
                    global_this_name,
                    function_name,
                    prototype_name,
                    apply_name,
                    "apply",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: callee_check.body,
                      expr: member.object,
                    },
                    global_this_name,
                    function_name,
                    "Function",
                    apply_name,
                    "apply",
                  ) || matches!(
                    member_object_alias_kind,
                    Some(DestructuredAliasKind::FunctionPrototypeApply)
                  );
                  let obj_is_bind_invoker = expr_is_function_prototype_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: callee_check.body,
                      expr: member.object,
                    },
                    global_this_name,
                    function_name,
                    prototype_name,
                    bind_name,
                    "bind",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: callee_check.body,
                      expr: member.object,
                    },
                    global_this_name,
                    function_name,
                    "Function",
                    bind_name,
                    "bind",
                  ) || matches!(
                    member_object_alias_kind,
                    Some(DestructuredAliasKind::FunctionPrototypeBind)
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
                    let target_alias_kind = semantic_destructured_aliases.resolve_expr(ExprRef {
                      body: body_id,
                      expr: target_arg,
                    });

                    if expr_is_ident_or_global_this_member(
                      resolver,
                      &const_aliases,
                      ExprRef {
                        body: body_id,
                        expr: target_arg,
                      },
                      global_this_name,
                      eval_name,
                      "eval",
                    ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Eval)) {
                      diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                        "`eval` is forbidden when `native_strict` is enabled",
                        Span::new(file, target_span),
                      ));
                    }
                    if expr_is_ident_or_global_this_member(
                      resolver,
                      &const_aliases,
                      ExprRef {
                        body: body_id,
                        expr: target_arg,
                      },
                      global_this_name,
                      function_name,
                      "Function",
                    ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Function)) {
                      diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                        "`Function` constructor is forbidden when `native_strict` is enabled",
                        Span::new(file, target_span),
                      ));
                    }
                    if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                      resolver,
                      &const_aliases,
                      ExprRef {
                        body: body_id,
                        expr: target_arg,
                      },
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
                      resolver,
                      &const_aliases,
                      ExprRef {
                        body: body_id,
                        expr: target_arg,
                      },
                      global_this_name,
                      proxy_name,
                      "Proxy",
                    ) || expr_is_builtin_member(
                      resolver,
                      &const_aliases,
                      ExprRef {
                        body: body_id,
                        expr: target_arg,
                      },
                      global_this_name,
                      proxy_name,
                      "Proxy",
                      revocable_name,
                      "revocable",
                    ) || matches!(
                      target_alias_kind,
                      Some(DestructuredAliasKind::Proxy | DestructuredAliasKind::ProxyRevocable)
                    ) {
                      diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                        "`Proxy` is forbidden when `native_strict` is enabled",
                        Span::new(file, target_span),
                      ));
                    }

                    if expr_is_builtin_member(
                      resolver,
                      &const_aliases,
                      ExprRef {
                        body: body_id,
                        expr: target_arg,
                      },
                      global_this_name,
                      object_name,
                      "Object",
                      set_prototype_of_name,
                      "setPrototypeOf",
                    ) || expr_is_builtin_member(
                      resolver,
                      &const_aliases,
                      ExprRef {
                        body: body_id,
                        expr: target_arg,
                      },
                      global_this_name,
                      reflect_name,
                      "Reflect",
                      set_prototype_of_name,
                      "setPrototypeOf",
                    ) || matches!(
                      target_alias_kind,
                      Some(
                        DestructuredAliasKind::ObjectSetPrototypeOf | DestructuredAliasKind::ReflectSetPrototypeOf
                      )
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
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_arg,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        define_property_name,
                        "defineProperty",
                      ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ObjectDefineProperty));
                      let mut target_is_reflect_define_property = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_arg,
                        },
                        global_this_name,
                        reflect_name,
                        "Reflect",
                        define_property_name,
                        "defineProperty",
                      ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ReflectDefineProperty));
                      let mut target_is_object_define_properties = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_arg,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        define_properties_name,
                        "defineProperties",
                      ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ObjectDefineProperties));
                      let mut target_is_object_assign = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_arg,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        assign_name,
                        "assign",
                      ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ObjectAssign));

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
                                      let bound_object_alias_kind =
                                        semantic_destructured_aliases.resolve_expr(ExprRef {
                                          body: body_id,
                                          expr: bound_member.object,
                                        });
                                      let bound_is_object_define_property = expr_is_builtin_member(
                                        resolver,
                                        &const_aliases,
                                        ExprRef {
                                          body: body_id,
                                          expr: bound_member.object,
                                        },
                                        global_this_name,
                                        object_name,
                                        "Object",
                                        define_property_name,
                                        "defineProperty",
                                      ) || matches!(
                                        bound_object_alias_kind,
                                        Some(DestructuredAliasKind::ObjectDefineProperty)
                                      );
                                      let bound_is_reflect_define_property = expr_is_builtin_member(
                                        resolver,
                                        &const_aliases,
                                        ExprRef {
                                          body: body_id,
                                          expr: bound_member.object,
                                        },
                                        global_this_name,
                                        reflect_name,
                                        "Reflect",
                                        define_property_name,
                                        "defineProperty",
                                      ) || matches!(
                                        bound_object_alias_kind,
                                        Some(DestructuredAliasKind::ReflectDefineProperty)
                                      );
                                      let bound_is_object_define_properties = expr_is_builtin_member(
                                        resolver,
                                        &const_aliases,
                                        ExprRef {
                                          body: body_id,
                                          expr: bound_member.object,
                                        },
                                        global_this_name,
                                        object_name,
                                        "Object",
                                        define_properties_name,
                                        "defineProperties",
                                      ) || matches!(
                                        bound_object_alias_kind,
                                        Some(DestructuredAliasKind::ObjectDefineProperties)
                                      );
                                      let bound_is_object_assign = expr_is_builtin_member(
                                        resolver,
                                        &const_aliases,
                                        ExprRef {
                                          body: body_id,
                                          expr: bound_member.object,
                                        },
                                        global_this_name,
                                        object_name,
                                        "Object",
                                        assign_name,
                                        "assign",
                                      ) || matches!(bound_object_alias_kind, Some(DestructuredAliasKind::ObjectAssign));
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
                            resolver,
                            &const_aliases,
                            ExprRef {
                              body: body_id,
                              expr: target_obj,
                            },
                            prototype_name,
                            proto_name,
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
                            resolver,
                            &const_aliases,
                            ExprRef {
                              body: body_id,
                              expr: target_obj,
                            },
                            prototype_name,
                            proto_name,
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
                            resolver,
                            &const_aliases,
                            ExprRef {
                              body: body_id,
                              expr: target_obj,
                            },
                            prototype_name,
                            proto_name,
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
                  resolver,
                  &const_aliases,
                  ExprRef {
                    body: callee_check.body,
                    expr: member.object,
                  },
                  global_this_name,
                  reflect_name,
                  "Reflect",
                  apply_name,
                  "apply",
                ) || matches!(member_object_alias_kind, Some(DestructuredAliasKind::ReflectApply));
                let obj_is_reflect_construct = expr_is_builtin_member(
                  resolver,
                  &const_aliases,
                  ExprRef {
                    body: callee_check.body,
                    expr: member.object,
                  },
                  global_this_name,
                  reflect_name,
                  "Reflect",
                  construct_name,
                  "construct",
                ) || matches!(
                  member_object_alias_kind,
                  Some(DestructuredAliasKind::ReflectConstruct)
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
                        let target_alias_kind = semantic_destructured_aliases.resolve_expr(ExprRef {
                          body: body_id,
                          expr: target_arg,
                        });

                        if expr_is_ident_or_global_this_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          eval_name,
                          "eval",
                        ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Eval)) {
                          diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                            "`eval` is forbidden when `native_strict` is enabled",
                            Span::new(file, target_span),
                          ));
                        }
                        if expr_is_ident_or_global_this_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          function_name,
                          "Function",
                        ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Function)) {
                          diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                            "`Function` constructor is forbidden when `native_strict` is enabled",
                            Span::new(file, target_span),
                          ));
                        }
                        if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
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
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          proxy_name,
                          "Proxy",
                        ) || expr_is_builtin_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          proxy_name,
                          "Proxy",
                          revocable_name,
                          "revocable",
                        ) || matches!(
                          target_alias_kind,
                          Some(DestructuredAliasKind::Proxy | DestructuredAliasKind::ProxyRevocable)
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
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          function_name,
                          prototype_name,
                          call_name,
                          "call",
                        ) || expr_is_builtin_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          function_name,
                          "Function",
                          call_name,
                          "call",
                        ) || matches!(
                          target_alias_kind,
                          Some(DestructuredAliasKind::FunctionPrototypeCall)
                        );
                        let target_is_apply_invoker = expr_is_function_prototype_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          function_name,
                          prototype_name,
                          apply_name,
                          "apply",
                        ) || expr_is_builtin_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          function_name,
                          "Function",
                          apply_name,
                          "apply",
                        ) || matches!(
                          target_alias_kind,
                          Some(DestructuredAliasKind::FunctionPrototypeApply)
                        );
                        let target_is_bind_invoker = expr_is_function_prototype_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          function_name,
                          prototype_name,
                          bind_name,
                          "bind",
                        ) || expr_is_builtin_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          function_name,
                          "Function",
                          bind_name,
                          "bind",
                        ) || matches!(
                          target_alias_kind,
                          Some(DestructuredAliasKind::FunctionPrototypeBind)
                        );
                        if target_is_call_invoker || target_is_apply_invoker || target_is_bind_invoker {
                          if let Some(called_target) = reflect_args.get(1).copied() {
                            let called_target_span = result
                              .expr_spans
                              .get(called_target.0 as usize)
                              .copied()
                              .or_else(|| body.exprs.get(called_target.0 as usize).map(|expr| expr.span))
                              .unwrap_or(target_span);
                            let called_target_alias_kind =
                              semantic_destructured_aliases.resolve_expr(ExprRef {
                                body: body_id,
                                expr: called_target,
                              });
  
                            if expr_is_ident_or_global_this_member(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: called_target,
                              },
                              global_this_name,
                              eval_name,
                              "eval",
                            ) || matches!(called_target_alias_kind, Some(DestructuredAliasKind::Eval))
                            {
                              diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                                "`eval` is forbidden when `native_strict` is enabled",
                                Span::new(file, called_target_span),
                              ));
                            }
                            if expr_is_ident_or_global_this_member(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: called_target,
                              },
                              global_this_name,
                              function_name,
                              "Function",
                            ) || matches!(
                              called_target_alias_kind,
                              Some(DestructuredAliasKind::Function)
                            ) {
                              diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                                "`Function` constructor is forbidden when `native_strict` is enabled",
                                Span::new(file, called_target_span),
                              ));
                            }
                            if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: called_target,
                              },
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
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: called_target,
                              },
                              global_this_name,
                              proxy_name,
                              "Proxy",
                            ) || expr_is_builtin_member(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: called_target,
                              },
                              global_this_name,
                              proxy_name,
                              "Proxy",
                              revocable_name,
                              "revocable",
                            ) || matches!(
                              called_target_alias_kind,
                              Some(DestructuredAliasKind::Proxy | DestructuredAliasKind::ProxyRevocable)
                            ) {
                              diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                                "`Proxy` is forbidden when `native_strict` is enabled",
                                Span::new(file, called_target_span),
                              ));
                            }
 
                            if expr_is_builtin_member(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: called_target,
                              },
                              global_this_name,
                              object_name,
                              "Object",
                              set_prototype_of_name,
                              "setPrototypeOf",
                            ) || expr_is_builtin_member(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: called_target,
                              },
                              global_this_name,
                              reflect_name,
                              "Reflect",
                              set_prototype_of_name,
                              "setPrototypeOf",
                            ) || matches!(
                              called_target_alias_kind,
                              Some(
                                DestructuredAliasKind::ObjectSetPrototypeOf | DestructuredAliasKind::ReflectSetPrototypeOf
                              )
                            ) {
                              let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                              diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                                "prototype mutation is forbidden when `native_strict` is enabled",
                                Span::new(file, span),
                              ));
                            }
 
                            let called_target_is_object_define_property = expr_is_builtin_member(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: called_target,
                              },
                              global_this_name,
                              object_name,
                              "Object",
                              define_property_name,
                              "defineProperty",
                            ) || matches!(
                              called_target_alias_kind,
                              Some(DestructuredAliasKind::ObjectDefineProperty)
                            );
                            let called_target_is_reflect_define_property = expr_is_builtin_member(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: called_target,
                              },
                              global_this_name,
                              reflect_name,
                              "Reflect",
                              define_property_name,
                              "defineProperty",
                            ) || matches!(
                              called_target_alias_kind,
                              Some(DestructuredAliasKind::ReflectDefineProperty)
                            );
                            let called_target_is_object_define_properties = expr_is_builtin_member(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: called_target,
                              },
                              global_this_name,
                              object_name,
                              "Object",
                              define_properties_name,
                              "defineProperties",
                            ) || matches!(
                              called_target_alias_kind,
                              Some(DestructuredAliasKind::ObjectDefineProperties)
                            );
                            let called_target_is_object_assign = expr_is_builtin_member(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: called_target,
                              },
                              global_this_name,
                              object_name,
                              "Object",
                              assign_name,
                              "assign",
                            ) || matches!(
                              called_target_alias_kind,
                              Some(DestructuredAliasKind::ObjectAssign)
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
                                      resolver,
                                      &const_aliases,
                                      ExprRef {
                                        body: body_id,
                                        expr: target_obj,
                                      },
                                      prototype_name,
                                      proto_name,
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
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          object_name,
                          "Object",
                          set_prototype_of_name,
                          "setPrototypeOf",
                        ) || expr_is_builtin_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          reflect_name,
                          "Reflect",
                          set_prototype_of_name,
                          "setPrototypeOf",
                        ) || matches!(
                          target_alias_kind,
                          Some(
                            DestructuredAliasKind::ObjectSetPrototypeOf | DestructuredAliasKind::ReflectSetPrototypeOf
                          )
                        ) {
                          let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                          diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                            "prototype mutation is forbidden when `native_strict` is enabled",
                            Span::new(file, span),
                          ));
                        }

                        let target_is_object_define_property = expr_is_builtin_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          object_name,
                          "Object",
                          define_property_name,
                          "defineProperty",
                        ) || matches!(
                          target_alias_kind,
                          Some(DestructuredAliasKind::ObjectDefineProperty)
                        );
                        let target_is_reflect_define_property = expr_is_builtin_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          reflect_name,
                          "Reflect",
                          define_property_name,
                          "defineProperty",
                        ) || matches!(
                          target_alias_kind,
                          Some(DestructuredAliasKind::ReflectDefineProperty)
                        );
                        let target_is_object_define_properties = expr_is_builtin_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          object_name,
                          "Object",
                          define_properties_name,
                          "defineProperties",
                        ) || matches!(
                          target_alias_kind,
                          Some(DestructuredAliasKind::ObjectDefineProperties)
                        );
                        let target_is_object_assign = expr_is_builtin_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          object_name,
                          "Object",
                          assign_name,
                          "assign",
                        ) || matches!(
                          target_alias_kind,
                          Some(DestructuredAliasKind::ObjectAssign)
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
                                  resolver,
                                  &const_aliases,
                                  ExprRef {
                                    body: body_id,
                                    expr: target_obj,
                                  },
                                  prototype_name,
                                  proto_name,
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
                        let target_alias_kind = semantic_destructured_aliases.resolve_expr(ExprRef {
                          body: body_id,
                          expr: target_arg,
                        });

                        if expr_is_ident_or_global_this_member(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          function_name,
                          "Function",
                        ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Function)) {
                          diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                            "`Function` constructor is forbidden when `native_strict` is enabled",
                            Span::new(file, target_span),
                          ));
                        }
                        if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
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
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_arg,
                          },
                          global_this_name,
                          proxy_name,
                          "Proxy",
                        ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Proxy)) {
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
                  resolver,
                  &const_aliases,
                  ExprRef {
                    body: callee_check.body,
                    expr: member.object,
                  },
                  global_this_name,
                  object_name,
                  "Object",
                  define_property_name,
                  "defineProperty",
                ) || matches!(member_object_alias_kind, Some(DestructuredAliasKind::ObjectDefineProperty));
                let obj_is_reflect_define_property = expr_is_builtin_member(
                  resolver,
                  &const_aliases,
                  ExprRef {
                    body: callee_check.body,
                    expr: member.object,
                  },
                  global_this_name,
                  reflect_name,
                  "Reflect",
                  define_property_name,
                  "defineProperty",
                ) || matches!(member_object_alias_kind, Some(DestructuredAliasKind::ReflectDefineProperty));
                let obj_is_object_define_properties = expr_is_builtin_member(
                  resolver,
                  &const_aliases,
                  ExprRef {
                    body: callee_check.body,
                    expr: member.object,
                  },
                  global_this_name,
                  object_name,
                  "Object",
                  define_properties_name,
                  "defineProperties",
                ) || matches!(
                  member_object_alias_kind,
                  Some(DestructuredAliasKind::ObjectDefineProperties)
                );
                let obj_is_object_assign = expr_is_builtin_member(
                  resolver,
                  &const_aliases,
                  ExprRef {
                    body: callee_check.body,
                    expr: member.object,
                  },
                  global_this_name,
                  object_name,
                  "Object",
                  assign_name,
                  "assign",
                ) || matches!(member_object_alias_kind, Some(DestructuredAliasKind::ObjectAssign));

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
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_obj,
                        },
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
                          resolver,
                          &const_aliases,
                          ExprRef {
                            body: body_id,
                            expr: target_obj,
                          },
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
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: target_obj,
                        },
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
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: target_obj,
                              },
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
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: target_obj,
                              },
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
                            resolver,
                            &const_aliases,
                            ExprRef {
                              body: body_id,
                              expr: target_obj,
                            },
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

            // This block inspects nested expressions under the callee (e.g. bound call helpers).
            // Those expression IDs are body-local; only run the analysis when the callee comes from
            // the same body as the callsite to avoid mixing expression IDs across bodies.
            if callee_check.body == body_id {
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
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: bind_member.object,
                              },
                              global_this_name,
                              object_name,
                              "Object",
                              define_property_name,
                              "defineProperty",
                            );
                            let is_reflect_define_property = expr_is_builtin_member(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: bind_member.object,
                              },
                              global_this_name,
                              reflect_name,
                              "Reflect",
                              define_property_name,
                              "defineProperty",
                            );
                            let is_object_define_properties = expr_is_builtin_member(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: bind_member.object,
                              },
                              global_this_name,
                              object_name,
                              "Object",
                              define_properties_name,
                              "defineProperties",
                            );
                            let is_object_assign = expr_is_builtin_member(
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: bind_member.object,
                              },
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
                                        resolver,
                                        &const_aliases,
                                        ExprRef {
                                          body: body_id,
                                          expr: first_arg,
                                        },
                                        prototype_name,
                                        proto_name,
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
                                        resolver,
                                        &const_aliases,
                                        ExprRef {
                                          body: body_id,
                                          expr: first_arg,
                                        },
                                        prototype_name,
                                        proto_name,
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
                                        resolver,
                                        &const_aliases,
                                        ExprRef {
                                          body: body_id,
                                          expr: first_arg,
                                        },
                                        prototype_name,
                                        proto_name,
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
                                            resolver,
                                            &const_aliases,
                                            ExprRef {
                                              body: body_id,
                                              expr: first_arg,
                                            },
                                            prototype_name,
                                            proto_name,
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
                                            resolver,
                                            &const_aliases,
                                            ExprRef {
                                              body: body_id,
                                              expr: first_arg,
                                            },
                                            prototype_name,
                                            proto_name,
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
                                          resolver,
                                          &const_aliases,
                                          ExprRef {
                                            body: body_id,
                                            expr: first_arg,
                                          },
                                          prototype_name,
                                          proto_name,
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

            }

            if callee_check.body == body_id {
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
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: bound_member.object,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        define_property_name,
                        "defineProperty",
                      );
                      let is_reflect_define_property = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: bound_member.object,
                        },
                        global_this_name,
                        reflect_name,
                        "Reflect",
                        define_property_name,
                        "defineProperty",
                      );
                      let is_object_define_properties = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: bound_member.object,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        define_properties_name,
                        "defineProperties",
                      );
                      let is_object_assign = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: bound_member.object,
                        },
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
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: first_arg,
                              },
                              prototype_name,
                              proto_name,
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
                                resolver,
                                &const_aliases,
                                ExprRef {
                                  body: body_id,
                                  expr: first_arg,
                                },
                                prototype_name,
                                proto_name,
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
                              resolver,
                              &const_aliases,
                              ExprRef {
                                body: body_id,
                                expr: first_arg,
                              },
                              prototype_name,
                              proto_name,
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
            }

            if let ExprKind::Member(member) = &callee.kind {
              let obj_is_reflect = expr_is_ident_or_global_this_member(
                resolver,
                &const_aliases,
                ExprRef {
                  body: callee_check.body,
                  expr: member.object,
                },
                global_this_name,
                reflect_name,
                "Reflect",
              );
              let prop_is_apply =
                object_key_is_ident(&member.property, apply_name)
                  || object_key_is_string(&member.property, "apply")
                  || object_key_is_literal_string(callee_body, &member.property, "apply");
              if obj_is_reflect && prop_is_apply {
                if let Some(target_arg) = call.args.first().filter(|arg| !arg.spread).map(|arg| arg.expr) {
                  let target_span = result
                    .expr_spans
                    .get(target_arg.0 as usize)
                    .copied()
                    .or_else(|| body.exprs.get(target_arg.0 as usize).map(|expr| expr.span))
                    .unwrap_or(callee_span);
                  let target_alias_kind = semantic_destructured_aliases.resolve_expr(ExprRef {
                    body: body_id,
                    expr: target_arg,
                  });

                  if expr_is_ident_or_global_this_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    eval_name,
                    "eval",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Eval)) {
                    diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                      "`eval` is forbidden when `native_strict` is enabled",
                      Span::new(file, target_span),
                    ));
                  }
                  if expr_is_ident_or_global_this_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    "Function",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::Function)) {
                    diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                      "`Function` constructor is forbidden when `native_strict` is enabled",
                      Span::new(file, target_span),
                    ));
                  }
                  if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
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
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    proxy_name,
                    "Proxy",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    proxy_name,
                    "Proxy",
                    revocable_name,
                    "revocable",
                  ) || matches!(
                    target_alias_kind,
                    Some(DestructuredAliasKind::Proxy | DestructuredAliasKind::ProxyRevocable)
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
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    prototype_name,
                    call_name,
                    "call",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    "Function",
                    call_name,
                    "call",
                  ) || matches!(
                    target_alias_kind,
                    Some(DestructuredAliasKind::FunctionPrototypeCall)
                  );
                  let target_is_apply_invoker = expr_is_function_prototype_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    prototype_name,
                    apply_name,
                    "apply",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    "Function",
                    apply_name,
                    "apply",
                  ) || matches!(
                    target_alias_kind,
                    Some(DestructuredAliasKind::FunctionPrototypeApply)
                  );
                  let target_is_bind_invoker = expr_is_function_prototype_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    prototype_name,
                    bind_name,
                    "bind",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    function_name,
                    "Function",
                    bind_name,
                    "bind",
                  ) || matches!(
                    target_alias_kind,
                    Some(DestructuredAliasKind::FunctionPrototypeBind)
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
                    let called_target_alias_kind =
                      semantic_destructured_aliases.resolve_expr(ExprRef {
                        body: body_id,
                        expr: called_target,
                      });

                    if expr_is_ident_or_global_this_member(
                      resolver,
                      &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                      global_this_name,
                      eval_name,
                      "eval",
                    ) || matches!(called_target_alias_kind, Some(DestructuredAliasKind::Eval)) {
                      diagnostics.push(codes::NATIVE_STRICT_EVAL.error(
                        "`eval` is forbidden when `native_strict` is enabled",
                        Span::new(file, called_target_span),
                      ));
                      }
                      if expr_is_ident_or_global_this_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                      global_this_name,
                      function_name,
                      "Function",
                    ) || matches!(
                      called_target_alias_kind,
                      Some(DestructuredAliasKind::Function)
                    ) {
                      diagnostics.push(codes::NATIVE_STRICT_NEW_FUNCTION.error(
                        "`Function` constructor is forbidden when `native_strict` is enabled",
                        Span::new(file, called_target_span),
                      ));
                      }
                      if let Some(constructor_span) = expr_is_function_constructor_via_constructor_access(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
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
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                      global_this_name,
                      proxy_name,
                      "Proxy",
                    ) || expr_is_builtin_member(
                      resolver,
                      &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        proxy_name,
                      "Proxy",
                      revocable_name,
                      "revocable",
                    ) || matches!(
                      called_target_alias_kind,
                      Some(DestructuredAliasKind::Proxy | DestructuredAliasKind::ProxyRevocable)
                    ) {
                      diagnostics.push(codes::NATIVE_STRICT_PROXY.error(
                        "`Proxy` is forbidden when `native_strict` is enabled",
                        Span::new(file, called_target_span),
                      ));
                      }

                      if expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        set_prototype_of_name,
                        "setPrototypeOf",
                      ) || expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        reflect_name,
                      "Reflect",
                      set_prototype_of_name,
                      "setPrototypeOf",
                    ) || matches!(
                      called_target_alias_kind,
                      Some(
                        DestructuredAliasKind::ObjectSetPrototypeOf | DestructuredAliasKind::ReflectSetPrototypeOf
                      )
                    ) {
                      let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                      diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                        "prototype mutation is forbidden when `native_strict` is enabled",
                        Span::new(file, span),
                        ));
                      }

                      let mut called_target_is_object_define_property = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        define_property_name,
                        "defineProperty",
                      ) || matches!(
                        called_target_alias_kind,
                        Some(DestructuredAliasKind::ObjectDefineProperty)
                      );
                      let mut called_target_is_reflect_define_property = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        reflect_name,
                        "Reflect",
                        define_property_name,
                        "defineProperty",
                      ) || matches!(
                        called_target_alias_kind,
                        Some(DestructuredAliasKind::ReflectDefineProperty)
                      );
                      let mut called_target_is_object_define_properties = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        define_properties_name,
                        "defineProperties",
                      ) || matches!(
                        called_target_alias_kind,
                        Some(DestructuredAliasKind::ObjectDefineProperties)
                      );
                      let mut called_target_is_object_assign = expr_is_builtin_member(
                        resolver,
                        &const_aliases,
                        ExprRef {
                          body: body_id,
                          expr: called_target,
                        },
                        global_this_name,
                        object_name,
                        "Object",
                        assign_name,
                        "assign",
                      ) || matches!(
                        called_target_alias_kind,
                        Some(DestructuredAliasKind::ObjectAssign)
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
                                      let bound_object_alias_kind =
                                        semantic_destructured_aliases.resolve_expr(ExprRef {
                                          body: body_id,
                                          expr: bound_member.object,
                                        });
                                      let bound_is_object_define_property = expr_is_builtin_member(
                                        resolver,
                                        &const_aliases,
                                        ExprRef {
                                        body: body_id,
                                        expr: bound_member.object,
                                      },
                                      global_this_name,
                                        object_name,
                                        "Object",
                                        define_property_name,
                                        "defineProperty",
                                      ) || matches!(
                                        bound_object_alias_kind,
                                        Some(DestructuredAliasKind::ObjectDefineProperty)
                                      );
                                      let bound_is_reflect_define_property = expr_is_builtin_member(
                                        resolver,
                                        &const_aliases,
                                        ExprRef {
                                        body: body_id,
                                        expr: bound_member.object,
                                      },
                                      global_this_name,
                                        reflect_name,
                                        "Reflect",
                                        define_property_name,
                                        "defineProperty",
                                      ) || matches!(
                                        bound_object_alias_kind,
                                        Some(DestructuredAliasKind::ReflectDefineProperty)
                                      );
                                      let bound_is_object_define_properties = expr_is_builtin_member(
                                        resolver,
                                        &const_aliases,
                                        ExprRef {
                                        body: body_id,
                                        expr: bound_member.object,
                                      },
                                      global_this_name,
                                        object_name,
                                        "Object",
                                        define_properties_name,
                                        "defineProperties",
                                      ) || matches!(
                                        bound_object_alias_kind,
                                        Some(DestructuredAliasKind::ObjectDefineProperties)
                                      );
                                      let bound_is_object_assign = expr_is_builtin_member(
                                        resolver,
                                        &const_aliases,
                                        ExprRef {
                                        body: body_id,
                                        expr: bound_member.object,
                                      },
                                      global_this_name,
                                        object_name,
                                        "Object",
                                        assign_name,
                                        "assign",
                                      ) || matches!(
                                        bound_object_alias_kind,
                                        Some(DestructuredAliasKind::ObjectAssign)
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
                                resolver,
                                &const_aliases,
                                ExprRef {
                                  body: body_id,
                                  expr: target_obj,
                                },
                                prototype_name,
                                proto_name,
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
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    object_name,
                    "Object",
                    set_prototype_of_name,
                    "setPrototypeOf",
                  ) || expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    reflect_name,
                    "Reflect",
                    set_prototype_of_name,
                    "setPrototypeOf",
                  ) || matches!(
                    target_alias_kind,
                    Some(
                      DestructuredAliasKind::ObjectSetPrototypeOf | DestructuredAliasKind::ReflectSetPrototypeOf
                    )
                  ) {
                    let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
                    diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
                      "prototype mutation is forbidden when `native_strict` is enabled",
                      Span::new(file, span),
                    ));
                  }

                  let target_is_object_define_property = expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    object_name,
                    "Object",
                    define_property_name,
                    "defineProperty",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ObjectDefineProperty));
                  let target_is_reflect_define_property = expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    reflect_name,
                    "Reflect",
                    define_property_name,
                    "defineProperty",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ReflectDefineProperty));
                  let target_is_object_define_properties = expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    object_name,
                    "Object",
                    define_properties_name,
                    "defineProperties",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ObjectDefineProperties));
                  let target_is_object_assign = expr_is_builtin_member(
                    resolver,
                    &const_aliases,
                    ExprRef {
                      body: body_id,
                      expr: target_arg,
                    },
                    global_this_name,
                    object_name,
                    "Object",
                    assign_name,
                    "assign",
                  ) || matches!(target_alias_kind, Some(DestructuredAliasKind::ObjectAssign));

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
                            resolver,
                            &const_aliases,
                            ExprRef {
                              body: body_id,
                              expr: target_obj,
                            },
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
                                resolver,
                                &const_aliases,
                                ExprRef {
                                  body: body_id,
                                  expr: bound_member.object,
                                },
                                global_this_name,
                                object_name,
                                "Object",
                                define_property_name,
                                "defineProperty",
                              );
                              let is_reflect_define_property = expr_is_builtin_member(
                                resolver,
                                &const_aliases,
                                ExprRef {
                                  body: body_id,
                                  expr: bound_member.object,
                                },
                                global_this_name,
                                reflect_name,
                                "Reflect",
                                define_property_name,
                                "defineProperty",
                              );
                              let is_object_define_properties = expr_is_builtin_member(
                                resolver,
                                &const_aliases,
                                ExprRef {
                                  body: body_id,
                                  expr: bound_member.object,
                                },
                                global_this_name,
                                object_name,
                                "Object",
                                define_properties_name,
                                "defineProperties",
                              );
                              let is_object_assign = expr_is_builtin_member(
                                resolver,
                                &const_aliases,
                                ExprRef {
                                  body: body_id,
                                  expr: bound_member.object,
                                },
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
                                          resolver,
                                          &const_aliases,
                                          ExprRef {
                                            body: body_id,
                                            expr: first_arg,
                                          },
                                          prototype_name,
                                          proto_name,
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
                                            resolver,
                                            &const_aliases,
                                            ExprRef {
                                              body: body_id,
                                              expr: first_arg,
                                            },
                                            prototype_name,
                                            proto_name,
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
                                          resolver,
                                          &const_aliases,
                                          ExprRef {
                                            body: body_id,
                                            expr: first_arg,
                                          },
                                          prototype_name,
                                          proto_name,
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
            resolver,
            &const_aliases,
            ExprRef {
              body: body_id,
              expr: call.callee,
            },
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
            resolver,
            &const_aliases,
            ExprRef {
              body: body_id,
              expr: call.callee,
            },
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
              resolver,
              &const_aliases,
              ExprRef {
                body: body_id,
                expr: call.callee,
              },
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
              resolver,
              &const_aliases,
              ExprRef {
                body: callee_check.body,
                expr: member.object,
              },
              global_this_name,
              proxy_name,
              "Proxy",
            );
            let obj_is_object = expr_is_ident_or_global_this_member(
              resolver,
              &const_aliases,
              ExprRef {
                body: callee_check.body,
                expr: member.object,
              },
              global_this_name,
              object_name,
              "Object",
            );
            let obj_is_reflect = expr_is_ident_or_global_this_member(
              resolver,
              &const_aliases,
              ExprRef {
                body: callee_check.body,
                expr: member.object,
              },
              global_this_name,
              reflect_name,
              "Reflect",
            );

            if obj_is_proxy
              && (object_key_is_ident(&member.property, revocable_name)
                || object_key_is_string(&member.property, "revocable")
                || object_key_is_literal_string(callee_body, &member.property, "revocable"))
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
                || object_key_is_literal_string(callee_body, &member.property, "construct");
            if obj_is_reflect && prop_is_construct {
              if let Some(target_arg) = call.args.first().filter(|arg| !arg.spread).map(|arg| arg.expr) {
                let target_span = result
                  .expr_spans
                  .get(target_arg.0 as usize)
                  .copied()
                  .or_else(|| body.exprs.get(target_arg.0 as usize).map(|expr| expr.span))
                  .unwrap_or(callee_span);
                if expr_is_ident_or_global_this_member(
                  resolver,
                  &const_aliases,
                  ExprRef {
                    body: body_id,
                    expr: target_arg,
                  },
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
                  resolver,
                  &const_aliases,
                  ExprRef {
                    body: body_id,
                    expr: target_arg,
                  },
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
                  resolver,
                  &const_aliases,
                  ExprRef {
                    body: body_id,
                    expr: target_arg,
                  },
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
                || object_key_is_literal_string(callee_body, &member.property, "setPrototypeOf"))
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
                  || object_key_is_literal_string(callee_body, &member.property, "defineProperty");
                let is_define_properties = object_key_is_ident(&member.property, define_properties_name)
                  || object_key_is_string(&member.property, "defineProperties")
                  || object_key_is_literal_string(callee_body, &member.property, "defineProperties");
                let is_assign = object_key_is_ident(&member.property, assign_name)
                  || object_key_is_string(&member.property, "assign")
                  || object_key_is_literal_string(callee_body, &member.property, "assign");

              let is_object_define_property = obj_is_object && is_define_property;
              let is_object_define = obj_is_object && (is_define_property || is_define_properties || is_assign);
              let is_reflect_define_property = obj_is_reflect && is_define_property;
              let is_reflect_define = is_reflect_define_property;

              let mut is_proto_mutation = expr_chain_contains_proto_mutation(
                resolver,
                &const_aliases,
                ExprRef {
                  body: body_id,
                  expr: first_arg,
                },
                prototype_name,
                proto_name,
              );
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
      ExprKind::Assignment { target, .. } => {
        if pat_contains_proto_mutation(
          body,
          body_id,
          *target,
          prototype_name,
          proto_name,
          &const_aliases,
          resolver,
        ) {
          let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
          diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
            "prototype mutation is forbidden when `native_strict` is enabled",
            Span::new(file, span),
          ));
        }
      }
      ExprKind::Update { expr: target_expr, .. } => {
        if expr_chain_contains_proto_mutation(
          resolver,
          &const_aliases,
          ExprRef {
            body: body_id,
            expr: *target_expr,
          },
          prototype_name,
          proto_name,
        ) {
          let span = result.expr_spans.get(idx).copied().unwrap_or(expr.span);
          diagnostics.push(codes::NATIVE_STRICT_PROTOTYPE_MUTATION.error(
            "prototype mutation is forbidden when `native_strict` is enabled",
            Span::new(file, span),
          ));
        }
      }
      ExprKind::Unary { op, expr: target_expr } => {
        if *op == hir_js::UnaryOp::Delete
          && expr_chain_contains_proto_mutation(
            resolver,
            &const_aliases,
            ExprRef {
              body: body_id,
              expr: *target_expr,
            },
            prototype_name,
            proto_name,
          )
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
          let Some(key_outer) = body.exprs.get(key_expr.0 as usize) else {
            continue;
          };
          let key_expr_unwrapped = expr_unwrap_ts_noop(body, *key_expr);
          let Some(key) = body.exprs.get(key_expr_unwrapped.0 as usize) else {
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
            .unwrap_or(key_outer.span);
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
          let Some(key_outer) = body.exprs.get(key_expr.0 as usize) else {
            continue;
          };
          let key_expr_unwrapped = expr_unwrap_ts_noop(body, *key_expr);
          let Some(key) = body.exprs.get(key_expr_unwrapped.0 as usize) else {
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
            .unwrap_or(key_outer.span);
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
      let Some(key_outer) = body.exprs.get(key_expr.0 as usize) else {
        continue;
      };
      let key_expr_unwrapped = expr_unwrap_ts_noop(body, *key_expr);
      let Some(key) = body.exprs.get(key_expr_unwrapped.0 as usize) else {
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
        .unwrap_or(key_outer.span);
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
          let Some(key_outer) = body.exprs.get(key_expr.0 as usize) else {
            continue;
          };
          let key_expr_unwrapped = expr_unwrap_ts_noop(body, *key_expr);
          let Some(key) = body.exprs.get(key_expr_unwrapped.0 as usize) else {
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
            .unwrap_or(key_outer.span);
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
