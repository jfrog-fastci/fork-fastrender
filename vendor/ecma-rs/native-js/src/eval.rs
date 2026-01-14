//! Minimal evaluation harness used by `native-js` tests.
//!
//! This is *not* a full JS engine; it is a tiny evaluator over `hir-js` that is
//! intentionally strict about supported constructs. Its primary purpose is to
//! validate identifier/binding resolution (`Resolver`) under shadowing/import
//! scenarios.

use std::collections::HashMap;

use hir_js::{
  AssignOp, BinaryOp, Body, BodyId, CallExpr, ExprId, ExprKind, ImportKind, Literal, StmtId,
  StmtKind,
};
use typecheck_ts::{DefId, FileId, FileKey, Program};

use crate::resolve::BindingId;
use crate::Resolver;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
  Number(i64),
  Function(EvalFunction),
  Undefined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvalFunction {
  pub def: DefId,
  pub body: BodyId,
  pub file: FileId,
}

#[derive(Debug)]
pub struct EvalError {
  pub message: String,
}

impl EvalError {
  fn new(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
    }
  }
}

type EvalResult<T> = Result<T, EvalError>;

/// Tiny evaluator over `hir-js` that uses `native-js::Resolver` for identifier binding.
pub struct Evaluator<'p> {
  program: &'p Program,
  resolver: Resolver<'p>,
  modules: HashMap<FileId, ModuleInstance>,
}

#[derive(Debug)]
struct ModuleInstance {
  lowered: std::sync::Arc<hir_js::LowerResult>,
  bindings: HashMap<BindingId, Value>,
  exports: HashMap<String, Value>,
}

impl<'p> Evaluator<'p> {
  pub fn new(program: &'p Program) -> Self {
    Self {
      program,
      resolver: Resolver::new(program),
      modules: HashMap::new(),
    }
  }

  /// Evaluate `export function <name>() { ... }` and return its numeric result.
  pub fn run_exported_function_i64(&mut self, file: FileId, export: &str) -> EvalResult<i64> {
    self.eval_module(file)?;
    let module = self.modules.get(&file).unwrap();
    let Some(value) = module.exports.get(export).cloned() else {
      return Err(EvalError::new(format!(
        "export `{export}` not found in file {file:?}"
      )));
    };
    let Value::Function(func) = value else {
      return Err(EvalError::new(format!(
        "export `{export}` is not a function (got {value:?})"
      )));
    };
    let result = self.call_function(&func, &[])?;
    match result {
      Value::Number(n) => Ok(n),
      other => Err(EvalError::new(format!(
        "function `{export}` returned non-number value: {other:?}"
      ))),
    }
  }

  fn eval_module(&mut self, file: FileId) -> EvalResult<()> {
    if self.modules.contains_key(&file) {
      return Ok(());
    }

    let lowered = self
      .program
      .hir_lowered(file)
      .ok_or_else(|| EvalError::new(format!("missing HIR lowering for file {file:?}")))?;

    // Prepare module instance first to support cyclic-ish graphs (best-effort).
    self.modules.insert(
      file,
      ModuleInstance {
        lowered: std::sync::Arc::clone(&lowered),
        bindings: HashMap::new(),
        exports: HashMap::new(),
      },
    );

    // Evaluate module dependencies (imports + runtime re-exports) in source order.
    //
    // This mirrors ECMAScript's evaluation ordering rules: traverse module requests
    // in source order, evaluating dependencies before the module itself.
    #[derive(Clone, Copy)]
    struct ModuleRequest<'a> {
      span: diagnostics::TextRange,
      specifier: &'a str,
    }
    let mut module_requests: Vec<ModuleRequest<'_>> = Vec::new();
    for import in &lowered.hir.imports {
      let ImportKind::Es(es) = &import.kind else {
        return Err(EvalError::new("unsupported import kind"));
      };
      if es.is_type_only {
        continue;
      }
      module_requests.push(ModuleRequest {
        span: import.span,
        specifier: es.specifier.value.as_str(),
      });
    }
    for export in &lowered.hir.exports {
      match &export.kind {
        hir_js::ExportKind::Named(named) => {
          let Some(source) = named.source.as_ref() else {
            continue;
          };
          if named.is_type_only {
            continue;
          }
          let has_value_specifiers = named.specifiers.iter().any(|s| !s.is_type_only);
          let is_side_effect_export = named.specifiers.is_empty();
          if !has_value_specifiers && !is_side_effect_export {
            continue;
          }
          module_requests.push(ModuleRequest {
            span: export.span,
            specifier: source.value.as_str(),
          });
        }
        hir_js::ExportKind::ExportAll(all) => {
          if all.is_type_only {
            continue;
          }
          module_requests.push(ModuleRequest {
            span: export.span,
            specifier: all.source.value.as_str(),
          });
        }
        _ => {}
      }
    }
    module_requests.sort_by_key(|req| (req.span.start, req.span.end));
    for req in module_requests {
      let dep_file = self.resolve_module_specifier(file, req.specifier)?;
      self.eval_module(dep_file)?;
    }

