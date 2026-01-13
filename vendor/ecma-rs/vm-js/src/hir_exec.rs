use crate::code::{CompiledFunctionRef, CompiledScript};
use crate::conversion_ops::ToPrimitiveHint;
use crate::exec::{ResolvedBinding, RuntimeEnv};
use crate::function::ThisMode;
use crate::for_in::ForInEnumerator;
use crate::iterator;
use crate::property::{PropertyDescriptor, PropertyKey, PropertyKind};
use crate::tick::DEFAULT_TICK_EVERY;
use crate::tick::vec_try_extend_from_slice_with_ticks;
use crate::{EnvBinding, GcEnv, GcObject, Scope, Value, Vm, VmError, VmHost, VmHostHooks};
use std::collections::HashSet;
use std::cmp::Ordering;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Flow {
  Normal(Option<Value>),
  Return(Value),
  Break(Option<hir_js::NameId>),
  Continue(Option<hir_js::NameId>),
}

impl Flow {
  fn normal(value: Value) -> Self {
    Flow::Normal(Some(value))
  }

  fn empty() -> Self {
    Flow::Normal(None)
  }

  fn update_empty(self, value: Option<Value>) -> Self {
    match self {
      Flow::Normal(None) => Flow::Normal(value),
      other => other,
    }
  }
}

fn throw_reference_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let value = crate::new_reference_error(scope, intr, message)?;
  Ok(VmError::Throw(value))
}

fn root_property_key(scope: &mut Scope<'_>, key: PropertyKey) -> Result<(), VmError> {
  match key {
    PropertyKey::String(s) => {
      scope.push_root(Value::String(s))?;
    }
    PropertyKey::Symbol(s) => {
      scope.push_root(Value::Symbol(s))?;
    }
  }
  Ok(())
}

