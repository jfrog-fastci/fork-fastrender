use knowledge_base::ApiDatabase;
use hir_js::{
  Body, BodyId, ExprId, ExprKind, ImportKind, Literal, LowerResult, NameId, ObjectKey, PatId,
  PatKind, StmtKind,
};
use std::collections::BTreeMap;
use std::collections::BTreeSet;

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

fn collect_pat_idents(body: &Body, pat: PatId, out: &mut BTreeSet<NameId>) {
  let Some(pat) = body.pats.get(pat.0 as usize) else {
    return;
  };
  match &pat.kind {
    PatKind::Ident(name) => {
      out.insert(*name);
    }
    PatKind::Array(arr) => {
      for element in arr.elements.iter().flatten() {
        collect_pat_idents(body, element.pat, out);
      }
      if let Some(rest) = arr.rest {
        collect_pat_idents(body, rest, out);
      }
    }
    PatKind::Object(obj) => {
      for prop in obj.props.iter() {
        collect_pat_idents(body, prop.value, out);
      }
      if let Some(rest) = obj.rest {
        collect_pat_idents(body, rest, out);
      }
    }
    PatKind::Rest(inner) => collect_pat_idents(body, **inner, out),
    PatKind::Assign { target, .. } => collect_pat_idents(body, *target, out),
    PatKind::AssignTarget(_) => {}
  }
}