    // Evaluate imports.
    let imports = lowered.hir.imports.clone();
    for import in imports {
      let ImportKind::Es(es) = import.kind else {
        return Err(EvalError::new("unsupported import kind"));
      };
      if es.is_type_only {
        continue;
      }
      let spec = es.specifier.value;
      let dep_file = self.resolve_module_specifier(file, &spec)?;
      self.eval_module(dep_file)?;
      let dep_exports = self
        .modules
        .get(&dep_file)
        .expect("dep module evaluated")
        .exports
        .clone();
      let dep_export_defs = self.program.exports_of(dep_file);

      // Only implement named imports for now.
      for named in es.named {
        if named.is_type_only {
          continue;
        }
        let imported_name = lowered
          .names
          .resolve(named.imported)
          .ok_or_else(|| EvalError::new("missing imported name"))?;
        let Some(value) = dep_exports.get(imported_name).cloned() else {
          return Err(EvalError::new(format!(
            "imported name `{imported_name}` not found in dep module"
          )));
        };
        let export_def = dep_export_defs
          .get(imported_name)
          .and_then(|entry| {
            entry
              .def
              .or_else(|| self.program.symbol_info(entry.symbol).and_then(|info| info.def))
          })
          .ok_or_else(|| {
            EvalError::new(format!("missing export def for imported name `{imported_name}`"))
          })?;
        let bindings = &mut self.modules.get_mut(&file).unwrap().bindings;
        bindings.insert(BindingId::Def(export_def), value.clone());
        if let Some(local_def) = named.local_def {
          bindings.insert(BindingId::Def(local_def), value);
        }
      }
    }

    // Execute top-level body.
    let root_body = self.body_for(&lowered, lowered.hir.root_body)?;
    self.exec_body_statements(file, root_body, &mut Vec::new())?;