fn concat_strings(
  scope: &mut Scope<'_>,
  a: crate::GcString,
  b: crate::GcString,
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<crate::GcString, VmError> {
  // Root both inputs while allocating the concatenated string.
  let mut scope = scope.reborrow();
  scope.push_roots(&[Value::String(a), Value::String(b)])?;

  let (a_units_len, b_units_len) = {
    let heap = scope.heap();
    (
      heap.get_string(a)?.as_code_units().len(),
      heap.get_string(b)?.as_code_units().len(),
    )
  };

  let total_len = a_units_len
    .checked_add(b_units_len)
    .ok_or(VmError::OutOfMemory)?;

  let mut units: Vec<u16> = Vec::new();
  units
    .try_reserve_exact(total_len)
    .map_err(|_| VmError::OutOfMemory)?;

  {
    let heap = scope.heap();
    vec_try_extend_from_slice_with_ticks(&mut units, heap.get_string(a)?.as_code_units(), || tick())?;
    vec_try_extend_from_slice_with_ticks(&mut units, heap.get_string(b)?.as_code_units(), || tick())?;
  }

  scope.alloc_string_from_u16_vec(units)
}

fn vec_try_extend_utf16_from_str_with_ticks(
  out: &mut Vec<u16>,
  s: &str,
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<(), VmError> {
  // Extend `out` with the UTF-16 encoding of `s` using fallible allocation and periodic ticks.
  //
  // We buffer into a fixed-size stack array so we can:
  // - use `Vec::try_reserve` to avoid panicking on OOM, and
  // - tick periodically so large template literal segments can't perform long stretches of
  //   uninterruptible work.
  let mut buf = [0u16; DEFAULT_TICK_EVERY];
  let mut buf_len = 0usize;
  let mut need_tick = false;

  for unit in s.encode_utf16() {
    if need_tick {
      tick()?;
      need_tick = false;
    }

    buf[buf_len] = unit;
    buf_len += 1;

    if buf_len == buf.len() {
      out
        .try_reserve(buf_len)
        .map_err(|_| VmError::OutOfMemory)?;
      out.extend_from_slice(&buf[..buf_len]);
      buf_len = 0;
      // Defer ticking until we know there is at least one more code unit.
      need_tick = true;
    }
  }

  if buf_len != 0 {
    out
      .try_reserve(buf_len)
      .map_err(|_| VmError::OutOfMemory)?;
    out.extend_from_slice(&buf[..buf_len]);
  }

  Ok(())
}

struct HirEvaluator<'vm> {
  vm: &'vm mut Vm,
  host: &'vm mut dyn VmHost,
  hooks: &'vm mut dyn VmHostHooks,
  env: &'vm mut RuntimeEnv,
  strict: bool,
  this: Value,
  new_target: Value,
  script: Arc<CompiledScript>,
}

impl<'vm> HirEvaluator<'vm> {
  fn hir(&self) -> &hir_js::LowerResult {
    self.script.hir.as_ref()
  }

  fn resolve_name(&self, id: hir_js::NameId) -> Result<String, VmError> {
    Ok(
      self
        .hir()
        .names
        .resolve(id)
        .ok_or(VmError::InvariantViolation(
          "hir name id missing from interner",
        ))?
        .to_owned(),
    )
  }

  fn get_body(&self, id: hir_js::BodyId) -> Result<&hir_js::Body, VmError> {
    self
      .hir()
      .body(id)
      .ok_or(VmError::InvariantViolation("hir body id missing from compiled script"))
  }

  fn get_stmt<'a>(&self, body: &'a hir_js::Body, id: hir_js::StmtId) -> Result<&'a hir_js::Stmt, VmError> {
    body
      .stmts
      .get(id.0 as usize)
      .ok_or(VmError::InvariantViolation("hir stmt id out of bounds"))
  }

  fn get_expr<'a>(&self, body: &'a hir_js::Body, id: hir_js::ExprId) -> Result<&'a hir_js::Expr, VmError> {
    body
      .exprs
      .get(id.0 as usize)
      .ok_or(VmError::InvariantViolation("hir expr id out of bounds"))
  }

  fn get_pat<'a>(&self, body: &'a hir_js::Body, id: hir_js::PatId) -> Result<&'a hir_js::Pat, VmError> {
    body
      .pats
      .get(id.0 as usize)
      .ok_or(VmError::InvariantViolation("hir pat id out of bounds"))
  }

  fn detect_use_strict_directive(&mut self, body: &hir_js::Body) -> Result<bool, VmError> {
    const TICK_EVERY: usize = 32;
    for (i, stmt_id) in body.root_stmts.iter().enumerate() {
      if i % TICK_EVERY == 0 {
        self.vm.tick()?;
      }
      let stmt = self.get_stmt(body, *stmt_id)?;
      let hir_js::StmtKind::Expr(expr_id) = stmt.kind else {
        break;
      };
      let expr = self.get_expr(body, expr_id)?;
      let hir_js::ExprKind::Literal(hir_js::Literal::String(s)) = &expr.kind else {
        break;
      };
      if s.lossy == "use strict" {
        // Treat as strict; HIR does not currently preserve parenthesization metadata for directive
        // prologues (unlike the parse-js AST), so this is best-effort.
        return Ok(true);
      }
      break;
    }
    Ok(false)
  }

  fn alloc_user_function_object(
    &mut self,
    scope: &mut Scope<'_>,
    body_id: hir_js::BodyId,
    name: &str,
    is_arrow: bool,
  ) -> Result<GcObject, VmError> {
    let func_body = self.get_body(body_id)?;
    let Some(func_meta) = func_body.function.as_ref() else {
      return Err(VmError::InvariantViolation("function body missing function metadata"));
    };
    if func_meta.generator {
      return Err(VmError::Unimplemented(if func_meta.async_ {
        "async generator functions"
      } else {
        "generator functions"
      }));
    }
    if func_meta.async_ {
      return Err(VmError::Unimplemented("async functions (hir-js compiled path)"));
    }

    let length = u32::try_from(func_meta.params.len()).unwrap_or(u32::MAX);

    // Root inputs across string allocation + function allocation in case either triggers GC.
    let mut scope = scope.reborrow();
    let closure_env = Some(self.env.lexical_env());
    scope.push_env_root(self.env.lexical_env())?;
    scope.push_root(self.this)?;
    scope.push_root(self.new_target)?;

    let name_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(name_s))?;

    let this_mode = if is_arrow {
      ThisMode::Lexical
    } else if self.strict {
      ThisMode::Strict
    } else {
      ThisMode::Global
    };

    let func_obj = scope.alloc_user_function_with_env(
      CompiledFunctionRef {
        script: self.script.clone(),
        body: body_id,
      },
      name_s,
      length,
      this_mode,
      /* is_strict */ self.strict,
      closure_env,
    )?;

    // Arrow functions capture lexical `this`/`new.target`.
    if is_arrow {
      scope.heap_mut().set_function_bound_this(func_obj, self.this)?;
      scope
        .heap_mut()
        .set_function_bound_new_target(func_obj, self.new_target)?;
    }

    // Best-effort function `[[Prototype]]` / `[[Realm]]` metadata.
    if let Some(intr) = self.vm.intrinsics() {
      scope
        .heap_mut()
        .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
    }
    scope
      .heap_mut()
      .set_function_realm(func_obj, self.env.global_object())?;
    if let Some(realm) = self.vm.current_realm() {
      scope.heap_mut().set_function_job_realm(func_obj, realm)?;
    }
    if let Some(script_or_module) = self.vm.get_active_script_or_module() {
      let token = self.vm.intern_script_or_module(script_or_module)?;
      scope
        .heap_mut()
        .set_function_script_or_module_token(func_obj, Some(token))?;
    }

    // Ordinary functions are constructors and get a `.prototype` object. This is required for
    // `instanceof` and user code that accesses `F.prototype` (even though `new` is not yet
    // implemented in the compiled path).
    if !is_arrow {
      let _ = crate::function_properties::make_constructor(&mut scope, func_obj)?;
    }

    Ok(func_obj)
  }

  fn instantiate_function_body(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    args: &[Value],
  ) -> Result<(), VmError> {
    let Some(func_meta) = body.function.as_ref() else {
      return Err(VmError::InvariantViolation("function body missing function metadata"));
    };

    // Bind parameters.
    for (idx, param) in func_meta.params.iter().enumerate() {
      self.vm.tick()?;
      if param.rest {
        return Err(VmError::Unimplemented("rest parameters (hir-js compiled path)"));
      }
      let pat = self.get_pat(body, param.pat)?;
      let hir_js::PatKind::Ident(name_id) = pat.kind else {
        return Err(VmError::Unimplemented(
          "non-identifier parameters (hir-js compiled path)",
        ));
      };
      let name = self.resolve_name(name_id)?;
      let arg_value = args.get(idx).copied().unwrap_or(Value::Undefined);

      // Default parameters.
      let value = if matches!(arg_value, Value::Undefined) {
        if let Some(default_expr) = param.default {
          self.eval_expr(scope, body, default_expr)?
        } else {
          Value::Undefined
        }
      } else {
        arg_value
      };

      // Parameters are mutable bindings in the function environment.
      let env_rec = self.env.lexical_env();
      if !scope.heap().env_has_binding(env_rec, name.as_str())? {
        scope.env_create_mutable_binding(env_rec, name.as_str())?;
      }
      scope
        .heap_mut()
        .env_initialize_binding(env_rec, name.as_str(), value)?;
    }

    // Hoist function declarations (best-effort).
    //
    // This enables simple recursion and calling a function before its declaration statement is
    // executed.
    self.instantiate_var_decls(scope, body, body.root_stmts.as_slice())?;
    self.instantiate_function_decls(scope, body, body.root_stmts.as_slice())?;

    // Create `let` / `const` bindings for the entire function body statement list up-front so TDZ
    // + shadowing semantics are correct.
    self.instantiate_lexical_decls(scope, body, body.root_stmts.as_slice(), self.env.lexical_env())?;

    Ok(())
  }

  fn instantiate_var_decls(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
  ) -> Result<(), VmError> {
    for stmt_id in stmts {
      self.vm.tick()?;
      let stmt = self.get_stmt(body, *stmt_id)?;
      match &stmt.kind {
        hir_js::StmtKind::Var(decl) => {
          if decl.kind == hir_js::VarDeclKind::Var {
            for declarator in &decl.declarators {
              self.vm.tick()?;
              let pat = self.get_pat(body, declarator.pat)?;
              let hir_js::PatKind::Ident(name_id) = pat.kind else {
                return Err(VmError::Unimplemented(
                  "non-identifier variable declarations (hir-js compiled path)",
                ));
              };
              let name = self.resolve_name(name_id)?;
              self.env.declare_var(self.vm, scope, name.as_str())?;
            }
          }
        }
        hir_js::StmtKind::For { init, body: inner, .. } => {
          if let Some(hir_js::ForInit::Var(decl)) = init {
            if decl.kind == hir_js::VarDeclKind::Var {
              for declarator in &decl.declarators {
                self.vm.tick()?;
                let pat = self.get_pat(body, declarator.pat)?;
                let hir_js::PatKind::Ident(name_id) = pat.kind else {
                  return Err(VmError::Unimplemented(
                    "non-identifier variable declarations (hir-js compiled path)",
                  ));
                };
                let name = self.resolve_name(name_id)?;
                self.env.declare_var(self.vm, scope, name.as_str())?;
              }
            }
          }
          self.instantiate_var_decls(scope, body, std::slice::from_ref(inner))?;
        }
        hir_js::StmtKind::ForIn { left, body: inner, .. } => {
          if let hir_js::ForHead::Var(decl) = left {
            if decl.kind == hir_js::VarDeclKind::Var {
              for declarator in &decl.declarators {
                self.vm.tick()?;
                let pat = self.get_pat(body, declarator.pat)?;
                let hir_js::PatKind::Ident(name_id) = pat.kind else {
                  return Err(VmError::Unimplemented(
                    "non-identifier variable declarations (hir-js compiled path)",
                  ));
                };
                let name = self.resolve_name(name_id)?;
                self.env.declare_var(self.vm, scope, name.as_str())?;
              }
            }
          }
          self.instantiate_var_decls(scope, body, std::slice::from_ref(inner))?;
        }
        hir_js::StmtKind::Block(inner) => {
          self.instantiate_var_decls(scope, body, inner.as_slice())?;
        }
        hir_js::StmtKind::If {
          consequent,
          alternate,
          ..
        } => {
          self.instantiate_var_decls(scope, body, std::slice::from_ref(consequent))?;
          if let Some(alt) = alternate {
            self.instantiate_var_decls(scope, body, std::slice::from_ref(alt))?;
          }
        }
        hir_js::StmtKind::While { body: inner, .. }
        | hir_js::StmtKind::DoWhile { body: inner, .. }
        | hir_js::StmtKind::Labeled { body: inner, .. }
        | hir_js::StmtKind::With { body: inner, .. } => {
          self.instantiate_var_decls(scope, body, std::slice::from_ref(inner))?;
        }
        hir_js::StmtKind::Try {
          block,
          catch,
          finally_block,
        } => {
          self.instantiate_var_decls(scope, body, std::slice::from_ref(block))?;
          if let Some(catch) = catch {
            self.instantiate_var_decls(scope, body, std::slice::from_ref(&catch.body))?;
          }
          if let Some(finally_block) = finally_block {
            self.instantiate_var_decls(scope, body, std::slice::from_ref(finally_block))?;
          }
        }
        hir_js::StmtKind::Switch { cases, .. } => {
          for case in cases {
            self.instantiate_var_decls(scope, body, case.consequent.as_slice())?;
          }
        }
        _ => {}
      }
    }
    Ok(())
  }

  fn instantiate_function_decls(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
  ) -> Result<(), VmError> {
    for stmt_id in stmts {
      self.vm.tick()?;
      let stmt = self.get_stmt(body, *stmt_id)?;
      match &stmt.kind {
        hir_js::StmtKind::Decl(def_id) => {
          // Only hoist function declarations. Class declarations are currently unimplemented in the
          // compiled path.
          let def = self
            .hir()
            .def(*def_id)
            .ok_or(VmError::InvariantViolation("hir def id missing from compiled script"))?;
          let Some(body_id) = def.body else {
            continue;
          };
          let decl_body = self.get_body(body_id)?;
          if decl_body.kind != hir_js::BodyKind::Function {
            continue;
          }
          let name = self.resolve_name(def.name)?;
          let func_obj =
            self.alloc_user_function_object(scope, body_id, name.as_str(), /* is_arrow */ false)?;
          // Root the function object while assigning into the environment.
          let mut assign_scope = scope.reborrow();
          assign_scope.push_root(Value::Object(func_obj))?;
          self.env.set_var(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            &mut assign_scope,
            name.as_str(),
            Value::Object(func_obj),
          )?;
        }
        hir_js::StmtKind::Block(inner) => {
          self.instantiate_function_decls(scope, body, inner.as_slice())?;
        }
        hir_js::StmtKind::If {
          consequent,
          alternate,
          ..
        } => {
          self.instantiate_function_decls(scope, body, std::slice::from_ref(consequent))?;
          if let Some(alt) = alternate {
            self.instantiate_function_decls(scope, body, std::slice::from_ref(alt))?;
          }
        }
        hir_js::StmtKind::While { body: inner, .. }
        | hir_js::StmtKind::DoWhile { body: inner, .. }
        | hir_js::StmtKind::Labeled { body: inner, .. }
        | hir_js::StmtKind::With { body: inner, .. } => {
          self.instantiate_function_decls(scope, body, std::slice::from_ref(inner))?;
        }
        hir_js::StmtKind::For { body: inner, .. } => {
          self.instantiate_function_decls(scope, body, std::slice::from_ref(inner))?;
        }
        hir_js::StmtKind::Try {
          block,
          catch,
          finally_block,
        } => {
          self.instantiate_function_decls(scope, body, std::slice::from_ref(block))?;
          if let Some(catch) = catch {
            self.instantiate_function_decls(scope, body, std::slice::from_ref(&catch.body))?;
          }
          if let Some(finally_block) = finally_block {
            self.instantiate_function_decls(scope, body, std::slice::from_ref(finally_block))?;
          }
        }
        hir_js::StmtKind::Switch { cases, .. } => {
          for case in cases {
            self.instantiate_function_decls(scope, body, case.consequent.as_slice())?;
          }
        }
        _ => {}
      }
    }
    Ok(())
  }

  fn instantiate_lexical_decls(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
    env: GcEnv,
  ) -> Result<(), VmError> {
    for stmt_id in stmts {
      self.vm.tick()?;
      let stmt = self.get_stmt(body, *stmt_id)?;
      let hir_js::StmtKind::Var(decl) = &stmt.kind else {
        continue;
      };
      self.instantiate_lexical_decl(scope, body, decl, env)?;
    }
    Ok(())
  }

  fn instantiate_lexical_decl(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    decl: &hir_js::VarDecl,
    env: GcEnv,
  ) -> Result<(), VmError> {
    match decl.kind {
      hir_js::VarDeclKind::Let | hir_js::VarDeclKind::Const => {}
      _ => return Ok(()),
    }

    for declarator in &decl.declarators {
      self.vm.tick()?;
      let pat = self.get_pat(body, declarator.pat)?;
      let hir_js::PatKind::Ident(name_id) = pat.kind else {
        return Err(VmError::Unimplemented(
          "non-identifier variable declarations (hir-js compiled path)",
        ));
      };
      let name = self.resolve_name(name_id)?;

      // Keep the engine robust against malformed HIR (e.g. a binding already exists).
      if scope.heap().env_has_binding(env, name.as_str())? {
        continue;
      }

      match decl.kind {
        hir_js::VarDeclKind::Let => scope.env_create_mutable_binding(env, name.as_str())?,
        hir_js::VarDeclKind::Const => scope.env_create_immutable_binding(env, name.as_str())?,
        _ => unreachable!(),
      }
    }
    Ok(())
  }

  fn eval_stmt_list(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
  ) -> Result<Flow, VmError> {
    let mut last: Option<Value> = None;
    for stmt_id in stmts {
      let flow = self.eval_stmt(scope, body, *stmt_id)?;
      match flow {
        Flow::Normal(v) => {
          if v.is_some() {
            last = v;
          }
        }
        abrupt => return Ok(abrupt.update_empty(last)),
      }
    }
    Ok(Flow::Normal(last))
  }

  fn eval_stmt(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmt_id: hir_js::StmtId,
  ) -> Result<Flow, VmError> {
    // Budget once per statement evaluation.
    self.vm.tick()?;

    let stmt = self.get_stmt(body, stmt_id)?;
    match &stmt.kind {
      hir_js::StmtKind::Expr(expr_id) => {
        let v = self.eval_expr(scope, body, *expr_id)?;
        Ok(Flow::normal(v))
      }
      hir_js::StmtKind::Return(expr) => {
        let v = match expr {
          Some(id) => self.eval_expr(scope, body, *id)?,
          None => Value::Undefined,
        };
        Ok(Flow::Return(v))
      }
      hir_js::StmtKind::Block(stmts) => {
        // Block-scoped lexical environment.
        let prev = self.env.lexical_env();
        let block_env = scope.env_create(Some(prev))?;
        self.env.set_lexical_env(scope.heap_mut(), block_env);
        let result = (|| {
          self.instantiate_lexical_decls(scope, body, stmts.as_slice(), block_env)?;
          self.eval_stmt_list(scope, body, stmts.as_slice())
        })();
        self.env.set_lexical_env(scope.heap_mut(), prev);
        result
      }
      hir_js::StmtKind::If {
        test,
        consequent,
        alternate,
      } => {
        let test_value = self.eval_expr(scope, body, *test)?;
        if scope.heap().to_boolean(test_value)? {
          self.eval_stmt(scope, body, *consequent)
        } else if let Some(alt) = alternate {
          self.eval_stmt(scope, body, *alt)
        } else {
          Ok(Flow::empty())
        }
      }
      hir_js::StmtKind::While { test, body: inner } => {
        loop {
          // Ensure empty loops still consume budget.
          self.vm.tick()?;
          let test_value = self.eval_expr(scope, body, *test)?;
          if !scope.heap().to_boolean(test_value)? {
            return Ok(Flow::empty());
          }
          match self.eval_stmt(scope, body, *inner)? {
            Flow::Normal(_) => {}
            Flow::Continue(None) => {}
            Flow::Continue(Some(label)) => return Ok(Flow::Continue(Some(label))),
            Flow::Break(None) => return Ok(Flow::empty()),
            Flow::Break(Some(label)) => return Ok(Flow::Break(Some(label))),
            Flow::Return(v) => return Ok(Flow::Return(v)),
          }
        }
      }
      hir_js::StmtKind::DoWhile { test, body: inner } => {
        loop {
          self.vm.tick()?;
          match self.eval_stmt(scope, body, *inner)? {
            Flow::Normal(_) => {}
            Flow::Continue(None) => {}
            Flow::Continue(Some(label)) => return Ok(Flow::Continue(Some(label))),
            Flow::Break(None) => return Ok(Flow::empty()),
            Flow::Break(Some(label)) => return Ok(Flow::Break(Some(label))),
            Flow::Return(v) => return Ok(Flow::Return(v)),
          }
          let test_value = self.eval_expr(scope, body, *test)?;
          if !scope.heap().to_boolean(test_value)? {
            return Ok(Flow::empty());
          }
        }
      }
      hir_js::StmtKind::For {
        init,
        test,
        update,
        body: inner,
      } => {
        // Lexically-declared `for` loops require per-iteration environments so closures capture the
        // correct binding value (ECMA-262 `CreatePerIterationEnvironment`).
        let lexical_init = match init {
          Some(hir_js::ForInit::Var(decl))
            if matches!(decl.kind, hir_js::VarDeclKind::Let | hir_js::VarDeclKind::Const) =>
          {
            Some(decl)
          }
          _ => None,
        };

        if let Some(init_decl) = lexical_init {
          let outer_lex = self.env.lexical_env();
          let result = (|| -> Result<Flow, VmError> {
            // Create a loop-scoped declarative environment for the lexical declaration and evaluate
            // the initializer with TDZ semantics.
            let loop_env = scope.env_create(Some(outer_lex))?;
            self.env.set_lexical_env(scope.heap_mut(), loop_env);

            // Bind names in TDZ before evaluating initializers (only identifier patterns for now).
            for declarator in &init_decl.declarators {
              self.vm.tick()?;
              let pat = self.get_pat(body, declarator.pat)?;
              let hir_js::PatKind::Ident(name_id) = pat.kind else {
                return Err(VmError::Unimplemented(
                  "non-identifier for-loop bindings (hir-js compiled path)",
                ));
              };
              let name = self.resolve_name(name_id)?;
              match init_decl.kind {
                hir_js::VarDeclKind::Let => {
                  scope.env_create_mutable_binding(loop_env, name.as_str())?;
                }
                hir_js::VarDeclKind::Const => {
                  scope.env_create_immutable_binding(loop_env, name.as_str())?;
                }
                _ => unreachable!("checked in lexical_init match"),
              }
            }

            // Evaluate initializer(s) and initialize the bindings.
            self.eval_var_decl(scope, body, init_decl)?;

            // Enter the first per-iteration environment.
            let mut iter_env = self.create_for_triple_per_iteration_env(scope, outer_lex, loop_env)?;
            self.env.set_lexical_env(scope.heap_mut(), iter_env);

            loop {
              // Ensure empty loops still consume budget.
              self.vm.tick()?;

              if let Some(test) = test {
                let test_value = self.eval_expr(scope, body, *test)?;
                if !scope.heap().to_boolean(test_value)? {
                  return Ok(Flow::empty());
                }
              }

              match self.eval_stmt(scope, body, *inner)? {
                Flow::Normal(_) => {}
                Flow::Continue(None) => {}
                Flow::Continue(Some(label)) => return Ok(Flow::Continue(Some(label))),
                Flow::Break(None) => return Ok(Flow::empty()),
                Flow::Break(Some(label)) => return Ok(Flow::Break(Some(label))),
                Flow::Return(v) => return Ok(Flow::Return(v)),
              }

              // Create the next iteration's environment *before* evaluating the update expression so
              // closures created in the body do not observe the post-update value.
              iter_env = self.create_for_triple_per_iteration_env(scope, outer_lex, iter_env)?;
              self.env.set_lexical_env(scope.heap_mut(), iter_env);

              if let Some(update) = update {
                let _ = self.eval_expr(scope, body, *update)?;
              }
            }
          })();

          // Always restore the outer lexical environment so later statements run in the correct
          // scope.
          self.env.set_lexical_env(scope.heap_mut(), outer_lex);
          return result;
        }
        if let Some(init) = init {
          match init {
            hir_js::ForInit::Expr(expr) => {
              let _ = self.eval_expr(scope, body, *expr)?;
            }
            hir_js::ForInit::Var(decl) => {
              self.eval_var_decl(scope, body, decl)?;
            }
          }
        }

        loop {
          // Ensure empty loops still consume budget.
          self.vm.tick()?;
          if let Some(test) = test {
            let test_value = self.eval_expr(scope, body, *test)?;
            if !scope.heap().to_boolean(test_value)? {
              return Ok(Flow::empty());
            }
          }

          match self.eval_stmt(scope, body, *inner)? {
            Flow::Normal(_) => {}
            Flow::Continue(None) => {}
            Flow::Continue(Some(label)) => return Ok(Flow::Continue(Some(label))),
            Flow::Break(None) => return Ok(Flow::empty()),
            Flow::Break(Some(label)) => return Ok(Flow::Break(Some(label))),
            Flow::Return(v) => return Ok(Flow::Return(v)),
          }

          if let Some(update) = update {
            let _ = self.eval_expr(scope, body, *update)?;
          }
        }
      }
      hir_js::StmtKind::ForIn {
        left,
        right,
        body: inner,
        is_for_of,
        await_,
      } => {
        if *await_ {
          return Err(VmError::Unimplemented("for await..of (hir-js compiled path)"));
        }

        if *is_for_of {
          // --- for..of ---
          let iterable = self.eval_expr(scope, body, *right)?;

          // Root the iterable + iterator record while evaluating the loop body.
          let mut iter_scope = scope.reborrow();
          iter_scope.push_root(iterable)?;

          let mut iterator_record = iterator::get_iterator(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            &mut iter_scope,
            iterable,
          )?;
          iter_scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

          // Root the current iteration value across binding + body evaluation.
          let iter_value_root_idx = iter_scope.heap().root_stack.len();
          iter_scope.push_root(Value::Undefined)?;

          // Per-iteration lexical environments for `let`/`const` in the head.
          let outer_lex: GcEnv = self.env.lexical_env();

          loop {
            // Tick once per iteration so `for (x of xs) {}` is budgeted even when the body is empty.
            self.vm.tick()?;

            let next_value = match iterator::iterator_step_value(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              &mut iter_scope,
              &mut iterator_record,
            ) {
              Ok(v) => v,
              // Spec: `ForIn/OfBodyEvaluation` does not perform `IteratorClose` on errors produced
              // while stepping the iterator (`next`/`done`/`value`).
              Err(err) => return Err(err),
            };

            let Some(iter_value) = next_value else {
              break;
            };

            // Root the iteration value so env/binding work can allocate/GC safely.
            iter_scope.heap_mut().root_stack[iter_value_root_idx] = iter_value;

            let mut iter_env: Option<GcEnv> = None;
            if let hir_js::ForHead::Var(var_decl) = left {
              if matches!(var_decl.kind, hir_js::VarDeclKind::Let | hir_js::VarDeclKind::Const) {
                let env = iter_scope.env_create(Some(outer_lex))?;
                self.env.set_lexical_env(iter_scope.heap_mut(), env);
                iter_env = Some(env);
              } else if !matches!(var_decl.kind, hir_js::VarDeclKind::Var) {
                return Err(VmError::Unimplemented(
                  "for-of loop variable declaration kind (hir-js compiled path)",
                ));
              }
            }

            // Binding errors must close the iterator.
            if let Err(err) = self.bind_for_in_of_head(&mut iter_scope, body, left, iter_value) {
              if iter_env.is_some() {
                self.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
              }

              // Root the thrown value (if any) across iterator closing, since it can allocate / GC.
              if let Some(v) = err.thrown_value() {
                iter_scope.push_root(v)?;
              }
              match iterator::iterator_close(
                self.vm,
                &mut *self.host,
                &mut *self.hooks,
                &mut iter_scope,
                &iterator_record,
                iterator::CloseCompletionKind::Throw,
              ) {
                Ok(()) => return Err(err),
                Err(close_err) => return Err(close_err),
              }
            }

            let flow = match self.eval_stmt(&mut iter_scope, body, *inner) {
              Ok(f) => f,
              Err(err) => {
                if iter_env.is_some() {
                  self.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
                }
                if let Some(v) = err.thrown_value() {
                  iter_scope.push_root(v)?;
                }
                match iterator::iterator_close(
                  self.vm,
                  &mut *self.host,
                  &mut *self.hooks,
                  &mut iter_scope,
                  &iterator_record,
                  iterator::CloseCompletionKind::Throw,
                ) {
                  Ok(()) => return Err(err),
                  Err(close_err) => return Err(close_err),
                }
              }
            };

            if iter_env.is_some() {
              self.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
            }

            match flow {
              Flow::Normal(_) => {}
              Flow::Continue(None) => {}
              Flow::Continue(Some(label)) => {
                if let Err(err) = iterator::iterator_close(
                  self.vm,
                  &mut *self.host,
                  &mut *self.hooks,
                  &mut iter_scope,
                  &iterator_record,
                  iterator::CloseCompletionKind::NonThrow,
                ) {
                  return Err(err);
                }
                return Ok(Flow::Continue(Some(label)));
              }
              Flow::Break(None) => {
                if let Err(err) = iterator::iterator_close(
                  self.vm,
                  &mut *self.host,
                  &mut *self.hooks,
                  &mut iter_scope,
                  &iterator_record,
                  iterator::CloseCompletionKind::NonThrow,
                ) {
                  return Err(err);
                }
                return Ok(Flow::empty());
              }
              Flow::Break(Some(label)) => {
                if let Err(err) = iterator::iterator_close(
                  self.vm,
                  &mut *self.host,
                  &mut *self.hooks,
                  &mut iter_scope,
                  &iterator_record,
                  iterator::CloseCompletionKind::NonThrow,
                ) {
                  return Err(err);
                }
                return Ok(Flow::Break(Some(label)));
              }
              Flow::Return(v) => {
                // Root the return value across iterator closing.
                iter_scope.push_root(v)?;
                if let Err(err) = iterator::iterator_close(
                  self.vm,
                  &mut *self.host,
                  &mut *self.hooks,
                  &mut iter_scope,
                  &iterator_record,
                  iterator::CloseCompletionKind::NonThrow,
                ) {
                  return Err(err);
                }
                return Ok(Flow::Return(v));
              }
            }
          }

          Ok(Flow::empty())
        } else {
          // --- for..in ---
          let rhs_value = self.eval_expr(scope, body, *right)?;

          // Root the RHS while converting to object; `ToObject` can allocate/GC and the RHS might
          // not be reachable from any heap object.
          let mut iter_scope = scope.reborrow();
          iter_scope.push_root(rhs_value)?;
          let object = iter_scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, rhs_value)?;

          // Root the base object while enumerating keys and executing the loop body.
          iter_scope.push_root(Value::Object(object))?;

          let mut enumerator = ForInEnumerator::new(object);

          // Root the current key value across binding + body evaluation.
          let key_value_root_idx = iter_scope.heap().root_stack.len();
          iter_scope.push_root(Value::Undefined)?;

          // Per-iteration lexical environments for `let`/`const` in the head.
          let outer_lex: GcEnv = self.env.lexical_env();

          loop {
            let next_key = enumerator.next_key(
              self.vm,
              &mut iter_scope,
              &mut *self.host,
              &mut *self.hooks,
            )?;
            let Some(key_s) = next_key else {
              break;
            };

            // Tick once per iteration so `for (k in o) {}` is budgeted even when the body is empty.
            self.vm.tick()?;

            let iter_value = Value::String(key_s);
            iter_scope.heap_mut().root_stack[key_value_root_idx] = iter_value;

            let mut iter_env: Option<GcEnv> = None;
            if let hir_js::ForHead::Var(var_decl) = left {
              if matches!(var_decl.kind, hir_js::VarDeclKind::Let | hir_js::VarDeclKind::Const) {
                let env = iter_scope.env_create(Some(outer_lex))?;
                self.env.set_lexical_env(iter_scope.heap_mut(), env);
                iter_env = Some(env);
              } else if !matches!(var_decl.kind, hir_js::VarDeclKind::Var) {
                return Err(VmError::Unimplemented(
                  "for-in loop variable declaration kind (hir-js compiled path)",
                ));
              }
            }

            if let Err(err) = self.bind_for_in_of_head(&mut iter_scope, body, left, iter_value) {
              if iter_env.is_some() {
                self.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
              }
              return Err(err);
            }

            let flow = match self.eval_stmt(&mut iter_scope, body, *inner) {
              Ok(f) => f,
              Err(err) => {
                if iter_env.is_some() {
                  self.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
                }
                return Err(err);
              }
            };

            if iter_env.is_some() {
              self.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
            }

            match flow {
              Flow::Normal(_) => {}
              Flow::Continue(None) => {}
              Flow::Continue(Some(label)) => return Ok(Flow::Continue(Some(label))),
              Flow::Break(None) => return Ok(Flow::empty()),
              Flow::Break(Some(label)) => return Ok(Flow::Break(Some(label))),
              Flow::Return(v) => return Ok(Flow::Return(v)),
            }
          }

          Ok(Flow::empty())
        }
      }
      hir_js::StmtKind::Switch { discriminant, cases } => {
        // Evaluate the discriminant once (before creating the switch case lexical environment).
        let discriminant_value = self.eval_expr(scope, body, *discriminant)?;

        // Root the discriminant across selector evaluation and case-body execution, which may
        // allocate and trigger GC.
        let mut switch_scope = scope.reborrow();
        switch_scope.push_root(discriminant_value)?;

        // `switch` creates a new lexical environment for the entire case block.
        let outer = self.env.lexical_env();
        let switch_env = switch_scope.env_create(Some(outer))?;
        self
          .env
          .set_lexical_env(switch_scope.heap_mut(), switch_env);

        let result = (|| -> Result<Flow, VmError> {
          const CASE_TICK_EVERY: usize = 32;

          // Create `let` / `const` bindings for the entire case block up-front so TDZ + shadowing
          // semantics are correct across case selectors and clause bodies.
          for (i, case) in cases.iter().enumerate() {
            // Budget case traversal even when the case bodies are empty.
            if i % CASE_TICK_EVERY == 0 {
              self.vm.tick()?;
            }
            self.instantiate_lexical_decls(
              &mut switch_scope,
              body,
              case.consequent.as_slice(),
              switch_env,
            )?;
          }

          // Find the first matching case (or the `default` case).
          let mut default_idx: Option<usize> = None;
          let mut start_idx: Option<usize> = None;
          for (i, case) in cases.iter().enumerate() {
            // Budget case traversal even when case tests/bodies are empty.
            if i % CASE_TICK_EVERY == 0 {
              self.vm.tick()?;
            }
            match case.test {
              None => {
                if default_idx.is_none() {
                  default_idx = Some(i);
                }
              }
              Some(test_expr) => {
                let case_value = self.eval_expr(&mut switch_scope, body, test_expr)?;
                if self.strict_equality_comparison(&mut switch_scope, discriminant_value, case_value)? {
                  start_idx = Some(i);
                  break;
                }
              }
            }
          }
          if start_idx.is_none() {
            start_idx = default_idx;
          }

          // ECMA-262 `CaseBlockEvaluation`: `V` starts as `undefined` and is never ~empty~ for normal
          // completion.
          let v_root_idx = switch_scope.heap().root_stack.len();
          switch_scope.push_root(Value::Undefined)?;
          let mut v = Value::Undefined;

          if let Some(start) = start_idx {
            // Execute clause bodies sequentially (with fallthrough) starting at the selected case.
            for (case_idx, case) in cases.iter().enumerate().skip(start) {
              if case_idx % CASE_TICK_EVERY == 0 {
                self.vm.tick()?;
              }
              for stmt_id in &case.consequent {
                match self.eval_stmt(&mut switch_scope, body, *stmt_id)? {
                  Flow::Normal(value) => {
                    if let Some(value) = value {
                      v = value;
                      switch_scope.heap_mut().root_stack[v_root_idx] = value;
                    }
                  }
                  // Unlabeled `break` exits the switch.
                  Flow::Break(None) => return Ok(Flow::Normal(Some(v))),
                  // Labeled control flow propagates.
                  Flow::Break(Some(label)) => return Ok(Flow::Break(Some(label))),
                  Flow::Continue(label) => return Ok(Flow::Continue(label)),
                  Flow::Return(value) => return Ok(Flow::Return(value)),
                }
              }
            }
          }

          Ok(Flow::Normal(Some(v)))
        })();

        // Restore the outer lexical environment no matter how control leaves the switch.
        self.env.set_lexical_env(switch_scope.heap_mut(), outer);
        result
      }
      hir_js::StmtKind::Break(label) => Ok(Flow::Break(*label)),
      hir_js::StmtKind::Continue(label) => Ok(Flow::Continue(*label)),
      hir_js::StmtKind::Var(decl) => {
        self.eval_var_decl(scope, body, decl)?;
        Ok(Flow::empty())
      }
      hir_js::StmtKind::Decl(def_id) => {
        // Function declarations are handled during instantiation. Class declarations are currently
        // unimplemented in the compiled path.
        let def = self
          .hir()
          .def(*def_id)
          .ok_or(VmError::InvariantViolation("hir def id missing from compiled script"))?;
        let Some(body_id) = def.body else {
          return Ok(Flow::empty());
        };
        let decl_body = self.get_body(body_id)?;
        if decl_body.kind == hir_js::BodyKind::Function {
          // Already hoisted.
          Ok(Flow::empty())
        } else {
          Err(VmError::Unimplemented("non-function declaration (hir-js compiled path)"))
        }
      }
      hir_js::StmtKind::Throw(expr) => {
        let v = self.eval_expr(scope, body, *expr)?;
        Err(VmError::Throw(v))
      }
      hir_js::StmtKind::Try {
        block,
        catch,
        finally_block,
      } => {
        // Evaluate the try/catch/finally statement in a nested scope so any roots pushed while
        // coercing internal errors (TypeError, etc.) don't leak into surrounding statement lists.
        let mut try_scope = scope.reborrow();

        // 1. Evaluate the try block and capture either:
        //    - a normal/abrupt Flow, or
        //    - a catchable thrown value, or
        //    - an uncatachable VM error (termination/OOM/etc).
        let mut pending: Result<Flow, VmError> = match self.eval_stmt(&mut try_scope, body, *block) {
          Ok(flow) => Ok(flow),
          Err(err) => {
            // Propagate non-catchable VM errors immediately (no catch/finally semantics).
            if !(err.is_throw_completion() || matches!(err, VmError::RangeError(_))) {
              return Err(err);
            }

            // Coerce internal helper errors into a JS throw value when intrinsics exist, so
            // `try/catch` can observe them.
            let err = crate::vm::coerce_error_to_throw(&*self.vm, &mut try_scope, err);
            if err.thrown_value().is_none() {
              return Err(err);
            }
            Err(err)
          }
        };

        // 2. If the try block threw (or produced an internal throw-completion), run the catch
        //    clause if present.
        if let (Err(thrown_err), Some(catch_clause)) = (&pending, catch.as_ref()) {
          if let Some(thrown_value) = thrown_err.thrown_value() {
            let mut catch_scope = try_scope.reborrow();
            // Root the thrown value across catch environment creation and binding initialization,
            // both of which may allocate and trigger GC.
            catch_scope.push_root(thrown_value)?;

            let outer_env = self.env.lexical_env();
            let catch_env = catch_scope.env_create(Some(outer_env))?;
            self.env.set_lexical_env(catch_scope.heap_mut(), catch_env);

            let catch_result = (|| -> Result<Flow, VmError> {
              if let Some(param_pat_id) = catch_clause.param {
                let pat = self.get_pat(body, param_pat_id)?;
                let hir_js::PatKind::Ident(name_id) = pat.kind else {
                  return Err(VmError::Unimplemented(
                    "catch parameter pattern (hir-js compiled path)",
                  ));
                };
                let name = self.resolve_name(name_id)?;
                catch_scope.env_create_mutable_binding(catch_env, name.as_str())?;
                catch_scope
                  .heap_mut()
                  .env_initialize_binding(catch_env, name.as_str(), thrown_value)?;
              }

              self.eval_stmt(&mut catch_scope, body, catch_clause.body)
            })();

            // Always restore the outer env, even if catch body throws/returns/etc.
            self.env.set_lexical_env(catch_scope.heap_mut(), outer_env);

            pending = catch_result;
          }
        }

        // 3. Always execute `finally` if present.
        if let Some(finally_stmt) = finally_block {
          let mut finally_scope = try_scope.reborrow();

          // Root the pending completion's value (if any) while evaluating `finally`, which may
          // allocate and trigger GC.
          let pending_value: Option<Value> = match &pending {
            Ok(flow) => match flow {
              Flow::Normal(v) => *v,
              Flow::Return(v) => Some(*v),
              Flow::Break(_) | Flow::Continue(_) => None,
            },
            Err(err) => err.thrown_value(),
          };
          if let Some(v) = pending_value {
            finally_scope.push_root(v)?;
          }

          let finally_result = self.eval_stmt(&mut finally_scope, body, *finally_stmt);
          match finally_result {
            Ok(Flow::Normal(_)) => {
              // Normal completion from `finally` does not override the pending completion.
            }
            Ok(abrupt) => {
              // Abrupt completion (return/break/continue) overrides.
              pending = Ok(abrupt);
            }
            Err(err) => {
              // A throw from `finally` overrides.
              pending = Err(err);
            }
          }
        }

        // Per spec, empty normal completion becomes `undefined`.
        match pending {
          Ok(flow) => Ok(flow.update_empty(Some(Value::Undefined))),
          Err(err) => Err(err),
        }
      }
      hir_js::StmtKind::Labeled { label, body: inner } => {
        let flow = self.eval_stmt(scope, body, *inner)?;
        match flow {
          Flow::Break(Some(target)) if target == *label => Ok(Flow::empty()),
          Flow::Continue(Some(target)) if target == *label => Ok(Flow::Continue(None)),
          other => Ok(other),
        }
      }
      hir_js::StmtKind::With { object, body: inner } => {
        // Minimal ECMA-262 `WithStatement` evaluation:
        //
        // - Evaluate the object expression, then `ToObject` it.
        // - Create an ObjectEnvironmentRecord with `with_environment = true`.
        // - Evaluate the body with that env record as the current lexical environment.
        let mut with_scope = scope.reborrow();
        let object_value = self.eval_expr(&mut with_scope, body, *object)?;
        with_scope.push_root(object_value)?;
        let binding_object =
          with_scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, object_value)?;
        with_scope.push_root(Value::Object(binding_object))?;

        let outer = self.env.lexical_env();
        let with_env = with_scope.alloc_object_env_record(binding_object, Some(outer), true)?;
        self.env.set_lexical_env(with_scope.heap_mut(), with_env);

        let result = self.eval_stmt(&mut with_scope, body, *inner);

        // Always restore the outer lexical environment so later statements run in the correct
        // scope.
        self.env.set_lexical_env(with_scope.heap_mut(), outer);
        result
      }
      hir_js::StmtKind::Empty | hir_js::StmtKind::Debugger => Ok(Flow::empty()),
      other => Err(match other {
        _ => VmError::Unimplemented("statement (hir-js compiled path)"),
      }),
    }
  }

  fn bind_for_in_of_head(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    head: &hir_js::ForHead,
    value: Value,
  ) -> Result<(), VmError> {
    match head {
      hir_js::ForHead::Pat(pat_id) => {
        let pat = self.get_pat(body, *pat_id)?;
        let hir_js::PatKind::Ident(name_id) = pat.kind else {
          return Err(VmError::Unimplemented(
            "for-in/of assignment pattern (hir-js compiled path)",
          ));
        };
        let name = self.resolve_name(name_id)?;
        self.env.set(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          scope,
          name.as_str(),
          value,
          self.strict,
        )
      }
      hir_js::ForHead::Var(var_decl) => {
        if !matches!(
          var_decl.kind,
          hir_js::VarDeclKind::Var | hir_js::VarDeclKind::Let | hir_js::VarDeclKind::Const
        ) {
          return Err(VmError::Unimplemented(
            "for-in/of loop variable declaration kind (hir-js compiled path)",
          ));
        }
        if var_decl.declarators.len() != 1 {
          return Err(VmError::Unimplemented(
            "for-in/of variable declaration list (hir-js compiled path)",
          ));
        }
        let declarator = &var_decl.declarators[0];
        if declarator.init.is_some() {
          return Err(VmError::Unimplemented(
            "for-in/of loop head initializers (hir-js compiled path)",
          ));
        }
        self.bind_var_decl_pat(
          scope,
          body,
          declarator.pat,
          var_decl.kind,
          /* init_missing */ false,
          value,
        )
      }
    }
  }

  fn eval_var_decl(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    decl: &hir_js::VarDecl,
  ) -> Result<(), VmError> {
    for declarator in &decl.declarators {
      self.vm.tick()?;
      let init_missing = declarator.init.is_none();
      let value = match declarator.init {
        Some(init) => self.eval_expr(scope, body, init)?,
        None => Value::Undefined,
      };
      self.bind_var_decl_pat(scope, body, declarator.pat, decl.kind, init_missing, value)?;
    }
    Ok(())
  }

  fn create_for_triple_per_iteration_env(
    &mut self,
    scope: &mut Scope<'_>,
    outer: GcEnv,
    last_env: GcEnv,
  ) -> Result<GcEnv, VmError> {
    let crate::env::EnvRecord::Declarative(last) = scope.heap().get_env_record(last_env)? else {
      return Err(VmError::InvariantViolation(
        "for-loop per-iteration environment must be declarative",
      ));
    };

    let bindings = &last.bindings;
    let mut new_bindings: Vec<EnvBinding> = Vec::new();
    new_bindings
      .try_reserve_exact(bindings.len())
      .map_err(|_| VmError::OutOfMemory)?;

    const TICK_EVERY: usize = 32;
    for (i, binding) in bindings.iter().enumerate() {
      if i % TICK_EVERY == 0 {
        self.vm.tick()?;
      }
      new_bindings.push(*binding);
    }

    scope.alloc_env_record(Some(outer), &new_bindings)
  }

  fn bind_var_decl_pat(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    pat_id: hir_js::PatId,
    kind: hir_js::VarDeclKind,
    init_missing: bool,
    value: Value,
  ) -> Result<(), VmError> {
    let pat = self.get_pat(body, pat_id)?;
    let hir_js::PatKind::Ident(name_id) = pat.kind else {
      return Err(VmError::Unimplemented(
        "non-identifier variable declarations (hir-js compiled path)",
      ));
    };
    let name = self.resolve_name(name_id)?;

    match kind {
      hir_js::VarDeclKind::Var => {
        // `var x;` is a no-op: it does not assign `undefined` if the binding already exists.
        //
        // The binding is ensured via the hoisting pass, but `var` declarations can also appear in
        // runtime-evaluated constructs (e.g. `for (var x; ...)`) so we preserve correct semantics
        // here as well.
        if init_missing {
          self.env.declare_var(self.vm, scope, name.as_str())
        } else {
          self.env.set_var(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            scope,
            name.as_str(),
            value,
          )
        }
      }
      hir_js::VarDeclKind::Let => {
        let env_rec = self.env.lexical_env();
        if !scope.heap().env_has_binding(env_rec, name.as_str())? {
          scope.env_create_mutable_binding(env_rec, name.as_str())?;
        }
        scope
          .heap_mut()
          .env_initialize_binding(env_rec, name.as_str(), value)
      }
      hir_js::VarDeclKind::Const => {
        if init_missing {
          // Should have been caught as a syntax error, but keep the engine robust.
          return Err(VmError::TypeError("Missing initializer in const declaration"));
        }
        let env_rec = self.env.lexical_env();
        if !scope.heap().env_has_binding(env_rec, name.as_str())? {
          scope.env_create_immutable_binding(env_rec, name.as_str())?;
        }
        scope
          .heap_mut()
          .env_initialize_binding(env_rec, name.as_str(), value)
      }
      hir_js::VarDeclKind::Using | hir_js::VarDeclKind::AwaitUsing => Err(VmError::Unimplemented(
        "using declarations (hir-js compiled path)",
      )),
    }
  }

  fn eval_expr(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    expr_id: hir_js::ExprId,
  ) -> Result<Value, VmError> {
    // Budget once per expression evaluation.
    self.vm.tick()?;

    let expr = self.get_expr(body, expr_id)?;
    match &expr.kind {
      hir_js::ExprKind::Missing => Ok(Value::Undefined),
      hir_js::ExprKind::Ident(name_id) => {
        let name = self.resolve_name(*name_id)?;
        match self
          .env
          .get(self.vm, &mut *self.host, &mut *self.hooks, scope, name.as_str())?
        {
          Some(v) => Ok(v),
          None => {
            let msg = format!("{} is not defined", name);
            Err(throw_reference_error(self.vm, scope, &msg)?)
          }
        }
      }
      hir_js::ExprKind::This => Ok(self.this),
      hir_js::ExprKind::NewTarget => Ok(self.new_target),
      hir_js::ExprKind::Literal(lit) => self.eval_literal(scope, lit),
      hir_js::ExprKind::Unary { op, expr } => self.eval_unary(scope, body, *op, *expr),
      hir_js::ExprKind::Update { op, expr, prefix } => self.eval_update(scope, body, *op, *expr, *prefix),
      hir_js::ExprKind::Binary { op, left, right } => self.eval_binary(scope, body, *op, *left, *right),
      hir_js::ExprKind::Assignment { op, target, value } => self.eval_assignment(scope, body, *op, *target, *value),
      hir_js::ExprKind::Call(call) => self.eval_call(scope, body, call),
      hir_js::ExprKind::Member(member) => self.eval_member(scope, body, member),
      hir_js::ExprKind::Conditional {
        test,
        consequent,
        alternate,
      } => {
        let test_v = self.eval_expr(scope, body, *test)?;
        if scope.heap().to_boolean(test_v)? {
          self.eval_expr(scope, body, *consequent)
        } else {
          self.eval_expr(scope, body, *alternate)
        }
      }
      hir_js::ExprKind::Array(arr) => self.eval_array_literal(scope, body, arr),
      hir_js::ExprKind::Object(obj) => self.eval_object_literal(scope, body, obj),
      hir_js::ExprKind::FunctionExpr {
        body: func_body,
        name,
        is_arrow,
        ..
      } => {
        let name_str = name
          .as_ref()
          .and_then(|id| self.hir().names.resolve(*id))
          .unwrap_or("")
          .to_owned();
        let func_obj =
          self.alloc_user_function_object(scope, *func_body, name_str.as_str(), *is_arrow)?;
        Ok(Value::Object(func_obj))
      }
      hir_js::ExprKind::Template(tpl) => self.eval_template_literal(scope, body, tpl),
      other => Err(match other {
        hir_js::ExprKind::ClassExpr { .. } => VmError::Unimplemented("class expression (hir-js compiled path)"),
        hir_js::ExprKind::TaggedTemplate { .. } => VmError::Unimplemented("template literal (hir-js compiled path)"),
        hir_js::ExprKind::Await { .. } => VmError::Unimplemented("await (hir-js compiled path)"),
        hir_js::ExprKind::Yield { .. } => VmError::Unimplemented("yield (hir-js compiled path)"),
        hir_js::ExprKind::ImportCall { .. } | hir_js::ExprKind::ImportMeta => {
          VmError::Unimplemented("import() / import.meta (hir-js compiled path)")
        }
        hir_js::ExprKind::Super => VmError::Unimplemented("super (hir-js compiled path)"),
        hir_js::ExprKind::Jsx(_) => VmError::Unimplemented("jsx (hir-js compiled path)"),
        hir_js::ExprKind::TypeAssertion { .. }
        | hir_js::ExprKind::NonNull { .. }
        | hir_js::ExprKind::Satisfies { .. } => VmError::Unimplemented("typescript type syntax (hir-js compiled path)"),
        _ => VmError::Unimplemented("expression (hir-js compiled path)"),
      }),
    }
  }

  fn iterator_close_on_error(
    &mut self,
    scope: &mut Scope<'_>,
    record: &crate::iterator::IteratorRecord,
    err: VmError,
  ) -> VmError {
    if record.done {
      return err;
    }
    // If we are going to return the original error, ensure any thrown value survives across
    // iterator closing (which can allocate/run JS).
    let mut close_scope = scope.reborrow();
    if let Some(v) = err.thrown_value() {
      // If rooting fails (OOM), propagate that error (best-effort).
      if let Err(root_err) = close_scope.push_root(v) {
        return root_err;
      }
    }

    let original_is_throw = err.is_throw_completion();
    match crate::iterator::iterator_close(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      &mut close_scope,
      record,
      crate::iterator::CloseCompletionKind::Throw,
    ) {
      Ok(()) => err,
      Err(close_err) => {
        // Do not replace VM-fatal errors (OOM/termination/etc) with a JS-catchable iterator-closing
        // exception.
        if original_is_throw {
          close_err
        } else {
          err
        }
      }
    }
  }

  fn eval_array_literal(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    arr: &hir_js::ArrayLiteral,
  ) -> Result<Value, VmError> {
    let mut arr_scope = scope.reborrow();
    let arr_obj = arr_scope.alloc_array(0)?;
    arr_scope.push_root(Value::Object(arr_obj))?;

    // Best-effort `[[Prototype]]` wiring so builtins like `%Array.prototype%.push` work when a
    // realm/intrinsics are present.
    if let Some(intr) = self.vm.intrinsics() {
      arr_scope
        .heap_mut()
        .object_set_prototype(arr_obj, Some(intr.array_prototype()))?;
    }

    let mut next_index: u32 = 0;
    for elem in &arr.elements {
      match elem {
        hir_js::ArrayElement::Empty => {
          // Per-hole tick: `[,,,,]` can have arbitrarily many elements without nested expression
          // evaluations.
          self.vm.tick()?;
          next_index = next_index
            .checked_add(1)
            .ok_or(VmError::RangeError("Array literal length exceeds 2^32-1"))?;
        }
        hir_js::ArrayElement::Expr(expr_id) => {
          if next_index == u32::MAX {
            return Err(VmError::RangeError("Array literal length exceeds 2^32-1"));
          }
          let idx = next_index;

          let mut elem_scope = arr_scope.reborrow();
          let value = self.eval_expr(&mut elem_scope, body, *expr_id)?;
          elem_scope.push_root(value)?;

          let key_s = elem_scope.alloc_u32_index_string(idx)?;
          elem_scope.push_root(Value::String(key_s))?;
          let key = PropertyKey::from_string(key_s);
          elem_scope.create_data_property_or_throw(arr_obj, key, value)?;

          next_index = next_index
            .checked_add(1)
            .ok_or(VmError::RangeError("Array literal length exceeds 2^32-1"))?;
        }
        hir_js::ArrayElement::Spread(expr_id) => {
          let mut spread_scope = arr_scope.reborrow();
          let spread_value = self.eval_expr(&mut spread_scope, body, *expr_id)?;
          spread_scope.push_root(spread_value)?;

          let mut iter = crate::iterator::get_iterator(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            &mut spread_scope,
            spread_value,
          )?;

          // Root `iter.iterator` before any further operations so we can safely close on later
          // errors. Use `extra_roots` to keep `next_method` alive if rooting the iterator triggers
          // GC.
          if let Err(err) =
            spread_scope.push_roots_with_extra_roots(&[iter.iterator], &[iter.next_method], &[])
          {
            return Err(self.iterator_close_on_error(&mut spread_scope, &iter, err));
          }
          if let Err(err) = spread_scope.push_root(iter.next_method) {
            return Err(self.iterator_close_on_error(&mut spread_scope, &iter, err));
          }

          loop {
            let next_value = match crate::iterator::iterator_step_value(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              &mut spread_scope,
              &mut iter,
            ) {
              Ok(v) => v,
              // Spec: array spread does not perform `IteratorClose` on errors produced while
              // stepping the iterator (`next`/`done`/`value`).
              Err(err) => return Err(err),
            };

            let Some(value) = next_value else {
              break;
            };

            let step_res: Result<(), VmError> = (|| {
              // Per-spread-element tick: spreading large iterators should be budgeted even when the
              // iterator's `next()` is native/cheap.
              self.vm.tick()?;

              if next_index == u32::MAX {
                return Err(VmError::RangeError("Array literal length exceeds 2^32-1"));
              }
              let idx = next_index;

              let mut elem_scope = spread_scope.reborrow();
              elem_scope.push_root(value)?;
              let key_s = elem_scope.alloc_u32_index_string(idx)?;
              elem_scope.push_root(Value::String(key_s))?;
              let key = PropertyKey::from_string(key_s);
              elem_scope.create_data_property_or_throw(arr_obj, key, value)?;

              next_index = next_index
                .checked_add(1)
                .ok_or(VmError::RangeError("Array literal length exceeds 2^32-1"))?;
              Ok(())
            })();
            if let Err(err) = step_res {
              return Err(self.iterator_close_on_error(&mut spread_scope, &iter, err));
            }
          }
        }
      }
    }

    // Match interpreter behavior: explicitly write the final length so trailing holes are
    // represented correctly (e.g. `[1,,].length === 2`).
    let length_key_s = arr_scope.alloc_string("length")?;
    let length_desc = PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(next_index as f64),
        writable: true,
      },
    };
    arr_scope.define_property(arr_obj, PropertyKey::from_string(length_key_s), length_desc)?;

    Ok(Value::Object(arr_obj))
  }

  fn eval_template_literal(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    tpl: &hir_js::TemplateLiteral,
  ) -> Result<Value, VmError> {
    let mut units: Vec<u16> = Vec::new();

    vec_try_extend_utf16_from_str_with_ticks(&mut units, tpl.head.as_str(), || self.vm.tick())?;

    for span in &tpl.spans {
      // Charge budget per span so a template with many expressions can't run unbounded without
      // consuming fuel/deadline budget.
      self.vm.tick()?;

      let value = self.eval_expr(scope, body, span.expr)?;
      let value_s = scope.to_string(self.vm, &mut *self.host, &mut *self.hooks, value)?;

      {
        let heap = scope.heap();
        vec_try_extend_from_slice_with_ticks(
          &mut units,
          heap.get_string(value_s)?.as_code_units(),
          || self.vm.tick(),
        )?;
      }

      vec_try_extend_utf16_from_str_with_ticks(&mut units, span.literal.as_str(), || self.vm.tick())?;
    }

    let out = scope.alloc_string_from_u16_vec(units)?;
    Ok(Value::String(out))
  }

  fn eval_literal(&mut self, scope: &mut Scope<'_>, lit: &hir_js::Literal) -> Result<Value, VmError> {
    match lit {
      hir_js::Literal::Number(s) => Ok(Value::Number(s.parse::<f64>().unwrap_or(f64::NAN))),
      hir_js::Literal::String(s) => {
        let js = match &s.code_units {
          Some(units) => scope.alloc_string_from_code_units(units.as_ref())?,
          None => scope.alloc_string_from_utf8(&s.lossy)?,
        };
        Ok(Value::String(js))
      }
      hir_js::Literal::Boolean(b) => Ok(Value::Bool(*b)),
      hir_js::Literal::Null => Ok(Value::Null),
      hir_js::Literal::Undefined => Ok(Value::Undefined),
      hir_js::Literal::BigInt(value) => {
        let b = crate::JsBigInt::parse_ascii_radix_with_tick(value, 10, &mut || self.vm.tick())?;
        let handle = scope.alloc_bigint(b)?;
        Ok(Value::BigInt(handle))
      }
      hir_js::Literal::Regex(literal) => {
        let intr = self
          .vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

        let literal = literal.as_str();
        // `hir-js` stores regexp literals verbatim including the leading `/` and any flags (matching
        // the parse-js AST representation).
        if !literal.starts_with('/') {
          return Err(VmError::Unimplemented("invalid RegExp literal"));
        }

        let mut in_class = false;
        let mut escaped = false;
        let mut end_pat: Option<usize> = None;
        // Budget scanning for the closing `/` so enormous regexp literals can't monopolize CPU.
        const TICK_EVERY: usize = 1024;
        let mut steps = 0usize;
        for (i, ch) in literal.char_indices().skip(1) {
          if steps % TICK_EVERY == 0 {
            self.vm.tick()?;
          }
          steps += 1;
          if escaped {
            escaped = false;
            continue;
          }
          match ch {
            '\\' => escaped = true,
            '[' => in_class = true,
            ']' => in_class = false,
            '/' if !in_class => {
              end_pat = Some(i);
              break;
            }
            _ => {}
          }
        }
        let Some(end_pat) = end_pat else {
          return Err(VmError::Unimplemented("unterminated RegExp literal"));
        };
        let pattern = &literal[1..end_pat];
        let flags = &literal[end_pat + 1..];

        let pattern_s = scope.alloc_string(pattern)?;
        scope.push_root(Value::String(pattern_s))?;
        let flags_s = scope.alloc_string(flags)?;
        scope.push_root(Value::String(flags_s))?;

        let ctor = Value::Object(intr.regexp_constructor());
        self.vm.construct_with_host_and_hooks(
          &mut *self.host,
          scope,
          &mut *self.hooks,
          ctor,
          &[Value::String(pattern_s), Value::String(flags_s)],
          ctor,
        )
      }
    }
  }

  fn eval_unary(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    op: hir_js::UnaryOp,
    expr: hir_js::ExprId,
  ) -> Result<Value, VmError> {
    match op {
      hir_js::UnaryOp::Delete => {
        let target_expr = self.get_expr(body, expr)?;
        match &target_expr.kind {
          hir_js::ExprKind::Ident(name_id) => {
            if self.strict {
              // Strict mode delete of an unqualified identifier is an early SyntaxError. The
              // compiled HIR path can still observe it, so surface an equivalent error.
              let diag = diagnostics::Diagnostic::error(
                "VMJS0002",
                "Delete of an unqualified identifier in strict mode.",
                diagnostics::Span {
                  file: diagnostics::FileId(0),
                  range: diagnostics::TextRange::new(0, 0),
                },
              );
              return Err(VmError::Syntax(vec![diag]));
            }

            let name = self.resolve_name(*name_id)?;
            match self.env.resolve_binding_reference(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              scope,
              name.as_str(),
            )? {
              ResolvedBinding::Declarative { .. } => Ok(Value::Bool(false)),
              ResolvedBinding::Object {
                binding_object,
                name,
              } => {
                let mut del_scope = scope.reborrow();
                del_scope.push_root(Value::Object(binding_object))?;
                let key_s = del_scope.alloc_string(name)?;
                del_scope.push_root(Value::String(key_s))?;
                let key = PropertyKey::from_string(key_s);
                Ok(Value::Bool(crate::spec_ops::internal_delete_with_host_and_hooks(
                  self.vm,
                  &mut del_scope,
                  &mut *self.host,
                  &mut *self.hooks,
                  binding_object,
                  key,
                )?))
              }
              ResolvedBinding::GlobalProperty { name } => {
                let global_object = self.env.global_object();
                let mut del_scope = scope.reborrow();
                del_scope.push_root(Value::Object(global_object))?;
                let key_s = del_scope.alloc_string(name)?;
                del_scope.push_root(Value::String(key_s))?;
                let key = PropertyKey::from_string(key_s);
                Ok(Value::Bool(crate::spec_ops::internal_delete_with_host_and_hooks(
                  self.vm,
                  &mut del_scope,
                  &mut *self.host,
                  &mut *self.hooks,
                  global_object,
                  key,
                )?))
              }
              ResolvedBinding::Unresolvable { .. } => Ok(Value::Bool(true)),
            }
          }
          hir_js::ExprKind::Member(member) => {
            // Optional chaining delete (`delete o?.x`) short-circuits to `true` if the base is
            // nullish and does not evaluate the property expression.
            let base = self.eval_expr(scope, body, member.object)?;
            if member.optional && matches!(base, Value::Null | Value::Undefined) {
              return Ok(Value::Bool(true));
            }

            // Root base across key evaluation + boxing + delete.
            let mut del_scope = scope.reborrow();
            del_scope.push_root(base)?;

            let key = self.eval_object_key(&mut del_scope, body, &member.property)?;
            root_property_key(&mut del_scope, key)?;

            let object = del_scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base)?;
            del_scope.push_root(Value::Object(object))?;

            let ok = crate::spec_ops::internal_delete_with_host_and_hooks(
              self.vm,
              &mut del_scope,
              &mut *self.host,
              &mut *self.hooks,
              object,
              key,
            )?;
            if self.strict && !ok {
              return Err(VmError::TypeError("Cannot delete property"));
            }
            Ok(Value::Bool(ok))
          }
          // `delete` of non-reference expressions always returns `true` (after evaluating the
          // operand for side effects).
          _ => {
            let _ = self.eval_expr(scope, body, expr)?;
            Ok(Value::Bool(true))
          }
        }
      }
      hir_js::UnaryOp::Not => {
        let v = self.eval_expr(scope, body, expr)?;
        Ok(Value::Bool(!scope.heap().to_boolean(v)?))
      }
      hir_js::UnaryOp::BitNot => {
        let v = self.eval_expr(scope, body, expr)?;
        let mut tick = || self.vm.tick();
        let n = crate::ops::to_number_with_tick(scope.heap_mut(), v, &mut tick)?;
        Ok(Value::Number((!to_int32(n)) as f64))
      }
      hir_js::UnaryOp::Plus => {
        let v = self.eval_expr(scope, body, expr)?;
        let mut tick = || self.vm.tick();
        Ok(Value::Number(crate::ops::to_number_with_tick(
          scope.heap_mut(),
          v,
          &mut tick,
        )?))
      }
      hir_js::UnaryOp::Minus => {
        let v = self.eval_expr(scope, body, expr)?;
        let mut tick = || self.vm.tick();
        Ok(Value::Number(-crate::ops::to_number_with_tick(
          scope.heap_mut(),
          v,
          &mut tick,
        )?))
      }
      hir_js::UnaryOp::Typeof => {
        // Special-case `typeof unboundIdentifier` so it evaluates to `"undefined"` without
        // throwing a ReferenceError.
        let operand_expr = self.get_expr(body, expr)?;
        let v = if let hir_js::ExprKind::Ident(name_id) = &operand_expr.kind {
          let name = self.resolve_name(*name_id)?;
          match self.env.get(self.vm, &mut *self.host, &mut *self.hooks, scope, name.as_str())? {
            Some(v) => v,
            None => {
              return Ok(Value::String(scope.alloc_string("undefined")?));
            }
          }
        } else {
          self.eval_expr(scope, body, expr)?
        };

        let type_name = typeof_name(scope.heap(), v)?;
        Ok(Value::String(scope.alloc_string(type_name)?))
      }
      hir_js::UnaryOp::Void => {
        let _ = self.eval_expr(scope, body, expr)?;
        Ok(Value::Undefined)
      }
      _ => Err(VmError::Unimplemented("unary operator (hir-js compiled path)")),
    }
  }

  fn eval_update(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    op: hir_js::UpdateOp,
    expr: hir_js::ExprId,
    prefix: bool,
  ) -> Result<Value, VmError> {
    let delta = match op {
      hir_js::UpdateOp::Increment => 1.0,
      hir_js::UpdateOp::Decrement => -1.0,
    };

    let target_expr = self.get_expr(body, expr)?;
    match &target_expr.kind {
      hir_js::ExprKind::Ident(name_id) => {
        let name = self.resolve_name(*name_id)?;

        // Root the name as `ResolvedBinding` borrows it and `env` operations can invoke user code.
        let reference = self.env.resolve_binding_reference(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          scope,
          name.as_str(),
        )?;

        let old_value = self.get_value_from_resolved_binding(scope, reference)?;
        let old_num = scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, old_value)?;
        let new_num = old_num + delta;
        let new_value = Value::Number(new_num);

        // Assignment can invoke user code (e.g. setters via `with` envs). Root the value first.
        let mut assign_scope = scope.reborrow();
        assign_scope.push_root(new_value)?;
        self.env.set_resolved_binding(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut assign_scope,
          reference,
          new_value,
          self.strict,
        )?;

        if prefix {
          Ok(new_value)
        } else {
          Ok(Value::Number(old_num))
        }
      }
      hir_js::ExprKind::Member(member) => {
        let base = self.eval_expr(scope, body, member.object)?;

        let mut update_scope = scope.reborrow();
        // Root the original base across `ToObject`, key allocation, `[[Get]]` and `[[Set]]`.
        update_scope.push_root(base)?;
        let obj = update_scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base)?;
        update_scope.push_root(Value::Object(obj))?;

        let key = self.eval_object_key(&mut update_scope, body, &member.property)?;
        root_property_key(&mut update_scope, key)?;

        let receiver = base;
        let old_value = update_scope.get_with_host_and_hooks(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          obj,
          key,
          receiver,
        )?;

        let old_num = update_scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, old_value)?;
        let new_num = old_num + delta;
        let new_value = Value::Number(new_num);

        let ok = crate::spec_ops::internal_set_with_host_and_hooks(
          self.vm,
          &mut update_scope,
          &mut *self.host,
          &mut *self.hooks,
          obj,
          key,
          new_value,
          receiver,
        )?;
        if !ok && self.strict {
          return Err(VmError::TypeError("Cannot assign to read-only property"));
        }

        if prefix {
          Ok(new_value)
        } else {
          Ok(Value::Number(old_num))
        }
      }
      _ => Err(VmError::Unimplemented("update target (hir-js compiled path)")),
    }
  }

  fn eval_binary(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    op: hir_js::BinaryOp,
    left: hir_js::ExprId,
    right: hir_js::ExprId,
  ) -> Result<Value, VmError> {
    // Logical operators are short-circuiting.
    match op {
      hir_js::BinaryOp::LogicalOr => {
        let l = self.eval_expr(scope, body, left)?;
        if scope.heap().to_boolean(l)? {
          return Ok(l);
        }
        return self.eval_expr(scope, body, right);
      }
      hir_js::BinaryOp::LogicalAnd => {
        let l = self.eval_expr(scope, body, left)?;
        if !scope.heap().to_boolean(l)? {
          return Ok(l);
        }
        return self.eval_expr(scope, body, right);
      }
      hir_js::BinaryOp::NullishCoalescing => {
        let l = self.eval_expr(scope, body, left)?;
        if matches!(l, Value::Null | Value::Undefined) {
          return self.eval_expr(scope, body, right);
        }
        return Ok(l);
      }
      hir_js::BinaryOp::In => {
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let l = self.eval_expr(scope, body, left)?;
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(l)?;
        let r = self.eval_expr(&mut rhs_scope, body, right)?;
        let Value::Object(obj) = r else {
          return Err(VmError::TypeError("Right-hand side of 'in' should be an object"));
        };

        // Root RHS object across `ToPropertyKey` and `[[HasProperty]]` (which can invoke proxy
        // traps and user code).
        rhs_scope.push_root(Value::Object(obj))?;

        let key = rhs_scope.to_property_key(self.vm, &mut *self.host, &mut *self.hooks, l)?;
        root_property_key(&mut rhs_scope, key)?;
        let has = crate::spec_ops::internal_has_property_with_host_and_hooks(
          self.vm,
          &mut rhs_scope,
          &mut *self.host,
          &mut *self.hooks,
          obj,
          key,
        )?;
        return Ok(Value::Bool(has));
      }
      _ => {}
    }

    let l = self.eval_expr(scope, body, left)?;
    let r = self.eval_expr(scope, body, right)?;

    match op {
      hir_js::BinaryOp::Add => {
        self.addition_operator(scope, l, r)
      }
      hir_js::BinaryOp::Subtract => {
        let mut tick = || self.vm.tick();
        Ok(Value::Number(
          crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?
            - crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?,
        ))
      }
      hir_js::BinaryOp::Multiply => {
        let mut tick = || self.vm.tick();
        Ok(Value::Number(
          crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?
            * crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?,
        ))
      }
      hir_js::BinaryOp::Divide => {
        let mut tick = || self.vm.tick();
        Ok(Value::Number(
          crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?
            / crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?,
        ))
      }
      hir_js::BinaryOp::Remainder => {
        let mut tick = || self.vm.tick();
        Ok(Value::Number(
          crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?
            % crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?,
        ))
      }
      hir_js::BinaryOp::Exponent => {
        let mut tick = || self.vm.tick();
        let base = crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?;
        let exp = crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?;
        Ok(Value::Number(base.powf(exp)))
      }
      hir_js::BinaryOp::ShiftLeft => {
        let mut tick = || self.vm.tick();
        let ln = crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?;
        let rn = crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?;
        let shift = to_uint32(rn) & 0x1f;
        Ok(Value::Number(to_int32(ln).wrapping_shl(shift) as f64))
      }
      hir_js::BinaryOp::ShiftRight => {
        let mut tick = || self.vm.tick();
        let ln = crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?;
        let rn = crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?;
        let shift = to_uint32(rn) & 0x1f;
        Ok(Value::Number(to_int32(ln).wrapping_shr(shift) as f64))
      }
      hir_js::BinaryOp::ShiftRightUnsigned => {
        let mut tick = || self.vm.tick();
        let ln = crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?;
        let rn = crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?;
        let shift = to_uint32(rn) & 0x1f;
        Ok(Value::Number(to_uint32(ln).wrapping_shr(shift) as f64))
      }
      hir_js::BinaryOp::BitOr => {
        let mut tick = || self.vm.tick();
        let ln = crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?;
        let rn = crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?;
        Ok(Value::Number((to_int32(ln) | to_int32(rn)) as f64))
      }
      hir_js::BinaryOp::BitAnd => {
        let mut tick = || self.vm.tick();
        let ln = crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?;
        let rn = crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?;
        Ok(Value::Number((to_int32(ln) & to_int32(rn)) as f64))
      }
      hir_js::BinaryOp::BitXor => {
        let mut tick = || self.vm.tick();
        let ln = crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?;
        let rn = crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?;
        Ok(Value::Number((to_int32(ln) ^ to_int32(rn)) as f64))
      }
      hir_js::BinaryOp::Equality => Ok(Value::Bool(self.abstract_equality_comparison(scope, l, r)?)),
      hir_js::BinaryOp::Inequality => Ok(Value::Bool(!self.abstract_equality_comparison(scope, l, r)?)),
      hir_js::BinaryOp::StrictEquality => Ok(Value::Bool(self.strict_equality_comparison(scope, l, r)?)),
      hir_js::BinaryOp::StrictInequality => Ok(Value::Bool(!self.strict_equality_comparison(scope, l, r)?)),
      hir_js::BinaryOp::LessThan => {
        let mut tick = || self.vm.tick();
        Ok(Value::Bool(
          crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?
            < crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?,
        ))
      }
      hir_js::BinaryOp::LessEqual => {
        let mut tick = || self.vm.tick();
        Ok(Value::Bool(
          crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?
            <= crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?,
        ))
      }
      hir_js::BinaryOp::GreaterThan => {
        let mut tick = || self.vm.tick();
        Ok(Value::Bool(
          crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?
            > crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?,
        ))
      }
      hir_js::BinaryOp::GreaterEqual => {
        let mut tick = || self.vm.tick();
        Ok(Value::Bool(
          crate::ops::to_number_with_tick(scope.heap_mut(), l, &mut tick)?
            >= crate::ops::to_number_with_tick(scope.heap_mut(), r, &mut tick)?,
        ))
      }
      hir_js::BinaryOp::Instanceof => Ok(Value::Bool(self.instanceof_operator(scope, l, r)?)),
      hir_js::BinaryOp::Comma => {
        let _ = l;
        Ok(r)
      }
      _ => Err(VmError::Unimplemented("binary operator (hir-js compiled path)")),
    }
  }

  /// ECMA-262 Strict Equality Comparison (`===`) for the VM's supported value types.
  fn strict_equality_comparison(
    &mut self,
    scope: &mut Scope<'_>,
    a: Value,
    b: Value,
  ) -> Result<bool, VmError> {
    use Value::*;

    // Root inputs for the duration of the comparison so accessing their underlying heap data is GC
    // safe, and so `tick()` calls in the string comparison can't observe freed handles.
    let mut scope = scope.reborrow();
    scope.push_roots(&[a, b])?;

    Ok(match (a, b) {
      (Undefined, Undefined) => true,
      (Null, Null) => true,
      (Bool(ax), Bool(by)) => ax == by,
      // IEEE equality already implements JS semantics for `===`:
      // - NaN is never equal to NaN
      // - +0 and -0 compare equal.
      (Number(ax), Number(by)) => ax == by,
      (BigInt(ax), BigInt(by)) => scope.heap().get_bigint(ax)? == scope.heap().get_bigint(by)?,
      (String(ax), String(by)) => {
        let a = scope.heap().get_string(ax)?.as_code_units();
        let b = scope.heap().get_string(by)?.as_code_units();
        crate::tick::code_units_eq_with_ticks(a, b, || self.vm.tick())?
      }
      (Symbol(ax), Symbol(by)) => ax == by,
      (Object(ax), Object(by)) => ax == by,
      _ => false,
    })
  }

  fn instanceof_operator(
    &mut self,
    scope: &mut Scope<'_>,
    object: Value,
    mut constructor: Value,
  ) -> Result<bool, VmError> {
    // Root inputs for the duration of the operation: `instanceof` can allocate when performing
    // `GetMethod`/`Get`/`Call`.
    let mut scope = scope.reborrow();
    scope.push_roots(&[object, constructor])?;

    // InstanceofOperator(O, C) (ECMA-262).
    //
    // Spec: https://tc39.es/ecma262/#sec-instanceofoperator
    let has_instance_sym = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?
      .well_known_symbols()
      .has_instance;
    let has_instance_key = PropertyKey::from_symbol(has_instance_sym);

    // Bound functions (`C.[[BoundTargetFunction]]`) delegate to `InstanceofOperator(O, BC)` as part
    // of `OrdinaryHasInstance`. Implement that delegation here (iteratively) so `instanceof` never
    // recurses through a deep `.bind()` chain and so the bound target's `@@hasInstance` is consulted
    // per spec.
    let mut bound_steps = 0usize;

    loop {
      // Root inputs for the duration of this iteration: `instanceof` can allocate when performing
      // `GetMethod`/`Get`/`Call`.
      let mut iter_scope = scope.reborrow();
      // Root the *current* constructor value (which can change when delegating bound functions).
      iter_scope.push_root(constructor)?;

      // 1. If Type(C) is not Object, throw a TypeError exception.
      let Value::Object(constructor_obj) = constructor else {
        return Err(VmError::TypeError(
          "Right-hand side of 'instanceof' is not an object",
        ));
      };

      // 2. GetMethod(C, @@hasInstance).
      let method = crate::spec_ops::get_method_with_host_and_hooks(
        self.vm,
        &mut iter_scope,
        &mut *self.host,
        &mut *self.hooks,
        Value::Object(constructor_obj),
        has_instance_key,
      )?;

      if let Some(method) = method {
        // Root `method` across the call. When `C` is a Proxy, `GetMethod(C, @@hasInstance)` can
        // return a function that is not reachable from any rooted object (it can be synthesized by
        // the Proxy's `get` trap), and we must keep it alive until the call begins.
        iter_scope.push_root(method)?;

        let result = self.vm.call_with_host_and_hooks(
          &mut *self.host,
          &mut iter_scope,
          &mut *self.hooks,
          method,
          Value::Object(constructor_obj),
          &[object],
        )?;
        return Ok(iter_scope.heap().to_boolean(result)?);
      }

      // 3. If IsCallable(C) is false, throw a TypeError exception.
      if !iter_scope.heap().is_callable(constructor)? {
        return Err(VmError::TypeError(
          "Right-hand side of 'instanceof' is not callable",
        ));
      }

      // `OrdinaryHasInstance` step 2 (bound function delegation):
      //
      // If `C` has `[[BoundTargetFunction]]`, delegate to `InstanceofOperator(O, BC)` which will
      // consult `BC[@@hasInstance]` (including Proxy `get` traps).
      if let Ok(func) = iter_scope.heap().get_function(constructor_obj) {
        if let Some(bound_target) = func.bound_target {
          // Budget extremely deep bound chains and prevent hangs if an invariant is violated.
          const TICK_EVERY: usize = 32;
          if bound_steps != 0 && bound_steps % TICK_EVERY == 0 {
            self.vm.tick()?;
          }
          if bound_steps >= crate::MAX_PROTOTYPE_CHAIN {
            return Err(VmError::PrototypeChainTooDeep);
          }
          bound_steps += 1;
          constructor = Value::Object(bound_target);
          continue;
        }
      }

      return self.ordinary_has_instance(&mut iter_scope, constructor_obj, object);
    }
  }

  fn ordinary_has_instance(
    &mut self,
    scope: &mut Scope<'_>,
    constructor: GcObject,
    object: Value,
  ) -> Result<bool, VmError> {
    // If the LHS is not an object, `instanceof` is `false` without further observable actions.
    let Value::Object(object) = object else {
      return Ok(false);
    };

    // P = Get(C, "prototype").
    let prototype_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(prototype_s))?;
    let prototype = scope.get_with_host_and_hooks(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      constructor,
      PropertyKey::from_string(prototype_s),
      Value::Object(constructor),
    )?;

    let Value::Object(prototype) = prototype else {
      return Err(VmError::TypeError(
        "Function has non-object prototype in instanceof check",
      ));
    };

    // Root `prototype` for the duration of the algorithm. For Proxy constructors, `Get(C,
    // "prototype")` can return an object that is not reachable from the constructor/target, and we
    // must keep it alive across the prototype-chain walk.
    scope.push_root(Value::Object(prototype))?;

    // Walk `object`'s prototype chain until we find `prototype` or reach the end.
    let mut current = scope.get_prototype_of_with_host_and_hooks(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      object,
    )?;
    let mut steps = 0usize;
    let mut visited: HashSet<GcObject> = HashSet::new();
    while let Some(obj) = current {
      // Budget the prototype traversal: hostile inputs can synthesize extremely deep chains (up to
      // the engine hard limit) inside a single `instanceof` expression. Observe fuel/deadline /
      // interrupt budgets periodically while walking.
      //
      // Note: avoid ticking on the first iteration so shallow `instanceof` checks don't
      // effectively double-charge fuel (the surrounding expression evaluation already ticks).
      const TICK_EVERY: usize = 32;
      if steps != 0 && steps % TICK_EVERY == 0 {
        self.vm.tick()?;
      }

      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      if visited.try_reserve(1).is_err() {
        return Err(VmError::OutOfMemory);
      }
      if !visited.insert(obj) {
        return Err(VmError::PrototypeCycle);
      }

      // Root this prototype step. A Proxy `getPrototypeOf` trap can return an arbitrary object that
      // is not necessarily reachable from the original LHS, and the VM must keep it alive until
      // the algorithm completes.
      scope.push_root(Value::Object(obj))?;

      if obj == prototype {
        return Ok(true);
      }
      current = scope.get_prototype_of_with_host_and_hooks(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        obj,
      )?;
    }

    Ok(false)
  }

  /// ECMA-262 Abstract Equality Comparison (`==`) for the VM's supported value types.
  fn abstract_equality_comparison(
    &mut self,
    scope: &mut Scope<'_>,
    a: Value,
    b: Value,
  ) -> Result<bool, VmError> {
    use Value::*;

    // Root inputs for the duration of the comparison: `ToPrimitive` can invoke user code and
    // allocate.
    let mut scope = scope.reborrow();
    scope.push_root(a)?;
    scope.push_root(b)?;

    let mut x = a;
    let mut y = b;
    loop {
      match (x, y) {
        // Same type => strict equality.
        (Undefined, Undefined) => return Ok(true),
        (Null, Null) => return Ok(true),
        (Bool(ax), Bool(by)) => return Ok(ax == by),
        (Number(ax), Number(by)) => return Ok(ax == by),
        (BigInt(ax), BigInt(by)) => {
          let ax = scope.heap().get_bigint(ax)?;
          let by = scope.heap().get_bigint(by)?;
          return Ok(ax == by);
        }
        (String(ax), String(by)) => {
          let a = scope.heap().get_string(ax)?.as_code_units();
          let b = scope.heap().get_string(by)?.as_code_units();
          return Ok(crate::tick::code_units_eq_with_ticks(a, b, || self.vm.tick())?);
        }
        (Symbol(ax), Symbol(by)) => return Ok(ax == by),
        (Object(ax), Object(by)) => return Ok(ax == by),

        // `null == undefined`.
        (Undefined, Null) | (Null, Undefined) => return Ok(true),

        // Number/string.
        (Number(_), String(_)) => {
          let n = scope.heap_mut().to_number_with_tick(y, || self.vm.tick())?;
          y = Number(n);
        }
        (String(_), Number(_)) => {
          let n = scope.heap_mut().to_number_with_tick(x, || self.vm.tick())?;
          x = Number(n);
        }

        // BigInt/string.
        (BigInt(ax), String(bs)) => {
          let mut tick = || self.vm.tick();
          let Some(bi) = string_to_bigint(scope.heap(), bs, &mut tick)? else {
            return Ok(false);
          };
          return Ok(scope.heap().get_bigint(ax)? == &bi);
        }
        (String(as_), BigInt(by)) => {
          let mut tick = || self.vm.tick();
          let Some(bi) = string_to_bigint(scope.heap(), as_, &mut tick)? else {
            return Ok(false);
          };
          return Ok(scope.heap().get_bigint(by)? == &bi);
        }

        // Boolean => ToNumber.
        (Bool(ax), _) => {
          x = Number(if ax { 1.0 } else { 0.0 });
        }
        (_, Bool(by)) => {
          y = Number(if by { 1.0 } else { 0.0 });
        }

        // Object => ToPrimitive (default hint).
        (Object(_), String(_) | Number(_) | BigInt(_) | Symbol(_)) => {
          x = scope.to_primitive(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            x,
            ToPrimitiveHint::Default,
          )?;
          scope.push_root(x)?;
        }
        (String(_) | Number(_) | BigInt(_) | Symbol(_), Object(_)) => {
          y = scope.to_primitive(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            y,
            ToPrimitiveHint::Default,
          )?;
          scope.push_root(y)?;
        }

        // BigInt/Number.
        (BigInt(ax), Number(by)) => {
          return Ok(matches!(
            bigint_compare_number(scope.heap(), ax, by)?,
            Some(Ordering::Equal)
          ));
        }
        (Number(ax), BigInt(by)) => {
          return Ok(matches!(
            bigint_compare_number(scope.heap(), by, ax)?,
            Some(Ordering::Equal)
          ));
        }

        _ => return Ok(false),
      }
    }
  }

  fn eval_assignment(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    op: hir_js::AssignOp,
    target: hir_js::PatId,
    value: hir_js::ExprId,
  ) -> Result<Value, VmError> {
    match op {
      hir_js::AssignOp::Assign => {
        // Root the RHS across assignment target evaluation in case it allocates and triggers GC.
        let mut scope = scope.reborrow();
        let v = self.eval_expr(&mut scope, body, value)?;
        scope.push_root(v)?;
        self.assign_to_pat(&mut scope, body, target, v)?;
        Ok(v)
      }
      hir_js::AssignOp::AddAssign
      | hir_js::AssignOp::SubAssign
      | hir_js::AssignOp::MulAssign
      | hir_js::AssignOp::DivAssign
      | hir_js::AssignOp::RemAssign
      | hir_js::AssignOp::ExponentAssign => self.eval_compound_assignment(scope, body, op, target, value),
      _ => Err(VmError::Unimplemented(
        "compound assignment (hir-js compiled path)",
      )),
    }
  }

  fn eval_compound_assignment(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    op: hir_js::AssignOp,
    target: hir_js::PatId,
    value: hir_js::ExprId,
  ) -> Result<Value, VmError> {
    // Only identifier + member targets are supported in the compiled path for now.
    let pat = self.get_pat(body, target)?;

    match pat.kind {
      hir_js::PatKind::Ident(name_id) => {
        let name = self.resolve_name(name_id)?;

        let mut scope = scope.reborrow();

        let reference = self.env.resolve_binding_reference(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut scope,
          name.as_str(),
        )?;

        let left = self.get_value_from_resolved_binding(&mut scope, reference)?;

        // Root LHS across RHS evaluation and operator application.
        scope.push_root(left)?;

        let right = self.eval_expr(&mut scope, body, value)?;
        scope.push_root(right)?;

        let out = self.apply_compound_assignment_op(&mut scope, op, left, right)?;

        // Root the result across binding resolution/assignment.
        scope.push_root(out)?;
        self.env.set_resolved_binding(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut scope,
          reference,
          out,
          self.strict,
        )?;
        Ok(out)
      }
      hir_js::PatKind::AssignTarget(expr_id) => {
        let target_expr = self.get_expr(body, expr_id)?;
        match &target_expr.kind {
          hir_js::ExprKind::Ident(name_id) => {
            let name = self.resolve_name(*name_id)?;

            let mut scope = scope.reborrow();

            let reference = self.env.resolve_binding_reference(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              &mut scope,
              name.as_str(),
            )?;

            let left = self.get_value_from_resolved_binding(&mut scope, reference)?;

            scope.push_root(left)?;
            let right = self.eval_expr(&mut scope, body, value)?;
            scope.push_root(right)?;

            let out = self.apply_compound_assignment_op(&mut scope, op, left, right)?;
            scope.push_root(out)?;
            self.env.set_resolved_binding(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              &mut scope,
              reference,
              out,
              self.strict,
            )?;
            Ok(out)
          }
          hir_js::ExprKind::Member(member) => {
            let object = self.eval_expr(scope, body, member.object)?;
            let Value::Object(obj) = object else {
              return Err(VmError::TypeError("member assignment requires object"));
            };

            let mut scope = scope.reborrow();
            scope.push_root(Value::Object(obj))?;

            let key = self.eval_object_key(&mut scope, body, &member.property)?;
            root_property_key(&mut scope, key)?;
            let receiver = Value::Object(obj);

            let left = scope.get_with_host_and_hooks(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              obj,
              key,
              receiver,
            )?;
            scope.push_root(left)?;

            let right = self.eval_expr(&mut scope, body, value)?;
            scope.push_root(right)?;

            let out = self.apply_compound_assignment_op(&mut scope, op, left, right)?;
            scope.push_root(out)?;

            let ok = crate::spec_ops::internal_set_with_host_and_hooks(
              self.vm,
              &mut scope,
              &mut *self.host,
              &mut *self.hooks,
              obj,
              key,
              out,
              receiver,
            )?;
            if !ok && self.strict {
              return Err(VmError::TypeError("Cannot assign to read-only property"));
            }
            Ok(out)
          }
          _ => Err(VmError::Unimplemented(
            "assignment target (hir-js compiled path)",
          )),
        }
      }
      _ => Err(VmError::Unimplemented(
        "assignment pattern (hir-js compiled path)",
      )),
    }
  }

  fn apply_compound_assignment_op(
    &mut self,
    scope: &mut Scope<'_>,
    op: hir_js::AssignOp,
    left: Value,
    right: Value,
  ) -> Result<Value, VmError> {
    match op {
      hir_js::AssignOp::AddAssign => self.addition_operator(scope, left, right),
      hir_js::AssignOp::SubAssign => {
        // Root operands while performing `ToNumber`, which can invoke user code.
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        Ok(Value::Number(
          scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, left)?
            - scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, right)?,
        ))
      }
      hir_js::AssignOp::MulAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        Ok(Value::Number(
          scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, left)?
            * scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, right)?,
        ))
      }
      hir_js::AssignOp::DivAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        Ok(Value::Number(
          scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, left)?
            / scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, right)?,
        ))
      }
      hir_js::AssignOp::RemAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        Ok(Value::Number(
          scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, left)?
            % scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, right)?,
        ))
      }
      hir_js::AssignOp::ExponentAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        let base = scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, left)?;
        let exp = scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, right)?;
        Ok(Value::Number(base.powf(exp)))
      }
      _ => Err(VmError::Unimplemented(
        "compound assignment operator (hir-js compiled path)",
      )),
    }
  }

  fn addition_operator(&mut self, scope: &mut Scope<'_>, l: Value, r: Value) -> Result<Value, VmError> {
    // Root operands while coercing/allocating.
    let mut scope = scope.reborrow();
    scope.push_roots(&[l, r])?;
    let lp = scope.to_primitive(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      l,
      ToPrimitiveHint::Default,
    )?;
    let rp = scope.to_primitive(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      r,
      ToPrimitiveHint::Default,
    )?;
    scope.push_roots(&[lp, rp])?;
    if matches!(lp, Value::String(_)) || matches!(rp, Value::String(_)) {
      let ls = scope.heap_mut().to_string(lp)?;
      let rs = scope.heap_mut().to_string(rp)?;
      let out = concat_strings(&mut scope, ls, rs, || self.vm.tick())?;
      Ok(Value::String(out))
    } else {
      let mut tick = || self.vm.tick();
      let ln = crate::ops::to_number_with_tick(scope.heap_mut(), lp, &mut tick)?;
      let rn = crate::ops::to_number_with_tick(scope.heap_mut(), rp, &mut tick)?;
      Ok(Value::Number(ln + rn))
    }
  }

  fn get_value_from_resolved_binding(
    &mut self,
    scope: &mut Scope<'_>,
    reference: ResolvedBinding<'_>,
  ) -> Result<Value, VmError> {
    match reference {
      ResolvedBinding::Declarative { env, name } => match scope.heap().env_get_binding_value(env, name, false) {
        Ok(v) => Ok(v),
        // TDZ sentinel from `Heap::{env_get_binding_value, env_set_mutable_binding}`.
        Err(VmError::Throw(Value::Null)) => {
          let msg = crate::fallible_format::try_format_error_message(
            "Cannot access '",
            name,
            "' before initialization",
          )?;
          Err(throw_reference_error(self.vm, scope, &msg)?)
        }
        Err(err) => Err(err),
      },
      ResolvedBinding::Object {
        binding_object,
        name,
      } => {
        let receiver = Value::Object(binding_object);
        let mut key_scope = scope.reborrow();
        key_scope.push_root(receiver)?;
        let key_s = key_scope.alloc_string(name)?;
        key_scope.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);
        key_scope.get_with_host_and_hooks(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          binding_object,
          key,
          receiver,
        )
      }
      ResolvedBinding::GlobalProperty { name } => {
        let global_object = self.env.global_object();
        let receiver = Value::Object(global_object);
        let mut key_scope = scope.reborrow();
        key_scope.push_root(receiver)?;
        let key_s = key_scope.alloc_string(name)?;
        key_scope.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);
        key_scope.get_with_host_and_hooks(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          global_object,
          key,
          receiver,
        )
      }
      ResolvedBinding::Unresolvable { name } => {
        let msg = format!("{} is not defined", name);
        Err(throw_reference_error(self.vm, scope, &msg)?)
      }
    }
  }

  fn assign_to_pat(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    pat_id: hir_js::PatId,
    value: Value,
  ) -> Result<(), VmError> {
    let pat = self.get_pat(body, pat_id)?;
    match pat.kind {
      hir_js::PatKind::Ident(name_id) => {
        let name = self.resolve_name(name_id)?;
        self.env.set(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          scope,
          name.as_str(),
          value,
          self.strict,
        )
      }
      hir_js::PatKind::AssignTarget(expr_id) => {
        let target_expr = self.get_expr(body, expr_id)?;
        match &target_expr.kind {
          hir_js::ExprKind::Ident(name_id) => {
            let name = self.resolve_name(*name_id)?;
            self.env.set(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              scope,
              name.as_str(),
              value,
              self.strict,
            )
          }
          hir_js::ExprKind::Member(member) => {
            self.assign_to_member(scope, body, member, value)
          }
          _ => Err(VmError::Unimplemented(
            "assignment target (hir-js compiled path)",
          )),
        }
      }
      _ => Err(VmError::Unimplemented(
        "assignment pattern (hir-js compiled path)",
      )),
    }
  }

  fn eval_member(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    member: &hir_js::MemberExpr,
  ) -> Result<Value, VmError> {
    let base = self.eval_expr(scope, body, member.object)?;
    if member.optional && matches!(base, Value::Null | Value::Undefined) {
      return Ok(Value::Undefined);
    }

    let mut scope = scope.reborrow();
    // Root the original base value across `ToObject` + key evaluation + `[[Get]]` in case any step
    // allocates / triggers GC.
    scope.push_root(base)?;

    let obj = scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base)?;
    scope.push_root(Value::Object(obj))?;

    let key = self.eval_object_key(&mut scope, body, &member.property)?;
    root_property_key(&mut scope, key)?;
    // Spec: `GetV` uses the original base value as the receiver (`this` value) for `[[Get]]`.
    let receiver = base;

    scope.get_with_host_and_hooks(self.vm, &mut *self.host, &mut *self.hooks, obj, key, receiver)
  }

  fn assign_to_member(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    member: &hir_js::MemberExpr,
    value: Value,
  ) -> Result<(), VmError> {
    let base = self.eval_expr(scope, body, member.object)?;

    let mut scope = scope.reborrow();
    // Root base + value across `ToObject` + key evaluation + `[[Set]]` (assignment may invoke
    // accessors/proxy traps and allocate).
    scope.push_roots(&[base, value])?;

    let obj = scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base)?;
    scope.push_root(Value::Object(obj))?;

    let key = self.eval_object_key(&mut scope, body, &member.property)?;
    root_property_key(&mut scope, key)?;

    // Spec: `PutValue` uses the original base value as the receiver (`this` value) for `[[Set]]`.
    let receiver = base;
    let ok = crate::spec_ops::internal_set_with_host_and_hooks(
      self.vm,
      &mut scope,
      &mut *self.host,
      &mut *self.hooks,
      obj,
      key,
      value,
      receiver,
    )?;
    if ok {
      Ok(())
    } else if self.strict {
      Err(VmError::TypeError("Cannot assign to read-only property"))
    } else {
      // Sloppy-mode assignment to a non-writable/non-extensible target fails silently.
      Ok(())
    }
  }

  fn eval_object_key(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    key: &hir_js::ObjectKey,
  ) -> Result<PropertyKey, VmError> {
    match key {
      hir_js::ObjectKey::Ident(name_id) => {
        let name = self.resolve_name(*name_id)?;
        Ok(PropertyKey::from_string(scope.alloc_string(name.as_str())?))
      }
      hir_js::ObjectKey::String(s) => Ok(PropertyKey::from_string(scope.alloc_string(s)?)),
      hir_js::ObjectKey::Number(s) => Ok(PropertyKey::from_string(scope.alloc_string(s)?)),
      hir_js::ObjectKey::Computed(expr_id) => {
        let v = self.eval_expr(scope, body, *expr_id)?;
        let key = scope.heap_mut().to_property_key(v)?;
        Ok(key)
      }
    }
  }

  fn eval_object_literal(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    obj: &hir_js::ObjectLiteral,
  ) -> Result<Value, VmError> {
    let obj_val = scope.alloc_object()?;
    // Object literals inherit from %Object.prototype% (when intrinsics are available).
    //
    // The heap can be used without an initialized realm in some low-level unit tests; in that case
    // `vm.intrinsics()` is `None` and the object remains null-prototype shaped.
    if let Some(intr) = self.vm.intrinsics() {
      scope
        .heap_mut()
        .object_set_prototype(obj_val, Some(intr.object_prototype()))?;
    }
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(obj_val))?;

    for prop in &obj.properties {
      self.vm.tick()?;
      match prop {
        hir_js::ObjectProperty::KeyValue { key, value, .. } => {
          let key = self.eval_object_key(&mut scope, body, key)?;
          root_property_key(&mut scope, key)?;
          let v = self.eval_expr(&mut scope, body, *value)?;
          let _ = scope.create_data_property(obj_val, key, v)?;
        }
        hir_js::ObjectProperty::Spread(expr_id) => {
          let src_value = self.eval_expr(&mut scope, body, *expr_id)?;
          // Root the spread source across `CopyDataProperties` (which can allocate and invoke user
          // code via Proxy traps and accessors).
          scope.push_root(src_value)?;
          crate::spec_ops::copy_data_properties_with_host_and_hooks(
            self.vm,
            &mut scope,
            &mut *self.host,
            &mut *self.hooks,
            obj_val,
            src_value,
            &[],
          )?;
        }
        hir_js::ObjectProperty::Getter { key, body: getter_body } => {
          let key = self.eval_object_key(&mut scope, body, key)?;
          root_property_key(&mut scope, key)?;

          // If a setter was already defined earlier in the literal, preserve it.
          let mut existing_set = Value::Undefined;
          if let Some(desc) = scope.heap().get_own_property(obj_val, key)? {
            if let PropertyKind::Accessor { set, .. } = desc.kind {
              existing_set = set;
            }
          }

          let func_obj =
            self.alloc_user_function_object(&mut scope, *getter_body, /* name */ "", /* is_arrow */ false)?;
          scope.push_root(Value::Object(func_obj))?;

          scope.define_property(
            obj_val,
            key,
            PropertyDescriptor {
              enumerable: true,
              configurable: true,
              kind: PropertyKind::Accessor {
                get: Value::Object(func_obj),
                set: existing_set,
              },
            },
          )?;
        }
        hir_js::ObjectProperty::Setter { key, body: setter_body } => {
          let key = self.eval_object_key(&mut scope, body, key)?;
          root_property_key(&mut scope, key)?;

          // If a getter was already defined earlier in the literal, preserve it.
          let mut existing_get = Value::Undefined;
          if let Some(desc) = scope.heap().get_own_property(obj_val, key)? {
            if let PropertyKind::Accessor { get, .. } = desc.kind {
              existing_get = get;
            }
          }

          let func_obj =
            self.alloc_user_function_object(&mut scope, *setter_body, /* name */ "", /* is_arrow */ false)?;
          scope.push_root(Value::Object(func_obj))?;

          scope.define_property(
            obj_val,
            key,
            PropertyDescriptor {
              enumerable: true,
              configurable: true,
              kind: PropertyKind::Accessor {
                get: existing_get,
                set: Value::Object(func_obj),
              },
            },
          )?;
        }
      }
    }

    Ok(Value::Object(obj_val))
  }

  fn eval_call(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    call: &hir_js::CallExpr,
  ) -> Result<Value, VmError> {
    if call.is_new {
      return Err(VmError::Unimplemented("new (hir-js compiled path)"));
    }

    // Only support non-spread arguments for now.
    if call.args.iter().any(|arg| arg.spread) {
      return Err(VmError::Unimplemented("spread arguments (hir-js compiled path)"));
    }

    let mut scope = scope.reborrow();

    // Method call detection: `obj.prop(...)` uses `this = obj`.
    let (callee_value, this_value) = match &self.get_expr(body, call.callee)?.kind {
      hir_js::ExprKind::Member(member) => {
        let base = self.eval_expr(&mut scope, body, member.object)?;
        if member.optional && matches!(base, Value::Null | Value::Undefined) {
          return Ok(Value::Undefined);
        }
        // Root base across `ToObject` + key evaluation + `[[Get]]` in case any step allocates /
        // triggers GC.
        scope.push_root(base)?;

        let obj = scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base)?;
        scope.push_root(Value::Object(obj))?;
        let key = self.eval_object_key(&mut scope, body, &member.property)?;
        root_property_key(&mut scope, key)?;

        // Property access (`[[Get]]`) uses the original base value as the receiver.
        let receiver = base;
        let func =
          scope.get_with_host_and_hooks(self.vm, &mut *self.host, &mut *self.hooks, obj, key, receiver)?;
        // Method calls use the original base value as `this` (strict-mode functions observe the
        // primitive `this` value, matching JS semantics).
        (func, base)
      }
      _ => {
        let callee_value = self.eval_expr(&mut scope, body, call.callee)?;
        (callee_value, Value::Undefined)
      }
    };

    if call.optional && matches!(callee_value, Value::Null | Value::Undefined) {
      return Ok(Value::Undefined);
    }

    // Root callee/this while evaluating args.
    scope.push_roots(&[callee_value, this_value])?;

    let mut args: Vec<Value> = Vec::new();
    args
      .try_reserve_exact(call.args.len())
      .map_err(|_| VmError::OutOfMemory)?;
    for arg in &call.args {
      let v = self.eval_expr(&mut scope, body, arg.expr)?;
      scope.push_root(v)?;
      args.push(v);
    }

    self.vm.call_with_host_and_hooks(
      &mut *self.host,
      &mut scope,
      &mut *self.hooks,
      callee_value,
      this_value,
      args.as_slice(),
    )
  }
}

