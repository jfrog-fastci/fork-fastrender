use crate::code::{CompiledFunctionRef, CompiledScript};
use crate::conversion_ops::ToPrimitiveHint;
use crate::exec::RuntimeEnv;
use crate::function::ThisMode;
use crate::property::PropertyKey;
use crate::{GcObject, Scope, Value, Vm, VmError, VmHost, VmHostHooks};
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

fn concat_strings(scope: &mut Scope<'_>, a: crate::GcString, b: crate::GcString) -> Result<crate::GcString, VmError> {
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
    units.extend_from_slice(heap.get_string(a)?.as_code_units());
    units.extend_from_slice(heap.get_string(b)?.as_code_units());
  }

  scope.alloc_string_from_u16_vec(units)
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
        .to_string(),
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
    self.instantiate_function_decls(scope, body, body.root_stmts.as_slice())?;

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
        let result = self.eval_stmt_list(scope, body, stmts.as_slice());
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
      hir_js::StmtKind::Labeled { label, body: inner } => {
        let flow = self.eval_stmt(scope, body, *inner)?;
        match flow {
          Flow::Break(Some(target)) if target == *label => Ok(Flow::empty()),
          Flow::Continue(Some(target)) if target == *label => Ok(Flow::Continue(None)),
          other => Ok(other),
        }
      }
      hir_js::StmtKind::Empty | hir_js::StmtKind::Debugger => Ok(Flow::empty()),
      other => Err(match other {
        hir_js::StmtKind::ForIn { .. } => VmError::Unimplemented("for-in/of (hir-js compiled path)"),
        hir_js::StmtKind::Switch { .. } => VmError::Unimplemented("switch (hir-js compiled path)"),
        hir_js::StmtKind::Try { .. } => VmError::Unimplemented("try/catch/finally (hir-js compiled path)"),
        hir_js::StmtKind::With { .. } => VmError::Unimplemented("with (hir-js compiled path)"),
        _ => VmError::Unimplemented("statement (hir-js compiled path)"),
      }),
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
      hir_js::VarDeclKind::Var => self.env.set_var(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        scope,
        name.as_str(),
        value,
      ),
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
      hir_js::ExprKind::Literal(lit) => self.eval_literal(scope, lit),
      hir_js::ExprKind::Unary { op, expr } => self.eval_unary(scope, body, *op, *expr),
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
          .to_string();
        let func_obj =
          self.alloc_user_function_object(scope, *func_body, name_str.as_str(), *is_arrow)?;
        Ok(Value::Object(func_obj))
      }
      other => Err(match other {
        hir_js::ExprKind::Array(_) => VmError::Unimplemented("array literal (hir-js compiled path)"),
        hir_js::ExprKind::ClassExpr { .. } => VmError::Unimplemented("class expression (hir-js compiled path)"),
        hir_js::ExprKind::Template(_) | hir_js::ExprKind::TaggedTemplate { .. } => {
          VmError::Unimplemented("template literal (hir-js compiled path)")
        }
        hir_js::ExprKind::Await { .. } => VmError::Unimplemented("await (hir-js compiled path)"),
        hir_js::ExprKind::Yield { .. } => VmError::Unimplemented("yield (hir-js compiled path)"),
        hir_js::ExprKind::ImportCall { .. } | hir_js::ExprKind::ImportMeta => {
          VmError::Unimplemented("import() / import.meta (hir-js compiled path)")
        }
        hir_js::ExprKind::Super | hir_js::ExprKind::NewTarget => {
          VmError::Unimplemented("super/new.target (hir-js compiled path)")
        }
        hir_js::ExprKind::Update { .. } => VmError::Unimplemented("update expression (hir-js compiled path)"),
        hir_js::ExprKind::Jsx(_) => VmError::Unimplemented("jsx (hir-js compiled path)"),
        hir_js::ExprKind::TypeAssertion { .. }
        | hir_js::ExprKind::NonNull { .. }
        | hir_js::ExprKind::Satisfies { .. } => VmError::Unimplemented("typescript type syntax (hir-js compiled path)"),
        _ => VmError::Unimplemented("expression (hir-js compiled path)"),
      }),
    }
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
      hir_js::Literal::BigInt(_) => Err(VmError::Unimplemented("bigint literal (hir-js compiled path)")),
      hir_js::Literal::Regex(_) => Err(VmError::Unimplemented("regex literal (hir-js compiled path)")),
    }
  }

  fn eval_unary(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    op: hir_js::UnaryOp,
    expr: hir_js::ExprId,
  ) -> Result<Value, VmError> {
    let v = self.eval_expr(scope, body, expr)?;
    match op {
      hir_js::UnaryOp::Not => Ok(Value::Bool(!scope.heap().to_boolean(v)?)),
      hir_js::UnaryOp::Plus => Ok(Value::Number(crate::ops::to_number(scope.heap_mut(), v)?)),
      hir_js::UnaryOp::Minus => Ok(Value::Number(-crate::ops::to_number(scope.heap_mut(), v)?)),
      hir_js::UnaryOp::Void => {
        let _ = v;
        Ok(Value::Undefined)
      }
      _ => Err(VmError::Unimplemented("unary operator (hir-js compiled path)")),
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
      _ => {}
    }

    let l = self.eval_expr(scope, body, left)?;
    let r = self.eval_expr(scope, body, right)?;

    match op {
      hir_js::BinaryOp::Add => {
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
          let out = concat_strings(&mut scope, ls, rs)?;
          Ok(Value::String(out))
        } else {
          let ln = crate::ops::to_number(scope.heap_mut(), lp)?;
          let rn = crate::ops::to_number(scope.heap_mut(), rp)?;
          Ok(Value::Number(ln + rn))
        }
      }
      hir_js::BinaryOp::Subtract => Ok(Value::Number(
        crate::ops::to_number(scope.heap_mut(), l)? - crate::ops::to_number(scope.heap_mut(), r)?,
      )),
      hir_js::BinaryOp::Multiply => Ok(Value::Number(
        crate::ops::to_number(scope.heap_mut(), l)? * crate::ops::to_number(scope.heap_mut(), r)?,
      )),
      hir_js::BinaryOp::Divide => Ok(Value::Number(
        crate::ops::to_number(scope.heap_mut(), l)? / crate::ops::to_number(scope.heap_mut(), r)?,
      )),
      hir_js::BinaryOp::Remainder => Ok(Value::Number(
        crate::ops::to_number(scope.heap_mut(), l)? % crate::ops::to_number(scope.heap_mut(), r)?,
      )),
      hir_js::BinaryOp::Equality => Ok(Value::Bool(self.abstract_equality_comparison(scope, l, r)?)),
      hir_js::BinaryOp::Inequality => Ok(Value::Bool(!self.abstract_equality_comparison(scope, l, r)?)),
      hir_js::BinaryOp::StrictEquality => Ok(Value::Bool(l == r)),
      hir_js::BinaryOp::StrictInequality => Ok(Value::Bool(l != r)),
      hir_js::BinaryOp::LessThan => Ok(Value::Bool(
        crate::ops::to_number(scope.heap_mut(), l)? < crate::ops::to_number(scope.heap_mut(), r)?,
      )),
      hir_js::BinaryOp::LessEqual => Ok(Value::Bool(
        crate::ops::to_number(scope.heap_mut(), l)? <= crate::ops::to_number(scope.heap_mut(), r)?,
      )),
      hir_js::BinaryOp::GreaterThan => Ok(Value::Bool(
        crate::ops::to_number(scope.heap_mut(), l)? > crate::ops::to_number(scope.heap_mut(), r)?,
      )),
      hir_js::BinaryOp::GreaterEqual => Ok(Value::Bool(
        crate::ops::to_number(scope.heap_mut(), l)? >= crate::ops::to_number(scope.heap_mut(), r)?,
      )),
      hir_js::BinaryOp::Comma => {
        let _ = l;
        Ok(r)
      }
      _ => Err(VmError::Unimplemented("binary operator (hir-js compiled path)")),
    }
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
        (BigInt(ax), BigInt(by)) => return Ok(ax == by),
        (String(ax), String(by)) => return Ok(scope.heap().get_string(ax)? == scope.heap().get_string(by)?),
        (Symbol(ax), Symbol(by)) => return Ok(ax == by),
        (Object(ax), Object(by)) => return Ok(ax == by),

        // `null == undefined`.
        (Undefined, Null) | (Null, Undefined) => return Ok(true),

        // Number/string.
        (Number(_), String(_)) => {
          let n = scope.heap_mut().to_number(y)?;
          y = Number(n);
        }
        (String(_), Number(_)) => {
          let n = scope.heap_mut().to_number(x)?;
          x = Number(n);
        }

        // BigInt/string.
        (BigInt(ax), String(bs)) => {
          let Some(bi) = string_to_bigint(scope.heap(), bs)? else {
            return Ok(false);
          };
          return Ok(ax == bi);
        }
        (String(as_), BigInt(by)) => {
          let Some(bi) = string_to_bigint(scope.heap(), as_)? else {
            return Ok(false);
          };
          return Ok(bi == by);
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
          return Ok(matches!(bigint_compare_number(ax, by), Some(Ordering::Equal)));
        }
        (Number(ax), BigInt(by)) => {
          return Ok(matches!(bigint_compare_number(by, ax), Some(Ordering::Equal)));
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
    if op != hir_js::AssignOp::Assign {
      return Err(VmError::Unimplemented(
        "compound assignment (hir-js compiled path)",
      ));
    }
    let v = self.eval_expr(scope, body, value)?;
    self.assign_to_pat(scope, body, target, v)?;
    Ok(v)
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
    let object = self.eval_expr(scope, body, member.object)?;
    if member.optional && matches!(object, Value::Null | Value::Undefined) {
      return Ok(Value::Undefined);
    }
    let Value::Object(obj) = object else {
      return Err(VmError::TypeError("member access requires object"));
    };

    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(obj))?;

    let key = self.eval_object_key(&mut scope, body, &member.property)?;
    root_property_key(&mut scope, key)?;
    let receiver = Value::Object(obj);

    scope.ordinary_get_with_host_and_hooks(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      obj,
      key,
      receiver,
    )
  }

  fn assign_to_member(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    member: &hir_js::MemberExpr,
    value: Value,
  ) -> Result<(), VmError> {
    let object = self.eval_expr(scope, body, member.object)?;
    let Value::Object(obj) = object else {
      return Err(VmError::TypeError("member assignment requires object"));
    };

    let mut scope = scope.reborrow();
    // Root object + value while allocating the key and performing the assignment (which can invoke
    // accessors).
    scope.push_roots(&[Value::Object(obj), value])?;

    let key = self.eval_object_key(&mut scope, body, &member.property)?;
    root_property_key(&mut scope, key)?;

    let receiver = Value::Object(obj);
    let ok = scope.ordinary_set_with_host_and_hooks(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      obj,
      key,
      value,
      receiver,
    )?;
    if ok {
      Ok(())
    } else {
      Err(VmError::TypeError("assignment rejected"))
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
        hir_js::ObjectProperty::Spread(_) => {
          return Err(VmError::Unimplemented("object spread (hir-js compiled path)"));
        }
        hir_js::ObjectProperty::Getter { .. } | hir_js::ObjectProperty::Setter { .. } => {
          return Err(VmError::Unimplemented(
            "object accessors (hir-js compiled path)",
          ));
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
        let object = self.eval_expr(&mut scope, body, member.object)?;
        if member.optional && matches!(object, Value::Null | Value::Undefined) {
          return Ok(Value::Undefined);
        }
        let Value::Object(obj) = object else {
          return Err(VmError::TypeError("member call requires object"));
        };
        scope.push_root(Value::Object(obj))?;
        let key = self.eval_object_key(&mut scope, body, &member.property)?;
        root_property_key(&mut scope, key)?;

        let receiver = Value::Object(obj);
        let func = scope.ordinary_get_with_host_and_hooks(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          obj,
          key,
          receiver,
        )?;
        (func, receiver)
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

fn string_to_bigint(heap: &crate::Heap, s: crate::GcString) -> Result<Option<crate::JsBigInt>, VmError> {
  let raw = heap.get_string(s)?.to_utf8_lossy();
  let trimmed = raw.trim_matches(crate::ops::is_ecma_whitespace);
  if trimmed.is_empty() {
    return Ok(None);
  }

  let (negative, rest) = match trimmed.strip_prefix('-') {
    Some(rest) => (true, rest),
    None => match trimmed.strip_prefix('+') {
      Some(rest) => (false, rest),
      None => (false, trimmed),
    },
  };
  if rest.is_empty() {
    return Ok(None);
  }

  let (radix, digits) = if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
    (16u32, hex)
  } else if let Some(bin) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
    (2u32, bin)
  } else if let Some(oct) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
    (8u32, oct)
  } else {
    (10u32, rest)
  };
  if digits.is_empty() {
    return Ok(None);
  }

  let radix_bi = crate::JsBigInt::from_u128(radix as u128);
  let mut out = crate::JsBigInt::zero();
  for b in digits.bytes() {
    let digit = match b {
      b'0'..=b'9' => (b - b'0') as u32,
      b'a'..=b'z' => (b - b'a' + 10) as u32,
      b'A'..=b'Z' => (b - b'A' + 10) as u32,
      _ => return Ok(None),
    };
    if digit >= radix {
      return Ok(None);
    }
    out = out
      .checked_mul(radix_bi)
      .ok_or(VmError::Unimplemented("BigInt parse overflow"))?;
    out = out
      .checked_add(crate::JsBigInt::from_u128(digit as u128))
      .ok_or(VmError::Unimplemented("BigInt parse overflow"))?;
  }

  if negative {
    out = out.negate();
  }
  Ok(Some(out))
}

