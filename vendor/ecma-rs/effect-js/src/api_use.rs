use hir_js::{
  Body, CallExpr, ExprId, ExprKind, HirFile, ImportKind, Literal, MemberExpr, NameId, NameInterner,
  ObjectKey, PatId, PatKind, StmtKind, VarDeclKind,
};
use knowledge_base::{ApiId, ApiKind, KnowledgeBase};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiUseKind {
  Call,
  Construct,
  Get,
  Set,
  Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedApiUse {
  pub api: ApiId,
  pub kind: ApiUseKind,
}

pub fn resolve_api_use(
  file: &HirFile,
  body: &Body,
  expr: ExprId,
  names: &NameInterner,
  kb: &KnowledgeBase,
) -> Option<ResolvedApiUse> {
  let expr_node = body.exprs.get(expr.0 as usize)?;
  match &expr_node.kind {
    ExprKind::Call(call) => resolve_call_api_use(file, body, call, names, kb),
    ExprKind::Member(member) => resolve_member_api_use(file, body, expr, member, names, kb),
    ExprKind::Ident(_) => resolve_ident_api_use(file, body, expr, names, kb),
    ExprKind::Assignment { target, .. } => resolve_assignment_api_use(file, body, *target, names, kb),
    _ => None,
  }
}

fn resolve_call_api_use(
  file: &HirFile,
  body: &Body,
  call: &CallExpr,
  names: &NameInterner,
  kb: &KnowledgeBase,
) -> Option<ResolvedApiUse> {
  if call.optional {
    return None;
  }

  let callee_path = resolve_expr_path(file, body, call.callee, true, names)?;
  let entry = kb.get(&callee_path)?;

  Some(ResolvedApiUse {
    api: entry.id,
    kind: if call.is_new {
      ApiUseKind::Construct
    } else {
      ApiUseKind::Call
    },
  })
}

fn resolve_member_api_use(
  file: &HirFile,
  body: &Body,
  expr: ExprId,
  member: &MemberExpr,
  names: &NameInterner,
  kb: &KnowledgeBase,
) -> Option<ResolvedApiUse> {
  if member.optional {
    return None;
  }

  let path = resolve_expr_path(file, body, expr, true, names)?;
  let entry = kb.get(&path)?;

  let use_kind = match entry.kind {
    ApiKind::Getter | ApiKind::Value => ApiUseKind::Get,
    ApiKind::Function | ApiKind::Constructor => ApiUseKind::Value,
    ApiKind::Setter => ApiUseKind::Value,
  };

  Some(ResolvedApiUse {
    api: entry.id,
    kind: use_kind,
  })
}

fn resolve_ident_api_use(
  file: &HirFile,
  body: &Body,
  expr: ExprId,
  names: &NameInterner,
  kb: &KnowledgeBase,
) -> Option<ResolvedApiUse> {
  let path = resolve_expr_path(file, body, expr, false, names)?;
  let entry = kb.get(&path)?;
  Some(ResolvedApiUse {
    api: entry.id,
    kind: ApiUseKind::Value,
  })
}

fn resolve_assignment_api_use(
  file: &HirFile,
  body: &Body,
  target: PatId,
  names: &NameInterner,
  kb: &KnowledgeBase,
) -> Option<ResolvedApiUse> {
  let pat = body.pats.get(target.0 as usize)?;
  let PatKind::AssignTarget(target_expr) = pat.kind else {
    return None;
  };
  let path = resolve_expr_path(file, body, target_expr, true, names)?;
  let entry = kb.get(&path)?;
  match entry.kind {
    ApiKind::Setter | ApiKind::Value => Some(ResolvedApiUse {
      api: entry.id,
      kind: ApiUseKind::Set,
    }),
    _ => None,
  }
}

fn resolve_expr_path(
  file: &HirFile,
  body: &Body,
  expr: ExprId,
  allow_instance: bool,
  names: &NameInterner,
) -> Option<String> {
  let segments = resolve_expr_path_segments(file, body, expr, allow_instance, false, names)?;
  Some(segments.join("."))
}

fn resolve_expr_path_segments(
  file: &HirFile,
  body: &Body,
  expr: ExprId,
  allow_instance: bool,
  in_member: bool,
  names: &NameInterner,
) -> Option<Vec<String>> {
  let expr = body.exprs.get(expr.0 as usize)?;
  match &expr.kind {
    ExprKind::TypeAssertion { expr, .. }
    | ExprKind::NonNull { expr }
    | ExprKind::Satisfies { expr, .. } => {
      resolve_expr_path_segments(file, body, *expr, allow_instance, in_member, names)
    }

    ExprKind::Ident(name) => {
      resolve_ident_base(file, body, *name, expr.span.start, in_member, names)
    }
    ExprKind::Member(member) => {
      if member.optional {
        return None;
      }
      resolve_member_path_segments(file, body, expr, member, names)
    }
    ExprKind::Call(call) => {
      if call.optional {
        return None;
      }
      if let Some(module) = require_module_for_call(body, call, names) {
        return Some(vec![module]);
      }
      if allow_instance && call.is_new {
        // `new Ctor(...)` used as a value. Member access is handled by
        // `resolve_member_path_segments`, but allow callers to treat this as the
        // constructor itself.
        return resolve_expr_path_segments(file, body, call.callee, false, false, names);
      }
      None
    }
    _ => None,
  }
}

fn resolve_member_path_segments(
  file: &HirFile,
  body: &Body,
  _member_expr: &hir_js::Expr,
  member: &MemberExpr,
  names: &NameInterner,
) -> Option<Vec<String>> {
  let prop = object_key_to_string(&member.property, names)?;

  // Optional chaining is handled by callers (`resolve_api_use`) to avoid
  // reporting APIs that may not execute.
  debug_assert!(!member.optional);

  // Instance access on `new Ctor()`.
  if let Some(segs) = resolve_new_instance_member(file, body, member.object, &prop, names) {
    return Some(segs);
  }

  // Typed-only: instance access on an identifier with a locally inferred ctor
  // assignment (e.g. `const x = new URL(...); x.pathname`).
  #[cfg(feature = "typed")]
  if let Some(segs) = resolve_typed_ident_instance_member(
    file,
    body,
    _member_expr.span.start,
    member.object,
    &prop,
    names,
  ) {
    return Some(segs);
  }

  let mut base = resolve_expr_path_segments(file, body, member.object, true, true, names)?;
  base.push(prop);
  Some(base)
}

fn resolve_new_instance_member(
  file: &HirFile,
  body: &Body,
  object: ExprId,
  prop: &str,
  names: &NameInterner,
) -> Option<Vec<String>> {
  let ExprKind::Call(call) = &body.exprs.get(object.0 as usize)?.kind else {
    return None;
  };
  if call.optional || !call.is_new {
    return None;
  }

  let mut ctor = resolve_expr_path_segments(file, body, call.callee, false, false, names)?;
  ctor.push("prototype".to_string());
  ctor.push(prop.to_string());
  Some(ctor)
}

#[cfg(feature = "typed")]
fn resolve_typed_ident_instance_member(
  file: &HirFile,
  body: &Body,
  member_start: u32,
  object: ExprId,
  prop: &str,
  names: &NameInterner,
) -> Option<Vec<String>> {
  let ExprKind::Ident(name) = &body.exprs.get(object.0 as usize)?.kind else {
    return None;
  };

  let ctor = infer_constructor_for_ident(file, body, *name, member_start, names)?;
  let mut segs = ctor;
  segs.push("prototype".to_string());
  segs.push(prop.to_string());
  Some(segs)
}

#[cfg(feature = "typed")]
fn infer_constructor_for_ident(
  file: &HirFile,
  body: &Body,
  name: NameId,
  use_start: u32,
  names: &NameInterner,
) -> Option<Vec<String>> {
  let mut best: Option<(u32, Vec<String>)> = None;

  for stmt in &body.stmts {
    if stmt.span.start >= use_start {
      continue;
    }

    let StmtKind::Var(var) = &stmt.kind else {
      continue;
    };

    // Be conservative: only infer from `const`/`let` bindings.
    if !matches!(var.kind, VarDeclKind::Const | VarDeclKind::Let) {
      continue;
    }

    for declarator in &var.declarators {
      let pat = body.pats.get(declarator.pat.0 as usize)?;
      let PatKind::Ident(pat_name) = &pat.kind else {
        continue;
      };
      if *pat_name != name {
        continue;
      }

      let Some(init) = declarator.init else {
        continue;
      };
      let ExprKind::Call(call) = &body.exprs.get(init.0 as usize)?.kind else {
        continue;
      };
      if call.optional || !call.is_new {
        continue;
      }

      let Some(ctor) = resolve_expr_path_segments(file, body, call.callee, false, false, names)
      else {
        continue;
      };
      best = Some((stmt.span.start, ctor));
    }
  }

  best.map(|(_, ctor)| ctor)
}

fn resolve_ident_base(
  file: &HirFile,
  body: &Body,
  name: NameId,
  use_start: u32,
  in_member: bool,
  names: &NameInterner,
) -> Option<Vec<String>> {
  // If this body declares `name` (including `let`/`const` in TDZ), treat it as a
  // local binding and do not resolve to imports or globals. Only handle the
  // subset of locals that are `require()` bindings we can model.
  if is_name_declared_in_body(body, name) {
    return resolve_require_binding(body, name, use_start, names);
  }

  // ES `import` or TS `import =` bindings.
  for import in &file.imports {
    match &import.kind {
      ImportKind::Es(es) => {
        let spec = es.specifier.value.clone();
        if let Some(ns) = &es.namespace {
          if ns.local == name {
            return Some(vec![spec]);
          }
        }
        if let Some(default) = &es.default {
          if default.local == name {
            return Some(vec![spec]);
          }
        }
        for named in &es.named {
          if named.local == name {
            let imported = names.resolve(named.imported)?.to_string();
            return Some(vec![spec, imported]);
          }
        }
      }
      ImportKind::ImportEquals(ie) => {
        if ie.local.local != name {
          continue;
        }
        match &ie.target {
          hir_js::ImportEqualsTarget::Module(spec) => return Some(vec![spec.value.clone()]),
          hir_js::ImportEqualsTarget::Path(path) => {
            let mut segs = Vec::with_capacity(path.len());
            for part in path {
              segs.push(names.resolve(*part)?.to_string());
            }
            return Some(segs);
          }
        }
      }
    }
  }

  if let Some(segs) = resolve_require_binding(body, name, use_start, names) {
    return Some(segs);
  }

  let name_str = names.resolve(name)?.to_string();
  if in_member && !is_allowed_global_member_root(&name_str) {
    return None;
  }
  Some(vec![name_str])
}

fn object_key_to_string(key: &ObjectKey, names: &NameInterner) -> Option<String> {
  match key {
    ObjectKey::Ident(id) => Some(names.resolve(*id)?.to_string()),
    ObjectKey::String(s) => Some(s.clone()),
    ObjectKey::Number(n) => Some(n.clone()),
    ObjectKey::Computed(_) => None,
  }
}

fn require_module_for_call(body: &Body, call: &CallExpr, names: &NameInterner) -> Option<String> {
  if call.is_new || call.optional || call.args.len() != 1 {
    return None;
  }
  let ExprKind::Ident(callee) = &body.exprs.get(call.callee.0 as usize)?.kind else {
    return None;
  };
  if names.resolve(*callee)? != "require" {
    return None;
  }
  let arg = call.args.first()?;
  if arg.spread {
    return None;
  }
  let ExprKind::Literal(Literal::String(lit)) = &body.exprs.get(arg.expr.0 as usize)?.kind else {
    return None;
  };
  Some(lit.lossy.clone())
}

fn strip_transparent_wrappers(body: &Body, mut expr: ExprId) -> ExprId {
  loop {
    let Some(node) = body.exprs.get(expr.0 as usize) else {
      return expr;
    };
    match &node.kind {
      ExprKind::TypeAssertion { expr: inner, .. }
      | ExprKind::NonNull { expr: inner }
      | ExprKind::Satisfies { expr: inner, .. } => expr = *inner,
      _ => return expr,
    }
  }
}

fn extract_require_member_path(
  body: &Body,
  expr: ExprId,
  names: &NameInterner,
) -> Option<(String, Vec<String>)> {
  let expr = strip_transparent_wrappers(body, expr);
  if let ExprKind::Call(call) = &body.exprs.get(expr.0 as usize)?.kind {
    if let Some(module) = require_module_for_call(body, call, names) {
      return Some((module, Vec::new()));
    }
  }

  let ExprKind::Member(mem) = &body.exprs.get(expr.0 as usize)?.kind else {
    return None;
  };
  if mem.optional {
    return None;
  }
  let prop = object_key_to_string(&mem.property, names)?;
  let (module, mut path) = extract_require_member_path(body, mem.object, names)?;
  path.push(prop);
  Some((module, path))
}

fn resolve_require_binding(
  body: &Body,
  name: NameId,
  use_start: u32,
  names: &NameInterner,
) -> Option<Vec<String>> {
  let mut best: Option<(u32, Vec<String>)> = None;

  for stmt_id in body.root_stmts.iter().copied() {
    let stmt = body.stmts.get(stmt_id.0 as usize)?;
    if stmt.span.start >= use_start {
      continue;
    }
    let StmtKind::Var(var) = &stmt.kind else {
      continue;
    };
    if !matches!(var.kind, VarDeclKind::Const | VarDeclKind::Let) {
      continue;
    }

    for declarator in &var.declarators {
      let Some(init) = declarator.init else {
        continue;
      };

      match &body.pats.get(declarator.pat.0 as usize)?.kind {
        PatKind::Ident(local) if *local == name => {
          let Some((module, path)) = extract_require_member_path(body, init, names) else {
            continue;
          };
          let mut segs = vec![module];
          segs.extend(path);
          best = Some((stmt.span.start, segs));
        }
        PatKind::Object(obj) => {
          if obj.rest.is_some() {
            continue;
          }
          let Some((module, prefix_path)) = extract_require_member_path(body, init, names) else {
            continue;
          };
          for prop in &obj.props {
            let Some(key) = object_key_to_string(&prop.key, names) else {
              continue;
            };
            let PatKind::Ident(local) = &body.pats.get(prop.value.0 as usize)?.kind else {
              continue;
            };
            if *local != name {
              continue;
            }
            let mut segs = vec![module.clone()];
            segs.extend(prefix_path.clone());
            segs.push(key);
            best = Some((stmt.span.start, segs));
          }
        }
        _ => {}
      }
    }
  }

  best.map(|(_, segs)| segs)
}

fn is_name_declared_in_body(body: &Body, name: NameId) -> bool {
  if let Some(func) = &body.function {
    for param in &func.params {
      if pat_binds_name(body, param.pat, name) {
        return true;
      }
    }
  }

  // Model only root-level `var`/`let`/`const` names. Nested block-scoped
  // declarations do not shadow identifiers outside their block, and modeling
  // scope precisely is outside the goal of this best-effort resolver.
  for stmt_id in body.root_stmts.iter().copied() {
    let Some(stmt) = body.stmts.get(stmt_id.0 as usize) else {
      continue;
    };
    let StmtKind::Var(var) = &stmt.kind else {
      continue;
    };
    for declarator in &var.declarators {
      if pat_binds_name(body, declarator.pat, name) {
        return true;
      }
    }
  }

  false
}

fn pat_binds_name(body: &Body, pat: hir_js::PatId, name: NameId) -> bool {
  let Some(pat) = body.pats.get(pat.0 as usize) else {
    return false;
  };
  match &pat.kind {
    PatKind::Ident(id) => *id == name,
    PatKind::Array(arr) => {
      for element in arr.elements.iter().flatten() {
        if pat_binds_name(body, element.pat, name) {
          return true;
        }
      }
      arr
        .rest
        .is_some_and(|rest| pat_binds_name(body, rest, name))
    }
    PatKind::Object(obj) => {
      for prop in &obj.props {
        if pat_binds_name(body, prop.value, name) {
          return true;
        }
      }
      obj
        .rest
        .is_some_and(|rest| pat_binds_name(body, rest, name))
    }
    PatKind::Rest(inner) => pat_binds_name(body, **inner, name),
    PatKind::Assign { target, .. } => pat_binds_name(body, *target, name),
    PatKind::AssignTarget(_) => false,
  }
}

fn is_allowed_global_member_root(name: &str) -> bool {
  matches!(
    name,
    "globalThis"
      | "window"
      | "self"
      | "global"
      | "console"
      | "process"
      | "Buffer"
      | "Array"
      | "BigInt"
      | "Boolean"
      | "Date"
      | "Error"
      | "EvalError"
      | "Function"
      | "JSON"
      | "Map"
      | "Math"
      | "Number"
      | "Object"
      | "Promise"
      | "Proxy"
      | "RangeError"
      | "ReferenceError"
      | "Reflect"
      | "RegExp"
      | "Set"
      | "String"
      | "Symbol"
      | "SyntaxError"
      | "TypeError"
      | "URIError"
      | "URL"
      | "URLSearchParams"
      | "Request"
      | "Response"
      | "Headers"
      | "TextDecoder"
      | "TextEncoder"
      | "WeakMap"
      | "WeakSet"
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use effect_model::{EffectSet, EffectTemplate, PurityTemplate};
  use hir_js::ExprKind;
  use knowledge_base::{ApiDatabase, ApiKind, ApiSemantics};

  fn kb_for_tests() -> ApiDatabase {
    ApiDatabase::from_entries([
      ApiSemantics {
        id: ApiId::from_name("URL"),
        name: "URL".to_string(),
        kind: ApiKind::Constructor,
        aliases: vec![],
        effects: EffectTemplate::Pure,
        effect_summary: EffectSet::empty(),
        purity: PurityTemplate::Pure,
        async_: None,
        idempotent: None,
        deterministic: None,
        parallelizable: None,
        semantics: None,
        signature: None,
        since: None,
        until: None,
        properties: Default::default(),
      },
      ApiSemantics {
        id: ApiId::from_name("URL.prototype.pathname"),
        name: "URL.prototype.pathname".to_string(),
        kind: ApiKind::Getter,
        aliases: vec![],
        effects: EffectTemplate::Pure,
        effect_summary: EffectSet::empty(),
        purity: PurityTemplate::Pure,
        async_: None,
        idempotent: None,
        deterministic: None,
        parallelizable: None,
        semantics: None,
        signature: None,
        since: None,
        until: None,
        properties: Default::default(),
      },
      ApiSemantics {
        id: ApiId::from_name("Math.sqrt"),
        name: "Math.sqrt".to_string(),
        kind: ApiKind::Function,
        aliases: vec![],
        effects: EffectTemplate::Pure,
        effect_summary: EffectSet::empty(),
        purity: PurityTemplate::Pure,
        async_: None,
        idempotent: None,
        deterministic: None,
        parallelizable: None,
        semantics: None,
        signature: None,
        since: None,
        until: None,
        properties: Default::default(),
      },
      ApiSemantics {
        id: ApiId::from_name("Math.PI"),
        name: "Math.PI".to_string(),
        kind: ApiKind::Value,
        aliases: vec![],
        effects: EffectTemplate::Pure,
        effect_summary: EffectSet::empty(),
        purity: PurityTemplate::Pure,
        async_: None,
        idempotent: None,
        deterministic: None,
        parallelizable: None,
        semantics: None,
        signature: None,
        since: None,
        until: None,
        properties: Default::default(),
      },
      ApiSemantics {
        id: ApiId::from_name("Map"),
        name: "Map".to_string(),
        kind: ApiKind::Constructor,
        aliases: vec![],
        effects: EffectTemplate::Unknown,
        effect_summary: EffectSet::UNKNOWN,
        purity: PurityTemplate::Unknown,
        async_: None,
        idempotent: None,
        deterministic: None,
        parallelizable: None,
        semantics: None,
        signature: None,
        since: None,
        until: None,
        properties: Default::default(),
      },
      ApiSemantics {
        id: ApiId::from_name("Response.prototype.json"),
        name: "Response.prototype.json".to_string(),
        kind: ApiKind::Function,
        aliases: vec![],
        effects: EffectTemplate::Pure,
        effect_summary: EffectSet::empty(),
        purity: PurityTemplate::Pure,
        async_: None,
        idempotent: None,
        deterministic: None,
        parallelizable: None,
        semantics: None,
        signature: None,
        since: None,
        until: None,
        properties: Default::default(),
      },
      ApiSemantics {
        id: ApiId::from_name("node:fs.readFile"),
        name: "node:fs.readFile".to_string(),
        kind: ApiKind::Function,
        aliases: vec![],
        effects: EffectTemplate::Pure,
        effect_summary: EffectSet::empty(),
        purity: PurityTemplate::Pure,
        async_: None,
        idempotent: None,
        deterministic: None,
        parallelizable: None,
        semantics: None,
        signature: None,
        since: None,
        until: None,
        properties: Default::default(),
      },
      ApiSemantics {
        id: ApiId::from_name("Foo.prototype.bar"),
        name: "Foo.prototype.bar".to_string(),
        kind: ApiKind::Value,
        aliases: vec![],
        effects: EffectTemplate::Unknown,
        effect_summary: EffectSet::UNKNOWN,
        purity: PurityTemplate::Unknown,
        async_: None,
        idempotent: None,
        deterministic: None,
        parallelizable: None,
        semantics: None,
        signature: None,
        since: None,
        until: None,
        properties: Default::default(),
      },
    ])
  }

  fn find_expr(
    body: &hir_js::Body,
    mut predicate: impl FnMut(&ExprKind) -> bool,
  ) -> ExprId {
    for (idx, expr) in body.exprs.iter().enumerate() {
      if predicate(&expr.kind) {
        return ExprId(idx as u32);
      }
    }
    panic!("expr not found");
  }

  #[test]
  fn resolves_constructor_and_getter() {
    let source = r#"const p = new URL("https://example.com").pathname;"#;
    let lowered = hir_js::lower_from_source(source).expect("lower");
    let file = lowered.hir.as_ref();
    let names = &lowered.names;
    let body = lowered.body(file.root_body).expect("root body");

    let new_call = find_expr(body, |kind| match kind {
      ExprKind::Call(call) => {
        call.is_new
          && matches!(
            body.exprs.get(call.callee.0 as usize).map(|e| &e.kind),
            Some(ExprKind::Ident(id)) if names.resolve(*id) == Some("URL")
          )
      }
      _ => false,
    });

    let pathname_member = find_expr(body, |kind| match kind {
      ExprKind::Member(member) => matches!(
        &member.property,
        ObjectKey::Ident(id) if names.resolve(*id) == Some("pathname")
      ),
      _ => false,
    });

    let kb = kb_for_tests();

    assert_eq!(
      resolve_api_use(file, body, new_call, names, &kb),
      Some(ResolvedApiUse {
        api: ApiId::from_name("URL"),
        kind: ApiUseKind::Construct,
      })
    );

    assert_eq!(
      resolve_api_use(file, body, pathname_member, names, &kb),
      Some(ResolvedApiUse {
        api: ApiId::from_name("URL.prototype.pathname"),
        kind: ApiUseKind::Get,
      })
    );
  }

  #[test]
  fn resolves_math_sqrt_call() {
    let source = r#"Math.sqrt(4);"#;
    let lowered = hir_js::lower_from_source(source).expect("lower");
    let file = lowered.hir.as_ref();
    let names = &lowered.names;
    let body = lowered.body(file.root_body).expect("root body");

    let call_expr = find_expr(body, |kind| matches!(kind, ExprKind::Call(_)));

    let kb = kb_for_tests();
    assert_eq!(
      resolve_api_use(file, body, call_expr, names, &kb),
      Some(ResolvedApiUse {
        api: ApiId::from_name("Math.sqrt"),
        kind: ApiUseKind::Call,
      })
    );
  }

  #[test]
  fn resolves_ident_value() {
    let source = r#"const ctor = URL;"#;
    let lowered = hir_js::lower_from_source(source).expect("lower");
    let file = lowered.hir.as_ref();
    let names = &lowered.names;
    let body = lowered.body(file.root_body).expect("root body");

    let url_ident = find_expr(body, |kind| match kind {
      ExprKind::Ident(id) => names.resolve(*id) == Some("URL"),
      _ => false,
    });

    let kb = kb_for_tests();
    assert_eq!(
      resolve_api_use(file, body, url_ident, names, &kb),
      Some(ResolvedApiUse {
        api: ApiId::from_name("URL"),
        kind: ApiUseKind::Value,
      })
    );
  }

  #[test]
  fn resolves_value_property_read_as_get() {
    let source = r#"Math.PI;"#;
    let lowered = hir_js::lower_from_source(source).expect("lower");
    let file = lowered.hir.as_ref();
    let names = &lowered.names;
    let body = lowered.body(file.root_body).expect("root body");

    let pi_member = find_expr(body, |kind| match kind {
      ExprKind::Member(member) => matches!(
        &member.property,
        ObjectKey::Ident(id) if names.resolve(*id) == Some("PI")
      ),
      _ => false,
    });

    let kb = kb_for_tests();
    assert_eq!(
      resolve_api_use(file, body, pi_member, names, &kb),
      Some(ResolvedApiUse {
        api: ApiId::from_name("Math.PI"),
        kind: ApiUseKind::Get,
      })
    );
  }

  #[test]
  fn resolves_assignment_to_value_property_as_set() {
    let source = r#"new Foo().bar = 1;"#;
    let lowered = hir_js::lower_from_source(source).expect("lower");
    let file = lowered.hir.as_ref();
    let names = &lowered.names;
    let body = lowered.body(file.root_body).expect("root body");

    let assign_expr = find_expr(body, |kind| matches!(kind, ExprKind::Assignment { .. }));

    let kb = kb_for_tests();
    assert_eq!(
      resolve_api_use(file, body, assign_expr, names, &kb),
      Some(ResolvedApiUse {
        api: ApiId::from_name("Foo.prototype.bar"),
        kind: ApiUseKind::Set,
      })
    );
  }

  #[test]
  fn resolves_map_constructor() {
    let source = r#"new Map();"#;
    let lowered = hir_js::lower_from_source(source).expect("lower");
    let file = lowered.hir.as_ref();
    let names = &lowered.names;
    let body = lowered.body(file.root_body).expect("root body");

    let call_expr = find_expr(body, |kind| match kind {
      ExprKind::Call(call) => call.is_new && matches!(
        body.exprs.get(call.callee.0 as usize).map(|e| &e.kind),
        Some(ExprKind::Ident(id)) if names.resolve(*id) == Some("Map")
      ),
      _ => false,
    });

    let kb = kb_for_tests();
    assert_eq!(
      resolve_api_use(file, body, call_expr, names, &kb),
      Some(ResolvedApiUse {
        api: ApiId::from_name("Map"),
        kind: ApiUseKind::Construct,
      })
    );
  }

  #[test]
  fn resolves_require_namespace_call() {
    let source = r#"
const fs = require("node:fs");
fs.readFile("x", () => {});
"#;
    let lowered = hir_js::lower_from_source(source).expect("lower");
    let file = lowered.hir.as_ref();
    let names = &lowered.names;
    let body = lowered.body(file.root_body).expect("root body");

    let call_expr = find_expr(body, |kind| match kind {
      ExprKind::Call(call) => {
        !call.is_new
          && matches!(
            body.exprs.get(call.callee.0 as usize).map(|e| &e.kind),
            Some(ExprKind::Member(member))
              if matches!(
                &member.property,
                ObjectKey::Ident(id) if names.resolve(*id) == Some("readFile")
              )
          )
      }
      _ => false,
    });

    let kb = kb_for_tests();
    assert_eq!(
      resolve_api_use(file, body, call_expr, names, &kb),
      Some(ResolvedApiUse {
        api: ApiId::from_name("node:fs.readFile"),
        kind: ApiUseKind::Call,
      })
    );
  }

  #[test]
  fn resolves_default_import_namespace_call() {
    let source = r#"
import fs from "node:fs";
fs.readFile("x", () => {});
"#;
    let lowered = hir_js::lower_from_source(source).expect("lower");
    let file = lowered.hir.as_ref();
    let names = &lowered.names;
    let body = lowered.body(file.root_body).expect("root body");

    let call_expr = find_expr(body, |kind| match kind {
      ExprKind::Call(call) => {
        !call.is_new
          && matches!(
            body.exprs.get(call.callee.0 as usize).map(|e| &e.kind),
            Some(ExprKind::Member(member))
              if matches!(
                &member.property,
                ObjectKey::Ident(id) if names.resolve(*id) == Some("readFile")
              )
          )
      }
      _ => false,
    });

    let kb = kb_for_tests();
    assert_eq!(
      resolve_api_use(file, body, call_expr, names, &kb),
      Some(ResolvedApiUse {
        api: ApiId::from_name("node:fs.readFile"),
        kind: ApiUseKind::Call,
      })
    );
  }

  #[cfg(feature = "typed")]
  #[test]
  fn resolves_inferred_receiver_method_call() {
    let source = r#"
const resp = new Response();
resp.json();
"#;
    let lowered = hir_js::lower_from_source(source).expect("lower");
    let file = lowered.hir.as_ref();
    let names = &lowered.names;
    let body = lowered.body(file.root_body).expect("root body");

    let call_expr = find_expr(body, |kind| match kind {
      ExprKind::Call(call) => !call.is_new,
      _ => false,
    });

    let kb = kb_for_tests();
    assert_eq!(
      resolve_api_use(file, body, call_expr, names, &kb),
      Some(ResolvedApiUse {
        api: ApiId::from_name("Response.prototype.json"),
        kind: ApiUseKind::Call,
      })
    );
  }
}