fn string_to_bigint(
  heap: &crate::Heap,
  s: crate::GcString,
  tick: &mut impl FnMut() -> Result<(), VmError>,
) -> Result<Option<crate::JsBigInt>, VmError> {
  let units = heap.get_string(s)?.as_code_units();
  crate::JsBigInt::parse_utf16_string_with_tick(units, tick)
}

fn bigint_compare_number(
  heap: &crate::Heap,
  bi: crate::GcBigInt,
  n: f64,
) -> Result<Option<Ordering>, VmError> {
  if n.is_nan() {
    return Ok(None);
  }
  if n == f64::INFINITY {
    return Ok(Some(Ordering::Less));
  }
  if n == f64::NEG_INFINITY {
    return Ok(Some(Ordering::Greater));
  }

  let bi = heap.get_bigint(bi)?;

  // Treat +0 and -0 as equal.
  if n == 0.0 {
    if bi.is_zero() {
      return Ok(Some(Ordering::Equal));
    }
    return Ok(Some(if bi.is_negative() {
      Ordering::Less
    } else {
      Ordering::Greater
    }));
  }

  if n.fract() == 0.0 {
    let Some(n_big) = crate::JsBigInt::from_f64_exact(n)? else {
      return Ok(None);
    };
    return Ok(Some(bi.cmp(&n_big)));
  }

  if n > 0.0 {
    let floor = n.floor();
    let Some(floor_big) = crate::JsBigInt::from_f64_exact(floor)? else {
      return Ok(None);
    };
    let ord = bi.cmp(&floor_big);
    return Ok(Some(if ord == Ordering::Greater {
      Ordering::Greater
    } else {
      Ordering::Less
    }));
  }

  // n < 0.0
  let ceil = n.ceil();
  let Some(ceil_big) = crate::JsBigInt::from_f64_exact(ceil)? else {
    return Ok(None);
  };
  let ord = bi.cmp(&ceil_big);
  Ok(Some(if ord == Ordering::Less {
    Ordering::Less
  } else {
    Ordering::Greater
  }))
}

