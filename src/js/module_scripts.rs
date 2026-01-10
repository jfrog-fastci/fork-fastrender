//! Minimal HTML module script support (`<script type="module">`).
//!
//! FastRender's embedded JS engine (`vm-js`) does not currently implement ECMAScript module
//! instantiation/evaluation. To unblock real-world fixtures that ship as ESM bundles, we implement a
//! small host-side module graph loader that:
//! - fetches module sources via [`crate::resource::ResourceFetcher`],
//! - resolves static `import` / `export ... from` specifiers against a base URL,
//! - transforms ESM syntax into classic-script-compatible code,
//! - and emits a single classic script bundle that installs a tiny module runtime on `globalThis`.
//!
//! The runtime provides caching and basic circular import handling so repeated imports share a
//! single module instance and cycles do not recurse infinitely.

use crate::error::{Error, RenderStage, Result};
use crate::js::import_maps::ImportMapState;
use crate::js::url_resolve::{resolve_url, UrlResolveError};
use crate::resource::{
  ensure_http_success, ensure_script_mime_sane, FetchDestination, FetchRequest, ResourceFetcher,
};
use crate::render_control::{check_active, check_active_periodic};

use parse_js::ast::import_export::{ExportNames, ImportNames, ModuleExportImportName};
use parse_js::ast::stmt::decl::{ClassDecl, FuncDecl, PatDecl, VarDecl};
use parse_js::ast::stmt::{ExportDefaultExprStmt, ExportListStmt, ImportStmt, Stmt};
use parse_js::ast::{expr::pat::Pat, node::Node, stx::TopLevel};
use parse_js::error::SyntaxErrorType;
use parse_js::{parse_with_options_cancellable_by, Dialect, ParseOptions, SourceType};

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug, Clone)]
struct ModuleInfo {
  transformed_body: String,
  dependencies: Vec<String>,
}

#[derive(Debug, Clone)]
enum CachedModule {
  Loading,
  Loaded(ModuleInfo),
}

/// Host-side loader/bundler for static ECMAScript module graphs.
///
/// A single `ModuleGraphLoader` can be reused across multiple module scripts to reuse the fetch +
/// parse + transform cache.
#[derive(Clone)]
pub struct ModuleGraphLoader {
  fetcher: Arc<dyn ResourceFetcher>,
  cache: HashMap<String, CachedModule>,
}

impl ModuleGraphLoader {
  pub fn new(fetcher: Arc<dyn ResourceFetcher>) -> Self {
    Self {
      fetcher,
      cache: HashMap::new(),
    }
  }

  /// Build a classic-script bundle for an external module entry point.
  pub fn build_bundle_for_url(&mut self, entry_url: &str, max_module_bytes: usize) -> Result<String> {
    // Preserve historical behavior: bare specifiers are rejected unless import maps are explicitly
    // supplied via `build_bundle_for_url_with_import_maps`.
    let mut resolver = |specifier: &str, base_url: &str| {
      resolve_module_specifier_without_import_maps(specifier, base_url)
    };
    self.load_external_module(&mut resolver, entry_url, max_module_bytes)?;
    self.emit_bundle(entry_url)
  }

  /// Build a classic-script bundle for an inline module entry point.
  ///
  /// `inline_id` is an opaque stable identifier for caching; `base_url` is the document base URL
  /// used to resolve relative imports in the inline module.
  pub fn build_bundle_for_inline(
    &mut self,
    inline_id: &str,
    base_url: &str,
    source: &str,
    max_module_bytes: usize,
  ) -> Result<String> {
    if source.as_bytes().len() > max_module_bytes {
      return Err(Error::Other(format!(
        "inline module is too large ({} bytes > max {})",
        source.as_bytes().len(),
        max_module_bytes
      )));
    }

    let mut resolver = |specifier: &str, base_url: &str| {
      resolve_module_specifier_without_import_maps(specifier, base_url)
    };
    self.load_inline_module(&mut resolver, inline_id, base_url, source, max_module_bytes)?;
    self.emit_bundle(inline_id)
  }

  /// Build a classic-script bundle for an external module entry point using WHATWG HTML import maps.
  pub fn build_bundle_for_url_with_import_maps(
    &mut self,
    import_map_state: &mut ImportMapState,
    entry_url: &str,
    max_module_bytes: usize,
  ) -> Result<String> {
    {
      let mut resolver = |specifier: &str, base_url: &str| {
        resolve_module_specifier_with_import_maps(import_map_state, specifier, base_url)
      };
      self.load_external_module(&mut resolver, entry_url, max_module_bytes)?;
    }
    self.emit_bundle(entry_url)
  }