    // Collect exports.
    let exports = lowered.hir.exports.clone();
    for export in exports {
      match export.kind {
        hir_js::ExportKind::Named(named) => {
          if named.is_type_only {
            continue;
          }
          if let Some(source) = named.source.as_ref() {
            let dep_file = self.resolve_module_specifier(file, &source.value)?;
            self.eval_module(dep_file)?;
            let dep_exports = self
              .modules
              .get(&dep_file)
              .expect("dep module evaluated")
              .exports
              .clone();
            for spec in named.specifiers {
              if spec.is_type_only {
                continue;
              }
              let Some(local_name) = lowered.names.resolve(spec.local) else {
                continue;
              };
              let Some(exported_name) = lowered.names.resolve(spec.exported) else {
                continue;
              };
              let Some(value) = dep_exports.get(local_name).cloned() else {
                return Err(EvalError::new(format!(
                  "re-exported name `{local_name}` not found in dep module"
                )));
              };
              self
                .modules
                .get_mut(&file)
                .unwrap()
                .exports
                .insert(exported_name.to_string(), value);
            }
            continue;
          }
          for spec in named.specifiers {
            if spec.is_type_only {
              continue;
            }
            let Some(local_def) = spec.local_def else {
              continue;
            };
            let Some(exported_name) = lowered.names.resolve(spec.exported) else {
              continue;
            };
            let value = self
              .modules
              .get(&file)
              .unwrap()
              .bindings
              .get(&BindingId::Def(local_def))
              .cloned()
              .unwrap_or(Value::Undefined);
            self
              .modules
              .get_mut(&file)
              .unwrap()
              .exports
              .insert(exported_name.to_string(), value);
          }
        }
        hir_js::ExportKind::ExportAll(all) => {
          if all.is_type_only {
            continue;
          }
          let dep_file = self.resolve_module_specifier(file, &all.source.value)?;
          self.eval_module(dep_file)?;
          let dep_exports = self
            .modules
            .get(&dep_file)
            .expect("dep module evaluated")
            .exports
            .clone();
          let exports = &mut self.modules.get_mut(&file).unwrap().exports;
          for (name, value) in dep_exports {
            if name == "default" {
              continue;
            }
            exports.entry(name).or_insert(value);
          }
        }
        _ => {
          // Keep this evaluator minimal.
        }
      }
    }