fn bigint_compare_number(bi: crate::JsBigInt, n: f64) -> Option<Ordering> {
  if n.is_nan() {
    return None;
  }
  if n == f64::INFINITY {
    return Some(Ordering::Less);
  }
  if n == f64::NEG_INFINITY {
    return Some(Ordering::Greater);
  }

  if n == 0.0 {
    if bi.is_zero() {
      return Some(Ordering::Equal);
    }
    if bi.is_negative() {
      return Some(Ordering::Less);
    }
    return Some(Ordering::Greater);
  }

  let bi_neg = bi.is_negative();
  let n_neg = n.is_sign_negative();
  if bi_neg != n_neg {
    return Some(if bi_neg { Ordering::Less } else { Ordering::Greater });
  }

  let bi_abs = if bi_neg { bi.negate() } else { bi };
  let n_abs = n.abs();
  let ord = compare_positive_bigint_and_positive_number(bi_abs, n_abs);
  Some(if bi_neg { ord.reverse() } else { ord })
}

fn compare_positive_bigint_and_positive_number(bi: crate::JsBigInt, n: f64) -> Ordering {
  debug_assert!(!bi.is_negative());
  debug_assert!(n.is_finite());
  debug_assert!(n >= 0.0);

  if n == 0.0 {
    return if bi.is_zero() {
      Ordering::Equal
    } else {
      Ordering::Greater
    };
  }

  if n.fract() != 0.0 {
    let floor = n.floor();
    let Some(floor_bi) = f64_to_bigint_integral(floor) else {
      return Ordering::Less;
    };
    if bi <= floor_bi {
      Ordering::Less
    } else {
      Ordering::Greater
    }
  } else {
    match f64_to_bigint_integral(n) {
      Some(n_bi) => bi.cmp(&n_bi),
      None => Ordering::Less,
    }
  }
}