  /// Build a classic-script bundle for an inline module entry point using WHATWG HTML import maps.
  pub fn build_bundle_for_inline_with_import_maps(
    &mut self,
    import_map_state: &mut ImportMapState,
    inline_id: &str,
    base_url: &str,
    source: &str,
    max_module_bytes: usize,
  ) -> Result<String> {
    if source.as_bytes().len() > max_module_bytes {
      return Err(Error::Other(format!(
        "inline module is too large ({} bytes > max {})",
        source.as_bytes().len(),
        max_module_bytes
      )));
    }

    {
      let mut resolver = |specifier: &str, base_url: &str| {
        resolve_module_specifier_with_import_maps(import_map_state, specifier, base_url)
      };
      self.load_inline_module(&mut resolver, inline_id, base_url, source, max_module_bytes)?;
    }
    self.emit_bundle(inline_id)
  }

  fn emit_bundle(&self, entry_id: &str) -> Result<String> {
    let mut closure: HashSet<String> = HashSet::new();
    self.collect_transitive_closure(entry_id, &mut closure)?;

    let mut module_ids: Vec<String> = closure.into_iter().collect();
    module_ids.sort();

    let mut out = String::new();
    out.push_str(MODULE_RUNTIME_SOURCE);
    out.push('\n');

    for id in module_ids {
      let Some(CachedModule::Loaded(module)) = self.cache.get(&id) else {
        return Err(Error::Other(format!("module missing from cache: {id}")));
      };
      out.push_str("__fastrDefineModule(");
      out.push_str(&js_string_literal(&id)?);
      out.push_str(", function(exports, __import) {\n");
      out.push_str("\"use strict\";\n");
      out.push_str(&module.transformed_body);
      if !module.transformed_body.ends_with('\n') {
        out.push('\n');
      }
      out.push_str("});\n");
    }

    out.push_str("__fastrImportModule(");
    out.push_str(&js_string_literal(entry_id)?);
    out.push_str(");\n");

    Ok(out)
  }

  fn collect_transitive_closure(&self, id: &str, out: &mut HashSet<String>) -> Result<()> {
    if !out.insert(id.to_string()) {
      return Ok(());
    }
    let Some(CachedModule::Loaded(module)) = self.cache.get(id) else {
      return Err(Error::Other(format!("module not loaded: {id}")));
    };
    for dep in &module.dependencies {
      self.collect_transitive_closure(dep, out)?;
    }
    Ok(())
  }

  fn load_inline_module(
    &mut self,
    resolver: &mut impl FnMut(&str, &str) -> Result<String>,
    inline_id: &str,
    base_url: &str,
    source: &str,
    max_module_bytes: usize,
  ) -> Result<()> {
    if let Some(existing) = self.cache.get(inline_id) {
      match existing {
        CachedModule::Loading => return Ok(()),
        CachedModule::Loaded(_) => return Ok(()),
      }
    }

    self
      .cache
      .insert(inline_id.to_string(), CachedModule::Loading);
    let result = (|| {
      let transform = transform_module_source(inline_id, base_url, source, resolver)?;
      for dep in &transform.dependencies {
        self.load_external_module(resolver, dep, max_module_bytes)?;
      }
      self.cache.insert(
        inline_id.to_string(),
        CachedModule::Loaded(ModuleInfo {
          transformed_body: transform.transformed_body,
          dependencies: transform.dependencies,
        }),
      );
      Ok(())
    })();

    if result.is_err() {
      self.cache.remove(inline_id);
    }
    result
  }

  fn load_external_module(
    &mut self,
    resolver: &mut impl FnMut(&str, &str) -> Result<String>,
    url: &str,
    max_module_bytes: usize,
  ) -> Result<()> {
    if let Some(existing) = self.cache.get(url) {
      match existing {
        CachedModule::Loading => return Ok(()),
        CachedModule::Loaded(_) => return Ok(()),
      }
    }

    self.cache.insert(url.to_string(), CachedModule::Loading);
    let result = (|| {
      // Module scripts are fetched in CORS mode (like `<script type="module">`).
      // Use `fetch_partial_with_request` to enforce `max_module_bytes` without downloading an
      // unbounded response body.
      let max_fetch = max_module_bytes.saturating_add(1);
      let req = FetchRequest::new(url, FetchDestination::ScriptCors);
      let res = self.fetcher.fetch_partial_with_request(req, max_fetch)?;
      ensure_http_success(&res, url)?;
      ensure_script_mime_sane(&res, url)?;
      if res.bytes.len() > max_module_bytes {
        return Err(Error::Other(format!(
          "module {url} is too large ({} bytes > max {})",
          res.bytes.len(),
          max_module_bytes
        )));
      }
      let source = String::from_utf8(res.bytes).map_err(|err| {
        Error::Other(format!("module {url} response was not valid UTF-8: {err}"))
      })?;

      let transform = transform_module_source(url, url, &source, resolver)?;
      for dep in &transform.dependencies {
        self.load_external_module(resolver, dep, max_module_bytes)?;
      }
      self.cache.insert(
        url.to_string(),
        CachedModule::Loaded(ModuleInfo {
          transformed_body: transform.transformed_body,
          dependencies: transform.dependencies,
        }),
      );
      Ok(())
    })();

    if result.is_err() {
      self.cache.remove(url);
    }
    result
  }
}