fn to_int32(n: f64) -> i32 {
  if !n.is_finite() || n == 0.0 {
    return 0;
  }
  // ECMA-262 `ToInt32`: truncate then compute modulo 2^32.
  let int = n.trunc();
  const TWO_32: f64 = 4_294_967_296.0;
  const TWO_31: f64 = 2_147_483_648.0;

  let mut int = int % TWO_32;
  if int < 0.0 {
    int += TWO_32;
  }
  if int >= TWO_31 {
    (int - TWO_32) as i32
  } else {
    int as i32
  }
}

fn to_uint32(n: f64) -> u32 {
  if !n.is_finite() || n == 0.0 {
    return 0;
  }
  // ECMA-262 `ToUint32`: truncate then compute modulo 2^32.
  let int = n.trunc();
  const TWO_32: f64 = 4_294_967_296.0;
  let mut int = int % TWO_32;
  if int < 0.0 {
    int += TWO_32;
  }
  int as u32
}

fn typeof_name(heap: &crate::Heap, value: Value) -> Result<&'static str, VmError> {
  Ok(match value {
    Value::Undefined => "undefined",
    Value::Null => "object",
    Value::Bool(_) => "boolean",
    Value::Number(_) => "number",
    Value::BigInt(_) => "bigint",
    Value::String(_) => "string",
    Value::Symbol(_) => "symbol",
    Value::Object(_) => {
      if heap.is_callable(value)? {
        "function"
      } else {
        "object"
      }
    }
  })
}

