use knowledge_base::ApiDatabase;
use hir_js::{
  Body, BodyId, ExprId, ExprKind, ImportKind, Literal, LowerResult, NameId, ObjectKey, PatKind,
  StmtKind, VarDeclKind,
};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingTarget {
  pub module: String,
  pub path: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BindingEvent {
  start: u32,
  target: BindingTarget,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RequireBindings {
  bindings: BTreeMap<NameId, Vec<BindingEvent>>,
}

impl RequireBindings {
  fn insert(&mut self, name: NameId, start: u32, target: BindingTarget) {
    self
      .bindings
      .entry(name)
      .or_default()
      .push(BindingEvent { start, target });
  }

  fn resolve(&self, name: NameId, use_start: u32) -> Option<&BindingTarget> {
    let events = self.bindings.get(&name)?;
    events
      .iter()
      .rev()
      .find(|event| event.start <= use_start)
      .map(|event| &event.target)
  }
}

fn collect_import_bindings(lower: &LowerResult) -> RequireBindings {
  let mut bindings = RequireBindings::default();
  for import in lower.hir.imports.iter() {
    match &import.kind {
      ImportKind::Es(es) => {
        let module = es.specifier.value.clone();
        if let Some(ns) = &es.namespace {
          bindings.insert(
            ns.local,
            0,
            BindingTarget {
              module: module.clone(),
              path: Vec::new(),
            },
          );
        }
        for named in es.named.iter() {
          let Some(imported) = lower.names.resolve(named.imported) else {
            continue;
          };
          bindings.insert(
            named.local,
            0,
            BindingTarget {
              module: module.clone(),
              path: vec![imported.to_string()],
            },
          );
        }
      }
      ImportKind::ImportEquals(eq) => {
        if let hir_js::ImportEqualsTarget::Module(spec) = &eq.target {
          bindings.insert(
            eq.local.local,
            0,
            BindingTarget {
              module: spec.value.clone(),
              path: Vec::new(),
            },
          );
        }
      }
    }
  }
  bindings
}

/// Collect CommonJS `require()` bindings for a single body.
///
/// This is intentionally conservative and only models a subset of patterns:
/// - `const ns = require("node:fs")`
/// - `const { readFile, writeFile: wf } = require("node:fs")`
/// - `const rf = require("node:fs").readFile`
pub fn collect_require_bindings(lower: &LowerResult, body_id: BodyId) -> RequireBindings {
  let mut bindings = RequireBindings::default();
  let Some(body) = lower.body(body_id) else {
    return bindings;
  };

  for stmt_id in body.root_stmts.iter().copied() {
    let stmt = &body.stmts[stmt_id.0 as usize];
    let start = stmt.span.start;
    let StmtKind::Var(var_decl) = &stmt.kind else {
      continue;
    };
    if !matches!(var_decl.kind, VarDeclKind::Const | VarDeclKind::Let) {
      continue;
    }

    for decl in var_decl.declarators.iter() {
      let Some(init) = decl.init else {
        continue;
      };
      match &body.pats[decl.pat.0 as usize].kind {
        PatKind::Ident(local) => {
          if let Some((module, path)) = extract_require_member_path(lower, body, init) {
            bindings.insert(
              *local,
              start,
              BindingTarget {
                module,
                path,
              },
            );
          }
        }
        PatKind::Object(obj) => {
          if obj.rest.is_some() {
            continue;
          }
          let Some(module) = extract_require_module(lower, body, init) else {
            continue;
          };
          let mut collected = Vec::new();
          for prop in obj.props.iter() {
            let Some(key) = object_key_to_static_string(lower, &prop.key) else {
              collected.clear();
              break;
            };
            let PatKind::Ident(local) = &body.pats[prop.value.0 as usize].kind else {
              collected.clear();
              break;
            };
            collected.push((*local, key));
          }
          for (local, key) in collected {
            bindings.insert(
              local,
              start,
              BindingTarget {
                module: module.clone(),
                path: vec![key],
              },
            );
          }
        }
        _ => {}
      }
    }
  }

  bindings
}

fn extract_require_module(lower: &LowerResult, body: &Body, expr: ExprId) -> Option<String> {
  let ExprKind::Call(call) = &body.exprs[expr.0 as usize].kind else {
    return None;
  };
  if call.optional || call.is_new || call.args.len() != 1 {
    return None;
  }
  let ExprKind::Ident(callee) = &body.exprs[call.callee.0 as usize].kind else {
    return None;
  };
  if lower.names.resolve(*callee)? != "require" {
    return None;
  }

  let arg = &call.args[0];
  if arg.spread {
    return None;
  }
  let ExprKind::Literal(Literal::String(lit)) = &body.exprs[arg.expr.0 as usize].kind else {
    return None;
  };
  Some(lit.lossy.clone())
}

fn object_key_to_static_string(lower: &LowerResult, key: &ObjectKey) -> Option<String> {
  match key {
    ObjectKey::Ident(id) => lower.names.resolve(*id).map(|s| s.to_string()),
    ObjectKey::String(s) => Some(s.clone()),
    _ => None,
  }
}

fn extract_require_member_path(
  lower: &LowerResult,
  body: &Body,
  expr: ExprId,
) -> Option<(String, Vec<String>)> {
  if let Some(module) = extract_require_module(lower, body, expr) {
    return Some((module, Vec::new()));
  }

  let ExprKind::Member(mem) = &body.exprs[expr.0 as usize].kind else {
    return None;
  };
  if mem.optional {
    return None;
  }
  let prop = object_key_to_static_string(lower, &mem.property)?;
  let (module, mut path) = extract_require_member_path(lower, body, mem.object)?;
  path.push(prop);
  Some((module, path))
}

fn flatten_member_chain(
  lower: &LowerResult,
  body: &Body,
  expr: ExprId,
) -> Option<(ExprId, Vec<String>)> {
  let mut base = expr;
  let mut path = Vec::new();

  loop {
    let ExprKind::Member(mem) = &body.exprs[base.0 as usize].kind else {
      break;
    };
    if mem.optional {
      return None;
    }
    let prop = object_key_to_static_string(lower, &mem.property)?;
    path.push(prop);
    base = mem.object;
  }

  path.reverse();
  Some((base, path))
}

fn join_api(module: &str, path: &[String]) -> String {
  if path.is_empty() {
    module.to_string()
  } else {
    format!("{module}.{}", path.join("."))
  }
}

fn lookup_api<'a>(db: &'a ApiDatabase, module: &str, path: &[String]) -> Option<&'a str> {
  let canonical = join_api(module, path);
  if let Some(api) = db.get(&canonical) {
    return Some(api.name.as_str());
  }

  if module.starts_with("node:") {
    return None;
  }

  let canonical_node = join_api(&format!("node:{module}"), path);
  db.get(&canonical_node).map(|api| api.name.as_str())
}