const MODULE_RUNTIME_SOURCE: &str = r#"
if (!globalThis.__fastrModuleRegistry) {
  // url -> { fn: (exports, __import) => void, exports: object, state: 0|1|2 }
  globalThis.__fastrModuleRegistry = {};
  globalThis.__fastrDefineModule = function (url, fn) {
    var reg = globalThis.__fastrModuleRegistry;
    if (reg[url]) return;
    reg[url] = { fn: fn, exports: {}, state: 0 };
  };
  globalThis.__fastrImportModule = function (url) {
    var reg = globalThis.__fastrModuleRegistry;
    var rec = reg[url];
    if (!rec) throw new Error("module not found: " + url);
    if (rec.state === 2) return rec.exports;
    if (rec.state === 1) return rec.exports; // circular import: return partial exports
    rec.state = 1;
    // Invoke module wrapper as a plain function call so `this` is `undefined` inside the wrapper,
    // matching ESM top-level `this` semantics.
    var fn = rec.fn;
    fn(rec.exports, globalThis.__fastrImportModule);
    rec.state = 2;
    return rec.exports;
  };
}
var __fastrDefineModule = globalThis.__fastrDefineModule;
var __fastrImportModule = globalThis.__fastrImportModule;
"#;

fn js_string_literal(value: &str) -> Result<String> {
  serde_json::to_string(value).map_err(|err| Error::Other(format!("failed to encode JS string: {err}")))
}

fn resolve_module_specifier_without_import_maps(specifier: &str, base_url: &str) -> Result<String> {
  // HTML's "resolve a module specifier" algorithm treats bare specifiers (those not starting with
  // `/`, `./`, or `../` and not parseable as an absolute URL) as failures unless import maps are
  // present. FastRender implements import map parsing/merging/resolution in `src/js/import_maps/`,
  // but callers must supply that state explicitly when bundling via
  // `ModuleGraphLoader::build_bundle_for_*_with_import_maps`.
  let allowed_relative = specifier.starts_with('/') || specifier.starts_with("./") || specifier.starts_with("../");
  if allowed_relative {
    return resolve_url(specifier, Some(base_url)).map_err(|err| Error::Other(format!("{err}")));
  }

  match resolve_url(specifier, None) {
    Ok(abs) => Ok(abs),
    Err(UrlResolveError::RelativeUrlWithoutBase) => Err(Error::Other(format!(
      "unsupported bare module specifier {specifier:?} (supply import maps via ModuleGraphLoader::build_bundle_for_*_with_import_maps)"
    ))),
    Err(err) => Err(Error::Other(format!("{err}"))),
  }
}

fn resolve_module_specifier_with_import_maps(
  state: &mut ImportMapState,
  specifier: &str,
  base_url: &str,
) -> Result<String> {
  let base_url = url::Url::parse(base_url)
    .map_err(|err| Error::Other(format!("invalid module base URL {base_url:?}: {err}")))?;
  super::import_maps::resolve_module_specifier(state, specifier, &base_url)
    .map(|url| url.to_string())
    .map_err(|err| Error::Other(err.to_string()))
}

#[derive(Debug)]
struct TransformOutput {
  transformed_body: String,
  dependencies: Vec<String>,
}