fn collect_lexical_names(body: &Body) -> BTreeSet<NameId> {
  let mut names = BTreeSet::new();

  if let Some(func) = &body.function {
    for param in func.params.iter() {
      collect_pat_idents(body, param.pat, &mut names);
    }
  }

  for stmt_id in body.root_stmts.iter().copied() {
    let stmt = &body.stmts[stmt_id.0 as usize];
    if let StmtKind::Var(var_decl) = &stmt.kind {
      for decl in var_decl.declarators.iter() {
        collect_pat_idents(body, decl.pat, &mut names);
      }
    }
  }

  names
}

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

  fn extend(&mut self, other: RequireBindings) {
    for (name, mut events) in other.bindings {
      self.bindings.entry(name).or_default().append(&mut events);
    }
  }

  fn hoist_all(&mut self, start: u32) {
    for events in self.bindings.values_mut() {
      for event in events.iter_mut() {
        event.start = start;
      }
    }
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
        if let Some(default) = &es.default {
          bindings.insert(
            default.local,
            0,
            BindingTarget {
              module: module.clone(),
              path: Vec::new(),
            },
          );
        }
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

#[cfg(feature = "typed")]
fn import_specifier_for_def(program: &typecheck_ts::Program, def: typecheck_ts::DefId) -> Option<String> {
  // Task 234: prefer the original import specifier captured on the import def.
  // This remains stable even when the resolved file's FileKey is a path or other
  // host-specific identifier (e.g. "node_fs.ts").
  let direct = program.import_specifier(def).filter(|s| !s.is_empty());
  if direct.is_some() {
    return direct;
  }

  // Older snapshots may miss `ImportData.specifier`; fall back to the unresolved
  // target's specifier when available.
  match program.def_kind(def) {
    Some(typecheck_ts::DefKind::Import(import)) => match import.target {
      typecheck_ts::ImportTarget::Unresolved { specifier } if !specifier.is_empty() => Some(specifier),
      typecheck_ts::ImportTarget::File(file) => {
        // Last-resort: avoid leaking filesystem paths into KB keys.
        //
        // Only accept file keys that already match a known KB naming convention.
        program
          .file_key(file)
          .map(|key| key.as_str().to_string())
          .filter(|key| key.starts_with("node:"))
      }
      _ => None,
    },
    _ => None,
  }
}

#[cfg(feature = "typed")]
fn collect_import_bindings_typed(program: &typecheck_ts::Program, lower: &LowerResult) -> RequireBindings {
  let mut bindings = RequireBindings::default();

  for import in lower.hir.imports.iter() {
    match &import.kind {
      ImportKind::Es(es) => {
        let fallback_module = es.specifier.value.as_str();

        if let Some(default) = &es.default {
          let (module, path) = default
            .local_def
            .and_then(|def| match program.def_kind(def) {
              Some(typecheck_ts::DefKind::Import(import)) => {
                let module = import_specifier_for_def(program, def).unwrap_or_else(|| fallback_module.to_string());
                let path = if import.original == "*" {
                  Vec::new()
                } else {
                  vec![import.original]
                };
                Some((module, path))
              }
              _ => None,
            })
            .unwrap_or_else(|| (fallback_module.to_string(), vec!["default".to_string()]));

          bindings.insert(default.local, 0, BindingTarget { module, path });
        }

        if let Some(ns) = &es.namespace {
          let (module, path) = ns
            .local_def
            .and_then(|def| match program.def_kind(def) {
              Some(typecheck_ts::DefKind::Import(import)) => {
                let module = import_specifier_for_def(program, def).unwrap_or_else(|| fallback_module.to_string());
                let path = if import.original == "*" {
                  Vec::new()
                } else {
                  vec![import.original]
                };
                Some((module, path))
              }
              _ => None,
            })
            .unwrap_or_else(|| (fallback_module.to_string(), Vec::new()));

          bindings.insert(ns.local, 0, BindingTarget { module, path });
        }

        for named in es.named.iter() {
          let resolved = named
            .local_def
            .and_then(|def| match program.def_kind(def) {
              Some(typecheck_ts::DefKind::Import(import)) => {
                let module = import_specifier_for_def(program, def).unwrap_or_else(|| fallback_module.to_string());
                let path = if import.original == "*" {
                  Vec::new()
                } else {
                  vec![import.original]
                };
                Some((module, path))
              }
              _ => None,
            })
            .or_else(|| {
              let imported = lower.names.resolve(named.imported)?;
              Some((fallback_module.to_string(), vec![imported.to_string()]))
            });
          let Some((module, path)) = resolved else {
            continue;
          };

          bindings.insert(named.local, 0, BindingTarget { module, path });
        }
      }
      ImportKind::ImportEquals(eq) => {
        let hir_js::ImportEqualsTarget::Module(spec) = &eq.target else {
          continue;
        };
        let fallback_module = spec.value.as_str();

        let (module, path) = eq
          .local
          .local_def
          .and_then(|def| match program.def_kind(def) {
            Some(typecheck_ts::DefKind::Import(import)) => {
              let module = import_specifier_for_def(program, def).unwrap_or_else(|| fallback_module.to_string());
              let path = if import.original == "*" {
                Vec::new()
              } else {
                vec![import.original]
              };
              Some((module, path))
            }
            _ => None,
          })
          .unwrap_or_else(|| (fallback_module.to_string(), Vec::new()));

        bindings.insert(eq.local.local, 0, BindingTarget { module, path });
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
    if !matches!(
      var_decl.kind,
      hir_js::VarDeclKind::Const | hir_js::VarDeclKind::Let
    ) {
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
          let Some((module, prefix_path)) = extract_require_member_path(lower, body, init) else {
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
            let mut path = prefix_path.clone();
            path.push(key);
            bindings.insert(
              local,
              start,
              BindingTarget {
                module: module.clone(),
                path,
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
  let expr = strip_transparent_wrappers(body, expr);
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
  let expr = strip_transparent_wrappers(body, expr);
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
  let mut base = strip_transparent_wrappers(body, expr);
  let mut path = Vec::new();

  loop {
    base = strip_transparent_wrappers(body, base);
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
  Some((strip_transparent_wrappers(body, base), path))
}

fn join_api(module: &str, path: &[String]) -> String {
  if path.is_empty() {
    module.to_string()
  } else {
    format!("{module}.{}", path.join("."))
  }
}

fn lookup_api<'a>(db: &'a ApiDatabase, module: &str, path: &[String]) -> Option<&'a str> {
  if module.starts_with("node:") {
    let canonical = join_api(module, path);
    return db.get(&canonical).map(|api| api.name.as_str());
  }

  let canonical_node = join_api(&format!("node:{module}"), path);
  if let Some(api) = db.get(&canonical_node) {
    return Some(api.name.as_str());
  }

  let canonical = join_api(module, path);
  db.get(&canonical).map(|api| api.name.as_str())
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
  if call.optional || call.is_new {
    return None;
  }

  let use_start = body.exprs[call.callee.0 as usize].span.start;
  let local_decls = collect_lexical_names(body);
  let local_bindings = collect_require_bindings(lower, body_id);
  let require_bindings = if body_id == lower.hir.root_body {
    local_bindings.clone()
  } else {
    // Top-level CommonJS `require()` bindings are visible to nested function bodies; the call
    // executes after module initialization, so treat the root body bindings as hoisted for the
    // purpose of best-effort resolution.
    let mut bindings = collect_require_bindings(lower, lower.hir.root_body);
    bindings.hoist_all(0);
    bindings.extend(local_bindings.clone());
    bindings
  };
  let import_bindings = collect_import_bindings(lower);

  let (base, member_path) = flatten_member_chain(lower, body, call.callee)?;

  match &body.exprs[base.0 as usize].kind {
    ExprKind::Ident(name) => {
      // Shadow outer bindings when this body declares the identifier (including parameters).
      //
      // `let`/`const` bindings are in TDZ before their declaration is evaluated, so even uses
      // *before* the declaration should not resolve to an outer `require()` binding.
      if local_decls.contains(name) {
        if let Some(target) = local_bindings.resolve(*name, use_start) {
          let mut path = target.path.clone();
          path.extend(member_path);
          return lookup_api(db, &target.module, &path);
        }
        return None;
      }

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

#[cfg(feature = "typed")]
pub fn resolve_api_call_typed<'a>(
  db: &'a ApiDatabase,
  program: &typecheck_ts::Program,
  lower: &LowerResult,
  body_id: BodyId,
  call_expr: ExprId,
) -> Option<&'a str> {
  let body = lower.body(body_id)?;
  let ExprKind::Call(call) = &body.exprs[call_expr.0 as usize].kind else {
    return None;
  };
  if call.optional || call.is_new {
    return None;
  }

  let use_start = body.exprs[call.callee.0 as usize].span.start;
  let local_decls = collect_lexical_names(body);
  let local_bindings = collect_require_bindings(lower, body_id);
  let require_bindings = if body_id == lower.hir.root_body {
    local_bindings.clone()
  } else {
    // Top-level CommonJS `require()` bindings are visible to nested function bodies; the call
    // executes after module initialization, so treat the root body bindings as hoisted for the
    // purpose of best-effort resolution.
    let mut bindings = collect_require_bindings(lower, lower.hir.root_body);
    bindings.hoist_all(0);
    bindings.extend(local_bindings.clone());
    bindings
  };
  let import_bindings = collect_import_bindings_typed(program, lower);

  let (base, member_path) = flatten_member_chain(lower, body, call.callee)?;

  match &body.exprs[base.0 as usize].kind {
    ExprKind::Ident(name) => {
      // Shadow outer bindings when this body declares the identifier (including parameters).
      //
      // `let`/`const` bindings are in TDZ before their declaration is evaluated, so even uses
      // *before* the declaration should not resolve to an outer `require()` binding.
      if local_decls.contains(name) {
        if let Some(target) = local_bindings.resolve(*name, use_start) {
          let mut path = target.path.clone();
          path.extend(member_path);
          return lookup_api(db, &target.module, &path);
        }
        return None;
      }

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
    let lowered = lower_from_source_with_kind(FileKind::Ts, source).unwrap();
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

  fn resolved_calls_all_bodies(source: &str) -> Vec<String> {
    let lowered = lower_from_source_with_kind(FileKind::Ts, source).unwrap();
    let db = crate::load_default_api_database();
    let mut resolved = Vec::new();

    for (&body_id, _) in lowered.body_index.iter() {
      let body = lowered.body(body_id).unwrap();
      for (idx, expr) in body.exprs.iter().enumerate() {
        if !matches!(expr.kind, ExprKind::Call(_)) {
          continue;
        }
        if let Some(id) = resolve_api_call(&db, &lowered, body_id, ExprId(idx as u32)) {
          resolved.push(id.to_string());
        }
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

  #[test]
  fn resolves_through_type_assertions_and_non_null() {
    let calls = resolved_calls(
      r#"
        const fs = require('node:fs') as any;
        fs!.readFile('x', () => {});
      "#,
    );
    assert_eq!(calls, vec!["node:fs.readFile"]);
  }

  #[test]
  fn resolves_require_without_node_prefix_to_node_canonical() {
    let calls = resolved_calls(
      r#"
        const fs = require('fs');
        fs.readFile('x', () => {});
      "#,
    );
    assert_eq!(calls, vec!["node:fs.readFile"]);
  }

  #[test]
  fn resolves_destructure_from_require_member_chain() {
    let calls = resolved_calls(
      r#"
        const { readFile, writeFile: wf } = require('node:fs').promises;
        readFile('x', () => {});
        wf('y', 'z');
      "#,
    );
    assert_eq!(calls, vec!["node:fs.promises.readFile", "node:fs.promises.writeFile"]);
  }

  #[test]
  fn resolves_namespace_import() {
    let calls = resolved_calls(
      r#"
        import * as fs from 'node:fs';
        fs.readFile('x', () => {});
      "#,
    );
    assert_eq!(calls, vec!["node:fs.readFile"]);
  }

  #[test]
  fn resolves_named_imports() {
    let calls = resolved_calls(
      r#"
        import { readFile, writeFile as wf } from 'fs';
        readFile('x', () => {});
        wf('y', 'z', () => {});
      "#,
    );
    assert_eq!(calls, vec!["node:fs.readFile", "node:fs.writeFile"]);
  }

  #[test]
  fn resolves_default_import() {
    let calls = resolved_calls(
      r#"
        import fs from 'node:fs';
        fs.readFile('x', () => {});
      "#,
    );
    assert_eq!(calls, vec!["node:fs.readFile"]);
  }

  #[test]
  fn resolves_import_equals_require() {
    let calls = resolved_calls(
      r#"
        import fs = require('fs');
        fs.readFile('x', () => {});
      "#,
    );
    assert_eq!(calls, vec!["node:fs.readFile"]);
  }

  #[test]
  fn resolves_require_subpath_module() {
    let calls = resolved_calls(
      r#"
        const fs = require('fs/promises');
        fs.readFile('x');
      "#,
    );
    assert_eq!(calls, vec!["node:fs/promises.readFile"]);
  }

  #[test]
  fn resolves_import_subpath_module() {
    let calls = resolved_calls(
      r#"
        import { readFile } from 'fs/promises';
        readFile('x');
      "#,
    );
    assert_eq!(calls, vec!["node:fs/promises.readFile"]);
  }

  #[test]
  fn resolves_root_require_bindings_inside_nested_bodies() {
    let calls = resolved_calls_all_bodies(
      r#"
        function foo() {
          fs.readFile('x', () => {});
        }

        const fs = require('node:fs');
      "#,
    );
    assert_eq!(calls, vec!["node:fs.readFile"]);
  }

  #[test]
  fn does_not_resolve_outer_binding_when_shadowed_by_param() {
    let calls = resolved_calls_all_bodies(
      r#"
        const fs = require('node:fs');

        function foo(fs: any) {
          fs.readFile('x', () => {});
        }
      "#,
    );
    assert_eq!(calls, Vec::<String>::new());
  }

  #[test]
  fn does_not_resolve_outer_binding_when_shadowed_by_local_decl() {
    let calls = resolved_calls_all_bodies(
      r#"
        const fs = require('node:fs');

        function foo() {
          const fs = 123;
          fs.readFile('x', () => {});
        }
      "#,
    );
    assert_eq!(calls, Vec::<String>::new());
  }
}