pub(crate) fn run_compiled_function(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  env: &mut RuntimeEnv,
  func: CompiledFunctionRef,
  strict: bool,
  this: Value,
  new_target: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let source = Arc::new(func.script.source.clone());
  env.set_source_info(source, 0, 0);

  let body = func
    .script
    .hir
    .body(func.body)
    .ok_or(VmError::InvariantViolation("compiled function body not found"))?;
  if body.kind != hir_js::BodyKind::Function {
    return Err(VmError::Unimplemented("compiled body is not a function"));
  }
  let Some(func_meta) = body.function.as_ref() else {
    return Err(VmError::InvariantViolation("function body missing metadata"));
  };
  if func_meta.generator {
    return Err(VmError::Unimplemented(if func_meta.async_ {
      "async generator functions"
    } else {
      "generator functions"
    }));
  }
  if func_meta.async_ {
    return Err(VmError::Unimplemented("async functions (hir-js compiled path)"));
  }

  let mut evaluator = HirEvaluator {
    vm,
    host,
    hooks,
    env,
    strict,
    this,
    new_target,
    script: func.script.clone(),
  };

  evaluator.instantiate_function_body(scope, body, args)?;

  match &func_meta.body {
    hir_js::FunctionBody::Expr(expr_id) => evaluator.eval_expr(scope, body, *expr_id),
    hir_js::FunctionBody::Block(stmts) => match evaluator.eval_stmt_list(scope, body, stmts.as_slice())? {
      Flow::Normal(_) => Ok(Value::Undefined),
      Flow::Return(v) => Ok(v),
      Flow::Break(..) => Err(VmError::Unimplemented("break outside of loop")),
      Flow::Continue(..) => Err(VmError::Unimplemented("continue outside of loop")),
    },
  }
}