fn transform_module_source(
  module_id: &str,
  base_url: &str,
  source: &str,
  resolve_specifier: &mut impl FnMut(&str, &str) -> Result<String>,
) -> Result<TransformOutput> {
  check_active(RenderStage::Script)?;
  let opts = ParseOptions {
    dialect: Dialect::Ecma,
    source_type: SourceType::Module,
  };
  const PARSE_CANCEL_STRIDE: usize = 1024;
  let mut parse_counter = 0usize;
  let mut render_cancel = None;
  let top: Node<TopLevel> = match parse_with_options_cancellable_by(source, opts, || {
    parse_counter = parse_counter.wrapping_add(1);
    if parse_counter % PARSE_CANCEL_STRIDE != 0 {
      return false;
    }
    match check_active(RenderStage::Script) {
      Ok(()) => false,
      Err(err) => {
        render_cancel = Some(err);
        true
      }
    }
  }) {
    Ok(top) => top,
    Err(err) => {
      if err.typ == SyntaxErrorType::Cancelled {
        if let Some(render_err) = render_cancel.take() {
          return Err(Error::Render(render_err));
        }
        // Fallback: the parser observed cancellation but the callback did not store the error.
        // Re-run the check to surface the structured render error.
        check_active(RenderStage::Script)?;
      }
      return Err(Error::Other(format!("failed to parse module {module_id}: {err}")));
    }
  };

  let mut replacements: Vec<(usize, usize, String)> = Vec::new();
  let mut hoisted: String = String::new();
  let mut deps: Vec<String> = Vec::new();
  let mut temp_idx: usize = 0;

  let mut deadline_counter = 0usize;
  for stmt in &top.stx.body {
    check_active_periodic(&mut deadline_counter, 256, RenderStage::Script)?;
    match &*stmt.stx {
      Stmt::Import(import_stmt) => {
        let import = &import_stmt.stx;
        if import.type_only {
          replacements.push((stmt.loc.0, stmt.loc.1, "\n".to_string()));
          continue;
        }
        let dep = resolve_specifier(&import.module, base_url)?;
        deps.push(dep.clone());
        hoisted.push_str(&emit_import_binding(import, &dep, &mut temp_idx)?);
        hoisted.push('\n');
        // Remove the original `import ...` statement; imports are hoisted.
        replacements.push((stmt.loc.0, stmt.loc.1, "\n".to_string()));
      }
      Stmt::ExportList(export_stmt) => {
        let export = &export_stmt.stx;
        if export.type_only {
          replacements.push((stmt.loc.0, stmt.loc.1, "\n".to_string()));
          continue;
        }

        if let Some(from) = export.from.as_deref() {
          // `export ... from "..."` behaves like a hoisted import.
          let dep = resolve_specifier(from, base_url)?;
          deps.push(dep.clone());
          hoisted.push_str(&emit_export_from(export, &dep, &mut temp_idx)?);
          hoisted.push('\n');
          replacements.push((stmt.loc.0, stmt.loc.1, "\n".to_string()));
        } else {
          // Local export list (`export { foo as bar }`).
          let replacement = emit_local_export_list(export)?;
          replacements.push((stmt.loc.0, stmt.loc.1, replacement));
        }
      }
      Stmt::ExportDefaultExpr(export_stmt) => {
        let export: &ExportDefaultExprStmt = &export_stmt.stx;
        let expr = slice_loc(source, export.expression.loc)?;
        let replacement = format!("exports[\"default\"] = ({expr});\n");
        replacements.push((stmt.loc.0, stmt.loc.1, replacement));
      }
      Stmt::VarDecl(var_decl) => {
        let decl: &VarDecl = &var_decl.stx;
        if !decl.export {
          continue;
        }
        let stmt_text = slice_loc(source, stmt.loc)?;
        let stripped = strip_export_prefix(stmt_text, false)?;
        let mut replacement = String::new();
        replacement.push_str(stripped);
        if !replacement.ends_with('\n') {
          replacement.push('\n');
        }
        for declarator in &decl.declarators {
          check_active_periodic(&mut deadline_counter, 256, RenderStage::Script)?;
          let mut names = Vec::new();
          collect_binding_idents(&declarator.pattern.stx, &mut names, &mut deadline_counter)?;
          for name in names {
            check_active_periodic(&mut deadline_counter, 256, RenderStage::Script)?;
            replacement.push_str("exports[");
            replacement.push_str(&js_string_literal(&name)?);
            replacement.push_str("] = ");
            replacement.push_str(&name);
            replacement.push_str(";\n");
          }
        }
        replacements.push((stmt.loc.0, stmt.loc.1, replacement));
      }
      Stmt::FunctionDecl(func_decl) => {
        let decl: &FuncDecl = &func_decl.stx;
        if !decl.export && !decl.export_default {
          continue;
        }
        let stmt_text = slice_loc(source, stmt.loc)?;

        let mut replacement = String::new();
        if decl.export_default {
          if let Some(name) = decl.name.as_ref().map(|n| n.stx.name.clone()) {
            replacement.push_str(&strip_export_prefix(stmt_text, true)?);
            if !replacement.ends_with('\n') {
              replacement.push('\n');
            }
            replacement.push_str("exports[\"default\"] = ");
            replacement.push_str(&name);
            replacement.push_str(";\n");
          } else {
            // `export default function () {}` => function expression assigned to default.
            let func_text = slice_loc(source, decl.function.loc)?;
            replacement.push_str("exports[\"default\"] = (");
            replacement.push_str(func_text);
            replacement.push_str(");\n");
          }
        } else {
          let Some(name) = decl.name.as_ref().map(|n| n.stx.name.clone()) else {
            return Err(Error::Other(format!(
              "exported function declaration in {module_id} was missing a name"
            )));
          };
          replacement.push_str(&strip_export_prefix(stmt_text, false)?);
          if !replacement.ends_with('\n') {
            replacement.push('\n');
          }
          replacement.push_str("exports[");
          replacement.push_str(&js_string_literal(&name)?);
          replacement.push_str("] = ");
          replacement.push_str(&name);
          replacement.push_str(";\n");
        }
        replacements.push((stmt.loc.0, stmt.loc.1, replacement));
      }
      Stmt::ClassDecl(class_decl) => {
        let decl: &ClassDecl = &class_decl.stx;
        if !decl.export && !decl.export_default {
          continue;
        }
        let stmt_text = slice_loc(source, stmt.loc)?;

        let mut replacement = String::new();
        if decl.export_default {
          if let Some(name) = decl.name.as_ref().map(|n| n.stx.name.clone()) {
            replacement.push_str(&strip_export_prefix(stmt_text, true)?);
            if !replacement.ends_with('\n') {
              replacement.push('\n');
            }
            replacement.push_str("exports[\"default\"] = ");
            replacement.push_str(&name);
            replacement.push_str(";\n");
          } else {
            // `export default class {}` => class expression assigned to default.
            // The class *body* is part of the original statement; strip the leading export keywords
            // and treat it as an expression.
            let stripped = strip_export_prefix(stmt_text, true)?;
            replacement.push_str("exports[\"default\"] = (");
            replacement.push_str(stripped.trim_start());
            replacement.push_str(");\n");
          }
        } else {
          let Some(name) = decl.name.as_ref().map(|n| n.stx.name.clone()) else {
            return Err(Error::Other(format!(
              "exported class declaration in {module_id} was missing a name"
            )));
          };
          replacement.push_str(&strip_export_prefix(stmt_text, false)?);
          if !replacement.ends_with('\n') {
            replacement.push('\n');
          }
          replacement.push_str("exports[");
          replacement.push_str(&js_string_literal(&name)?);
          replacement.push_str("] = ");
          replacement.push_str(&name);
          replacement.push_str(";\n");
        }
        replacements.push((stmt.loc.0, stmt.loc.1, replacement));
      }
      _ => {}
    }
  }

  replacements.sort_by_key(|(start, _, _)| *start);
  // Ensure replacements do not overlap.
  for w in replacements.windows(2) {
    check_active_periodic(&mut deadline_counter, 1024, RenderStage::Script)?;
    let (a_start, a_end, _) = w[0];
    let (b_start, _, _) = w[1];
    if b_start < a_end {
      return Err(Error::Other(format!(
        "module transform produced overlapping replacements in {module_id}: {a_start}..{a_end} overlaps {b_start}"
      )));
    }
  }

  let mut rewritten = String::new();
  let mut cursor = 0usize;
  for (start, end, repl) in replacements {
    check_active_periodic(&mut deadline_counter, 1024, RenderStage::Script)?;
    if start > cursor {
      rewritten.push_str(&source[cursor..start]);
    }
    rewritten.push_str(&repl);
    cursor = end;
  }
  if cursor < source.len() {
    rewritten.push_str(&source[cursor..]);
  }

  let mut transformed_body = String::new();
  transformed_body.push_str(&hoisted);
  transformed_body.push_str(&rewritten);

  Ok(TransformOutput {
    transformed_body,
    dependencies: deps,
  })
}