pub fn resolve_api_call<'a>(
  db: &'a ApiDatabase,
  lower: &LowerResult,
  body_id: BodyId,
  call_expr: ExprId,
) -> Option<&'a str> {
  let body = lower.body(body_id)?;
  let ExprKind::Call(call) = &body.exprs[call_expr.0 as usize].kind else {
    return None;
  };

  let use_start = body.exprs[call.callee.0 as usize].span.start;
  let require_bindings = collect_require_bindings(lower, body_id);
  let import_bindings = collect_import_bindings(lower);

  let (base, member_path) = flatten_member_chain(lower, body, call.callee)?;

  match &body.exprs[base.0 as usize].kind {
    ExprKind::Ident(name) => {
      if let Some(target) = require_bindings
        .resolve(*name, use_start)
        .or_else(|| import_bindings.resolve(*name, use_start))
      {
        let mut path = target.path.clone();
        path.extend(member_path);
        return lookup_api(db, &target.module, &path);
      }
      None
    }
    ExprKind::Call(_) => {
      let Some(module) = extract_require_module(lower, body, base) else {
        return None;
      };
      lookup_api(db, &module, &member_path)
    }
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::resolve_api_call;
  use hir_js::{lower_from_source_with_kind, ExprId, ExprKind, FileKind};

  fn resolved_calls(source: &str) -> Vec<String> {
    let lowered = lower_from_source_with_kind(FileKind::Js, source).unwrap();
    let db = crate::load_default_api_database();
    let body_id = lowered.hir.root_body;
    let body = lowered.body(body_id).unwrap();
    let mut resolved = Vec::new();

    for (idx, expr) in body.exprs.iter().enumerate() {
      if !matches!(expr.kind, ExprKind::Call(_)) {
        continue;
      }
      if let Some(id) = resolve_api_call(&db, &lowered, body_id, ExprId(idx as u32)) {
        resolved.push(id.to_string());
      }
    }

    resolved
  }

  #[test]
  fn resolves_namespace_require() {
    let calls = resolved_calls(
      r#"
        const fs = require('node:fs');
        fs.readFile('x', () => {});
      "#,
    );
    assert_eq!(calls, vec!["node:fs.readFile"]);
  }

  #[test]
  fn resolves_named_destructure_require() {
    let calls = resolved_calls(
      r#"
        const { readFile, writeFile: wf } = require('node:fs');
        readFile('x', () => {});
        wf('y', 'z', () => {});
      "#,
    );
    assert_eq!(calls, vec!["node:fs.readFile", "node:fs.writeFile"]);
  }

  #[test]
  fn resolves_member_alias_require() {
    let calls = resolved_calls(
      r#"
        const rf = require('node:fs').readFile;
        rf('x', () => {});
      "#,
    );
    assert_eq!(calls, vec!["node:fs.readFile"]);
  }

  #[test]
  fn prefers_latest_preceding_binding() {
    let calls = resolved_calls(
      r#"
        const rf = require('node:fs').writeFile;
        rf('a', 'b', () => {});
        const rf2 = require('node:fs').readFile;
        rf2('c', () => {});
      "#,
    );
    assert_eq!(calls, vec!["node:fs.writeFile", "node:fs.readFile"]);
  }

  #[test]
  fn does_not_use_bindings_declared_later() {
    let calls = resolved_calls(
      r#"
        readFile('x', () => {});
        const { readFile } = require('node:fs');
        readFile('y', () => {});
      "#,
    );
    assert_eq!(calls, vec!["node:fs.readFile"]);
  }
}