pub(crate) fn run_compiled_script(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  env: &mut RuntimeEnv,
  script: Arc<CompiledScript>,
) -> Result<Value, VmError> {
  let source = Arc::new(script.source.clone());
  env.set_source_info(source, 0, 0);

  let global_object = env.global_object();
  let mut evaluator = HirEvaluator {
    vm,
    host,
    hooks,
    env,
    // Best-effort strict detection.
    strict: false,
    this: Value::Object(global_object),
    new_target: Value::Undefined,
    script: script.clone(),
  };

  let hir = script.hir.as_ref();
  let body = hir
    .body(hir.root_body())
    .ok_or(VmError::InvariantViolation("compiled script root body not found"))?;

  evaluator.strict = evaluator.detect_use_strict_directive(body)?;

  // Hoist `var` declarations so lookups before declaration see `undefined` instead of throwing
  // ReferenceError.
  evaluator.instantiate_var_decls(scope, body, body.root_stmts.as_slice())?;

  // Hoist function declarations so they can be called before their declaration statement.
  evaluator.instantiate_function_decls(scope, body, body.root_stmts.as_slice())?;

  // Create `let` / `const` bindings up-front in the global lexical environment so TDZ + shadowing
  // semantics are correct.
  evaluator.instantiate_lexical_decls(
    scope,
    body,
    body.root_stmts.as_slice(),
    evaluator.env.lexical_env(),
  )?;

  match evaluator.eval_stmt_list(scope, body, body.root_stmts.as_slice())? {
    Flow::Normal(v) => Ok(v.unwrap_or(Value::Undefined)),
    Flow::Return(_) => Err(VmError::Unimplemented("return outside of function")),
    Flow::Break(..) => Err(VmError::Unimplemented("break outside of loop")),
    Flow::Continue(..) => Err(VmError::Unimplemented("continue outside of loop")),
  }
}