fn slice_loc<'a>(source: &'a str, loc: parse_js::loc::Loc) -> Result<&'a str> {
  let start = loc.0;
  let end = loc.1;
  if start > end || end > source.len() {
    return Err(Error::Other(format!(
      "invalid source range {start}..{end} for source length {}",
      source.len()
    )));
  }
  Ok(&source[start..end])
}

fn strip_export_prefix(stmt_text: &str, export_default: bool) -> Result<&str> {
  let trimmed = stmt_text.trim_start();
  let rest = trimmed
    .strip_prefix("export")
    .ok_or_else(|| Error::Other("expected statement to start with export".to_string()))?;
  let rest = rest.trim_start();
  if export_default {
    let rest = rest
      .strip_prefix("default")
      .ok_or_else(|| Error::Other("expected export default".to_string()))?;
    Ok(rest.trim_start())
  } else {
    Ok(rest)
  }
}

fn emit_import_binding(import: &ImportStmt, resolved_url: &str, temp_idx: &mut usize) -> Result<String> {
  let url_lit = js_string_literal(resolved_url)?;
  if import.default.is_none() && import.names.is_none() {
    return Ok(format!("__import({url_lit});"));
  }

  let tmp_name = format!("__m{temp_idx}");
  *temp_idx = temp_idx.saturating_add(1);

  let mut out = String::new();
  out.push_str("var ");
  out.push_str(&tmp_name);
  out.push_str(" = __import(");
  out.push_str(&url_lit);
  out.push_str(");\n");

  if let Some(default_decl) = import.default.as_ref() {
    let name = pat_decl_ident(default_decl)?;
    out.push_str("var ");
    out.push_str(name);
    out.push_str(" = ");
    out.push_str(&tmp_name);
    out.push_str("[\"default\"];");
    out.push('\n');
  }

  if let Some(names) = import.names.as_ref() {
    match names {
      ImportNames::All(alias) => {
        let name = pat_decl_ident(alias)?;
        out.push_str("var ");
        out.push_str(name);
        out.push_str(" = ");
        out.push_str(&tmp_name);
        out.push_str(";\n");
      }
      ImportNames::Specific(list) => {
        let mut deadline_counter = 0usize;
        for item in list {
          check_active_periodic(&mut deadline_counter, 256, RenderStage::Script)?;
          if item.stx.type_only {
            continue;
          }
          let importable = item.stx.importable.as_str();
          let alias = pat_decl_ident(&item.stx.alias)?;
          out.push_str("var ");
          out.push_str(alias);
          out.push_str(" = ");
          out.push_str(&tmp_name);
          out.push_str("[");
          out.push_str(&js_string_literal(importable)?);
          out.push_str("];\n");
        }
      }
    }
  }

  Ok(out)
}