    Ok(())
  }

  fn resolve_module_specifier(&self, from: FileId, specifier: &str) -> EvalResult<FileId> {
    let from_key = self
      .program
      .file_key(from)
      .ok_or_else(|| EvalError::new("missing file key"))?;

    // Only support `./foo.ts`-style relative specifiers in tests.
    let resolved = resolve_relative_file_key(from_key.as_str(), specifier)
      .ok_or_else(|| EvalError::new(format!("unsupported module specifier: {specifier}")))?;
    let key = FileKey::new(resolved);
    self
      .program
      .file_id(&key)
      .ok_or_else(|| EvalError::new(format!("module not loaded: {specifier}")))
  }

  fn call_function(&mut self, func: &EvalFunction, args: &[Value]) -> EvalResult<Value> {
    self.eval_module(func.file)?;
    let lowered = {
      let module = self.modules.get(&func.file).unwrap();
      std::sync::Arc::clone(&module.lowered)
    };
    let body = self.body_for(&lowered, func.body)?;
    let function = body
      .function
      .as_ref()
      .ok_or_else(|| EvalError::new("missing function metadata"))?;

    // One call frame for params/locals. We do not model block scopes separately;
    // bindings are resolved to stable identities (either `DefId` or synthetic
    // per-file symbol ids).
    let mut frame: HashMap<BindingId, Value> = HashMap::new();
    for (idx, param) in function.params.iter().enumerate() {
      let Some(binding) = self
        .resolver
        .for_file(func.file)
        .resolve_pat_ident(body, param.pat)
      else {
        continue;
      };
      let value = args.get(idx).cloned().unwrap_or(Value::Undefined);
      frame.insert(binding, value);
    }

    let mut frames = vec![frame];

    match &function.body {
      hir_js::FunctionBody::Block(stmts) => {
        for stmt in stmts {
          if let Some(ret) = self.exec_stmt(func.file, body, *stmt, &mut frames)? {
            return Ok(ret);
          }
        }
        Ok(Value::Undefined)
      }
      hir_js::FunctionBody::Expr(expr) => self.eval_expr(func.file, body, *expr, &mut frames),
    }
  }

  fn exec_body_statements(
    &mut self,
    file: FileId,
    body: &Body,
    frames: &mut Vec<HashMap<BindingId, Value>>,
  ) -> EvalResult<()> {
    for stmt in &body.root_stmts {
      let _ = self.exec_stmt(file, body, *stmt, frames)?;
    }
    Ok(())
  }

  fn exec_stmt(
    &mut self,
    file: FileId,
    body: &Body,
    stmt_id: StmtId,
    frames: &mut Vec<HashMap<BindingId, Value>>,
  ) -> EvalResult<Option<Value>> {
    let stmt = body
      .stmts
      .get(stmt_id.0 as usize)
      .ok_or_else(|| EvalError::new("missing stmt"))?;

    match &stmt.kind {
      StmtKind::Expr(expr) => {
        let _ = self.eval_expr(file, body, *expr, frames)?;
        Ok(None)
      }
      StmtKind::ExportDefaultExpr(expr) => {
        let _ = self.eval_expr(file, body, *expr, frames)?;
        Ok(None)
      }
      StmtKind::Return(expr) => {
        let value = if let Some(expr) = expr {
          self.eval_expr(file, body, *expr, frames)?
        } else {
          Value::Undefined
        };
        Ok(Some(value))
      }
      StmtKind::Block(stmts) => {
        for stmt in stmts {
          if let Some(ret) = self.exec_stmt(file, body, *stmt, frames)? {
            return Ok(Some(ret));
          }
        }
        Ok(None)
      }
      StmtKind::Var(var) => {
        for decl in &var.declarators {
          let Some(def) = self
            .resolver
            .for_file(file)
            .resolve_pat_ident(body, decl.pat)
          else {
            continue;
          };
          let value = if let Some(init) = decl.init {
            self.eval_expr(file, body, init, frames)?
          } else {
            Value::Undefined
          };
          self.store_binding(file, def, value, frames);
        }
        Ok(None)
      }
      StmtKind::Decl(def) => {
        // Only support function declarations for this evaluator.
        let module = self.modules.get(&file).unwrap();
        let def_data = module
          .lowered
          .def_index
          .get(def)
          .and_then(|idx| module.lowered.defs.get(*idx))
          .ok_or_else(|| EvalError::new("missing def data"))?;
        if def_data.path.kind != hir_js::DefKind::Function {
          return Ok(None);
        }
        let body_id = def_data.body.ok_or_else(|| EvalError::new("missing function body"))?;
        let func = Value::Function(EvalFunction {
          def: *def,
          body: body_id,
          file,
        });
        self.store_binding(file, BindingId::Def(*def), func, frames);
        Ok(None)
      }
      other => Err(EvalError::new(format!(
        "unsupported statement kind in evaluator: {other:?}"
      ))),
    }
  }

  fn eval_expr(
    &mut self,
    file: FileId,
    body: &Body,
    expr_id: ExprId,
    frames: &mut Vec<HashMap<BindingId, Value>>,
  ) -> EvalResult<Value> {
    let expr = body
      .exprs
      .get(expr_id.0 as usize)
      .ok_or_else(|| EvalError::new("missing expr"))?;

    match &expr.kind {
      ExprKind::Literal(lit) => match lit {
        Literal::Number(text) => {
          let n: i64 = text.parse().map_err(|_| EvalError::new("bad number"))?;
          Ok(Value::Number(n))
        }
        Literal::Undefined => Ok(Value::Undefined),
        other => Err(EvalError::new(format!("unsupported literal: {other:?}"))),
      },
      ExprKind::Ident(_) => {
        let def = self
          .resolver
          .for_file(file)
          .resolve_expr_ident(body, expr_id)
          .ok_or_else(|| EvalError::new("unresolved identifier"))?;
        self.load_binding(file, def, frames)
      }
      ExprKind::Binary { op, left, right } => {
        let left = self.eval_expr(file, body, *left, frames)?;
        let right = self.eval_expr(file, body, *right, frames)?;
        match (op, left, right) {
          (BinaryOp::Add, Value::Number(a), Value::Number(b)) => Ok(Value::Number(a + b)),
          (BinaryOp::Subtract, Value::Number(a), Value::Number(b)) => Ok(Value::Number(a - b)),
          (BinaryOp::Multiply, Value::Number(a), Value::Number(b)) => Ok(Value::Number(a * b)),
          (BinaryOp::Divide, Value::Number(a), Value::Number(b)) => Ok(Value::Number(a / b)),
          _ => Err(EvalError::new("unsupported binary op")),
        }
      }
      ExprKind::Assignment { op, target, value } => {
        let value = self.eval_expr(file, body, *value, frames)?;
        let def = self
          .resolver
          .for_file(file)
          .resolve_pat_ident(body, *target)
          .ok_or_else(|| {
            let pat_dbg = body.pats.get(target.0 as usize);
            EvalError::new(format!("unresolved assignment target: {pat_dbg:?}"))
          })?;
        match op {
          AssignOp::Assign => {
            self.store_binding(file, def, value.clone(), frames);
            Ok(value)
          }
          AssignOp::AddAssign => {
            let old = self.load_binding(file, def, frames)?;
            match (old, value) {
              (Value::Number(a), Value::Number(b)) => {
                let out = Value::Number(a + b);
                self.store_binding(file, def, out.clone(), frames);
                Ok(out)
              }
              _ => Err(EvalError::new("unsupported +=")),
            }
          }
          _ => Err(EvalError::new("unsupported assignment op")),
        }
      }
      ExprKind::Call(CallExpr { callee, args, .. }) => {
        let callee = self.eval_expr(file, body, *callee, frames)?;
        let Value::Function(func) = callee else {
          return Err(EvalError::new("callee is not a function"));
        };
        let mut evaluated_args = Vec::new();
        for arg in args {
          if arg.spread {
            return Err(EvalError::new("spread args not supported"));
          }
          evaluated_args.push(self.eval_expr(file, body, arg.expr, frames)?);
        }
        self.call_function(&func, &evaluated_args)
      }
      other => Err(EvalError::new(format!(
        "unsupported expression kind in evaluator: {other:?}"
      ))),
    }
  }

  fn store_binding(
    &mut self,
    file: FileId,
    binding: BindingId,
    value: Value,
    frames: &mut Vec<HashMap<BindingId, Value>>,
  ) {
    // Prefer updating an existing binding (assignment), falling back to inserting
    // in the current frame (declaration).
    for frame in frames.iter_mut().rev() {
      if frame.contains_key(&binding) {
        frame.insert(binding, value);
        return;
      }
    }

    if let Some(module) = self.modules.get_mut(&file) {
      if module.bindings.contains_key(&binding) || frames.is_empty() {
        module.bindings.insert(binding, value);
        return;
      }
    }

    if let Some(frame) = frames.last_mut() {
      frame.insert(binding, value);
    }
  }

  fn load_binding(
    &self,
    file: FileId,
    binding: BindingId,
    frames: &Vec<HashMap<BindingId, Value>>,
  ) -> EvalResult<Value> {
    for frame in frames.iter().rev() {
      if let Some(value) = frame.get(&binding).cloned() {
        return Ok(value);
      }
    }
    self
      .modules
      .get(&file)
      .and_then(|module| module.bindings.get(&binding).cloned())
      .ok_or_else(|| EvalError::new(format!("unbound binding {binding:?}")))
  }

  fn body_for<'a>(&self, lowered: &'a hir_js::LowerResult, body_id: BodyId) -> EvalResult<&'a Body> {
    let idx = lowered
      .body_index
      .get(&body_id)
      .copied()
      .ok_or_else(|| EvalError::new("missing body"))?;
    Ok(lowered.bodies[idx].as_ref())
  }
}

fn resolve_relative_file_key(from: &str, specifier: &str) -> Option<String> {
  if !(specifier.starts_with("./") || specifier.starts_with("../")) {
    return None;
  }

  // Treat `FileKey` values as POSIX-ish paths (they are opaque strings), and
  // resolve `./` and `../` segments deterministically without touching the FS.
  let mut out: Vec<&str> = from.split('/').collect();
  out.pop(); // drop the file name

  for part in specifier.split('/') {
    match part {
      "" | "." => {}
      ".." => {
        out.pop();
      }
      other => out.push(other),
    }
  }

  Some(out.join("/"))
}