fn f64_to_bigint_integral(n: f64) -> Option<crate::JsBigInt> {
  if !n.is_finite() {
    return None;
  }
  if n.fract() != 0.0 {
    return None;
  }
  if n == 0.0 {
    return Some(crate::JsBigInt::zero());
  }

  let negative = n.is_sign_negative();
  let abs = n.abs();
  if abs < 1.0 {
    return None;
  }

  let bits = abs.to_bits();
  let exp_bits = ((bits >> 52) & 0x7ff) as i32;
  let frac_bits = bits & ((1u64 << 52) - 1);
  if exp_bits == 0 {
    return None;
  }

  let e = exp_bits - 1023;
  debug_assert!(e >= 0);

  let m = (1u64 << 52) | frac_bits;

  let mut out = if e <= 52 {
    let shift = (52 - e) as u32;
    debug_assert!(shift <= 52);
    if shift != 0 {
      let mask = (1u64 << shift) - 1;
      if (m & mask) != 0 {
        return None;
      }
    }
    let int = m >> shift;
    crate::JsBigInt::from_u128(int as u128)
  } else {
    let shift = (e - 52) as u32;
    let base = crate::JsBigInt::from_u128(m as u128);
    base.checked_shl(shift)?
  };

  if negative {
    out = out.negate();
  }
  Some(out)
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

  // Hoist function declarations so they can be called before their declaration statement.
  evaluator.instantiate_function_decls(scope, body, body.root_stmts.as_slice())?;

  match evaluator.eval_stmt_list(scope, body, body.root_stmts.as_slice())? {
    Flow::Normal(v) => Ok(v.unwrap_or(Value::Undefined)),
    Flow::Return(_) => Err(VmError::Unimplemented("return outside of function")),
    Flow::Break(..) => Err(VmError::Unimplemented("break outside of loop")),
    Flow::Continue(..) => Err(VmError::Unimplemented("continue outside of loop")),
  }
}