fn emit_export_from(export: &ExportListStmt, resolved_url: &str, temp_idx: &mut usize) -> Result<String> {
  let url_lit = js_string_literal(resolved_url)?;
  match &export.names {
    ExportNames::Specific(list) => {
      let tmp_name = format!("__m{temp_idx}");
      *temp_idx = temp_idx.saturating_add(1);
      let mut out = String::new();
      out.push_str("var ");
      out.push_str(&tmp_name);
      out.push_str(" = __import(");
      out.push_str(&url_lit);
      out.push_str(");\n");
      let mut deadline_counter = 0usize;
      for name in list {
        check_active_periodic(&mut deadline_counter, 256, RenderStage::Script)?;
        if name.stx.type_only {
          continue;
        }
        let imported = name.stx.exportable.as_str();
        let exported = &name.stx.alias.stx.name;
        out.push_str("exports[");
        out.push_str(&js_string_literal(exported)?);
        out.push_str("] = ");
        out.push_str(&tmp_name);
        out.push_str("[");
        out.push_str(&js_string_literal(imported)?);
        out.push_str("];\n");
      }
      Ok(out)
    }
    ExportNames::All(Some(alias)) => {
      let name = &alias.stx.name;
      Ok(format!("exports[{}] = __import({});", js_string_literal(name)?, url_lit))
    }
    ExportNames::All(None) => {
      // Best-effort `export * from "module"`: copy enumerable properties excluding `default`.
      let tmp_name = format!("__m{temp_idx}");
      *temp_idx = temp_idx.saturating_add(1);
      Ok(format!(
        "var {tmp_name} = __import({url_lit});\nfor (var __k in {tmp_name}) {{ if (__k !== \"default\") exports[__k] = {tmp_name}[__k]; }}\n"
      ))
    }
  }
}

fn emit_local_export_list(export: &ExportListStmt) -> Result<String> {
  let ExportNames::Specific(list) = &export.names else {
    return Err(Error::Other(
      "local export list must be `export { ... }`".to_string(),
    ));
  };

  let mut out = String::new();
  let mut deadline_counter = 0usize;
  for name in list {
    check_active_periodic(&mut deadline_counter, 256, RenderStage::Script)?;
    if name.stx.type_only {
      continue;
    }
    let local = match &name.stx.exportable {
      ModuleExportImportName::Ident(id) => id.as_str(),
      ModuleExportImportName::Str(_) => {
        return Err(Error::Other(
          "string-named local exports are not supported".to_string(),
        ));
      }
    };
    let exported = &name.stx.alias.stx.name;
    out.push_str("exports[");
    out.push_str(&js_string_literal(exported)?);
    out.push_str("] = ");
    out.push_str(local);
    out.push_str(";\n");
  }
  Ok(out)
}

fn pat_decl_ident(decl: &Node<PatDecl>) -> Result<&str> {
  match &*decl.stx.pat.stx {
    Pat::Id(id) => Ok(id.stx.name.as_str()),
    other => Err(Error::Other(format!(
      "unsupported import binding pattern: {other:?}"
    ))),
  }
}

fn collect_binding_idents(
  decl: &PatDecl,
  out: &mut Vec<String>,
  deadline_counter: &mut usize,
) -> Result<()> {
  collect_pat_idents(&decl.pat, out, deadline_counter)
}

fn collect_pat_idents(
  pat: &Node<Pat>,
  out: &mut Vec<String>,
  deadline_counter: &mut usize,
) -> Result<()> {
  match &*pat.stx {
    Pat::Id(id) => out.push(id.stx.name.clone()),
    Pat::Arr(arr) => {
      for elem in &arr.stx.elements {
        check_active_periodic(deadline_counter, 256, RenderStage::Script)?;
        if let Some(elem) = elem {
          collect_pat_idents(&elem.target, out, deadline_counter)?;
        }
      }
      if let Some(rest) = arr.stx.rest.as_ref() {
        collect_pat_idents(rest, out, deadline_counter)?;
      }
    }
    Pat::Obj(obj) => {
      for prop in &obj.stx.properties {
        check_active_periodic(deadline_counter, 256, RenderStage::Script)?;
        collect_pat_idents(&prop.stx.target, out, deadline_counter)?;
      }
      if let Some(rest) = obj.stx.rest.as_ref() {
        collect_pat_idents(rest, out, deadline_counter)?;
      }
    }
    Pat::AssignTarget(_) => {
      return Err(Error::Other(
        "unsupported export binding pattern (assign target)".to_string(),
      ));
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::error::RenderError;
  use crate::render_control::RenderDeadline;
  use crate::resource::FetchedResource;
  use crate::js::import_maps::{create_import_map_parse_result, register_import_map};
  use url::Url;
  use std::collections::HashMap;
  use std::sync::Mutex;

  #[derive(Default)]
  struct MapFetcher {
    entries: Mutex<HashMap<String, Vec<u8>>>,
    counts: Mutex<HashMap<String, usize>>,
  }

  impl MapFetcher {
    fn insert(&self, url: &str, source: &str) {
      self
        .entries
        .lock()
        .unwrap()
        .insert(url.to_string(), source.as_bytes().to_vec());
    }

    fn count(&self, url: &str) -> usize {
      *self.counts.lock().unwrap().get(url).unwrap_or(&0)
    }
  }

  impl ResourceFetcher for MapFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      *self
        .counts
        .lock()
        .unwrap()
        .entry(url.to_string())
        .or_insert(0) += 1;
      let bytes = self
        .entries
        .lock()
        .unwrap()
        .get(url)
        .cloned()
        .ok_or_else(|| Error::Other(format!("missing fixture: {url}")))?;
      Ok(FetchedResource::with_final_url(bytes, None, Some(url.to_string())))
    }
  }

  #[derive(Default)]
  struct DestRecordingFetcher {
    entries: Mutex<HashMap<String, Vec<u8>>>,
    destinations: Mutex<Vec<FetchDestination>>,
  }

  impl DestRecordingFetcher {
    fn insert(&self, url: &str, source: &str) {
      self
        .entries
        .lock()
        .unwrap()
        .insert(url.to_string(), source.as_bytes().to_vec());
    }

    fn take_destinations(&self) -> Vec<FetchDestination> {
      std::mem::take(&mut *self.destinations.lock().unwrap())
    }
  }

  impl ResourceFetcher for DestRecordingFetcher {
    fn fetch(&self, url: &str) -> Result<FetchedResource> {
      let bytes = self
        .entries
        .lock()
        .unwrap()
        .get(url)
        .cloned()
        .ok_or_else(|| Error::Other(format!("missing fixture: {url}")))?;
      Ok(FetchedResource::with_final_url(bytes, None, Some(url.to_string())))
    }

    fn fetch_with_request(&self, req: FetchRequest<'_>) -> Result<FetchedResource> {
      self.destinations.lock().unwrap().push(req.destination);
      self.fetch(req.url)
    }
  }

  #[test]
  fn resolves_relative_module_specifier_against_base_url() {
    let resolved = resolve_module_specifier_without_import_maps("./dep.js", "https://example.com/dir/main.js").unwrap();
    assert_eq!(resolved, "https://example.com/dir/dep.js");
  }

  #[test]
  fn rejects_bare_specifiers_without_import_maps() {
    let err = resolve_module_specifier_without_import_maps("react", "https://example.com/dir/main.js").unwrap_err();
    assert!(
      err.to_string().contains("bare module specifier"),
      "got: {err}"
    );
  }

  #[test]
  fn resolves_bare_specifiers_via_import_maps() {
    let fetcher = Arc::new(MapFetcher::default());
    fetcher.insert(
      "https://example.com/main.js",
      r#"import React from "react"; globalThis.__react_default = React;"#,
    );
    fetcher.insert("https://example.com/vendor/react.js", r#"export default 123;"#);

    let base_url = Url::parse("https://example.com/index.html").unwrap();
    let parse_result = create_import_map_parse_result(r#"{ "imports": { "react": "/vendor/react.js" } }"#, &base_url);
    let mut state = ImportMapState::default();
    register_import_map(&mut state, parse_result).unwrap();

    let mut loader = ModuleGraphLoader::new(fetcher.clone());
    let _bundle = loader
      .build_bundle_for_url_with_import_maps(&mut state, "https://example.com/main.js", 1024 * 1024)
      .unwrap();

    assert_eq!(fetcher.count("https://example.com/vendor/react.js"), 1);
  }

  #[test]
  fn caches_fetched_modules_across_imports() {
    let fetcher = Arc::new(MapFetcher::default());
    fetcher.insert(
      "https://example.com/main.js",
      r#"import "./dep.js"; import "./dep.js"; globalThis.__ok = true;"#,
    );
    fetcher.insert("https://example.com/dep.js", r#"globalThis.__dep = true;"#);

    let mut loader = ModuleGraphLoader::new(fetcher.clone());
    let _bundle = loader
      .build_bundle_for_url("https://example.com/main.js", 1024 * 1024)
      .unwrap();

    assert_eq!(fetcher.count("https://example.com/dep.js"), 1);
  }

  #[test]
  fn handles_circular_imports_without_infinite_recursion() {
    let fetcher = Arc::new(MapFetcher::default());
    fetcher.insert("https://example.com/a.js", r#"import "./b.js";"#);
    fetcher.insert("https://example.com/b.js", r#"import "./a.js";"#);

    let mut loader = ModuleGraphLoader::new(fetcher.clone());
    let bundle = loader
      .build_bundle_for_url("https://example.com/a.js", 1024 * 1024)
      .unwrap();

    assert!(bundle.contains("__fastrDefineModule"));
    assert_eq!(fetcher.count("https://example.com/a.js"), 1);
    assert_eq!(fetcher.count("https://example.com/b.js"), 1);
  }

  #[test]
  fn module_graph_loader_fetches_modules_with_scriptcors_destination() {
    let fetcher = Arc::new(DestRecordingFetcher::default());
    fetcher.insert("https://example.com/main.js", r#"import "./dep.js";"#);
    fetcher.insert("https://example.com/dep.js", r#"export default 1;"#);

    let mut loader = ModuleGraphLoader::new(fetcher.clone());
    let _bundle = loader
      .build_bundle_for_url("https://example.com/main.js", 1024 * 1024)
      .unwrap();

    let destinations = fetcher.take_destinations();
    assert!(
      !destinations.is_empty(),
      "expected ModuleGraphLoader to issue at least one fetch_with_request"
    );
    assert!(
      destinations
        .iter()
        .all(|d| matches!(d, FetchDestination::ScriptCors)),
      "expected all module fetches to use FetchDestination::ScriptCors, got: {destinations:?}"
    );
  }

  #[test]
  fn module_graph_loader_respects_render_deadline() {
    let fetcher = Arc::new(MapFetcher::default());
    fetcher.insert(
      "https://example.com/main.js",
      r#"export const ok = true; globalThis.__ok = ok;"#,
    );
    let mut loader = ModuleGraphLoader::new(fetcher);

    // Immediate cancellation should abort module parse/transform work.
    let deadline = RenderDeadline::new(None, Some(Arc::new(|| true)));
    let err = crate::render_control::with_deadline(Some(&deadline), || {
      loader.build_bundle_for_url("https://example.com/main.js", 1024 * 1024)
    })
    .unwrap_err();

    match err {
      Error::Render(RenderError::Timeout { stage, .. }) => assert_eq!(stage, RenderStage::Script),
      other => panic!("expected script timeout render error, got {other:?}"),
    }
  }

  #[test]
  fn module_runtime_invokes_wrappers_without_binding_this() {
    // Ensure the embedded runtime does not invoke module wrappers as a method call (`rec.fn(...)`),
    // which would bind `this` to the record object. Module top-level `this` is `undefined`.
    assert!(
      MODULE_RUNTIME_SOURCE.contains("var fn = rec.fn"),
      "expected runtime to stash rec.fn into a local before calling it"
    );
    assert!(
      !MODULE_RUNTIME_SOURCE.contains("rec.fn(rec.exports"),
      "expected runtime to avoid method-call invocation, got:\n{MODULE_RUNTIME_SOURCE}"
    );
  }
}
