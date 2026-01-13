use crate::code::{CompiledFunctionRef, CompiledScript};
use crate::conversion_ops::ToPrimitiveHint;
use crate::exec::{perform_direct_eval_with_host_and_hooks, ResolvedBinding, RuntimeEnv, VarEnv};
use crate::fallible_format;
use crate::function::FunctionData;
use crate::function::ThisMode;
use crate::for_in::ForInEnumerator;
use crate::iterator;
use crate::module_loading;
use crate::property::{PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind};
use crate::tick::vec_try_extend_from_slice_with_ticks;
use crate::vm::EcmaFunctionKind;
use crate::{
  EnvBinding, ExecutionContext, GcBigInt, GcEnv, GcObject, ModuleId, RealmId, Scope, ScriptOrModule,
  StackFrame, Value, Vm, VmError, VmHost, VmHostHooks,
};
use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;
use parse_js::num::JsNumber;

fn compiled_constructor_body_construct(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  args: &[Value],
  new_target: Value,
) -> Result<Value, VmError> {
  // Determine whether this is a derived class constructor body and recover the containing class
  // constructor object.
  //
  // The wrapper function is annotated by `eval_class` so we can consult its containing class
  // constructor's `extends` value.
  let (class_constructor, is_derived) = {
    let func = scope.heap().get_function(callee)?;
    match func.data {
      FunctionData::ClassConstructorBody { class_constructor } => {
        let super_value =
          crate::class_fields::class_constructor_super_value(scope, class_constructor)?;
        (Some(class_constructor), !matches!(super_value, Value::Undefined))
      }
      _ => (None, false),
    }
  };

  // Extract the hidden compiled body function from the wrapper's native slots.
  let body_func = {
    let func = scope.heap().get_function(callee)?;
    func
      .native_slots
      .as_deref()
      .and_then(|slots| slots.first().copied())
  };
  let Some(Value::Object(body_func)) = body_func else {
    return Err(VmError::InvariantViolation(
      "compiled constructor wrapper missing body function slot",
    ));
  };

  let (func_ref, is_strict, realm, outer, home_object) = {
    let call_handler = scope.heap().get_function_call_handler(body_func)?;
    let crate::function::CallHandler::User(func_ref) = call_handler else {
      return Err(VmError::InvariantViolation(
        "compiled constructor body slot is not a compiled user function",
      ));
    };
    let f = scope.heap().get_function(body_func)?;
    (func_ref, f.is_strict, f.realm, f.closure_env, f.home_object)
  };

  // Determine the global object for the constructor body.
  let global_object = match realm {
    Some(obj) => obj,
    None => {
      // Match `Vm::call_user_function`'s best-effort behaviour: synthesize a minimal global object.
      // Root the body function across allocation so it isn't collected before we attach the realm.
      let mut init_scope = scope.reborrow();
      init_scope.push_root(Value::Object(body_func))?;
      let global_object = init_scope.alloc_object()?;
      init_scope.push_root(Value::Object(global_object))?;
      init_scope
        .heap_mut()
        .set_function_realm(body_func, global_object)?;
      global_object
    }
  };

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  // Base/ordinary constructor: allocate `this` up-front.
  //
  // Derived class constructors do not allocate `this` up-front; `this` is initialized by `super()`.
  if !is_derived {
    // Allocate the instance using `OrdinaryCreateFromConstructor(newTarget, %Object.prototype%)`.
    //
    // Root inputs across allocation in case it triggers GC.
    let mut scope = scope.reborrow();
    scope.push_roots(&[Value::Object(body_func), new_target])?;
    let this_obj = crate::spec_ops::ordinary_create_from_constructor_with_host_and_hooks(
      vm,
      &mut scope,
      host,
      hooks,
      new_target,
      intr.object_prototype(),
      &[],
      |scope| scope.alloc_object(),
    )?;

    // Root the newly-created `this` across environment creation and body execution.
    scope.push_root(Value::Object(this_obj))?;

    let func_env = scope.env_create(outer)?;
    let mut env = RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, func_env, func_env)?;

    let result = run_compiled_function(
      vm,
      &mut scope,
      host,
      hooks,
      &mut env,
      func_ref,
      is_strict,
      Value::Object(this_obj),
      /* this_initialized */ true,
      new_target,
      home_object,
      args,
      class_constructor,
      /* derived_constructor */ false,
      /* this_root_idx */ None,
    );

    env.teardown(scope.heap_mut());

    match result? {
      Value::Object(o) => Ok(Value::Object(o)),
      _ => Ok(Value::Object(this_obj)),
    }
  } else {
    // Derived ctor: run body with an uninitialized `this` value.
    //
    // Root inputs across env creation and body execution in case either triggers GC.
    let mut scope = scope.reborrow();
    scope.push_roots(&[Value::Object(body_func), new_target])?;
    // Reserve a root-stack slot for the derived constructor `this` value.
    //
    // `this` is initialized by `super()`, but the evaluator's `this` field is not traced by GC, so
    // `super()` must update this root slot once it returns.
    let this_root_idx = scope.heap().root_stack.len();
    scope.push_root(Value::Undefined)?;

    let func_env = scope.env_create(outer)?;
    let mut env = RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, func_env, func_env)?;

    let result = run_compiled_function(
      vm,
      &mut scope,
      host,
      hooks,
      &mut env,
      func_ref,
      is_strict,
      Value::Undefined,
      /* this_initialized */ false,
      new_target,
      home_object,
      args,
      class_constructor,
      /* derived_constructor */ true,
      Some(this_root_idx),
    );

    env.teardown(scope.heap_mut());

    match result? {
      Value::Object(o) => Ok(Value::Object(o)),
      _ => match scope.heap().root_stack.get(this_root_idx).copied().unwrap_or(Value::Undefined) {
        Value::Object(o) => Ok(Value::Object(o)),
        _ => Err(throw_reference_error(
          vm,
          &mut scope,
          "Derived constructor did not initialize `this` via super()",
        )?),
      },
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Flow {
  Normal(Option<Value>),
  Return(Value),
  Break(Option<hir_js::NameId>, Option<Value>),
  Continue(Option<hir_js::NameId>, Option<Value>),
}

impl Flow {
  fn normal(value: Value) -> Self {
    Flow::Normal(Some(value))
  }

  fn empty() -> Self {
    Flow::Normal(None)
  }

  fn value(&self) -> Option<Value> {
    match self {
      Flow::Normal(v) => *v,
      Flow::Return(v) => Some(*v),
      Flow::Break(_, v) => *v,
      Flow::Continue(_, v) => *v,
    }
  }

  fn update_empty(self, value: Option<Value>) -> Self {
    match self {
      Flow::Normal(None) => Flow::Normal(value),
      Flow::Break(label, None) => Flow::Break(label, value),
      Flow::Continue(label, None) => Flow::Continue(label, value),
      other => other,
    }
  }
}

#[derive(Debug, Clone, Copy)]
enum NumericValue {
  Number(f64),
  BigInt(GcBigInt),
}

/// RAII guard that truncates the heap root stack back to a prior length.
///
/// This is used to keep temporarily-rooted values alive across allocations/GC without leaking roots
/// into the caller's scope when evaluation returns early.
struct RootStackTruncateGuard {
  heap: *mut crate::Heap,
  len: usize,
}

impl RootStackTruncateGuard {
  fn new(heap: &mut crate::Heap, len: usize) -> Self {
    Self {
      heap: heap as *mut _,
      len,
    }
  }
}

impl Drop for RootStackTruncateGuard {
  fn drop(&mut self) {
    // Safety: the guard is only created from a live `Scope` borrow; the heap pointer remains valid
    // until the scope is dropped, which must happen after this guard (declared in the same scope).
    unsafe {
      (*self.heap).root_stack.truncate(self.len);
    }
  }
}

/// A minimal reference representation used for assignment evaluation.
///
/// This intentionally mirrors the interpreter (`exec.rs`) evaluation strategy:
/// - Evaluate the assignment target *reference* (binding resolution or member base+key) before
///   evaluating the RHS.
/// - Preserve observable order for computed member keys (`ToPropertyKey` before RHS).
/// - Preserve global binding resolution semantics by capturing whether a global property existed at
///   reference-evaluation time.
#[derive(Clone, Debug)]
enum AssignmentReference {
  Binding(BindingReference),
  /// A property reference.
  ///
  /// Note: per ECMA-262, the reference stores the *base value* (which may be a primitive). Property
  /// assignment performs `ToObject(base)` for the actual `[[Set]]` operation but uses the original
  /// base value as the receiver (`this` value) for `[[Set]]`.
  Property { base: Value, key: PropertyKey },
}

#[derive(Clone, Debug)]
enum BindingReference {
  Declarative { env: GcEnv, name: String },
  Object { binding_object: GcObject, name: String },
  GlobalProperty { name: String },
  Unresolvable { name: String },
}

impl BindingReference {
  fn name(&self) -> &str {
    match self {
      BindingReference::Declarative { name, .. }
      | BindingReference::Object { name, .. }
      | BindingReference::GlobalProperty { name }
      | BindingReference::Unresolvable { name } => name.as_str(),
    }
  }

  fn as_resolved_binding(&self) -> ResolvedBinding<'_> {
    match self {
      BindingReference::Declarative { env, name } => ResolvedBinding::Declarative {
        env: *env,
        name: name.as_str(),
      },
      BindingReference::Object {
        binding_object,
        name,
      } => ResolvedBinding::Object {
        binding_object: *binding_object,
        name: name.as_str(),
      },
      BindingReference::GlobalProperty { name } => ResolvedBinding::GlobalProperty { name: name.as_str() },
      BindingReference::Unresolvable { name } => ResolvedBinding::Unresolvable { name: name.as_str() },
    }
  }
}

#[derive(Debug, Clone, Copy)]
enum OptionalChainEval {
  /// A normal evaluation result.
  Value(Value),
  /// Indicates that an optional chain has short-circuited and the current chain expression should
  /// evaluate to `undefined`.
  ///
  /// This is distinct from `Value::Undefined` so chain continuations like `a?.b.c` can avoid
  /// confusing an *actual* `undefined` value (e.g. the property value of `a.b`) with an optional
  /// chain short-circuit (e.g. `a` was nullish).
  ShortCircuit,
}

impl OptionalChainEval {
  #[inline]
  fn into_value(self) -> Value {
    match self {
      OptionalChainEval::Value(v) => v,
      OptionalChainEval::ShortCircuit => Value::Undefined,
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

fn throw_type_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let value = crate::new_type_error(scope, intr, message)?;
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

fn maybe_set_anonymous_function_name(
  scope: &mut Scope<'_>,
  value: Value,
  name: &str,
) -> Result<(), VmError> {
  let Value::Object(func_obj) = value else {
    return Ok(());
  };

  // `SetFunctionName` only applies to actual Function objects. Callable Proxies are callable, but
  // they are not function objects and should not have their `name` mutated.
  let (current_name, is_native_non_constructable) = match scope.heap().get_function(func_obj) {
    Ok(f) => (
      f.name,
      matches!(f.call, crate::function::CallHandler::Native(_)) && f.construct.is_none(),
    ),
    Err(VmError::NotCallable) => return Ok(()),
    Err(err) => return Err(err),
  };

  // Name inference only applies to "anonymous function definitions" (ECMA-262) which excludes
  // anonymous built-in/native functions like Promise combinator element callbacks.
  //
  // `vm-js` represents user-defined class constructors as native functions (so they can throw when
  // called without `new`), so keep name inference enabled for constructable native functions.
  if is_native_non_constructable {
    return Ok(());
  }
  if !scope
    .heap()
    .get_string(current_name)?
    .as_code_units()
    .is_empty()
  {
    return Ok(());
  }

  // Root the function object while allocating the new name string and redefining `name`.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(func_obj))?;

  let name_s = scope.alloc_string(name)?;
  crate::function_properties::set_function_name(
    &mut scope,
    func_obj,
    PropertyKey::String(name_s),
    None,
  )?;
  Ok(())
}

#[derive(Clone, Copy, Debug)]
enum PatBindingKind {
  Var,
  Let,
  Const,
  Param,
}

struct HirEvaluator<'vm> {
  vm: &'vm mut Vm,
  host: &'vm mut dyn VmHost,
  hooks: &'vm mut dyn VmHostHooks,
  env: &'vm mut RuntimeEnv,
  strict: bool,
  this: Value,
  /// Whether the current `this` binding is initialized.
  ///
  /// This is relevant for **derived class constructors**, where `this` is uninitialized until
  /// `super()` returns. Accessing `this` before initialization must throw a ReferenceError.
  this_initialized: bool,
  /// The current class constructor object, when evaluating a user-written `constructor(...) { ... }`
  /// body.
  ///
  /// This is used to implement derived `super()` calls (and instance-field initialization timing).
  class_constructor: Option<GcObject>,
  /// True if the current function is a **derived** class constructor body.
  ///
  /// In derived constructors, `this` is uninitialized until `super()` returns.
  derived_constructor: bool,
  /// Index into the heap root stack used to keep a derived constructor's `this` value alive once it
  /// is initialized by `super()`.
  ///
  /// This is required because `this` is stored in the evaluator struct (a local Rust value), which
  /// is not traced by GC.
  this_root_idx: Option<usize>,
  new_target: Value,
  home_object: Option<GcObject>,
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

  fn next_non_trivia_byte_from_source(&mut self, offset: u32) -> Result<Option<u8>, VmError> {
    let bytes = self.script.source.text.as_bytes();
    let len = bytes.len();
    let mut idx = (offset as usize).min(len);
    const TICK_EVERY: usize = 256;
    let mut scanned: usize = 0;

    while idx < len {
      if scanned % TICK_EVERY == 0 {
        self.vm.tick()?;
      }
      scanned += 1;

      let b = bytes[idx];

      if b.is_ascii_whitespace() {
        idx += 1;
        continue;
      }

      // Skip JS comments.
      if b == b'/' && idx + 1 < len {
        let b1 = bytes[idx + 1];
        if b1 == b'/' {
          // Line comment.
          idx += 2;
          while idx < len && bytes[idx] != b'\n' {
            if scanned % TICK_EVERY == 0 {
              self.vm.tick()?;
            }
            scanned += 1;
            idx += 1;
          }
          continue;
        }
        if b1 == b'*' {
          // Block comment.
          idx += 2;
          while idx + 1 < len {
            if scanned % TICK_EVERY == 0 {
              self.vm.tick()?;
            }
            scanned += 1;

            if bytes[idx] == b'*' && bytes[idx + 1] == b'/' {
              idx += 2;
              break;
            }
            idx += 1;
          }
          // Unterminated block comment reaches EOF.
          continue;
        }
      }

      return Ok(Some(b));
    }

    Ok(None)
  }

  fn expr_is_parenthesized(&mut self, expr: &hir_js::Expr) -> Result<bool, VmError> {
    // HIR does not preserve parenthesization metadata. Use a best-effort heuristic based on the
    // expression span and scanning the original source text for parentheses.
    Ok(
      self.next_non_trivia_byte_from_source(expr.span.start)? == Some(b'(')
        || self.next_non_trivia_byte_from_source(expr.span.end)? == Some(b')'),
    )
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
      if self.expr_is_parenthesized(expr)? {
        // Parenthesized string literals are not directive prologues; the directive prologue ends
        // immediately once we see a non-directive statement.
        break;
      }
      if s.lossy == "use strict" {
        return Ok(true);
      }
    }
    Ok(false)
  }

  fn eval_for_in_of_rhs_with_tdz_env(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    left: &hir_js::ForHead,
    right: hir_js::ExprId,
  ) -> Result<Value, VmError> {
    let hir_js::ForHead::Var(var_decl) = left else {
      return self.eval_expr(scope, body, right);
    };
    if !matches!(
      var_decl.kind,
      hir_js::VarDeclKind::Let
        | hir_js::VarDeclKind::Const
        | hir_js::VarDeclKind::Using
        | hir_js::VarDeclKind::AwaitUsing
    ) {
      return self.eval_expr(scope, body, right);
    }

    // ECMA-262 `ForIn/OfHeadEvaluation`:
    // If the loop uses a lexical `ForDeclaration` (`let`/`const`/`using`/`await using`), create a
    // TDZ lexical environment for the bound names while evaluating the RHS expression.
    //
    // Closures created during RHS evaluation must capture this TDZ environment (not the loop body
    // envs).
    let old_lex = self.env.lexical_env();
    let tdz_env = scope.env_create(Some(old_lex))?;

    // Create uninitialized bindings for `BoundNames(ForDeclaration)`.
    for declarator in &var_decl.declarators {
      self.vm.tick()?;
      let mut names: Vec<hir_js::NameId> = Vec::new();
      self.collect_pat_idents(body, declarator.pat, &mut names)?;
      for name_id in names {
        let name = self.resolve_name(name_id)?;
        if scope.heap().env_has_binding(tdz_env, name.as_str())? {
          continue;
        }
        match var_decl.kind {
          hir_js::VarDeclKind::Let => scope.env_create_mutable_binding(tdz_env, name.as_str())?,
          hir_js::VarDeclKind::Const
          | hir_js::VarDeclKind::Using
          | hir_js::VarDeclKind::AwaitUsing => scope.env_create_immutable_binding(tdz_env, name.as_str())?,
          _ => {
            return Err(VmError::InvariantViolation(
              "unexpected VarDeclKind in for-in/of head TDZ environment creation",
            ));
          }
        }
      }
    }

    self.env.set_lexical_env(scope.heap_mut(), tdz_env);
    let rhs_res = self.eval_expr(scope, body, right);
    // Always restore the caller's lexical environment, even on abrupt completion.
    self.env.set_lexical_env(scope.heap_mut(), old_lex);
    rhs_res
  }

  fn alloc_user_function_object(
    &mut self,
    scope: &mut Scope<'_>,
    body_id: hir_js::BodyId,
    name: &str,
    is_arrow: bool,
    is_constructable: bool,
    name_binding: Option<&str>,
    kind: EcmaFunctionKind,
  ) -> Result<GcObject, VmError> {
    // Avoid holding references into `self.script.hir` across `vm.tick()` calls below: `tick()`
    // requires `&mut Vm`, so borrow HIR via a standalone `Arc` clone of the compiled script.
    let script = self.script.clone();
    let outer_strict = self.strict;

    let (length, is_async, is_generator, body_has_use_strict, def_span) = {
      let func_body = script
        .hir
        .body(body_id)
        .ok_or(VmError::InvariantViolation(
          "hir body id missing from compiled script",
        ))?;
      let Some(func_meta) = func_body.function.as_ref() else {
        return Err(VmError::InvariantViolation("function body missing function metadata"));
      };
      // `hir_js::Body.span` only covers the function's parameter list + body, which is not a valid
      // standalone snippet to parse as a function declaration/expression/method. Use the owning
      // `Def` span so we capture the full syntactic form (`function* f() {}`, `async () => {}`,
      // `{ m() {} }` member, etc).
      let def_span = script
        .hir
        .def(func_body.owner)
        .map(|d| d.span)
        // Best-effort fallback: use the body span if the owning def is missing.
        .unwrap_or(func_body.span);

      // ECMA-262 `length` is the number of parameters before the first one with a default/rest.
      //
      // This scan can be `O(N)` in the number of parameters, and function expressions/declarations
      // can have very large parameter lists (bounded by source size). Budget it explicitly so a
      // single function literal can't do unbounded work within a single statement/expression tick.
      const TICK_EVERY: usize = 32;
      let mut length: u32 = 0;
      for (i, param) in func_meta.params.iter().enumerate() {
        if i % TICK_EVERY == 0 {
          self.vm.tick()?;
        }
        if param.rest || param.default.is_some() {
          break;
        }
        length = length.saturating_add(1);
      }

      let body_has_use_strict = if outer_strict {
        // Already strict regardless of directives.
        false
      } else {
        // Only block-bodied functions can have directive prologues (expression-bodied arrow
        // functions do not).
        matches!(func_meta.body, hir_js::FunctionBody::Block(_))
          && self.detect_use_strict_directive(func_body)?
      };

      (length, func_meta.async_, func_meta.generator, body_has_use_strict, def_span)
    };

    let is_strict = outer_strict || body_has_use_strict;
    let is_async_generator = is_async && is_generator;
    // Async and generator functions are not constructable (per spec).
    let is_constructable = is_constructable && !is_async && !is_generator;

    // Root inputs across string allocation + function allocation in case either triggers GC.
    let mut scope = scope.reborrow();
    let outer_env = self.env.lexical_env();
    scope.push_env_root(outer_env)?;
    scope.push_root(self.this)?;
    scope.push_root(self.new_target)?;
    if let Some(home) = self.home_object {
      scope.push_root(Value::Object(home))?;
    }

    // Named function expressions introduce an inner immutable binding for their name so the body
    // can reliably reference itself (for recursion) even if the outer binding is reassigned.
    //
    // Spec: https://tc39.es/ecma262/#sec-runtime-semantics-instantiateordinaryfunctionexpression
    let (closure_env, name_binding_env) = if let Some(name) = name_binding.filter(|n| !n.is_empty()) {
      // Create the function name environment and keep it rooted across function allocation + name
      // binding initialization.
      let func_env = scope.env_create(Some(outer_env))?;
      scope.push_env_root(func_env)?;
      scope.env_create_immutable_binding(func_env, name)?;
      (Some(func_env), Some((func_env, name)))
    } else {
      (Some(outer_env), None)
    };

    let name_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(name_s))?;

    let this_mode = if is_arrow {
      ThisMode::Lexical
    } else if is_strict {
      ThisMode::Strict
    } else {
      ThisMode::Global
    };

    // HIR execution supports async functions, but generator / async-generator bodies are not yet
    // implemented (yield is unimplemented). Keep generator functions on the AST-backed path for now
    // so they can continue to execute via the interpreter.
    let func_obj = if is_generator {
      let code_id = self.vm.register_ecma_function(
        self.env.source(),
        def_span.start,
        def_span.end,
        kind,
      )?;
      scope.alloc_ecma_function(
        code_id,
        is_constructable,
        name_s,
        length,
        this_mode,
        is_strict,
        closure_env,
      )?
    } else {
      scope.alloc_user_function_with_env(
        CompiledFunctionRef {
          script,
          body: body_id,
        },
        is_constructable,
        name_s,
        length,
        this_mode,
        is_strict,
        closure_env,
      )?
    };

    // Root the function object while performing any additional allocations (e.g. `.prototype`
    // creation) and while assigning metadata that can invoke GC (directly or indirectly).
    scope.push_root(Value::Object(func_obj))?;

    // If this was a named function expression, initialize its name binding now that the function
    // object exists.
    if let Some((env, name)) = name_binding_env {
      scope
        .heap_mut()
        .env_initialize_binding(env, name, Value::Object(func_obj))?;
    }

    // Arrow functions capture lexical `this`/`new.target`.
    if is_arrow {
      scope.heap_mut().set_function_bound_this(func_obj, self.this)?;
      scope
        .heap_mut()
        .set_function_bound_new_target(func_obj, self.new_target)?;
      scope
        .heap_mut()
        .set_function_home_object(func_obj, self.home_object)?;
    }

    // Constructable functions get a `.prototype` object so `instanceof` works per spec.
    //
    // `OrdinaryHasInstance` requires `C.prototype` to be an object (and throws if it isn't). Some
    // callable function kinds (arrow functions, object literal methods/accessors, class methods)
    // are not constructable and do *not* have an own `"prototype"` property unless user code adds
    // one, so gate this initialization on the constructability metadata from HIR lowering.
    if is_constructable {
      let _ = crate::function_properties::make_constructor(&mut scope, func_obj)?;
    }

    // Best-effort function `[[Prototype]]` / `[[Realm]]` metadata.
    if let Some(intr) = self.vm.intrinsics() {
      let func_prototype = if is_generator {
        if is_async_generator {
          intr.async_generator_function_prototype()
        } else {
          intr.generator_function_prototype()
        }
      } else {
        intr.function_prototype()
      };
      scope
        .heap_mut()
        .object_set_prototype(func_obj, Some(func_prototype))?;
      if is_generator {
        if is_async_generator {
          crate::function_properties::make_async_generator_function_instance_prototype(
            &mut scope,
            func_obj,
            intr.async_generator_prototype(),
          )?;
        } else {
          crate::function_properties::make_generator_function_instance_prototype(
            &mut scope,
            func_obj,
            intr.generator_prototype(),
          )?;
        }
      }
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

    const PROLOGUE_TICK_EVERY: usize = 32;
    // Pre-create all parameter bindings before evaluating default initializers so identifier
    // references during parameter evaluation observe TDZ semantics.
    //
    // Note: best-effort: we create mutable bindings for all identifiers appearing in parameter
    // patterns (including destructuring). Binding initialization happens during the later binding
    // pass.
    let env_rec = self.env.lexical_env();
    for (idx, param) in func_meta.params.iter().enumerate() {
      self.vm.tick()?;
      if param.rest && idx + 1 != func_meta.params.len() {
        return Err(VmError::Unimplemented(
          "non-final rest parameter (hir-js compiled path)",
        ));
      }
      let mut names: Vec<hir_js::NameId> = Vec::new();
      self.collect_pat_idents(body, param.pat, &mut names)?;
      for name_id in names {
        let name = self.resolve_name(name_id)?;
        if !scope.heap().env_has_binding(env_rec, name.as_str())? {
          scope.env_create_mutable_binding(env_rec, name.as_str())?;
        }
      }
    }

    // Create a minimal `arguments` object for non-arrow functions.
    //
    // This must happen before default parameter initializers run so defaults can read `arguments`.
    // We do not implement mapped arguments objects yet.
    if !func_meta.is_arrow && !scope.heap().env_has_binding(env_rec, "arguments")? {
      let intr = self.vm.intrinsics();

      let args_obj = scope.alloc_arguments_object()?;
      scope.push_root(Value::Object(args_obj))?;

      // Best-effort `[[Prototype]]`: requires intrinsics. Unit tests and low-level embeddings can
      // execute compiled functions without a realm; in that case we still create an arguments object
      // with a null prototype.
      if let Some(intr) = intr {
        scope
          .heap_mut()
          .object_set_prototype(args_obj, Some(intr.object_prototype()))?;
      }

      let len = args.len() as f64;
      let len_key = PropertyKey::from_string(scope.alloc_string("length")?);
      scope.define_property(
        args_obj,
        len_key,
        PropertyDescriptor {
          enumerable: false,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Number(len),
            writable: true,
          },
        },
      )?;

      for (i, v) in args.iter().copied().enumerate() {
        if i % PROLOGUE_TICK_EVERY == 0 {
          self.vm.tick()?;
        }
        let mut idx_scope = scope.reborrow();
        idx_scope.push_root(v)?;
        let i_u32 = u32::try_from(i).map_err(|_| VmError::OutOfMemory)?;
        let key = PropertyKey::from_string(idx_scope.alloc_u32_index_string(i_u32)?);
        idx_scope.define_property(
          args_obj,
          key,
          PropertyDescriptor {
            enumerable: true,
            configurable: true,
            kind: PropertyKind::Data {
              value: v,
              writable: true,
            },
          },
        )?;
      }

      // Strict-mode `arguments` objects have poison-pill `callee`/`caller` accessors.
      if self.strict {
        if let Some(intr) = intr {
          let thrower = intr.throw_type_error();
          scope.push_root(Value::Object(thrower))?;
          for prop_name in ["callee", "caller"] {
            let key_s = scope.alloc_string(prop_name)?;
            scope.push_root(Value::String(key_s))?;
            let key = PropertyKey::from_string(key_s);
            scope.define_property(
              args_obj,
              key,
              PropertyDescriptor {
                enumerable: false,
                configurable: false,
                kind: PropertyKind::Accessor {
                  get: Value::Object(thrower),
                  set: Value::Object(thrower),
                },
              },
            )?;
          }
        }
      }

      scope.env_create_mutable_binding(env_rec, "arguments")?;
      scope.heap_mut().env_initialize_binding(env_rec, "arguments", Value::Object(args_obj))?;
    }

    // Bind parameters.
    for (idx, param) in func_meta.params.iter().enumerate() {
      self.vm.tick()?;
      let value = if param.rest {
        if idx + 1 != func_meta.params.len() {
          return Err(VmError::Unimplemented(
            "non-final rest parameter (hir-js compiled path)",
          ));
        }

        // `...rest` collects all remaining arguments starting at this parameter index.
        //
        // Materialize an actual Array (with `%Array.prototype%` when intrinsics are initialized)
        // so `rest.length` / indexing behave correctly.
        let rest_args = args.get(idx..).unwrap_or(&[]);
        let rest_array = crate::spec_ops::create_array_from_list(self.vm, scope, rest_args)?;
        Value::Object(rest_array)
      } else {
        let arg_value = args.get(idx).copied().unwrap_or(Value::Undefined);

        // Default parameters.
        if matches!(arg_value, Value::Undefined) {
          if let Some(default_expr) = param.default {
            self.eval_expr(scope, body, default_expr)?
          } else {
            Value::Undefined
          }
        } else {
          arg_value
        }
      };

      self.bind_pattern(scope, body, param.pat, value, PatBindingKind::Param)?;

      // Rest parameters are always final.
      if param.rest {
        break;
      }
    }

    // For block-bodied functions, create a dedicated function-body lexical environment nested
    // inside the function's VariableEnvironment (which holds `var` + parameter bindings).
    //
    // This matches `exec.rs::instantiate_function` and ensures that:
    // - `let`/`const`/`class` bindings do not live in the VariableEnvironment, and
    // - dynamic `var` declarations introduced via direct `eval` can correctly detect collisions
    //   with function-body lexical bindings.
    if matches!(func_meta.body, hir_js::FunctionBody::Block(_)) {
      let outer = self.env.lexical_env();
      let body_lex = scope.env_create(Some(outer))?;
      self.env.set_lexical_env(scope.heap_mut(), body_lex);
    }

    // Some early errors are still checked at runtime during instantiation so invalid declarations
    // do not partially pollute the function environment.
    //
    // For example, `let {x};` and `const x;` are syntax errors (missing initializers).
    if let hir_js::FunctionBody::Block(stmts) = &func_meta.body {
      self.early_error_missing_initializers_in_stmt_list(body, stmts.as_slice())?;
    }
    // Hoist function declarations (best-effort).
    //
    // This enables simple recursion and calling a function before its declaration statement is
    // executed.
    self.instantiate_var_decls(scope, body, body.root_stmts.as_slice())?;
    self.instantiate_function_decls(scope, body, body.root_stmts.as_slice(), /* annex_b */ false)?;
    // Create `let` / `const` bindings for the entire function body statement list up-front so TDZ
    // + shadowing semantics are correct.
    self.instantiate_lexical_decls(scope, body, body.root_stmts.as_slice(), self.env.lexical_env())?;

    Ok(())
  }

  fn early_error_missing_initializers_in_var_decl(
    &mut self,
    body: &hir_js::Body,
    decl: &hir_js::VarDecl,
  ) -> Result<(), VmError> {
    match decl.kind {
      // Destructuring `var`/`let` declarations always require an initializer (ECMA-262 early
      // error). This check must run before any hoisting so invalid declarations do not partially
      // pollute the function/global environment.
      hir_js::VarDeclKind::Var | hir_js::VarDeclKind::Let => {
        for declarator in &decl.declarators {
          self.vm.tick()?;
          if declarator.init.is_some() {
            continue;
          }

          let pat = self.get_pat(body, declarator.pat)?;
          if !matches!(pat.kind, hir_js::PatKind::Ident(_)) {
            let diag = diagnostics::Diagnostic::error(
              "VMJS0002",
              "Missing initializer in destructuring declaration",
              diagnostics::Span {
                file: diagnostics::FileId(0),
                range: pat.span,
              },
            );
            return Err(VmError::Syntax(vec![diag]));
          }
        }
      }
      // `const x;` is a syntax error (missing initializer).
      hir_js::VarDeclKind::Const => {
        for declarator in &decl.declarators {
          self.vm.tick()?;
          if declarator.init.is_some() {
            continue;
          }

          let pat = self.get_pat(body, declarator.pat)?;
          let diag = diagnostics::Diagnostic::error(
            "VMJS0002",
            "Missing initializer in const declaration",
            diagnostics::Span {
              file: diagnostics::FileId(0),
              range: pat.span,
            },
          );
          return Err(VmError::Syntax(vec![diag]));
        }
      }
      _ => {}
    }
    Ok(())
  }

  fn early_error_missing_initializers_in_stmt_list(
    &mut self,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
  ) -> Result<(), VmError> {
    for stmt_id in stmts {
      self.early_error_missing_initializers_in_stmt(body, *stmt_id)?;
    }
    Ok(())
  }

  fn early_error_missing_initializers_in_stmt(
    &mut self,
    body: &hir_js::Body,
    stmt_id: hir_js::StmtId,
  ) -> Result<(), VmError> {
    self.vm.tick()?;
    let stmt = self.get_stmt(body, stmt_id)?;
    match &stmt.kind {
      hir_js::StmtKind::Var(decl) => {
        self.early_error_missing_initializers_in_var_decl(body, decl)?;
      }
      hir_js::StmtKind::Block(stmts) => {
        self.early_error_missing_initializers_in_stmt_list(body, stmts.as_slice())?;
      }
      hir_js::StmtKind::If {
        consequent,
        alternate,
        ..
      } => {
        self.early_error_missing_initializers_in_stmt(body, *consequent)?;
        if let Some(alt) = alternate {
          self.early_error_missing_initializers_in_stmt(body, *alt)?;
        }
      }
      hir_js::StmtKind::While { body: inner, .. }
      | hir_js::StmtKind::DoWhile { body: inner, .. }
      | hir_js::StmtKind::Labeled { body: inner, .. }
      | hir_js::StmtKind::With { body: inner, .. } => {
        self.early_error_missing_initializers_in_stmt(body, *inner)?;
      }
      hir_js::StmtKind::For { init, body: inner, .. } => {
        if let Some(hir_js::ForInit::Var(decl)) = init {
          self.early_error_missing_initializers_in_var_decl(body, decl)?;
        }
        self.early_error_missing_initializers_in_stmt(body, *inner)?;
      }
      // Destructuring `for-in/of` loop heads do not require initializers (bindings are per-iteration
      // and are created by the loop itself).
      hir_js::StmtKind::ForIn { body: inner, .. } => {
        self.early_error_missing_initializers_in_stmt(body, *inner)?;
      }
      hir_js::StmtKind::Try {
        block,
        catch,
        finally_block,
      } => {
        self.early_error_missing_initializers_in_stmt(body, *block)?;
        if let Some(catch) = catch {
          self.early_error_missing_initializers_in_stmt(body, catch.body)?;
        }
        if let Some(finally) = finally_block {
          self.early_error_missing_initializers_in_stmt(body, *finally)?;
        }
      }
      hir_js::StmtKind::Switch { cases, .. } => {
        for case in cases {
          self.early_error_missing_initializers_in_stmt_list(body, case.consequent.as_slice())?;
        }
      }
      _ => {}
    }
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
              let mut names: Vec<hir_js::NameId> = Vec::new();
              self.collect_pat_idents(body, declarator.pat, &mut names)?;
              for name_id in names {
                let name = self.resolve_name(name_id)?;
                self.env.declare_var(self.vm, scope, name.as_str())?;
              }
            }
          }
        }
        hir_js::StmtKind::For { init, body: inner, .. } => {
          if let Some(hir_js::ForInit::Var(decl)) = init {
            if decl.kind == hir_js::VarDeclKind::Var {
              for declarator in &decl.declarators {
                self.vm.tick()?;
                let mut names: Vec<hir_js::NameId> = Vec::new();
                self.collect_pat_idents(body, declarator.pat, &mut names)?;
                for name_id in names {
                  let name = self.resolve_name(name_id)?;
                  self.env.declare_var(self.vm, scope, name.as_str())?;
                }
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
                let mut names: Vec<hir_js::NameId> = Vec::new();
                self.collect_pat_idents(body, declarator.pat, &mut names)?;
                for name_id in names {
                  let name = self.resolve_name(name_id)?;
                  self.env.declare_var(self.vm, scope, name.as_str())?;
                }
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

  fn collect_pat_idents(
    &mut self,
    body: &hir_js::Body,
    pat_id: hir_js::PatId,
    out: &mut Vec<hir_js::NameId>,
  ) -> Result<(), VmError> {
    // This traversal can be O(n) in the number of pattern nodes. Budget it explicitly so huge
    // destructuring patterns don't allow uninterruptible work within a single statement tick.
    const TICK_EVERY: usize = 256;
    fn visit(
      evaluator: &mut HirEvaluator<'_>,
      body: &hir_js::Body,
      pat_id: hir_js::PatId,
      out: &mut Vec<hir_js::NameId>,
      visited: &mut usize,
    ) -> Result<(), VmError> {
      // Avoid ticking on the first node: callers usually already tick once per statement/parameter.
      if *visited != 0 && *visited % TICK_EVERY == 0 {
        evaluator.vm.tick()?;
      }
      *visited = visited.saturating_add(1);
      let pat = evaluator.get_pat(body, pat_id)?;
      match &pat.kind {
        hir_js::PatKind::Ident(name_id) => {
          out.push(*name_id);
          Ok(())
        }
        hir_js::PatKind::Array(arr) => {
          for elem in &arr.elements {
            if let Some(elem) = elem {
              visit(evaluator, body, elem.pat, out, visited)?;
            }
          }
          if let Some(rest) = arr.rest {
            visit(evaluator, body, rest, out, visited)?;
          }
          Ok(())
        }
        hir_js::PatKind::Object(obj) => {
          for prop in &obj.props {
            visit(evaluator, body, prop.value, out, visited)?;
          }
          if let Some(rest) = obj.rest {
            visit(evaluator, body, rest, out, visited)?;
          }
          Ok(())
        }
        hir_js::PatKind::Rest(inner) => visit(evaluator, body, **inner, out, visited),
        hir_js::PatKind::Assign { target, .. } => visit(evaluator, body, *target, out, visited),
        hir_js::PatKind::AssignTarget(_) => Err(VmError::Unimplemented(
          "assignment target in declaration pattern (hir-js compiled path)",
        )),
      }
    }
    let mut visited: usize = 0;
    visit(self, body, pat_id, out, &mut visited)
  }

  fn instantiate_function_decls(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
    annex_b: bool,
  ) -> Result<(), VmError> {
    for stmt_id in stmts {
      self.vm.tick()?;
      let stmt = self.get_stmt(body, *stmt_id)?;
      match &stmt.kind {
        hir_js::StmtKind::Decl(def_id) => {
          // Only hoist function declarations.
          //
          // Class declarations have TDZ semantics and are evaluated as statements (they create the
          // binding during lexical instantiation, and initialize it when the declaration executes),
          // so we intentionally do not hoist them here.
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
          let Some(func_meta) = decl_body.function.as_ref() else {
            return Err(VmError::InvariantViolation("function body missing function metadata"));
          };
          // Annex B block-function hoisting applies only to ordinary (non-async, non-generator)
          // function declarations. Async/generator/async-generator declarations remain block-scoped
          // even in non-strict mode.
          if annex_b && (func_meta.async_ || func_meta.generator) {
            continue;
          }
          let name = self.resolve_name(def.name)?;
          let func_obj = self.alloc_user_function_object(
            scope,
            body_id,
            name.as_str(),
            /* is_arrow */ false,
            /* is_constructable */ true,
            /* name_binding */ None,
            EcmaFunctionKind::Decl,
          )?;
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
        // Strict mode: only top-level function declarations are var-scoped.
        //
        // Block-scoped function declarations are instantiated at block/switch entry in a fresh
        // lexical environment (see `instantiate_block_scoped_function_decls_in_stmt_list`).
        _ if self.strict => {}
        // Non-strict mode: treat block function declarations as var-scoped (Annex B-ish).
        //
        // Note: this applies only to ordinary functions; async/generator functions are instantiated
        // as block-scoped bindings (see `instantiate_block_scoped_function_decls_in_stmt_list`).
        hir_js::StmtKind::Block(inner) => {
          self.instantiate_function_decls(scope, body, inner.as_slice(), /* annex_b */ true)?;
        }
        hir_js::StmtKind::If {
          consequent,
          alternate,
          ..
        } => {
          self.instantiate_function_decls(
            scope,
            body,
            std::slice::from_ref(consequent),
            /* annex_b */ true,
          )?;
          if let Some(alt) = alternate {
            self.instantiate_function_decls(scope, body, std::slice::from_ref(alt), /* annex_b */ true)?;
          }
        }
        hir_js::StmtKind::While { body: inner, .. }
        | hir_js::StmtKind::DoWhile { body: inner, .. }
        | hir_js::StmtKind::Labeled { body: inner, .. }
        | hir_js::StmtKind::With { body: inner, .. } => {
          self.instantiate_function_decls(scope, body, std::slice::from_ref(inner), /* annex_b */ true)?;
        }
        hir_js::StmtKind::For { body: inner, .. } => {
          self.instantiate_function_decls(scope, body, std::slice::from_ref(inner), /* annex_b */ true)?;
        }
        hir_js::StmtKind::Try {
          block,
          catch,
          finally_block,
        } => {
          self.instantiate_function_decls(scope, body, std::slice::from_ref(block), /* annex_b */ true)?;
          if let Some(catch) = catch {
            self.instantiate_function_decls(
              scope,
              body,
              std::slice::from_ref(&catch.body),
              /* annex_b */ true,
            )?;
          }
          if let Some(finally_block) = finally_block {
            self.instantiate_function_decls(
              scope,
              body,
              std::slice::from_ref(finally_block),
              /* annex_b */ true,
            )?;
          }
        }
        hir_js::StmtKind::Switch { cases, .. } => {
          for case in cases {
            self.instantiate_function_decls(scope, body, case.consequent.as_slice(), /* annex_b */ true)?;
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
      match &stmt.kind {
        hir_js::StmtKind::Var(decl) => {
          self.instantiate_lexical_decl(scope, body, decl, env)?;
        }
        hir_js::StmtKind::Decl(def_id) => {
          let def = self
            .hir()
            .def(*def_id)
            .ok_or(VmError::InvariantViolation("hir def id missing from compiled script"))?;
          let Some(body_id) = def.body else {
            continue;
          };
          let decl_body = self.get_body(body_id)?;
          if decl_body.kind != hir_js::BodyKind::Class {
            continue;
          }
          let name = self.resolve_name(def.name)?;
          // Keep the engine robust against malformed HIR (e.g. a binding already exists).
          if scope.heap().env_has_binding(env, name.as_str())? {
            continue;
          }
          // Class declarations create mutable lexical bindings (like `let`).
          scope.env_create_mutable_binding(env, name.as_str())?;
        }
        _ => {}
      }
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
      let mut names: Vec<hir_js::NameId> = Vec::new();
      self.collect_pat_idents(body, declarator.pat, &mut names)?;
      for name_id in names {
        let name = self.resolve_name(name_id)?;

        // Keep the engine robust against malformed HIR (e.g. a binding already exists).
        if scope.heap().env_has_binding(env, name.as_str())? {
          continue;
        }

        match decl.kind {
          hir_js::VarDeclKind::Let => scope.env_create_mutable_binding(env, name.as_str())?,
          hir_js::VarDeclKind::Const => scope.env_create_immutable_binding(env, name.as_str())?,
          _ => {
            return Err(VmError::InvariantViolation(
              "unexpected VarDeclKind in lexical declaration instantiation",
            ));
          }
        }
      }
    }
    Ok(())
  }

  fn instantiate_block_scoped_function_decls_in_stmt_list(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    env: GcEnv,
    stmts: &[hir_js::StmtId],
  ) -> Result<(), VmError> {
    // In strict mode, all block-scoped function declarations are instantiated here at block entry.
    //
    // In non-strict mode, only async/generator/async-generator function declarations are treated
    // as block-scoped (Annex B does not apply to these forms). Ordinary function declarations are
    // handled by var-hoisting in `instantiate_function_decls`.
    for stmt_id in stmts {
      // Tick per statement list entry so large blocks of function declarations cannot be
      // instantiated without consuming fuel.
      self.vm.tick()?;
      let stmt = self.get_stmt(body, *stmt_id)?;
      let hir_js::StmtKind::Decl(def_id) = &stmt.kind else {
        continue;
      };
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
      let Some(func_meta) = decl_body.function.as_ref() else {
        return Err(VmError::InvariantViolation("function body missing function metadata"));
      };
      if !self.strict && !func_meta.async_ && !func_meta.generator {
        continue;
      }
      let name = self.resolve_name(def.name)?;
      let name_str = name.as_str();
      if scope.heap().env_has_binding(env, name_str)? {
        // Duplicate block-scoped declarations are early errors; keep the engine robust.
        return Err(VmError::TypeError("Identifier has already been declared"));
      }

      scope.env_create_mutable_binding(env, name_str)?;
      let func_obj = self.alloc_user_function_object(
        scope,
        body_id,
        name_str,
        /* is_arrow */ false,
        /* is_constructable */ true,
        /* name_binding */ None,
        EcmaFunctionKind::Decl,
      )?;

      let mut init_scope = scope.reborrow();
      init_scope.push_root(Value::Object(func_obj))?;
      init_scope
        .heap_mut()
        .env_initialize_binding(env, name_str, Value::Object(func_obj))?;
    }
    Ok(())
  }

  fn eval_stmt_list(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
  ) -> Result<Flow, VmError> {
    // Root the running completion value so `UpdateEmpty` last-value semantics are GC-safe:
    // a later statement in the list may allocate/GC while completing empty.
    //
    // This matches the interpreter (`exec.rs::eval_stmt_list`) which keeps `last_value` in a
    // heap-root across statement execution.
    let last_root = scope.heap_mut().add_root(Value::Undefined)?;
    let mut last: Option<Value> = None;

    let res = (|| {
      for stmt_id in stmts {
        let flow = self.eval_stmt(scope, body, *stmt_id)?;
        match flow {
          Flow::Normal(v) => {
            if let Some(v) = v {
              last = Some(v);
              scope.heap_mut().set_root(last_root, v);
            }
          }
          abrupt => return Ok(abrupt.update_empty(last)),
        }
      }
      Ok(Flow::Normal(last))
    })();

    scope.heap_mut().remove_root(last_root);
    res
  }

  fn eval_stmt(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmt_id: hir_js::StmtId,
  ) -> Result<Flow, VmError> {
    self.eval_stmt_labelled(scope, body, stmt_id, &[])
  }

  /// Evaluates a statement with an associated label set.
  ///
  /// This models ECMA-262 `LabelledEvaluation` / `LoopEvaluation` label propagation:
  /// nested label statements extend `label_set`, and iteration statements use it to determine which
  /// labelled `continue` completions are consumed by the loop.
  fn eval_stmt_labelled(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmt_id: hir_js::StmtId,
    label_set: &[hir_js::NameId],
  ) -> Result<Flow, VmError> {
    // Budget once per statement evaluation.
    self.vm.tick()?;

    let stmt = self.get_stmt(body, stmt_id)?;
    let res: Result<Flow, VmError> = match &stmt.kind {
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
          self.instantiate_block_scoped_function_decls_in_stmt_list(
            scope,
            body,
            block_env,
            stmts.as_slice(),
          )?;
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
        // ECMA-262 `LoopEvaluation` tracks a running completion value `V` starting at `undefined`.
        // Unlike block statement lists, loop statements never complete with an empty value.
        let v_root = scope.heap_mut().add_root(Value::Undefined)?;
        let mut v = Value::Undefined;

        let result = (|| -> Result<Flow, VmError> {
          loop {
            // Ensure empty loops still consume budget.
            self.vm.tick()?;
            let test_value = self.eval_expr(scope, body, *test)?;
            if !scope.heap().to_boolean(test_value)? {
              return Ok(Flow::Normal(Some(v)));
            }

            match self.eval_stmt(scope, body, *inner)? {
              Flow::Normal(value) => {
                if let Some(value) = value {
                  v = value;
                  scope.heap_mut().set_root(v_root, value);
                }
              }
              Flow::Continue(None, value) => {
                if let Some(value) = value {
                  v = value;
                  scope.heap_mut().set_root(v_root, value);
                }
              }
              Flow::Continue(Some(label), value) if label_set.iter().any(|l| *l == label) => {
                if let Some(value) = value {
                  v = value;
                  scope.heap_mut().set_root(v_root, value);
                }
              }
              Flow::Continue(Some(label), value) => {
                return Ok(Flow::Continue(Some(label), value).update_empty(Some(v)))
              }
              Flow::Break(None, break_value) => {
                let out = break_value.unwrap_or(v);
                v = out;
                scope.heap_mut().set_root(v_root, out);
                return Ok(Flow::Normal(Some(out)));
              }
              Flow::Break(Some(label), break_value) => {
                return Ok(Flow::Break(Some(label), break_value).update_empty(Some(v)))
              }
              Flow::Return(v) => return Ok(Flow::Return(v)),
            }
          }
        })();

        scope.heap_mut().remove_root(v_root);
        result
      }
      hir_js::StmtKind::DoWhile { test, body: inner } => {
        // ECMA-262 `LoopEvaluation` running completion value `V`.
        let v_root = scope.heap_mut().add_root(Value::Undefined)?;
        let mut v = Value::Undefined;

        let result = (|| -> Result<Flow, VmError> {
          loop {
            self.vm.tick()?;
            match self.eval_stmt(scope, body, *inner)? {
              Flow::Normal(value) => {
                if let Some(value) = value {
                  v = value;
                  scope.heap_mut().set_root(v_root, value);
                }
              }
              Flow::Continue(None, value) => {
                if let Some(value) = value {
                  v = value;
                  scope.heap_mut().set_root(v_root, value);
                }
              }
              Flow::Continue(Some(label), value) if label_set.iter().any(|l| *l == label) => {
                if let Some(value) = value {
                  v = value;
                  scope.heap_mut().set_root(v_root, value);
                }
              }
              Flow::Continue(Some(label), value) => {
                return Ok(Flow::Continue(Some(label), value).update_empty(Some(v)))
              }
              Flow::Break(None, break_value) => {
                let out = break_value.unwrap_or(v);
                v = out;
                scope.heap_mut().set_root(v_root, out);
                return Ok(Flow::Normal(Some(out)));
              }
              Flow::Break(Some(label), break_value) => {
                return Ok(Flow::Break(Some(label), break_value).update_empty(Some(v)))
              }
              Flow::Return(v) => return Ok(Flow::Return(v)),
            }

            let test_value = self.eval_expr(scope, body, *test)?;
            if !scope.heap().to_boolean(test_value)? {
              return Ok(Flow::Normal(Some(v)));
            }
          }
        })();

        scope.heap_mut().remove_root(v_root);
        result
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
          // ECMA-262 `LoopEvaluation` running completion value `V`.
          let v_root = scope.heap_mut().add_root(Value::Undefined)?;
          let mut v = Value::Undefined;

          let result = (|| -> Result<Flow, VmError> {
            // Create a loop-scoped declarative environment for the lexical declaration and evaluate
            // the initializer with TDZ semantics.
            let loop_env = scope.env_create(Some(outer_lex))?;
            self.env.set_lexical_env(scope.heap_mut(), loop_env);

            // Bind names in TDZ before evaluating initializers.
            //
            // This is required so default destructuring initializers like `let {x = x} = {}` throw
            // a ReferenceError instead of resolving `x` from an outer scope.
            for declarator in &init_decl.declarators {
              self.vm.tick()?;
              let mut names: Vec<hir_js::NameId> = Vec::new();
              self.collect_pat_idents(body, declarator.pat, &mut names)?;
              for name_id in names {
                let name = self.resolve_name(name_id)?;
                if scope.heap().env_has_binding(loop_env, name.as_str())? {
                  continue;
                }
                match init_decl.kind {
                  hir_js::VarDeclKind::Let => {
                    scope.env_create_mutable_binding(loop_env, name.as_str())?;
                  }
                  hir_js::VarDeclKind::Const => {
                    scope.env_create_immutable_binding(loop_env, name.as_str())?;
                  }
                  _ => {
                    return Err(VmError::InvariantViolation(
                      "unexpected VarDeclKind in lexical for-loop initialization",
                    ));
                  }
                }
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
                  return Ok(Flow::Normal(Some(v)));
                }
              }

              match self.eval_stmt(scope, body, *inner)? {
                Flow::Normal(value) => {
                  if let Some(value) = value {
                    v = value;
                    scope.heap_mut().set_root(v_root, value);
                  }
                }
                Flow::Continue(None, value) => {
                  if let Some(value) = value {
                    v = value;
                    scope.heap_mut().set_root(v_root, value);
                  }
                }
                Flow::Continue(Some(label), value) if label_set.iter().any(|l| *l == label) => {
                  if let Some(value) = value {
                    v = value;
                    scope.heap_mut().set_root(v_root, value);
                  }
                }
                Flow::Continue(Some(label), value) => {
                  return Ok(Flow::Continue(Some(label), value).update_empty(Some(v)))
                }
                Flow::Break(None, break_value) => {
                  let out = break_value.unwrap_or(v);
                  v = out;
                  scope.heap_mut().set_root(v_root, out);
                  return Ok(Flow::Normal(Some(out)));
                }
                Flow::Break(Some(label), break_value) => {
                  return Ok(Flow::Break(Some(label), break_value).update_empty(Some(v)))
                }
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
          scope.heap_mut().remove_root(v_root);
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

        // ECMA-262 `LoopEvaluation` running completion value `V`.
        let v_root = scope.heap_mut().add_root(Value::Undefined)?;
        let mut v = Value::Undefined;

        let result = (|| -> Result<Flow, VmError> {
          loop {
            // Ensure empty loops still consume budget.
            self.vm.tick()?;
            if let Some(test) = test {
              let test_value = self.eval_expr(scope, body, *test)?;
              if !scope.heap().to_boolean(test_value)? {
                return Ok(Flow::Normal(Some(v)));
              }
            }

            match self.eval_stmt(scope, body, *inner)? {
              Flow::Normal(value) => {
                if let Some(value) = value {
                  v = value;
                  scope.heap_mut().set_root(v_root, value);
                }
              }
              Flow::Continue(None, value) => {
                if let Some(value) = value {
                  v = value;
                  scope.heap_mut().set_root(v_root, value);
                }
              }
              Flow::Continue(Some(label), value) if label_set.iter().any(|l| *l == label) => {
                if let Some(value) = value {
                  v = value;
                  scope.heap_mut().set_root(v_root, value);
                }
              }
              Flow::Continue(Some(label), value) => {
                return Ok(Flow::Continue(Some(label), value).update_empty(Some(v)))
              }
              Flow::Break(None, break_value) => {
                let out = break_value.unwrap_or(v);
                v = out;
                scope.heap_mut().set_root(v_root, out);
                return Ok(Flow::Normal(Some(out)));
              }
              Flow::Break(Some(label), break_value) => {
                return Ok(Flow::Break(Some(label), break_value).update_empty(Some(v)))
              }
              Flow::Return(v) => return Ok(Flow::Return(v)),
            }

            if let Some(update) = update {
              let _ = self.eval_expr(scope, body, *update)?;
            }
          }
        })();

        scope.heap_mut().remove_root(v_root);
        result
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
          let iterable = self.eval_for_in_of_rhs_with_tdz_env(scope, body, left, *right)?;

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

          // ECMA-262 `LoopEvaluation` running completion value `V`.
          let v_root_idx = iter_scope.heap().root_stack.len();
          iter_scope.push_root(Value::Undefined)?;
          let mut v = Value::Undefined;

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
                // Per spec, `for..in/of` with lexical declarations creates a fresh binding per
                // iteration. Create the binding in TDZ, then initialize it during binding
                // initialization.
                if let Err(err) = self.instantiate_lexical_decl(&mut iter_scope, body, var_decl, env) {
                  self.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
                  return Err(err);
                }
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
              Flow::Normal(value) => {
                if let Some(value) = value {
                  v = value;
                  iter_scope.heap_mut().root_stack[v_root_idx] = value;
                }
              }
              Flow::Continue(None, value) => {
                if let Some(value) = value {
                  v = value;
                  iter_scope.heap_mut().root_stack[v_root_idx] = value;
                }
              }
              Flow::Continue(Some(label), value) if label_set.iter().any(|l| *l == label) => {
                if let Some(value) = value {
                  v = value;
                  iter_scope.heap_mut().root_stack[v_root_idx] = value;
                }
              }
              Flow::Continue(Some(label), value) => {
                let out_flow = Flow::Continue(Some(label), value).update_empty(Some(v));
                if let Some(v) = out_flow.value() {
                  iter_scope.push_root(v)?;
                }
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
                return Ok(out_flow);
              }
              Flow::Break(None, break_value) => {
                let out = break_value.unwrap_or(v);
                if let Some(v) = break_value {
                  iter_scope.push_root(v)?;
                }
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
                return Ok(Flow::Normal(Some(out)));
              }
              Flow::Break(Some(label), break_value) => {
                let out_flow = Flow::Break(Some(label), break_value).update_empty(Some(v));
                if let Some(v) = out_flow.value() {
                  iter_scope.push_root(v)?;
                }
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
                return Ok(out_flow);
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

          Ok(Flow::Normal(Some(v)))
        } else {
          // --- for..in ---
          let rhs_value = self.eval_for_in_of_rhs_with_tdz_env(scope, body, left, *right)?;
          // ECMA-262 `ForIn/OfHeadEvaluation` (iterationKind = enumerate):
          // If the RHS evaluates to `null` or `undefined`, iteration is skipped (no throw) and the
          // statement's completion value is `undefined` (not empty).
          if matches!(rhs_value, Value::Null | Value::Undefined) {
            return Ok(Flow::normal(Value::Undefined));
          }

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

          // ECMA-262 `LoopEvaluation` running completion value `V`.
          let v_root_idx = iter_scope.heap().root_stack.len();
          iter_scope.push_root(Value::Undefined)?;
          let mut v = Value::Undefined;

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
                // Per spec, `for..in` with lexical declarations creates a fresh binding per
                // iteration. Create the binding in TDZ, then initialize it during binding
                // initialization.
                if let Err(err) = self.instantiate_lexical_decl(&mut iter_scope, body, var_decl, env) {
                  self.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
                  return Err(err);
                }
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
              Flow::Normal(value) => {
                if let Some(value) = value {
                  v = value;
                  iter_scope.heap_mut().root_stack[v_root_idx] = value;
                }
              }
              Flow::Continue(None, value) => {
                if let Some(value) = value {
                  v = value;
                  iter_scope.heap_mut().root_stack[v_root_idx] = value;
                }
              }
              Flow::Continue(Some(label), value) if label_set.iter().any(|l| *l == label) => {
                if let Some(value) = value {
                  v = value;
                  iter_scope.heap_mut().root_stack[v_root_idx] = value;
                }
              }
              Flow::Continue(Some(label), value) => {
                return Ok(Flow::Continue(Some(label), value).update_empty(Some(v)))
              }
              Flow::Break(None, break_value) => {
                let out = break_value.unwrap_or(v);
                return Ok(Flow::Normal(Some(out)));
              }
              Flow::Break(Some(label), break_value) => {
                return Ok(Flow::Break(Some(label), break_value).update_empty(Some(v)))
              }
              Flow::Return(v) => return Ok(Flow::Return(v)),
            }
          }
          Ok(Flow::Normal(Some(v)))
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
          // Strict mode: block-scoped function declarations in the case block are instantiated in
          // the case block lexical environment (ECMA-262 `BlockDeclarationInstantiation`).
          for (i, case) in cases.iter().enumerate() {
            if i % CASE_TICK_EVERY == 0 {
              self.vm.tick()?;
            }
            self.instantiate_block_scoped_function_decls_in_stmt_list(
              &mut switch_scope,
              body,
              switch_env,
              case.consequent.as_slice(),
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
                  Flow::Break(None, break_value) => {
                    if let Some(value) = break_value {
                      v = value;
                      switch_scope.heap_mut().root_stack[v_root_idx] = value;
                    }
                    return Ok(Flow::Normal(Some(v)));
                  }
                  // Labeled control flow propagates.
                  Flow::Break(Some(label), break_value) => {
                    if let Some(value) = break_value {
                      v = value;
                      switch_scope.heap_mut().root_stack[v_root_idx] = value;
                    }
                    return Ok(Flow::Break(Some(label), break_value).update_empty(Some(v)));
                  }
                  Flow::Continue(label, continue_value) => {
                    if let Some(value) = continue_value {
                      v = value;
                      switch_scope.heap_mut().root_stack[v_root_idx] = value;
                    }
                    return Ok(Flow::Continue(label, continue_value).update_empty(Some(v)));
                  }
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
      hir_js::StmtKind::Break(label) => Ok(Flow::Break(*label, None)),
      hir_js::StmtKind::Continue(label) => Ok(Flow::Continue(*label, None)),
      hir_js::StmtKind::Var(decl) => {
        self.eval_var_decl(scope, body, decl)?;
        Ok(Flow::empty())
      }
      hir_js::StmtKind::Decl(def_id) => {
        // Function declarations are handled during instantiation.
        let def = self
          .hir()
          .def(*def_id)
          .ok_or(VmError::InvariantViolation("hir def id missing from compiled script"))?;
        let Some(body_id) = def.body else {
          return Ok(Flow::empty());
        };
        // Avoid borrowing the body through `self` across calls that mutably borrow `self` (class
        // evaluation mutates the runtime environment). Clone the HIR Arc so the body reference is
        // independent of `self`.
        let hir = self.script.hir.clone();
        let decl_body = hir
          .body(body_id)
          .ok_or(VmError::InvariantViolation("hir body id missing from compiled script"))?;

        if decl_body.kind == hir_js::BodyKind::Function {
          // Already hoisted.
          Ok(Flow::empty())
        } else if decl_body.kind == hir_js::BodyKind::Class {
          let name = self.resolve_name(def.name)?;

          // Per ECMAScript, class declarations are evaluated within a fresh lexical environment whose
          // outer is the surrounding lexical environment. That environment contains an immutable
          // binding for the class name so class element functions can reference the class even if the
          // outer binding is later reassigned.
          let outer = self.env.lexical_env();
          let class_env = scope.env_create(Some(outer))?;
          self.env.set_lexical_env(scope.heap_mut(), class_env);

          // Evaluate the class definition with an inner immutable name binding.
          let result = self.eval_class(scope, decl_body, Some(name.as_str()), name.as_str(), None);
          // Restore the outer environment regardless of how class evaluation completes.
          self.env.set_lexical_env(scope.heap_mut(), outer);
          let func_obj = result?;

          // Initialize the outer (mutable) class binding in the surrounding environment.
          //
          // Root the class constructor object first: creating the binding may allocate and trigger
          // GC.
          let mut init_scope = scope.reborrow();
          init_scope.push_root(Value::Object(func_obj))?;

          if !init_scope.heap().env_has_binding(outer, name.as_str())? {
            // Non-block statement contexts may not have performed lexical hoisting yet.
            init_scope.env_create_mutable_binding(outer, name.as_str())?;
          }
          init_scope
            .heap_mut()
            .env_initialize_binding(outer, name.as_str(), Value::Object(func_obj))?;

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
            if !err.is_throw_completion() {
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
                // Catch parameter bindings have TDZ semantics like other lexical bindings:
                // - bindings are instantiated uninitialized,
                // - then binding initialization runs (which can evaluate destructuring defaults).
                //
                // Pre-create all identifier bindings before evaluating any default initializers so
                // self-references (e.g. `catch ({ x = x }) {}`) throw a ReferenceError.
                let mut names: Vec<hir_js::NameId> = Vec::new();
                self.collect_pat_idents(body, param_pat_id, &mut names)?;
                for name_id in names {
                  let name = self.resolve_name(name_id)?;
                  if !catch_scope.heap().env_has_binding(catch_env, name.as_str())? {
                    catch_scope.env_create_mutable_binding(catch_env, name.as_str())?;
                  }
                }
                self.bind_pattern(
                  &mut catch_scope,
                  body,
                  param_pat_id,
                  thrown_value,
                  PatBindingKind::Let,
                )?;
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
            Ok(flow) => flow.value(),
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
        let mut new_label_set: Vec<hir_js::NameId> = label_set.to_vec();
        new_label_set.push(*label);
        let flow = self.eval_stmt_labelled(scope, body, *inner, new_label_set.as_slice())?;
        match flow {
          Flow::Break(Some(target), value) if target == *label => Ok(Flow::Normal(value)),
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
    };

    // Improve stack traces for compiled *module* execution by attributing thrown exceptions to the
    // currently executing HIR statement span (similar to `exec.rs::eval_stmt_labelled`).
    //
    // This is intentionally scoped to module execution so broader HIR stack-trace semantics (e.g.
    // call-site locations) can be addressed separately.
    if !matches!(
      self.vm.get_active_script_or_module(),
      Some(ScriptOrModule::Module(_))
    ) {
      return res;
    }

    // Only annotate throw-completions: termination/OOM/etc must propagate untouched.
    let Err(err) = res else {
      return res;
    };
    if !err.is_throw_completion() {
      return Err(err);
    }

    let source = self.script.source.as_ref();
    let (line, col) = source.line_col(stmt.span.start);
    let update_top_frame = |stack: &mut Vec<StackFrame>| {
      if let Some(top) = stack.first_mut() {
        top.source = source.name.clone();
        top.line = line;
        top.col = col;
      } else {
        stack.push(StackFrame {
          function: None,
          source: source.name.clone(),
          line,
          col,
        });
      }
    };

    let err = crate::vm::coerce_error_to_throw(&*self.vm, scope, err);
    match err {
      VmError::Throw(value) => {
        let mut stack = self.vm.capture_stack();
        update_top_frame(&mut stack);
        Err(VmError::ThrowWithStack { value, stack })
      }
      VmError::ThrowWithStack { value, mut stack } => {
        // Mirror the AST evaluator's behavior: only patch captured stacks that have no meaningful
        // top-frame location (typically captured while executing native code).
        if stack.first().is_none() || stack.first().is_some_and(|top| top.line == 0) {
          update_top_frame(&mut stack);
        }
        Err(VmError::ThrowWithStack { value, stack })
      }
      other => Err(other),
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
        // Reuse `assign_to_pat` so assignment targets like `for (obj.x of iterable) {}` work.
        // This also supports destructuring patterns via `assign_pattern`.
        self.assign_to_pat(scope, body, *pat_id, value)
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

        // For `let`/`const` bindings (including destructuring patterns), create all bound names in
        // TDZ before binding initialization. This ensures defaults like `for (let {x = x} of xs) {}`
        // correctly throw a ReferenceError instead of resolving `x` from an outer scope.
        if matches!(var_decl.kind, hir_js::VarDeclKind::Let | hir_js::VarDeclKind::Const) {
          let env_rec = self.env.lexical_env();
          let mut names: Vec<hir_js::NameId> = Vec::new();
          self.collect_pat_idents(body, declarator.pat, &mut names)?;
          for name_id in names {
            let name = self.resolve_name(name_id)?;
            if scope.heap().env_has_binding(env_rec, name.as_str())? {
              continue;
            }
            match var_decl.kind {
              hir_js::VarDeclKind::Let => scope.env_create_mutable_binding(env_rec, name.as_str())?,
              hir_js::VarDeclKind::Const => {
                scope.env_create_immutable_binding(env_rec, name.as_str())?
              }
              _ => {
                return Err(VmError::InvariantViolation(
                  "unexpected VarDeclKind in for-in/of TDZ binding creation",
                ));
              }
            }
          }
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
    match kind {
      hir_js::VarDeclKind::Var => {
        if init_missing {
          // `var x;` is a no-op: it does not assign `undefined` if the binding already exists.
          //
          // The binding is ensured via the hoisting pass, but `var` declarations can also appear in
          // runtime-evaluated constructs (e.g. `for (var x; ...)`) so we preserve correct semantics
          // here as well.
          let pat = self.get_pat(body, pat_id)?;
          if let hir_js::PatKind::Ident(name_id) = pat.kind {
            let name = self.resolve_name(name_id)?;
            self.env.declare_var(self.vm, scope, name.as_str())
          } else {
            // `var` destructuring without an initializer should have been rejected as a syntax
            // error, but keep the VM robust and let destructuring semantics raise a TypeError.
            self.bind_pattern(scope, body, pat_id, value, PatBindingKind::Var)
          }
        } else {
          self.bind_pattern(scope, body, pat_id, value, PatBindingKind::Var)
        }
      }
      hir_js::VarDeclKind::Let => self.bind_pattern(scope, body, pat_id, value, PatBindingKind::Let),
      hir_js::VarDeclKind::Const => {
        if init_missing {
          // Should have been caught as a syntax error, but keep the engine robust.
          return Err(VmError::TypeError("Missing initializer in const declaration"));
        }
        self.bind_pattern(scope, body, pat_id, value, PatBindingKind::Const)
      }
      hir_js::VarDeclKind::Using | hir_js::VarDeclKind::AwaitUsing => Err(VmError::Unimplemented(
        "using declarations (hir-js compiled path)",
      )),
    }
  }

  fn bind_identifier(
    &mut self,
    scope: &mut Scope<'_>,
    name_id: hir_js::NameId,
    value: Value,
    kind: PatBindingKind,
  ) -> Result<(), VmError> {
    let name = self.resolve_name(name_id)?;
    // `SetFunctionName`-like behaviour: when binding an anonymous function/class to an identifier,
    // infer its `name` from the identifier.
    maybe_set_anonymous_function_name(scope, value, name.as_str())?;
    match kind {
      PatBindingKind::Var => self.env.set_var(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        scope,
        name.as_str(),
        value,
      ),
      PatBindingKind::Let => {
        let env_rec = self.env.lexical_env();
        if !scope.heap().env_has_binding(env_rec, name.as_str())? {
          return Err(VmError::InvariantViolation(
            "`let` binding must be instantiated before initialization",
          ));
        }
        scope
          .heap_mut()
          .env_initialize_binding(env_rec, name.as_str(), value)
      }
      PatBindingKind::Const => {
        let env_rec = self.env.lexical_env();
        if !scope.heap().env_has_binding(env_rec, name.as_str())? {
          return Err(VmError::InvariantViolation(
            "`const` binding must be instantiated before initialization",
          ));
        }
        scope
          .heap_mut()
          .env_initialize_binding(env_rec, name.as_str(), value)
      }
      PatBindingKind::Param => {
        // Parameters are mutable bindings in the function environment.
        let env_rec = self.env.lexical_env();
        if !scope.heap().env_has_binding(env_rec, name.as_str())? {
          scope.env_create_mutable_binding(env_rec, name.as_str())?;
        }
        // Sloppy-mode functions with a simple parameter list may contain duplicate parameter names.
        // When a duplicate is encountered, the binding has already been initialized by the earlier
        // parameter and should be updated instead.
        match scope
          .heap()
          .env_get_binding_value(env_rec, name.as_str(), /* strict */ false)
        {
          Ok(_) => scope
            .heap_mut()
            .env_set_mutable_binding(env_rec, name.as_str(), value, /* strict */ false),
          // TDZ sentinel from `Heap::env_get_binding_value`.
          Err(VmError::Throw(Value::Null)) => scope
            .heap_mut()
            .env_initialize_binding(env_rec, name.as_str(), value),
          Err(err) => Err(err),
        }
      }
    }
  }

  fn iterator_close_on_err(
    &mut self,
    scope: &mut Scope<'_>,
    iterator_record: &crate::iterator::IteratorRecord,
    err: VmError,
  ) -> Result<(), VmError> {
    if iterator_record.done {
      return Err(err);
    }

    // Root any pending thrown value across `IteratorClose`, which can allocate and trigger GC.
    if err.is_throw_completion() {
      if let Some(thrown) = err.thrown_value() {
        scope.push_root(thrown)?;
      }
    }

    match crate::iterator::iterator_close(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      scope,
      iterator_record,
      crate::iterator::CloseCompletionKind::Throw,
    ) {
      Ok(()) => Err(err),
      Err(close_err) => Err(if err.is_throw_completion() {
        close_err
      } else {
        err
      }),
    }
  }

  fn bind_pattern(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    pat_id: hir_js::PatId,
    value: Value,
    kind: PatBindingKind,
  ) -> Result<(), VmError> {
    // Keep temporary roots local to this binding operation.
    let mut scope = scope.reborrow();
    // Root the input value so destructuring can allocate without the RHS being collected.
    let value = scope.push_root(value)?;

    let pat = self.get_pat(body, pat_id)?;
    match &pat.kind {
      hir_js::PatKind::Ident(name_id) => self.bind_identifier(&mut scope, *name_id, value, kind),
      hir_js::PatKind::Array(arr) => self.bind_array_pattern(&mut scope, body, arr, value, kind),
      hir_js::PatKind::Object(obj) => self.bind_object_pattern(&mut scope, body, obj, value, kind),
      hir_js::PatKind::Assign {
        target,
        default_value,
      } => {
        let v = if matches!(value, Value::Undefined) {
          self.eval_expr(&mut scope, body, *default_value)?
        } else {
          value
        };
        self.bind_pattern(&mut scope, body, *target, v, kind)
      }
      hir_js::PatKind::Rest(inner) => self.bind_pattern(&mut scope, body, **inner, value, kind),
      hir_js::PatKind::AssignTarget(_) => Err(VmError::Unimplemented(
        "assignment target pattern in binding context (hir-js compiled path)",
      )),
    }
  }

  fn bind_object_pattern(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    pat: &hir_js::ObjectPat,
    value: Value,
    kind: PatBindingKind,
  ) -> Result<(), VmError> {
    // Object destructuring follows `GetV` semantics: property lookup uses `ToObject(value)`, but
    // accessors must observe `this = value` (the original RHS value), not the boxed object.
    //
    // Root the original RHS value across boxing: `ToObject` can allocate and therefore trigger GC.
    let src_value = scope.push_root(value)?;
    let obj = scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, src_value)?;
    scope.push_root(Value::Object(obj))?;

    let mut excluded: Vec<PropertyKey> = Vec::new();
    if pat.rest.is_some() {
      excluded
        .try_reserve_exact(pat.props.len())
        .map_err(|_| VmError::OutOfMemory)?;
    }

    for prop in &pat.props {
      // Budget object destructuring by pattern size.
      self.vm.tick()?;
      let key = self.eval_object_key(scope, body, &prop.key)?;
      // Ensure keys stored for rest destructuring stay alive.
      if pat.rest.is_some() {
        root_property_key(scope, key)?;
        excluded.push(key);
      }

      // Keep temporary roots local to each property to avoid unbounded root-stack growth for large
      // patterns.
      let mut prop_scope = scope.reborrow();
      // Root key while performing the get/default evaluation.
      root_property_key(&mut prop_scope, key)?;

      let mut prop_value =
        prop_scope.get_with_host_and_hooks(self.vm, &mut *self.host, &mut *self.hooks, obj, key, src_value)?;
      if matches!(prop_value, Value::Undefined) {
        if let Some(default_expr) = prop.default_value {
          prop_value = self.eval_expr(&mut prop_scope, body, default_expr)?;
        }
      }

      self.bind_pattern(&mut prop_scope, body, prop.value, prop_value, kind)?;
    }

    let Some(rest_pat_id) = pat.rest else {
      return Ok(());
    };

    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

    // `...rest` uses `ObjectCreate(%Object.prototype%)` / `CopyDataProperties`.
    let rest_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
    scope.push_root(Value::Object(rest_obj))?;

    crate::spec_ops::copy_data_properties_with_host_and_hooks(
      self.vm,
      scope,
      &mut *self.host,
      &mut *self.hooks,
      rest_obj,
      Value::Object(obj),
      &excluded,
    )?;

    self.bind_pattern(scope, body, rest_pat_id, Value::Object(rest_obj), kind)
  }

  fn bind_array_pattern(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    pat: &hir_js::ArrayPat,
    value: Value,
    kind: PatBindingKind,
  ) -> Result<(), VmError> {
    // RequireObjectCoercible (ECMA-262): array destructuring disallows null/undefined but supports
    // primitives like String via iterator protocol.
    if matches!(value, Value::Undefined | Value::Null) {
      return Err(VmError::TypeError("array destructuring requires object coercible"));
    }

    let mut iterator_record =
      crate::iterator::get_iterator(self.vm, &mut *self.host, &mut *self.hooks, scope, value)?;
    // Root the iterator record across evaluation of defaults / nested bindings, which can allocate.
    scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

    for elem in &pat.elements {
      // Budget array destructuring by pattern size.
      if let Err(err) = self.vm.tick() {
        return self.iterator_close_on_err(scope, &iterator_record, err);
      }

      // Keep temporary roots local to each element.
      let mut elem_scope = scope.reborrow();

      let Some(elem) = elem else {
        // Elision: still advance the iterator but do not read `value`.
        if let Err(err) = crate::iterator::iterator_step(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut elem_scope,
          &mut iterator_record,
        ) {
          return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err);
        }
        continue;
      };

      let mut item = match crate::iterator::iterator_step_value(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        &mut elem_scope,
        &mut iterator_record,
      ) {
        Ok(Some(v)) => v,
        Ok(None) => Value::Undefined,
        Err(err) => return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err),
      };

      if matches!(item, Value::Undefined) {
        if let Some(default_expr) = elem.default_value {
          item = match self.eval_expr(&mut elem_scope, body, default_expr) {
            Ok(v) => v,
            Err(err) => return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err),
          };
        }
      }

      if let Err(err) = self.bind_pattern(&mut elem_scope, body, elem.pat, item, kind) {
        return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err);
      }
    }

    let Some(rest_pat_id) = pat.rest else {
      // Iterator binding initialization performs IteratorClose on normal completion when the
      // iterator is not exhausted.
      if !iterator_record.done {
        crate::iterator::iterator_close(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          scope,
          &iterator_record,
          crate::iterator::CloseCompletionKind::NonThrow,
        )?;
      }
      return Ok(());
    };

    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

    // Rest element must produce a real Array exotic object.
    let rest_arr = match scope.alloc_array(0) {
      Ok(arr) => arr,
      Err(err) => return self.iterator_close_on_err(scope, &iterator_record, err),
    };
    if let Err(err) = scope.push_root(Value::Object(rest_arr)) {
      return self.iterator_close_on_err(scope, &iterator_record, err);
    }
    if let Err(err) = scope
      .heap_mut()
      .object_set_prototype(rest_arr, Some(intr.array_prototype()))
    {
      return self.iterator_close_on_err(scope, &iterator_record, err);
    }

    let mut rest_idx: u32 = 0;
    loop {
      // Budget rest-element copying: `...rest` can iterate many remaining indices.
      if let Err(err) = self.vm.tick() {
        return self.iterator_close_on_err(scope, &iterator_record, err);
      }

      let next = match crate::iterator::iterator_step_value(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        scope,
        &mut iterator_record,
      ) {
        Ok(v) => v,
        Err(err) => return self.iterator_close_on_err(scope, &iterator_record, err),
      };
      let Some(v) = next else {
        break;
      };

      // Root the element value while allocating the property key and defining the property.
      let create_res = {
        let mut elem_scope = scope.reborrow();
        elem_scope.push_roots(&[Value::Object(rest_arr), v])?;

        let key_s = elem_scope.alloc_u32_index_string(rest_idx)?;
        let key = PropertyKey::from_string(key_s);
        root_property_key(&mut elem_scope, key)?;
        elem_scope.create_data_property_or_throw(rest_arr, key, v)
      };
      if let Err(err) = create_res {
        return self.iterator_close_on_err(scope, &iterator_record, err);
      }
      rest_idx = rest_idx.saturating_add(1);
    }

    let bind_res = self.bind_pattern(scope, body, rest_pat_id, Value::Object(rest_arr), kind);
    match bind_res {
      Ok(()) => Ok(()),
      Err(err) => self.iterator_close_on_err(scope, &iterator_record, err),
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
            let msg =
              fallible_format::try_format_error_message("", name.as_str(), " is not defined")?;
            Err(throw_reference_error(self.vm, scope, &msg)?)
          }
        }
      }
      hir_js::ExprKind::This => {
        if !self.this_initialized {
          return Err(throw_reference_error(
            self.vm,
            scope,
            "Must call super constructor in derived class before accessing 'this'",
          )?);
        }
        Ok(self.this)
      }
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
      hir_js::ExprKind::ImportMeta => {
        let Some(ScriptOrModule::Module(module)) = self.vm.get_active_script_or_module() else {
          return Err(VmError::Unimplemented("import.meta outside of modules"));
        };
        let obj = self
          .vm
          .get_or_create_import_meta_object(scope, &mut *self.hooks, module)?;
        Ok(Value::Object(obj))
      }
      hir_js::ExprKind::ImportCall { argument, attributes } => {
        // Dynamic `import()` expression.
        //
        // This delegates to the spec-shaped implementation in `module_loading::start_dynamic_import`.
        // Evaluate the specifier expression, then the optional `options` argument.
        //
        // Root the intermediate values while evaluating the second argument and while entering the
        // module loading algorithm, which may allocate and trigger GC.
        let mut import_scope = scope.reborrow();
        let specifier = self.eval_expr(&mut import_scope, body, *argument)?;
        import_scope.push_root(specifier)?;

        let options = match attributes {
          Some(options_expr) => self.eval_expr(&mut import_scope, body, *options_expr)?,
          None => Value::Undefined,
        };
        import_scope.push_root(options)?;

        let modules_ptr = self
          .vm
          .module_graph_ptr()
          .ok_or(VmError::Unimplemented("dynamic import requires a module graph"))?;
        // Safety: `Vm::module_graph_ptr` is only set by embeddings that ensure the graph outlives the
        // VM (see `Vm::set_module_graph` docs). `JsRuntime` stores the graph in a `Box`, so the pointer
        // remains stable even if the runtime is moved.
        let modules = unsafe { &mut *modules_ptr };

        module_loading::start_dynamic_import_with_host_and_hooks(
          self.vm,
          &mut import_scope,
          modules,
          &mut *self.host,
          &mut *self.hooks,
          self.env.global_object(),
          specifier,
          options,
        )
      }
      hir_js::ExprKind::FunctionExpr {
        body: func_body,
        name,
        is_arrow,
        ..
      } => {
        let mut name_str = name
          .as_ref()
          .and_then(|id| self.hir().names.resolve(*id))
          .unwrap_or("")
          .to_owned();
        // `hir-js` currently assigns arrow functions a placeholder name `"<arrow>"` so they can be
        // referenced in diagnostics. At runtime, arrow functions are anonymous unless a surrounding
        // expression applies `SetFunctionName` name inference.
        if *is_arrow {
          name_str.clear();
        }
        let func_obj = self.alloc_user_function_object(
          scope,
          *func_body,
          name_str.as_str(),
          *is_arrow,
          /* is_constructable */ !*is_arrow,
          // Named function expressions create an inner binding for their own name.
          // Arrow functions are always anonymous at the syntax level.
          /* name_binding */ (!*is_arrow && !name_str.is_empty()).then_some(name_str.as_str()),
          EcmaFunctionKind::Expr,
        )?;
        Ok(Value::Object(func_obj))
      }
      hir_js::ExprKind::ClassExpr { body: class_body, name, .. } => {
        let name_str = name
          .as_ref()
          .and_then(|id| self.hir().names.resolve(*id))
          .unwrap_or("")
          .to_owned();

        self.eval_class_expr(scope, *class_body, name.as_ref().map(|_| name_str.as_str()), None)
      }
      hir_js::ExprKind::Template(tpl) => self.eval_template_literal(scope, body, tpl),
      hir_js::ExprKind::TaggedTemplate { tag, template } => {
        self.eval_tagged_template(scope, body, expr.span, *tag, template)
      }
      other => Err(match other {
        hir_js::ExprKind::Await { .. } => VmError::Unimplemented("await (hir-js compiled path)"),
        hir_js::ExprKind::Yield { .. } => VmError::Unimplemented("yield (hir-js compiled path)"),
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
    // Untagged template literal evaluation:
    //   head + (ToString(expr) + literal)...
    //
    // Important: root the accumulator across any operation that can allocate (expression eval,
    // ToString, and string concatenation).
    let mut scope = scope.reborrow();

    let mut out = match tpl.cooked.first().and_then(|c| c.as_deref()) {
      Some(units) => scope.alloc_string_from_code_units(units)?,
      None => scope.alloc_string_from_utf8(&tpl.head)?,
    };

    for (idx, span) in tpl.spans.iter().enumerate() {
      // Root the accumulator across evaluation/coercion/allocations in this span.
      let mut span_scope = scope.reborrow();
      span_scope.push_root(Value::String(out))?;

      let value = self.eval_expr(&mut span_scope, body, span.expr)?;
      let value_s = span_scope.to_string(self.vm, &mut *self.host, &mut *self.hooks, value)?;

      // out += ToString(expr)
      let out_with_expr = concat_strings(&mut span_scope, out, value_s, || self.vm.tick())?;
      // Root intermediate concatenation result across allocation of the next literal.
      span_scope.push_root(Value::String(out_with_expr))?;

      // out += literal
      let lit_s = match tpl.cooked.get(idx + 1).and_then(|c| c.as_deref()) {
        Some(units) => span_scope.alloc_string_from_code_units(units)?,
        None => span_scope.alloc_string_from_utf8(&span.literal)?,
      };
      out = concat_strings(&mut span_scope, out_with_expr, lit_s, || self.vm.tick())?;
    }

    Ok(Value::String(out))
  }

  fn eval_tagged_template(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    expr_span: diagnostics::TextRange,
    tag: hir_js::ExprId,
    tpl: &hir_js::TemplateLiteral,
  ) -> Result<Value, VmError> {
    // Tagged template evaluation:
    //   1) Evaluate tag expression to get (callee, this) like CallExpression evaluation.
    //   2) GetTemplateObject (cached by realm+source+span).
    //   3) Evaluate substitutions left-to-right.
    //   4) Call callee(templateObject, ...substitutions).
    //
    // Important: optional chaining on the tag expression (`obj?.f\`...\``) short-circuits the entire
    // tagged template expression, skipping template object creation and substitution evaluation.

    let tag_expr = self.get_expr(body, tag)?;
    let mut scope = scope.reborrow();

    // Method call detection: `obj.prop\`...\`` uses `this = obj` (or primitive base).
    let (callee_value, this_value) = match &tag_expr.kind {
      hir_js::ExprKind::Member(member) => {
        let base = self.eval_expr(&mut scope, body, member.object)?;
        if member.optional && matches!(base, Value::Null | Value::Undefined) {
          return Ok(Value::Undefined);
        }

        // Root base across key evaluation / boxing / property access.
        scope.push_root(base)?;

        let key = self.eval_object_key(&mut scope, body, &member.property)?;
        root_property_key(&mut scope, key)?;

        let obj = match scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base) {
          Ok(obj) => obj,
          Err(VmError::TypeError(msg)) => return Err(throw_type_error(self.vm, &mut scope, msg)?),
          Err(err) => return Err(err),
        };
        scope.push_root(Value::Object(obj))?;

        let func =
          scope.get_with_host_and_hooks(self.vm, &mut *self.host, &mut *self.hooks, obj, key, base)?;
        (func, base)
      }
      _ => {
        let callee_value = self.eval_expr(&mut scope, body, tag)?;
        (callee_value, Value::Undefined)
      }
    };

    // Root callee/this for the remainder of the tagged template evaluation.
    scope.push_roots(&[callee_value, this_value])?;

    // Compute a stable span key matching interpreter semantics (env prefix/base offsets).
    let rel_start = expr_span.start.saturating_sub(self.env.prefix_len());
    let rel_end = expr_span.end.saturating_sub(self.env.prefix_len());
    let span_start = self.env.base_offset().saturating_add(rel_start);
    let span_end = self.env.base_offset().saturating_add(rel_end);

    let template_obj = self.vm.get_or_create_template_object(
      &mut scope,
      self.env.source(),
      span_start,
      span_end,
      tpl.raw.as_ref(),
      tpl.cooked.as_ref(),
    )?;
    scope.push_root(Value::Object(template_obj))?;

    let mut args: Vec<Value> = Vec::new();
    args
      .try_reserve_exact(tpl.spans.len().saturating_add(1))
      .map_err(|_| VmError::OutOfMemory)?;
    args.push(Value::Object(template_obj));

    for span in &tpl.spans {
      let value = self.eval_expr(&mut scope, body, span.expr)?;
      scope.push_root(value)?;
      args.push(value);
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
        // Mirror `exec.rs::eval_lit_regex`.
        let intr = self
          .vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let literal = literal.as_str();
        // `hir-js` stores regexp literals verbatim including the leading `/` and any flags (matching
        // the parse-js AST representation).
        if !literal.starts_with('/') {
          let err_obj = crate::error_object::new_syntax_error_object(
            scope,
            &intr,
            "Invalid regular expression literal",
          )?;
          return Err(VmError::Throw(err_obj));
        }

        // Stage 1: scan using classic RegExp character-class termination rules to extract flags.
        //
        // This is the legacy `in_class: bool` behaviour, expressed as a depth counter that only ever
        // takes values 0/1. We do this first because:
        // - It preserves behaviour for classic patterns like `/[[]/` where `[` inside a class is a
        //   literal character (not nested), and
        // - It gives us the literal flags suffix so we can detect `v` and optionally do a nested-class
        //   aware scan.
        //
        // Budget scanning for the closing `/` so enormous regexp literals can't monopolize CPU.
        const TICK_EVERY: usize = 1024;
        let mut escaped = false;
        let mut class_depth: usize = 0;
        let mut end_pat: Option<usize> = None;
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
            '[' if class_depth == 0 => class_depth = 1,
            ']' if class_depth > 0 => class_depth = 0,
            '/' if class_depth == 0 => {
              end_pat = Some(i);
              break;
            }
            _ => {}
          }
        }
        let Some(mut end_pat) = end_pat else {
          let err_obj = crate::error_object::new_syntax_error_object(
            scope,
            &intr,
            "Unterminated regular expression literal",
          )?;
          return Err(VmError::Throw(err_obj));
        };

        // If the literal flags contain `v`, do a nested character-class aware scan to avoid
        // mis-detecting the closing `/` for modern RegExp set notation (e.g. `/[[0-9]\\/]/v`).
        //
        // If the nested scan fails to find a terminator (e.g. the pattern contains unbalanced `[`/`]`
        // pairs under nesting semantics), fall back to the classic terminator position; the RegExp
        // constructor will report the pattern error.
        let mut has_v_flag = false;
        for (i, b) in literal[end_pat + 1..].bytes().enumerate() {
          // Avoid ticking on the first iteration so short flag strings don't effectively double-charge
          // fuel (the surrounding expression evaluation already ticks).
          if i != 0 && i % TICK_EVERY == 0 {
            self.vm.tick()?;
          }
          if b == b'v' {
            has_v_flag = true;
            break;
          }
        }
        if has_v_flag {
          let mut escaped = false;
          let mut depth: usize = 0;
          let mut nested_end: Option<usize> = None;
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
              '[' => depth = depth.saturating_add(1),
              ']' if depth > 0 => depth -= 1,
              '/' if depth == 0 => {
                nested_end = Some(i);
                break;
              }
              _ => {}
            }
          }
          if let Some(i) = nested_end {
            end_pat = i;
          }
        }

        let pattern = &literal[1..end_pat];
        let flags = &literal[end_pat + 1..];

        let mut scope = scope.reborrow();
        let pattern_s = scope.alloc_string(pattern)?;
        scope.push_root(Value::String(pattern_s))?;
        let flags_s = scope.alloc_string(flags)?;
        scope.push_root(Value::String(flags_s))?;

        let ctor = Value::Object(intr.regexp_constructor());
        self.vm.construct_with_host_and_hooks(
          &mut *self.host,
          &mut scope,
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
            // Super References are not deletable (ECMA-262 `delete` runtime semantics).
            //
            // Evaluating a super property reference requires an initialized `this` binding; in
            // derived constructors before `super()`, that evaluation throws first and must not be
            // masked by the delete semantics.
            //
            // For computed super property references, the key expression (including `ToPropertyKey`)
            // is evaluated before throwing *only after* `this` is initialized.
            let object_expr = self.get_expr(body, member.object)?;
            if matches!(object_expr.kind, hir_js::ExprKind::Super) {
              if self.derived_constructor && !self.this_initialized {
                return Err(throw_reference_error(
                  self.vm,
                  scope,
                  "Must call super constructor in derived class before accessing 'this'",
                )?);
              }
              if let hir_js::ObjectKey::Computed(expr_id) = &member.property {
                let member_value = self.eval_expr(scope, body, *expr_id)?;
                let mut key_scope = scope.reborrow();
                key_scope.push_root(member_value)?;
                let _ =
                  key_scope.to_property_key(self.vm, &mut *self.host, &mut *self.hooks, member_value)?;
              }
              return Err(throw_reference_error(
                self.vm,
                scope,
                "Cannot delete a super property",
              )?);
            }

            // Optional chaining delete (`delete o?.x`) short-circuits to `true` if the base is
            // nullish and does not evaluate the property expression.
            let base = match self.eval_chain_base(scope, body, member.object)? {
              OptionalChainEval::Value(v) => v,
              // `delete a?.b.c` is a delete of an optional chain continuation: if the chain short
              // circuits, the operand is not a reference and `delete` returns true.
              OptionalChainEval::ShortCircuit => return Ok(Value::Bool(true)),
            };
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

        // `~` uses `ToNumeric` and preserves BigInt.
        //
        // Spec: https://tc39.es/ecma262/#sec-bitwise-not-operator
        let mut not_scope = scope.reborrow();
        not_scope.push_root(v)?;
        let num = self.to_numeric(&mut not_scope, v)?;
        Ok(match num {
          NumericValue::Number(n) => Value::Number((!to_int32(n)) as f64),
          NumericValue::BigInt(b) => {
            let out = {
              let bi = not_scope.heap().get_bigint(b)?;
              bi.bitwise_not()?
            };
            let out = not_scope.alloc_bigint(out)?;
            Value::BigInt(out)
          }
        })
      }
      hir_js::UnaryOp::Plus => {
        let v = self.eval_expr(scope, body, expr)?;
        // Full `ToNumber` requires `ToPrimitive`, which can invoke user code.
        Ok(Value::Number(
          scope.to_number(self.vm, &mut *self.host, &mut *self.hooks, v)?,
        ))
      }
      hir_js::UnaryOp::Minus => {
        // Unary `-` uses `ToNumeric` and preserves BigInt.
        let v = self.eval_expr(scope, body, expr)?;
        let mut neg_scope = scope.reborrow();
        neg_scope.push_root(v)?;
        let num = self.to_numeric(&mut neg_scope, v)?;
        Ok(match num {
          NumericValue::Number(n) => Value::Number(-n),
          NumericValue::BigInt(b) => {
            let out = {
              let bi = neg_scope.heap().get_bigint(b)?;
              bi.neg()?
            };
            let out = neg_scope.alloc_bigint(out)?;
            Value::BigInt(out)
          }
        })
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
      hir_js::UpdateOp::Increment => 1i8,
      hir_js::UpdateOp::Decrement => -1i8,
    };

    let target_expr = self.get_expr(body, expr)?;
    match &target_expr.kind {
      hir_js::ExprKind::Ident(name_id) => {
        let name = self.resolve_name(*name_id)?;

        // Use a nested scope so any temporary roots created by `ToPrimitive`/BigInt arithmetic are
        // popped before returning to the caller.
        let mut update_scope = scope.reborrow();

        // Root the name as `ResolvedBinding` borrows it and `env` operations can invoke user code.
        let reference = self.env.resolve_binding_reference(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut update_scope,
          name.as_str(),
        )?;

        let old_value = self.get_value_from_resolved_binding(&mut update_scope, reference)?;
        let old_numeric = self.to_numeric(&mut update_scope, old_value)?;
        let (old_out, new_value) = match old_numeric {
          NumericValue::Number(n) => {
            let new_n = n + f64::from(delta);
            (Value::Number(n), Value::Number(new_n))
          }
          NumericValue::BigInt(b) => {
            let delta_bigint = crate::JsBigInt::from_i128(delta as i128)?;
            let out = {
              let bi = update_scope.heap().get_bigint(b)?;
              bi.add(&delta_bigint)?
            };
            let out = update_scope.alloc_bigint(out)?;
            (Value::BigInt(b), Value::BigInt(out))
          }
        };

        // Assignment can invoke user code (e.g. setters via `with` envs). Root the value first.
        update_scope.push_root(new_value)?;
        self.env.set_resolved_binding(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut update_scope,
          reference,
          new_value,
          self.strict,
        )?;

        if prefix {
          Ok(new_value)
        } else {
          Ok(old_out)
        }
      }
      hir_js::ExprKind::Member(member) => {
        let base = self.eval_expr(scope, body, member.object)?;

        let mut update_scope = scope.reborrow();
        // Root the original base across `ToObject`, key allocation, `[[Get]]` and `[[Set]]`.
        update_scope.push_root(base)?;

        let key = self.eval_object_key(&mut update_scope, body, &member.property)?;
        root_property_key(&mut update_scope, key)?;

        let obj = update_scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base)?;
        update_scope.push_root(Value::Object(obj))?;

        let receiver = base;
        let old_value = update_scope.get_with_host_and_hooks(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          obj,
          key,
          receiver,
        )?;

        let old_numeric = self.to_numeric(&mut update_scope, old_value)?;
        let (old_out, new_value) = match old_numeric {
          NumericValue::Number(n) => {
            let new_n = n + f64::from(delta);
            (Value::Number(n), Value::Number(new_n))
          }
          NumericValue::BigInt(b) => {
            let delta_bigint = crate::JsBigInt::from_i128(delta as i128)?;
            let out = {
              let bi = update_scope.heap().get_bigint(b)?;
              bi.add(&delta_bigint)?
            };
            let out = update_scope.alloc_bigint(out)?;
            (Value::BigInt(b), Value::BigInt(out))
          }
        };

        // Root the new value in case `[[Set]]` invokes accessors and triggers GC.
        update_scope.push_root(new_value)?;

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
          Ok(old_out)
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

    // Root the LHS across RHS evaluation: evaluating the RHS can allocate and trigger GC, and most
    // binary operators need the LHS value afterwards (including when later coercions invoke user
    // code).
    let mut scope = scope.reborrow();
    scope.push_root(l)?;

    let r = self.eval_expr(&mut scope, body, right)?;
    scope.push_root(r)?;

    let scope = &mut scope;

    match op {
      hir_js::BinaryOp::Add => {
        self.addition_operator(scope, l, r)
      }
      hir_js::BinaryOp::Subtract => {
        // Use `ToNumeric` (BigInt support).
        let mut scope = scope.reborrow();
        scope.push_roots(&[l, r])?;
        let ln = self.to_numeric(&mut scope, l)?;
        let rn = self.to_numeric(&mut scope, r)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => Ok(Value::Number(a - b)),
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              a.sub(b)?
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::BinaryOp::Multiply => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[l, r])?;
        let ln = self.to_numeric(&mut scope, l)?;
        let rn = self.to_numeric(&mut scope, r)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => Ok(Value::Number(a * b)),
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              let mut tick = || self.vm.tick();
              a.mul_with_tick(b, &mut tick)?
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::BinaryOp::Divide => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[l, r])?;
        let ln = self.to_numeric(&mut scope, l)?;
        let rn = self.to_numeric(&mut scope, r)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => Ok(Value::Number(a / b)),
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              if b.is_zero() {
                return Err(VmError::RangeError("Division by zero"));
              }
              let mut tick = || self.vm.tick();
              let (q, _r) = a.div_mod_with_tick(b, &mut tick)?;
              q
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::BinaryOp::Remainder => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[l, r])?;
        let ln = self.to_numeric(&mut scope, l)?;
        let rn = self.to_numeric(&mut scope, r)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => Ok(Value::Number(a % b)),
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              if b.is_zero() {
                return Err(VmError::RangeError("Division by zero"));
              }
              let mut tick = || self.vm.tick();
              let (_q, r) = a.div_mod_with_tick(b, &mut tick)?;
              r
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::BinaryOp::Exponent => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[l, r])?;
        let base = self.to_numeric(&mut scope, l)?;
        let exp = self.to_numeric(&mut scope, r)?;
        match (base, exp) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            Ok(Value::Number(crate::ops::number_exponentiate(a, b)))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              if b.is_negative() {
                return Err(VmError::RangeError("BigInt exponent must be >= 0"));
              }
              let mut tick = || self.vm.tick();
              a.pow_with_tick(b, &mut tick)?
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::BinaryOp::ShiftLeft => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[l, r])?;
        let ln = self.to_numeric(&mut scope, l)?;
        let rn = self.to_numeric(&mut scope, r)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            let shift = to_uint32(b) & 0x1f;
            Ok(Value::Number(to_int32(a).wrapping_shl(shift) as f64))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              // Match interpreter semantics: negative shift counts reverse direction and extremely
              // large magnitudes saturate to `u64::MAX`.
              let (shift_negative, shift) = bigint_shift_count(b);
              if shift_negative {
                a.shr(shift)?
              } else {
                a.shl(shift)?
              }
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::BinaryOp::ShiftRight => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[l, r])?;
        let ln = self.to_numeric(&mut scope, l)?;
        let rn = self.to_numeric(&mut scope, r)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            let shift = to_uint32(b) & 0x1f;
            Ok(Value::Number(to_int32(a).wrapping_shr(shift) as f64))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              // Match interpreter semantics: negative shift counts reverse direction and extremely
              // large magnitudes saturate to `u64::MAX`.
              let (shift_negative, shift) = bigint_shift_count(b);
              if shift_negative {
                a.shl(shift)?
              } else {
                a.shr(shift)?
              }
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::BinaryOp::ShiftRightUnsigned => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[l, r])?;
        let ln = self.to_numeric(&mut scope, l)?;
        let rn = self.to_numeric(&mut scope, r)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            let shift = to_uint32(b) & 0x1f;
            Ok(Value::Number((to_uint32(a) >> shift) as f64))
          }
          (NumericValue::BigInt(_), NumericValue::BigInt(_)) => Err(VmError::TypeError(
            "BigInt does not support unsigned right shift",
          )),
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::BinaryOp::BitOr => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[l, r])?;
        let ln = self.to_numeric(&mut scope, l)?;
        let rn = self.to_numeric(&mut scope, r)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            Ok(Value::Number((to_int32(a) | to_int32(b)) as f64))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              a.bitwise_or(b)?
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::BinaryOp::BitAnd => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[l, r])?;
        let ln = self.to_numeric(&mut scope, l)?;
        let rn = self.to_numeric(&mut scope, r)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            Ok(Value::Number((to_int32(a) & to_int32(b)) as f64))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              a.bitwise_and(b)?
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::BinaryOp::BitXor => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[l, r])?;
        let ln = self.to_numeric(&mut scope, l)?;
        let rn = self.to_numeric(&mut scope, r)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            Ok(Value::Number((to_int32(a) ^ to_int32(b)) as f64))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              a.bitwise_xor(b)?
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::BinaryOp::Equality => Ok(Value::Bool(self.abstract_equality_comparison(scope, l, r)?)),
      hir_js::BinaryOp::Inequality => Ok(Value::Bool(!self.abstract_equality_comparison(scope, l, r)?)),
      hir_js::BinaryOp::StrictEquality => Ok(Value::Bool(self.strict_equality_comparison(scope, l, r)?)),
      hir_js::BinaryOp::StrictInequality => Ok(Value::Bool(!self.strict_equality_comparison(scope, l, r)?)),
      hir_js::BinaryOp::LessThan => {
        Ok(Value::Bool(
          self
            .abstract_relational_comparison(scope, l, r, /* left_first */ true)?
            .unwrap_or(false),
        ))
      }
      hir_js::BinaryOp::LessEqual => {
        Ok(Value::Bool(match self.abstract_relational_comparison(scope, r, l, /* left_first */ false)? {
          None => false,
          Some(r) => !r,
        }))
      }
      hir_js::BinaryOp::GreaterThan => {
        Ok(Value::Bool(
          self
            .abstract_relational_comparison(scope, r, l, /* left_first */ false)?
            .unwrap_or(false),
        ))
      }
      hir_js::BinaryOp::GreaterEqual => {
        Ok(Value::Bool(match self.abstract_relational_comparison(scope, l, r, /* left_first */ true)? {
          None => false,
          Some(r) => !r,
        }))
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

  /// ECMA-262 Abstract Relational Comparison.
  ///
  /// Returns `Ok(None)` for the spec's `undefined` result (e.g. when comparing NaN).
  fn abstract_relational_comparison(
    &mut self,
    scope: &mut Scope<'_>,
    x: Value,
    y: Value,
    left_first: bool,
  ) -> Result<Option<bool>, VmError> {
    // Root inputs for the duration of the comparison: `ToPrimitive`/`ToNumeric` can allocate.
    let mut scope = scope.reborrow();
    scope.push_roots(&[x, y])?;

    // 1. ToPrimitive, hint Number (order depends on `left_first`).
    let (px, py) = if left_first {
      let px = scope.to_primitive(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        x,
        ToPrimitiveHint::Number,
      )?;
      scope.push_root(px)?;
      let py = scope.to_primitive(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        y,
        ToPrimitiveHint::Number,
      )?;
      scope.push_root(py)?;
      (px, py)
    } else {
      let py = scope.to_primitive(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        y,
        ToPrimitiveHint::Number,
      )?;
      scope.push_root(py)?;
      let px = scope.to_primitive(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        x,
        ToPrimitiveHint::Number,
      )?;
      scope.push_root(px)?;
      (px, py)
    };

    // 2. String/string => lexicographic code-unit comparison.
    if let (Value::String(sx), Value::String(sy)) = (px, py) {
      let a = scope.heap().get_string(sx)?.as_code_units();
      let b = scope.heap().get_string(sy)?.as_code_units();
      let ord = crate::tick::code_units_cmp_with_ticks(a, b, || self.vm.tick())?;
      return Ok(Some(ord == Ordering::Less));
    }

    // 2.b. BigInt/string => parse the string as a BigInt (StringToBigInt).
    match (px, py) {
      (Value::BigInt(a), Value::String(bs)) => {
        let mut tick = || self.vm.tick();
        let Some(bi) = string_to_bigint(scope.heap(), bs, &mut tick)? else {
          return Ok(None);
        };
        return Ok(Some(scope.heap().get_bigint(a)? < &bi));
      }
      (Value::String(as_), Value::BigInt(b)) => {
        let mut tick = || self.vm.tick();
        let Some(ai) = string_to_bigint(scope.heap(), as_, &mut tick)? else {
          return Ok(None);
        };
        return Ok(Some(&ai < scope.heap().get_bigint(b)?));
      }
      _ => {}
    }

    // 3. Otherwise => ToNumeric then numeric comparison.
    let nx = self.to_numeric(&mut scope, px)?;
    let ny = self.to_numeric(&mut scope, py)?;
    Ok(match (nx, ny) {
      (NumericValue::Number(a), NumericValue::Number(b)) => {
        if a.is_nan() || b.is_nan() {
          None
        } else {
          Some(a < b)
        }
      }
      (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
        let a = scope.heap().get_bigint(a)?;
        let b = scope.heap().get_bigint(b)?;
        Some(a < b)
      }
      (NumericValue::BigInt(a), NumericValue::Number(b)) => {
        bigint_compare_number(scope.heap(), a, b)?.map(|ord| ord == Ordering::Less)
      }
      (NumericValue::Number(a), NumericValue::BigInt(b)) => {
        bigint_compare_number(scope.heap(), b, a)?.map(|ord| ord == Ordering::Greater)
      }
    })
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
        // Spec note: plain assignment targets (identifiers / members) evaluate the reference (and
        // therefore computed property keys / `with` binding resolution) before evaluating the RHS.
        //
        // Destructuring assignment patterns evaluate the RHS first.
        let pat = self.get_pat(body, target)?;
        match &pat.kind {
          hir_js::PatKind::Ident(_) | hir_js::PatKind::AssignTarget(_) => {
            let reference = self.eval_assignment_reference(scope, body, target)?;

            let mut scope = scope.reborrow();
            self.root_assignment_reference(&mut scope, &reference)?;

            let v = self.eval_expr(&mut scope, body, value)?;
            scope.push_root(v)?;
            self.maybe_set_anonymous_function_name_for_assignment(&mut scope, &reference, v)?;
            self.put_value_to_assignment_reference(&mut scope, &reference, v)?;
            Ok(v)
          }
          _ => {
            // Root the RHS across pattern assignment evaluation in case it allocates and triggers GC.
            let mut scope = scope.reborrow();
            let v = self.eval_expr(&mut scope, body, value)?;
            scope.push_root(v)?;
            self.assign_to_pat(&mut scope, body, target, v)?;
            Ok(v)
          }
        }
      }
      hir_js::AssignOp::AddAssign
      | hir_js::AssignOp::SubAssign
      | hir_js::AssignOp::MulAssign
      | hir_js::AssignOp::DivAssign
      | hir_js::AssignOp::RemAssign
      | hir_js::AssignOp::ExponentAssign
      | hir_js::AssignOp::ShiftLeftAssign
      | hir_js::AssignOp::ShiftRightAssign
      | hir_js::AssignOp::ShiftRightUnsignedAssign
      | hir_js::AssignOp::BitOrAssign
      | hir_js::AssignOp::BitAndAssign
      | hir_js::AssignOp::BitXorAssign => self.eval_compound_assignment(scope, body, op, target, value),
      hir_js::AssignOp::LogicalAndAssign
      | hir_js::AssignOp::LogicalOrAssign
      | hir_js::AssignOp::NullishAssign => self.eval_logical_assignment(scope, body, op, target, value),
    }
  }

  fn eval_assignment_reference(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    target: hir_js::PatId,
  ) -> Result<AssignmentReference, VmError> {
    let pat = self.get_pat(body, target)?;
    match &pat.kind {
      hir_js::PatKind::Ident(name_id) => self.eval_assignment_binding_reference(scope, *name_id),
      hir_js::PatKind::AssignTarget(expr_id) => {
        let target_expr = self.get_expr(body, *expr_id)?;
        match &target_expr.kind {
          hir_js::ExprKind::Ident(name_id) => self.eval_assignment_binding_reference(scope, *name_id),
          hir_js::ExprKind::Member(member) => self.eval_assignment_member_reference(scope, body, member),
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

  fn eval_assignment_binding_reference(
    &mut self,
    scope: &mut Scope<'_>,
    name_id: hir_js::NameId,
  ) -> Result<AssignmentReference, VmError> {
    let name = self.resolve_name(name_id)?;
    let reference = self.env.resolve_binding_reference(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      scope,
      name.as_str(),
    )?;
    let owned = match reference {
      ResolvedBinding::Declarative { env, .. } => BindingReference::Declarative { env, name },
      ResolvedBinding::Object {
        binding_object, ..
      } => BindingReference::Object {
        binding_object,
        name,
      },
      ResolvedBinding::GlobalProperty { .. } => BindingReference::GlobalProperty { name },
      ResolvedBinding::Unresolvable { .. } => BindingReference::Unresolvable { name },
    };
    Ok(AssignmentReference::Binding(owned))
  }

  fn eval_assignment_member_reference(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    member: &hir_js::MemberExpr,
  ) -> Result<AssignmentReference, VmError> {
    if member.optional {
      // Optional chaining is never a valid assignment target; this should be rejected by early
      // errors before execution begins.
      return Err(VmError::InvariantViolation(
        "optional chaining used in assignment target",
      ));
    }

    let base = self.eval_expr(scope, body, member.object)?;

    let mut scope = scope.reborrow();
    // Root the base across key evaluation: `ToPropertyKey` can invoke user code and allocate.
    scope.push_root(base)?;
    let key = self.eval_object_key(&mut scope, body, &member.property)?;
    root_property_key(&mut scope, key)?;

    // `RequireObjectCoercible(base)` happens during reference evaluation, *before* the RHS is
    // evaluated. For computed member expressions, the key has already been evaluated at this point
    // (per spec).
    if matches!(base, Value::Null | Value::Undefined) {
      return Err(VmError::TypeError("Cannot convert undefined or null to object"));
    }

    Ok(AssignmentReference::Property { base, key })
  }

  fn root_assignment_reference(
    &self,
    scope: &mut Scope<'_>,
    reference: &AssignmentReference,
  ) -> Result<(), VmError> {
    let AssignmentReference::Property { base, key } = reference else {
      return Ok(());
    };
    // Root both base and key together so `push_roots` can treat them as extra roots if growing the
    // root stack triggers a GC.
    let roots = [
      *base,
      match key {
        PropertyKey::String(s) => Value::String(*s),
        PropertyKey::Symbol(s) => Value::Symbol(*s),
      },
    ];
    scope.push_roots(&roots)?;
    Ok(())
  }

  fn put_value_to_assignment_reference(
    &mut self,
    scope: &mut Scope<'_>,
    reference: &AssignmentReference,
    value: Value,
  ) -> Result<(), VmError> {
    match reference {
      AssignmentReference::Binding(reference) => self.env.set_resolved_binding(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        scope,
        reference.as_resolved_binding(),
        value,
        self.strict,
      ),
      AssignmentReference::Property { base, key } => {
        let mut set_scope = scope.reborrow();
        self.root_assignment_reference(&mut set_scope, reference)?;
        // Root `value` across `ToObject(base)` in case boxing triggers a GC.
        set_scope.push_root(value)?;
        let obj = set_scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, *base)?;
        set_scope.push_root(Value::Object(obj))?;

        let receiver = *base;
        let ok = crate::spec_ops::internal_set_with_host_and_hooks(
          self.vm,
          &mut set_scope,
          &mut *self.host,
          &mut *self.hooks,
          obj,
          *key,
          value,
          receiver,
        )?;
        if ok {
          Ok(())
        } else if self.strict {
          Err(VmError::TypeError("Cannot assign to read-only property"))
        } else {
          Ok(())
        }
      }
    }
  }

  fn maybe_set_anonymous_function_name_for_assignment(
    &mut self,
    scope: &mut Scope<'_>,
    reference: &AssignmentReference,
    value: Value,
  ) -> Result<(), VmError> {
    let Value::Object(func_obj) = value else {
      return Ok(());
    };

    // `SetFunctionName` only applies to actual Function objects. Callable Proxies are callable, but
    // are not function objects and should not have their `name` mutated.
    let (current_name, is_native_non_constructable) = match scope.heap().get_function(func_obj) {
      Ok(f) => (
        f.name,
        matches!(f.call, crate::function::CallHandler::Native(_)) && f.construct.is_none(),
      ),
      Err(VmError::NotCallable) => return Ok(()),
      Err(err) => return Err(err),
    };

    // Name inference only applies to "anonymous function definitions" (ECMA-262), which excludes
    // anonymous built-in/native functions such as Promise combinator element callbacks.
    //
    // `vm-js` represents user-defined class constructors as native functions (so they can throw
    // when called without `new`), so keep name inference enabled for constructable native
    // functions.
    if is_native_non_constructable {
      return Ok(());
    }
    if !scope
      .heap()
      .get_string(current_name)?
      .as_code_units()
      .is_empty()
    {
      return Ok(());
    }

    let key = match reference {
      AssignmentReference::Binding(name_ref) => {
        // Root the allocated key string: `set_function_name` may allocate and trigger GC while
        // pushing its own roots.
        let name_s = scope.alloc_string(name_ref.name())?;
        scope.push_root(Value::String(name_s))?;
        PropertyKey::String(name_s)
      }
      AssignmentReference::Property { key, .. } => *key,
    };

    crate::function_properties::set_function_name(scope, func_obj, key, None)?;
    Ok(())
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
            let base = self.eval_expr(scope, body, member.object)?;

            let mut scope = scope.reborrow();
            // Root base across `ToObject`, key evaluation, `[[Get]]` and `[[Set]]`. Compound
            // assignment evaluates the property reference once and then performs both a get and set.
            scope.push_root(base)?;

            let key = self.eval_object_key(&mut scope, body, &member.property)?;
            root_property_key(&mut scope, key)?;

            let obj = scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base)?;
            scope.push_root(Value::Object(obj))?;
            let receiver = base;

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

  fn eval_logical_assignment(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    op: hir_js::AssignOp,
    target: hir_js::PatId,
    value: hir_js::ExprId,
  ) -> Result<Value, VmError> {
    debug_assert!(matches!(
      op,
      hir_js::AssignOp::LogicalAndAssign
        | hir_js::AssignOp::LogicalOrAssign
        | hir_js::AssignOp::NullishAssign
    ));

    let should_assign = |scope: &Scope<'_>, left: Value| -> Result<bool, VmError> {
      Ok(match op {
        hir_js::AssignOp::LogicalAndAssign => scope.heap().to_boolean(left)?,
        hir_js::AssignOp::LogicalOrAssign => !scope.heap().to_boolean(left)?,
        hir_js::AssignOp::NullishAssign => matches!(left, Value::Null | Value::Undefined),
        _ => false,
      })
    };

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
        if !should_assign(&scope, left)? {
          return Ok(left);
        }

        // Root `left` across RHS evaluation and the subsequent assignment.
        scope.push_root(left)?;
        let right = self.eval_expr(&mut scope, body, value)?;
        scope.push_root(right)?;
        maybe_set_anonymous_function_name(&mut scope, right, name.as_str())?;
        self.env.set_resolved_binding(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut scope,
          reference,
          right,
          self.strict,
        )?;
        Ok(right)
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
            if !should_assign(&scope, left)? {
              return Ok(left);
            }

            scope.push_root(left)?;
            let right = self.eval_expr(&mut scope, body, value)?;
            scope.push_root(right)?;
            maybe_set_anonymous_function_name(&mut scope, right, name.as_str())?;
            self.env.set_resolved_binding(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              &mut scope,
              reference,
              right,
              self.strict,
            )?;
            Ok(right)
          }
          hir_js::ExprKind::Member(member) => {
            if member.optional {
              return Err(VmError::InvariantViolation(
                "optional chaining used in assignment target",
              ));
            }

            let base = self.eval_expr(scope, body, member.object)?;
            let mut scope = scope.reborrow();
            scope.push_root(base)?;

            let key = self.eval_object_key(&mut scope, body, &member.property)?;
            root_property_key(&mut scope, key)?;

            let obj = scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base)?;
            scope.push_root(Value::Object(obj))?;
            let receiver = base;

            let left = scope.get_with_host_and_hooks(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              obj,
              key,
              receiver,
            )?;
            if !should_assign(&scope, left)? {
              return Ok(left);
            }

            scope.push_root(left)?;
            let right = self.eval_expr(&mut scope, body, value)?;
            scope.push_root(right)?;
            let reference = AssignmentReference::Property { base, key };
            self.maybe_set_anonymous_function_name_for_assignment(&mut scope, &reference, right)?;

            let ok = crate::spec_ops::internal_set_with_host_and_hooks(
              self.vm,
              &mut scope,
              &mut *self.host,
              &mut *self.hooks,
              obj,
              key,
              right,
              receiver,
            )?;
            if !ok && self.strict {
              return Err(VmError::TypeError("Cannot assign to read-only property"));
            }
            Ok(right)
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
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        let ln = self.to_numeric(&mut scope, left)?;
        let rn = self.to_numeric(&mut scope, right)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => Ok(Value::Number(a - b)),
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              a.sub(b)?
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::AssignOp::MulAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        let ln = self.to_numeric(&mut scope, left)?;
        let rn = self.to_numeric(&mut scope, right)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => Ok(Value::Number(a * b)),
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              let mut tick = || self.vm.tick();
              a.mul_with_tick(b, &mut tick)?
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::AssignOp::DivAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        let ln = self.to_numeric(&mut scope, left)?;
        let rn = self.to_numeric(&mut scope, right)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => Ok(Value::Number(a / b)),
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              if b.is_zero() {
                return Err(VmError::RangeError("Division by zero"));
              }
              let mut tick = || self.vm.tick();
              let (q, _r) = a.div_mod_with_tick(b, &mut tick)?;
              q
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::AssignOp::RemAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        let ln = self.to_numeric(&mut scope, left)?;
        let rn = self.to_numeric(&mut scope, right)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => Ok(Value::Number(a % b)),
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              if b.is_zero() {
                return Err(VmError::RangeError("Division by zero"));
              }
              let mut tick = || self.vm.tick();
              let (_q, r) = a.div_mod_with_tick(b, &mut tick)?;
              r
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::AssignOp::ExponentAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        let base = self.to_numeric(&mut scope, left)?;
        let exp = self.to_numeric(&mut scope, right)?;
        match (base, exp) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            Ok(Value::Number(crate::ops::number_exponentiate(a, b)))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              if b.is_negative() {
                return Err(VmError::RangeError("BigInt exponent must be >= 0"));
              }
              let mut tick = || self.vm.tick();
              a.pow_with_tick(b, &mut tick)?
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::AssignOp::ShiftLeftAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        let ln = self.to_numeric(&mut scope, left)?;
        let rn = self.to_numeric(&mut scope, right)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            let shift = to_uint32(b) & 0x1f;
            Ok(Value::Number(to_int32(a).wrapping_shl(shift) as f64))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              let (shift_negative, shift) = bigint_shift_count(b);
              if shift_negative {
                a.shr(shift)?
              } else {
                a.shl(shift)?
              }
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::AssignOp::ShiftRightAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        let ln = self.to_numeric(&mut scope, left)?;
        let rn = self.to_numeric(&mut scope, right)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            let shift = to_uint32(b) & 0x1f;
            Ok(Value::Number(to_int32(a).wrapping_shr(shift) as f64))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              let (shift_negative, shift) = bigint_shift_count(b);
              if shift_negative {
                a.shl(shift)?
              } else {
                a.shr(shift)?
              }
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::AssignOp::ShiftRightUnsignedAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        let ln = self.to_numeric(&mut scope, left)?;
        let rn = self.to_numeric(&mut scope, right)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            let shift = to_uint32(b) & 0x1f;
            Ok(Value::Number(to_uint32(a).wrapping_shr(shift) as f64))
          }
          (NumericValue::BigInt(_), NumericValue::BigInt(_)) => Err(VmError::TypeError(
            "BigInt does not support unsigned right shift",
          )),
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::AssignOp::BitOrAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        let ln = self.to_numeric(&mut scope, left)?;
        let rn = self.to_numeric(&mut scope, right)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            Ok(Value::Number((to_int32(a) | to_int32(b)) as f64))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              a.bitwise_or(b)?
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::AssignOp::BitAndAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        let ln = self.to_numeric(&mut scope, left)?;
        let rn = self.to_numeric(&mut scope, right)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            Ok(Value::Number((to_int32(a) & to_int32(b)) as f64))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              a.bitwise_and(b)?
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
      }
      hir_js::AssignOp::BitXorAssign => {
        let mut scope = scope.reborrow();
        scope.push_roots(&[left, right])?;
        let ln = self.to_numeric(&mut scope, left)?;
        let rn = self.to_numeric(&mut scope, right)?;
        match (ln, rn) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            Ok(Value::Number((to_int32(a) ^ to_int32(b)) as f64))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = {
              let a = scope.heap().get_bigint(a)?;
              let b = scope.heap().get_bigint(b)?;
              a.bitwise_xor(b)?
            };
            let out = scope.alloc_bigint(out)?;
            Ok(Value::BigInt(out))
          }
          _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
        }
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
      let left_num = self.to_numeric(&mut scope, lp)?;
      let right_num = self.to_numeric(&mut scope, rp)?;
      match (left_num, right_num) {
        (NumericValue::Number(a), NumericValue::Number(b)) => Ok(Value::Number(a + b)),
        (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
          let out = {
            let a = scope.heap().get_bigint(a)?;
            let b = scope.heap().get_bigint(b)?;
            a.add(b)?
          };
          let out = scope.alloc_bigint(out)?;
          Ok(Value::BigInt(out))
        }
        _ => Err(VmError::TypeError("Cannot mix BigInt and other types")),
      }
    }
  }

  fn to_numeric(&mut self, scope: &mut Scope<'_>, value: Value) -> Result<NumericValue, VmError> {
    // ECMA-262 `ToNumeric`: ToPrimitive (hint Number), then return BigInt directly or convert to
    // Number.
    let prim = scope.to_primitive(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      value,
      ToPrimitiveHint::Number,
    )?;
    match prim {
      Value::BigInt(b) => {
        // Root the BigInt handle for the duration of `scope`: subsequent arithmetic can allocate.
        scope.push_root(Value::BigInt(b))?;
        Ok(NumericValue::BigInt(b))
      }
      other => Ok(NumericValue::Number(
        scope.heap_mut().to_number_with_tick(other, || self.vm.tick())?,
      )),
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
        let msg = crate::fallible_format::try_format_error_message("", name, " is not defined")?;
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
        maybe_set_anonymous_function_name(scope, value, name.as_str())?;
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
            maybe_set_anonymous_function_name(scope, value, name.as_str())?;
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
      _ => self.assign_pattern(scope, body, pat_id, value),
    }
  }

  fn assign_to_property_key(
    &mut self,
    scope: &mut Scope<'_>,
    base: Value,
    key: PropertyKey,
    value: Value,
  ) -> Result<(), VmError> {
    // Root inputs across `ToObject` + `[[Set]]`, which can allocate and invoke user code.
    let mut set_scope = scope.reborrow();
    let key_root = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    set_scope.push_roots(&[base, key_root, value])?;

    // Spec: `PutValue` uses `ToObject` and then calls `[[Set]]` with the *original* base value as
    // the receiver.
    let obj = set_scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base)?;
    set_scope.push_root(Value::Object(obj))?;

    let receiver = base;
    let ok = crate::spec_ops::internal_set_with_host_and_hooks(
      self.vm,
      &mut set_scope,
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
      Ok(())
    }
  }

  fn assign_pattern(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    pat_id: hir_js::PatId,
    value: Value,
  ) -> Result<(), VmError> {
    // Keep temporary roots local to this assignment operation.
    let mut scope = scope.reborrow();
    let value = scope.push_root(value)?;

    let pat = self.get_pat(body, pat_id)?;
    match &pat.kind {
      hir_js::PatKind::Array(arr) => self.assign_array_pattern(&mut scope, body, arr, value),
      hir_js::PatKind::Object(obj) => self.assign_object_pattern(&mut scope, body, obj, value),
      hir_js::PatKind::Assign {
        target,
        default_value,
      } => {
        let v = if matches!(value, Value::Undefined) {
          self.eval_expr(&mut scope, body, *default_value)?
        } else {
          value
        };
        self.assign_to_pat(&mut scope, body, *target, v)
      }
      hir_js::PatKind::Rest(inner) => self.assign_to_pat(&mut scope, body, **inner, value),
      // `assign_to_pat` handles identifier/AssignTarget forms.
      hir_js::PatKind::Ident(_) | hir_js::PatKind::AssignTarget(_) => {
        self.assign_to_pat(&mut scope, body, pat_id, value)
      }
    }
  }

  fn assign_object_pattern(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    pat: &hir_js::ObjectPat,
    value: Value,
  ) -> Result<(), VmError> {
    let names = self.hir().names.clone();

    // Root the original RHS value across boxing: `ToObject` can allocate and therefore trigger GC.
    let src_value = scope.push_root(value)?;
    let obj = scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, src_value)?;
    scope.push_root(Value::Object(obj))?;

    let mut excluded: Vec<PropertyKey> = Vec::new();
    if pat.rest.is_some() {
      excluded
        .try_reserve_exact(pat.props.len())
        .map_err(|_| VmError::OutOfMemory)?;
    }

    for prop in &pat.props {
      self.vm.tick()?;
      let key = self.eval_object_key(scope, body, &prop.key)?;
      if pat.rest.is_some() {
        root_property_key(scope, key)?;
        excluded.push(key);
      }

      let mut prop_scope = scope.reborrow();
      root_property_key(&mut prop_scope, key)?;

      // --- Assignment target evaluation order (ECMA-262 `KeyedDestructuringAssignmentEvaluation`) ---
      //
      // For destructuring *assignment* (not binding), the spec evaluates assignment targets
      // (including `ResolveBinding` and property-reference base + key expressions) before calling
      // `GetV(value, propertyKey)`.
      //
      // Additionally, for computed member targets (`obj[expr]`), the `ToPropertyKey` conversion is
      // delayed until `PutValue`, after `GetV` / default evaluation.
      enum PropTarget<'a> {
        Binding(ResolvedBinding<'a>),
        Member { base: Value, key: PropertyKey },
        ComputedMember { base: Value, key_value: Value },
        Pat(hir_js::PatId),
      }
      let mut target = PropTarget::Pat(prop.value);
      {
        let value_pat = self.get_pat(body, prop.value)?;
        match value_pat.kind {
          hir_js::PatKind::Ident(name_id) => {
            let name = names
              .resolve(name_id)
              .ok_or(VmError::InvariantViolation(
                "hir name id missing from interner",
              ))?;
            let binding = self.env.resolve_binding_reference(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              &mut prop_scope,
              name,
            )?;
            target = PropTarget::Binding(binding);
          }
          hir_js::PatKind::AssignTarget(expr_id) => {
            let target_expr = self.get_expr(body, expr_id)?;
            match &target_expr.kind {
              hir_js::ExprKind::Ident(name_id) => {
                let name = names
                  .resolve(*name_id)
                  .ok_or(VmError::InvariantViolation(
                    "hir name id missing from interner",
                  ))?;
                let binding = self.env.resolve_binding_reference(
                  self.vm,
                  &mut *self.host,
                  &mut *self.hooks,
                  &mut prop_scope,
                  name,
                )?;
                target = PropTarget::Binding(binding);
              }
              hir_js::ExprKind::Member(member) => {
                if member.optional {
                  return Err(VmError::InvariantViolation(
                    "optional chaining used in assignment target",
                  ));
                }
                let base = self.eval_expr(&mut prop_scope, body, member.object)?;
                let base = prop_scope.push_root(base)?;
                match &member.property {
                  hir_js::ObjectKey::Computed(expr_id) => {
                    let key_value = self.eval_expr(&mut prop_scope, body, *expr_id)?;
                    let key_value = prop_scope.push_root(key_value)?;
                    target = PropTarget::ComputedMember { base, key_value };
                  }
                  other => {
                    let member_key = self.eval_object_key(&mut prop_scope, body, other)?;
                    root_property_key(&mut prop_scope, member_key)?;
                    target = PropTarget::Member { base, key: member_key };
                  }
                }
              }
              _ => {}
            }
          }
          _ => {}
        }
      }

      let mut prop_value =
        prop_scope.get_with_host_and_hooks(self.vm, &mut *self.host, &mut *self.hooks, obj, key, src_value)?;
      if matches!(prop_value, Value::Undefined) {
        if let Some(default_expr) = prop.default_value {
          prop_value = self.eval_expr(&mut prop_scope, body, default_expr)?;
        }
      }

      // Root the extracted value across any allocations while constructing keys and performing
      // assignments. `GetV` / defaults may produce freshly-allocated objects that are otherwise
      // unreachable.
      let prop_value = prop_scope.push_root(prop_value)?;

      match target {
        PropTarget::Binding(binding) => {
          maybe_set_anonymous_function_name(&mut prop_scope, prop_value, binding.name())?;
          self.env.set_resolved_binding(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            &mut prop_scope,
            binding,
            prop_value,
            self.strict,
          )?;
        }
        PropTarget::Member { base, key } => {
          self.assign_to_property_key(&mut prop_scope, base, key, prop_value)?
        }
        PropTarget::ComputedMember { base, key_value } => {
          let key = prop_scope.to_property_key(self.vm, &mut *self.host, &mut *self.hooks, key_value)?;
          root_property_key(&mut prop_scope, key)?;
          self.assign_to_property_key(&mut prop_scope, base, key, prop_value)?
        }
        PropTarget::Pat(pat_id) => self.assign_to_pat(&mut prop_scope, body, pat_id, prop_value)?,
      };
    }

    let Some(rest_pat_id) = pat.rest else {
      return Ok(());
    };

    // Rest property assignment should evaluate the LHS before copying properties.
    enum RestTarget<'a> {
      Binding(ResolvedBinding<'a>),
      Member { base: Value, key: PropertyKey },
      ComputedMember { base: Value, key_value: Value },
      Pat(hir_js::PatId),
    }
    let mut rest_target = RestTarget::Pat(rest_pat_id);
    {
      let rest_pat = self.get_pat(body, rest_pat_id)?;
      match rest_pat.kind {
        hir_js::PatKind::Ident(name_id) => {
          let name = names
            .resolve(name_id)
            .ok_or(VmError::InvariantViolation(
              "hir name id missing from interner",
            ))?;
          let binding = self.env.resolve_binding_reference(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            scope,
            name,
          )?;
          rest_target = RestTarget::Binding(binding);
        }
        hir_js::PatKind::AssignTarget(expr_id) => {
          let target_expr = self.get_expr(body, expr_id)?;
          match &target_expr.kind {
            hir_js::ExprKind::Ident(name_id) => {
              let name = names
                .resolve(*name_id)
                .ok_or(VmError::InvariantViolation(
                  "hir name id missing from interner",
                ))?;
              let binding = self.env.resolve_binding_reference(
                self.vm,
                &mut *self.host,
                &mut *self.hooks,
                scope,
                name,
              )?;
              rest_target = RestTarget::Binding(binding);
            }
            hir_js::ExprKind::Member(member) => {
              if member.optional {
                return Err(VmError::InvariantViolation(
                  "optional chaining used in assignment target",
                ));
              }
              let base = self.eval_expr(scope, body, member.object)?;
              let base = scope.push_root(base)?;
              match &member.property {
                hir_js::ObjectKey::Computed(expr_id) => {
                  let key_value = self.eval_expr(scope, body, *expr_id)?;
                  let key_value = scope.push_root(key_value)?;
                  rest_target = RestTarget::ComputedMember { base, key_value };
                }
                other => {
                  let member_key = self.eval_object_key(scope, body, other)?;
                  root_property_key(scope, member_key)?;
                  rest_target = RestTarget::Member { base, key: member_key };
                }
              }
            }
            _ => {}
          }
        }
        _ => {}
      }
    }

    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

    let rest_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
    scope.push_root(Value::Object(rest_obj))?;

    crate::spec_ops::copy_data_properties_with_host_and_hooks(
      self.vm,
      scope,
      &mut *self.host,
      &mut *self.hooks,
      rest_obj,
      Value::Object(obj),
      &excluded,
    )?;

    match rest_target {
      RestTarget::Binding(binding) => {
        let rest_value = Value::Object(rest_obj);
        maybe_set_anonymous_function_name(scope, rest_value, binding.name())?;
        self.env.set_resolved_binding(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          scope,
          binding,
          rest_value,
          self.strict,
        )
      }
      RestTarget::Member { base, key } => {
        self.assign_to_property_key(scope, base, key, Value::Object(rest_obj))
      }
      RestTarget::ComputedMember { base, key_value } => {
        let key = scope.to_property_key(self.vm, &mut *self.host, &mut *self.hooks, key_value)?;
        root_property_key(scope, key)?;
        self.assign_to_property_key(scope, base, key, Value::Object(rest_obj))
      }
      RestTarget::Pat(pat_id) => self.assign_to_pat(scope, body, pat_id, Value::Object(rest_obj)),
    }
  }

  fn assign_array_pattern(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    pat: &hir_js::ArrayPat,
    value: Value,
  ) -> Result<(), VmError> {
    let names = self.hir().names.clone();

    if matches!(value, Value::Undefined | Value::Null) {
      return Err(VmError::TypeError("array destructuring requires object coercible"));
    }

    let mut iterator_record =
      crate::iterator::get_iterator(self.vm, &mut *self.host, &mut *self.hooks, scope, value)?;
    scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

    for elem in &pat.elements {
      if let Err(err) = self.vm.tick() {
        return self.iterator_close_on_err(scope, &iterator_record, err);
      }

      let mut elem_scope = scope.reborrow();

      let Some(elem) = elem else {
        if let Err(err) = crate::iterator::iterator_step(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut elem_scope,
          &mut iterator_record,
        ) {
          return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err);
        }
        continue;
      };

      // --- Assignment target evaluation order (ECMA-262 `IteratorDestructuringAssignmentEvaluation`) ---
      //
      // For destructuring assignment, the spec evaluates assignment targets (binding resolution /
      // member base + key expression) before consuming iterator values.
      //
      // For computed member targets (`obj[expr]`), `ToPropertyKey` conversion is delayed until
      // `PutValue`, after `IteratorValue` / default evaluation.
      enum ElemTarget<'a> {
        Binding(ResolvedBinding<'a>),
        Member { base: Value, key: PropertyKey },
        ComputedMember { base: Value, key_value: Value },
        Pat(hir_js::PatId),
      }
      let mut target = ElemTarget::Pat(elem.pat);
      {
        let elem_pat = self.get_pat(body, elem.pat)?;
        match elem_pat.kind {
          hir_js::PatKind::Ident(name_id) => {
            let name = names
              .resolve(name_id)
              .ok_or(VmError::InvariantViolation(
                "hir name id missing from interner",
              ))?;
            let binding = self.env.resolve_binding_reference(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              &mut elem_scope,
              name,
            )?;
            target = ElemTarget::Binding(binding);
          }
          hir_js::PatKind::AssignTarget(expr_id) => {
            let target_expr = self.get_expr(body, expr_id)?;
            match &target_expr.kind {
              hir_js::ExprKind::Ident(name_id) => {
                let name = names
                  .resolve(*name_id)
                  .ok_or(VmError::InvariantViolation(
                    "hir name id missing from interner",
                  ))?;
                let binding = self.env.resolve_binding_reference(
                  self.vm,
                  &mut *self.host,
                  &mut *self.hooks,
                  &mut elem_scope,
                  name,
                )?;
                target = ElemTarget::Binding(binding);
              }
              hir_js::ExprKind::Member(member) => {
                if member.optional {
                  return Err(VmError::InvariantViolation(
                    "optional chaining used in assignment target",
                  ));
                }
                let base = self.eval_expr(&mut elem_scope, body, member.object)?;
                let base = elem_scope.push_root(base)?;
                match &member.property {
                  hir_js::ObjectKey::Computed(expr_id) => {
                    let key_value = self.eval_expr(&mut elem_scope, body, *expr_id)?;
                    let key_value = elem_scope.push_root(key_value)?;
                    target = ElemTarget::ComputedMember { base, key_value };
                  }
                  other => {
                    let member_key = self.eval_object_key(&mut elem_scope, body, other)?;
                    root_property_key(&mut elem_scope, member_key)?;
                    target = ElemTarget::Member { base, key: member_key };
                  }
                }
              }
              _ => {}
            }
          }
          _ => {}
        }
      }

      let mut item = match crate::iterator::iterator_step_value(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        &mut elem_scope,
        &mut iterator_record,
      ) {
        Ok(Some(v)) => v,
        Ok(None) => Value::Undefined,
        Err(err) => return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err),
      };

      if matches!(item, Value::Undefined) {
        if let Some(default_expr) = elem.default_value {
          item = match self.eval_expr(&mut elem_scope, body, default_expr) {
            Ok(v) => v,
            Err(err) => return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err),
          };
        }
      }

      // Root the extracted value across key conversion + assignment. Iterator values can be
      // freshly-allocated objects unreachable from the heap except for this local binding.
      let item = match elem_scope.push_root(item) {
        Ok(v) => v,
        Err(err) => return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err),
      };

      let res = match target {
        ElemTarget::Binding(binding) => {
          maybe_set_anonymous_function_name(&mut elem_scope, item, binding.name())?;
          self.env.set_resolved_binding(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            &mut elem_scope,
            binding,
            item,
            self.strict,
          )
        }
        ElemTarget::Member { base, key } => self.assign_to_property_key(&mut elem_scope, base, key, item),
        ElemTarget::ComputedMember { base, key_value } => {
          let key = match elem_scope.to_property_key(self.vm, &mut *self.host, &mut *self.hooks, key_value) {
            Ok(key) => key,
            Err(err) => return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err),
          };
          if let Err(err) = root_property_key(&mut elem_scope, key) {
            return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err);
          }
          self.assign_to_property_key(&mut elem_scope, base, key, item)
        }
        ElemTarget::Pat(pat_id) => self.assign_to_pat(&mut elem_scope, body, pat_id, item),
      };
      if let Err(err) = res {
        return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err);
      }
    }

    let Some(rest_pat_id) = pat.rest else {
      if !iterator_record.done {
        crate::iterator::iterator_close(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          scope,
          &iterator_record,
          crate::iterator::CloseCompletionKind::NonThrow,
        )?;
      }
      return Ok(());
    };

    // Rest element assignment must evaluate the LHS target before consuming the iterator.
    enum RestTarget<'a> {
      Binding(ResolvedBinding<'a>),
      Member { base: Value, key: PropertyKey },
      ComputedMember { base: Value, key_value: Value },
      Pat(hir_js::PatId),
    }
    let mut rest_target = RestTarget::Pat(rest_pat_id);
    {
      let rest_pat = self.get_pat(body, rest_pat_id)?;
      match rest_pat.kind {
        hir_js::PatKind::Ident(name_id) => {
          let name = names
            .resolve(name_id)
            .ok_or(VmError::InvariantViolation(
              "hir name id missing from interner",
            ))?;
          let binding = self.env.resolve_binding_reference(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            scope,
            name,
          )?;
          rest_target = RestTarget::Binding(binding);
        }
        hir_js::PatKind::AssignTarget(expr_id) => {
          let target_expr = self.get_expr(body, expr_id)?;
          match &target_expr.kind {
            hir_js::ExprKind::Ident(name_id) => {
              let name = names
                .resolve(*name_id)
                .ok_or(VmError::InvariantViolation(
                  "hir name id missing from interner",
                ))?;
              let binding = self.env.resolve_binding_reference(
                self.vm,
                &mut *self.host,
                &mut *self.hooks,
                scope,
                name,
              )?;
              rest_target = RestTarget::Binding(binding);
            }
            hir_js::ExprKind::Member(member) => {
              if member.optional {
                return Err(VmError::InvariantViolation(
                  "optional chaining used in assignment target",
                ));
              }
              let base = self.eval_expr(scope, body, member.object)?;
              let base = scope.push_root(base)?;
              match &member.property {
                hir_js::ObjectKey::Computed(expr_id) => {
                  let key_value = self.eval_expr(scope, body, *expr_id)?;
                  let key_value = scope.push_root(key_value)?;
                  rest_target = RestTarget::ComputedMember { base, key_value };
                }
                other => {
                  let member_key = self.eval_object_key(scope, body, other)?;
                  root_property_key(scope, member_key)?;
                  rest_target = RestTarget::Member { base, key: member_key };
                }
              }
            }
            _ => {}
          }
        }
        _ => {}
      }
    }

    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

    let rest_arr = match scope.alloc_array(0) {
      Ok(arr) => arr,
      Err(err) => return self.iterator_close_on_err(scope, &iterator_record, err),
    };
    if let Err(err) = scope.push_root(Value::Object(rest_arr)) {
      return self.iterator_close_on_err(scope, &iterator_record, err);
    }
    if let Err(err) = scope
      .heap_mut()
      .object_set_prototype(rest_arr, Some(intr.array_prototype()))
    {
      return self.iterator_close_on_err(scope, &iterator_record, err);
    }

    let mut rest_idx: u32 = 0;
    loop {
      if let Err(err) = self.vm.tick() {
        return self.iterator_close_on_err(scope, &iterator_record, err);
      }

      let next = match crate::iterator::iterator_step_value(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        scope,
        &mut iterator_record,
      ) {
        Ok(v) => v,
        Err(err) => return self.iterator_close_on_err(scope, &iterator_record, err),
      };
      let Some(v) = next else {
        break;
      };

      let create_res = {
        let mut elem_scope = scope.reborrow();
        elem_scope.push_roots(&[Value::Object(rest_arr), v])?;
        let key_s = elem_scope.alloc_u32_index_string(rest_idx)?;
        let key = PropertyKey::from_string(key_s);
        root_property_key(&mut elem_scope, key)?;
        elem_scope.create_data_property_or_throw(rest_arr, key, v)
      };
      if let Err(err) = create_res {
        return self.iterator_close_on_err(scope, &iterator_record, err);
      }
      rest_idx = rest_idx.saturating_add(1);
    }

    let assign_res = match rest_target {
      RestTarget::Binding(binding) => {
        let rest_value = Value::Object(rest_arr);
        maybe_set_anonymous_function_name(scope, rest_value, binding.name())?;
        self.env.set_resolved_binding(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          scope,
          binding,
          rest_value,
          self.strict,
        )
      }
      RestTarget::Member { base, key } => self.assign_to_property_key(scope, base, key, Value::Object(rest_arr)),
      RestTarget::ComputedMember { base, key_value } => {
        let key = match scope.to_property_key(self.vm, &mut *self.host, &mut *self.hooks, key_value) {
          Ok(k) => k,
          Err(err) => return self.iterator_close_on_err(scope, &iterator_record, err),
        };
        if let Err(err) = root_property_key(scope, key) {
          return self.iterator_close_on_err(scope, &iterator_record, err);
        }
        self.assign_to_property_key(scope, base, key, Value::Object(rest_arr))
      }
      RestTarget::Pat(pat_id) => self.assign_to_pat(scope, body, pat_id, Value::Object(rest_arr)),
    };
    match assign_res {
      Ok(()) => Ok(()),
      Err(err) => self.iterator_close_on_err(scope, &iterator_record, err),
    }
  }

  fn eval_member(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    member: &hir_js::MemberExpr,
  ) -> Result<Value, VmError> {
    Ok(self.eval_member_chain(scope, body, member)?.into_value())
  }

  fn eval_chain_base(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    expr_id: hir_js::ExprId,
  ) -> Result<OptionalChainEval, VmError> {
    let expr = self.get_expr(body, expr_id)?;
    // Parenthesized expressions break optional-chain propagation:
    // `(a?.b).c` should not short-circuit `.c` when `a` is nullish.
    //
    // HIR does not preserve parse-js's `ParenthesizedExpr` metadata, so detect this by scanning the
    // original source for a `)` immediately following the expression span.
    let parenthesized = self.next_non_trivia_byte_from_source(expr.span.start)? == Some(b'(')
      || self.next_non_trivia_byte_from_source(expr.span.end)? == Some(b')');
    if parenthesized {
      return Ok(OptionalChainEval::Value(self.eval_expr(scope, body, expr_id)?));
    }
    match &expr.kind {
      hir_js::ExprKind::Member(member) => {
        // Budget once per expression evaluation.
        self.vm.tick()?;
        self.eval_member_chain(scope, body, member)
      }
      hir_js::ExprKind::Call(call) => {
        // Budget once per expression evaluation.
        self.vm.tick()?;
        self.eval_call_chain(scope, body, call)
      }
      _ => Ok(OptionalChainEval::Value(self.eval_expr(scope, body, expr_id)?)),
    }
  }

  fn eval_member_chain(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    member: &hir_js::MemberExpr,
  ) -> Result<OptionalChainEval, VmError> {
    // Optional chain continuation semantics: if the base expression short-circuited, the current
    // member access is not evaluated and the whole chain evaluates to `undefined`.
    let base = match self.eval_chain_base(scope, body, member.object)? {
      OptionalChainEval::Value(v) => v,
      OptionalChainEval::ShortCircuit => return Ok(OptionalChainEval::ShortCircuit),
    };
    if member.optional && matches!(base, Value::Null | Value::Undefined) {
      return Ok(OptionalChainEval::ShortCircuit);
    }

    // Root the base value across key evaluation / boxing / property access.
    let mut scope = scope.reborrow();
    // Root the original base value across `ToObject` + key evaluation + `[[Get]]` in case any step
    // allocates / triggers GC.
    scope.push_root(base)?;

    let key = self.eval_object_key(&mut scope, body, &member.property)?;
    root_property_key(&mut scope, key)?;

    // `GetValue` for property references: ToObject(base) then `[[Get]](key, Receiver=base)`.
    let obj = match scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base) {
      Ok(obj) => obj,
      Err(VmError::TypeError(msg)) => return Err(throw_type_error(self.vm, &mut scope, msg)?),
      Err(err) => return Err(err),
    };
    // Root the boxed object so host hooks/accessors can allocate freely.
    scope.push_root(Value::Object(obj))?;

    Ok(OptionalChainEval::Value(scope.get_with_host_and_hooks(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      obj,
      key,
      base,
    )?))
  }

  fn assign_to_member(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    member: &hir_js::MemberExpr,
    value: Value,
  ) -> Result<(), VmError> {
    // Optional chaining is never a valid assignment target; this should be rejected by an early
    // error pass before evaluation begins.
    if member.optional {
      return Err(VmError::InvariantViolation(
        "optional chaining used in assignment target",
      ));
    }

    let base = self.eval_expr(scope, body, member.object)?;

    // Root base + value while allocating the key and performing the assignment (which can invoke
    // accessors and/or Proxy traps).
    let mut scope = scope.reborrow();
    // Root base + value across `ToObject` + key evaluation + `[[Set]]` (assignment may invoke
    // accessors/proxy traps and allocate).
    scope.push_roots(&[base, value])?;

    let key = self.eval_object_key(&mut scope, body, &member.property)?;
    root_property_key(&mut scope, key)?;

    // `PutValue` for property references: ToObject(base) then `[[Set]](key, value, Receiver=base)`.
    let obj = match scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base) {
      Ok(obj) => obj,
      Err(VmError::TypeError(msg)) => return Err(throw_type_error(self.vm, &mut scope, msg)?),
      Err(err) => return Err(err),
    };
    scope.push_root(Value::Object(obj))?;
    let ok = crate::spec_ops::internal_set_with_host_and_hooks(
      self.vm,
      &mut scope,
      &mut *self.host,
      &mut *self.hooks,
      obj,
      key,
      value,
      base,
    )?;
    if ok {
      Ok(())
    } else if self.strict {
      Err(throw_type_error(
        self.vm,
        &mut scope,
        "Cannot assign to read-only property",
      )?)
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
      hir_js::ObjectKey::Number(raw) => {
        // Numeric literal property names are canonicalized in ECMAScript:
        // `ToPropertyKey(NumericLiteral)` uses `ToString(ToNumber(literal))`.
        //
        // HIR currently stores the source text of numeric literal property names (e.g. `0x10`,
        // `1_0`) so the compiled path must perform this canonicalization at runtime.
        //
        // Note: `parse_js::num::JsNumber::from_literal` is not tick-aware, so charge fuel based on
        // input length before parsing to avoid long uninterruptible work for pathological literals.
        if raw.len() > crate::tick::DEFAULT_TICK_EVERY {
          let mut i = crate::tick::DEFAULT_TICK_EVERY;
          while i < raw.len() {
            self.vm.tick()?;
            i = i.saturating_add(crate::tick::DEFAULT_TICK_EVERY);
          }
        }

        let n = JsNumber::from_literal(raw).map(|n| n.0).unwrap_or(f64::NAN);
        let key_s = scope.heap_mut().to_string(Value::Number(n))?;
        Ok(PropertyKey::from_string(key_s))
      }
      hir_js::ObjectKey::Computed(expr_id) => {
        let v = self.eval_expr(scope, body, *expr_id)?;
        // Computed property keys use full ECMAScript `ToPropertyKey`, which performs `ToPrimitive`
        // (hint String) and can invoke user code. Root the computed value across the conversion.
        let mut scope = scope.reborrow();
        scope.push_root(v)?;
        scope.to_property_key(self.vm, &mut *self.host, &mut *self.hooks, v)
      }
    }
  }

  /// ECMA-262-ish `NamedEvaluation` helper used by object literal property evaluation.
  ///
  /// This implements the observable behaviour of `SetFunctionName` for syntactic anonymous
  /// function/class definitions used as values, using the provided property key as the inferred
  /// name.
  ///
  /// For anonymous *class* expressions, the inferred name must be applied **before** defining class
  /// elements (so a `static name() {}` element can override the constructor's initial `"name"`
  /// property). This requires special handling during class construction.
  fn eval_expr_named(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    expr_id: hir_js::ExprId,
    name: PropertyKey,
  ) -> Result<Value, VmError> {
    // This is intentionally syntactic (based on the expression kind), not dynamic (based on the
    // runtime value), so e.g. `{ x: someFunc }` does not rename `someFunc` even if its current
    // `.name` is empty.
    let is_anonymous_function_def = {
      let expr = self.get_expr(body, expr_id)?;
      match &expr.kind {
        hir_js::ExprKind::FunctionExpr { name, is_arrow, .. } => *is_arrow || name.is_none(),
        hir_js::ExprKind::ClassExpr { name, .. } => name.is_none(),
        _ => false,
      }
    };

    if !is_anonymous_function_def {
      return self.eval_expr(scope, body, expr_id);
    }

    // Root the inferred name key across evaluation and `SetFunctionName` (which can allocate).
    let mut named_scope = scope.reborrow();
    root_property_key(&mut named_scope, name)?;

    // Anonymous class expressions must receive the inferred name during class construction.
    if let hir_js::ExprKind::ClassExpr { body: class_body, name: None, .. } =
      &self.get_expr(body, expr_id)?.kind
    {
      // `eval_expr` would have charged one tick at expression entry; preserve that budget behaviour
      // when we bypass it for class `NamedEvaluation`.
      self.vm.tick()?;
      return self.eval_class_expr(&mut named_scope, *class_body, None, Some(name));
    }

    let v = self.eval_expr(&mut named_scope, body, expr_id)?;
    named_scope.push_root(v)?;

    if let Value::Object(func_obj) = v {
      // `SetFunctionName` only applies to actual Function objects (not callable Proxies).
      let func_name = match named_scope.heap().get_function(func_obj) {
        Ok(f) => Some(f.name),
        Err(VmError::NotCallable) => None,
        Err(err) => return Err(err),
      };
      if let Some(current_name) = func_name {
        // Only infer a name for empty-name functions.
        if named_scope
          .heap()
          .get_string(current_name)?
          .as_code_units()
          .is_empty()
        {
          crate::function_properties::set_function_name(&mut named_scope, func_obj, name, None)?;
        }
      }
    }

    Ok(v)
  }

  fn eval_object_literal(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    obj: &hir_js::ObjectLiteral,
  ) -> Result<Value, VmError> {
    // Object literals inherit from %Object.prototype% (when intrinsics are available).
    //
    // The heap can be used without an initialized realm in some low-level unit tests; in that case
    // `vm.intrinsics()` is `None` and the object remains null-prototype shaped.
    let obj_val = if let Some(intr) = self.vm.intrinsics() {
      scope.alloc_object_with_prototype(Some(intr.object_prototype()))?
    } else {
      scope.alloc_object()?
    };
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(obj_val))?;

    for prop in &obj.properties {
      self.vm.tick()?;
      // Keep per-member roots local so large object literals do not accumulate stack roots.
      let mut member_scope = scope.reborrow();
      match prop {
        hir_js::ObjectProperty::KeyValue {
          key,
          value,
          method,
          ..
        } => {
          // `__proto__` in object literals is a special-cased data property definition that sets
          // the newly created object's prototype instead of defining an own `"__proto__"` property.
          //
          // This only applies to non-computed keys and non-method definitions. Computed
          // `["__proto__"]` keys always define a normal data property.
          let is_proto_key = !*method
            && match key {
              hir_js::ObjectKey::Ident(name_id) => self.hir().names.resolve(*name_id) == Some("__proto__"),
              hir_js::ObjectKey::String(s) => s == "__proto__",
              _ => false,
            };
          if is_proto_key {
            // Evaluate the RHS for side effects, then:
            // - if it's an object or null, update the object's [[Prototype]]
            // - otherwise, ignore it (no own "__proto__" property is created)
            let proto_value = self.eval_expr(&mut member_scope, body, *value)?;
            member_scope.push_root(proto_value)?;
            match proto_value {
              Value::Object(proto_obj) => {
                member_scope
                  .heap_mut()
                  .object_set_prototype(obj_val, Some(proto_obj))?;
              }
              Value::Null => {
                member_scope.heap_mut().object_set_prototype(obj_val, None)?;
              }
              _ => {}
            }
            continue;
          }

          let key = self.eval_object_key(&mut member_scope, body, key)?;
          root_property_key(&mut member_scope, key)?;

          let v = if *method {
            // Object literal method definitions (`{ m() {} }`) produce function objects that are not
            // constructable and therefore do not have an own `"prototype"` property.
            //
            // hir-js lowers these as `ObjectProperty::KeyValue { method: true, value: FunctionExpr }`,
            // so allocate the function object with the correct constructability here.
            match &self.get_expr(body, *value)?.kind {
              hir_js::ExprKind::FunctionExpr {
                body: func_body,
                is_arrow,
                ..
              } => Value::Object(self.alloc_user_function_object(
                &mut member_scope,
                *func_body,
                "",
                *is_arrow,
                /* is_constructable */ false,
                /* name_binding */ None,
                EcmaFunctionKind::ObjectMember,
              )?),
              _ => self.eval_expr(&mut member_scope, body, *value)?,
            }
          } else {
            // Spec-ish `NamedEvaluation` / `SetFunctionName` behaviour: for anonymous function/class
            // definitions used as property values, infer `name` from the property key.
            self.eval_expr_named(&mut member_scope, body, *value, key)?
          };

          // Root the value across `SetFunctionName` and `CreateDataProperty`.
          member_scope.push_root(v)?;

          // Methods use the property key as the function `name`.
          if *method {
            if let Value::Object(func_obj) = v {
              member_scope
                .heap_mut()
                .set_function_home_object(func_obj, Some(obj_val))?;
              crate::function_properties::set_function_name(&mut member_scope, func_obj, key, None)?;
            }
          }
          let _ = member_scope.create_data_property(obj_val, key, v)?;
        }
        hir_js::ObjectProperty::Spread(expr_id) => {
          let src_value = self.eval_expr(&mut member_scope, body, *expr_id)?;
          // Root the spread source across `CopyDataProperties` (which can allocate and invoke user
          // code via Proxy traps and accessors).
          member_scope.push_root(src_value)?;
          crate::spec_ops::copy_data_properties_with_host_and_hooks(
            self.vm,
            &mut member_scope,
            &mut *self.host,
            &mut *self.hooks,
            obj_val,
            src_value,
            &[],
          )?;
        }
        hir_js::ObjectProperty::Getter { key, body: getter_body } => {
          let key = self.eval_object_key(&mut member_scope, body, key)?;
          root_property_key(&mut member_scope, key)?;

          // If a setter was already defined earlier in the literal, preserve it.
          let mut existing_set = Value::Undefined;
          if let Some(desc) = member_scope.heap().get_own_property(obj_val, key)? {
            if let PropertyKind::Accessor { set, .. } = desc.kind {
              existing_set = set;
            }
          }

          let func_obj = self.alloc_user_function_object(
            &mut member_scope,
            *getter_body,
            /* name */ "",
            /* is_arrow */ false,
            /* is_constructable */ false,
            /* name_binding */ None,
            EcmaFunctionKind::ObjectMember,
          )?;
          member_scope.push_root(Value::Object(func_obj))?;
          member_scope
            .heap_mut()
            .set_function_home_object(func_obj, Some(obj_val))?;
          crate::function_properties::set_function_name(&mut member_scope, func_obj, key, Some("get"))?;
          crate::function_properties::set_function_length(&mut member_scope, func_obj, 0)?;

          member_scope.define_property(
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
          let key = self.eval_object_key(&mut member_scope, body, key)?;
          root_property_key(&mut member_scope, key)?;

          // If a getter was already defined earlier in the literal, preserve it.
          let mut existing_get = Value::Undefined;
          if let Some(desc) = member_scope.heap().get_own_property(obj_val, key)? {
            if let PropertyKind::Accessor { get, .. } = desc.kind {
              existing_get = get;
            }
          }

          let func_obj = self.alloc_user_function_object(
            &mut member_scope,
            *setter_body,
            /* name */ "",
            /* is_arrow */ false,
            /* is_constructable */ false,
            /* name_binding */ None,
            EcmaFunctionKind::ObjectMember,
          )?;
          member_scope.push_root(Value::Object(func_obj))?;
          member_scope
            .heap_mut()
            .set_function_home_object(func_obj, Some(obj_val))?;
          crate::function_properties::set_function_name(&mut member_scope, func_obj, key, Some("set"))?;
          crate::function_properties::set_function_length(&mut member_scope, func_obj, 1)?;

          member_scope.define_property(
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
      };
    }

    Ok(Value::Object(obj_val))
  }

  fn eval_call_arguments(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    call_args: &[hir_js::CallArg],
  ) -> Result<Vec<Value>, VmError> {
    let mut args: Vec<Value> = Vec::new();
    // Best-effort lower bound: spread args can expand beyond this.
    args
      .try_reserve_exact(call_args.len())
      .map_err(|_| VmError::OutOfMemory)?;

    for arg in call_args {
      if arg.spread {
        let spread_value = self.eval_expr(scope, body, arg.expr)?;
        scope.push_root(spread_value)?;

        let mut iter =
          iterator::get_iterator(self.vm, &mut *self.host, &mut *self.hooks, scope, spread_value)?;
        scope.push_roots(&[iter.iterator, iter.next_method])?;

        loop {
          let next_value = match iterator::iterator_step_value(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            scope,
            &mut iter,
          ) {
            Ok(v) => v,
            // Spec: spread argument evaluation does not perform `IteratorClose` on errors produced
            // while stepping the iterator (`next`/`done`/`value`).
            Err(err) => return Err(err),
          };
          let Some(value) = next_value else {
            break;
          };

          let step_res: Result<(), VmError> = (|| {
            // Per-spread-element tick: spreading large iterators should be budgeted even when the
            // iterator's `next()` is native/cheap.
            self.vm.tick()?;
            scope.push_root(value)?;
            args.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
            args.push(value);
            Ok(())
          })();
          if let Err(err) = step_res {
            return Err(self.iterator_close_on_error(scope, &iter, err));
          }
        }
      } else {
        let value = self.eval_expr(scope, body, arg.expr)?;
        scope.push_root(value)?;
        args.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
        args.push(value);
      }
    }

    Ok(args)
  }

  fn eval_call(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    call: &hir_js::CallExpr,
  ) -> Result<Value, VmError> {
    Ok(self.eval_call_chain(scope, body, call)?.into_value())
  }

  fn eval_call_chain(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    call: &hir_js::CallExpr,
  ) -> Result<OptionalChainEval, VmError> {
    // Track whether this call is *syntactically* a direct eval candidate (`eval(...)`).
    //
    // A call is only a direct eval if:
    // - it is syntactically `eval(...)`, and
    // - `eval` resolves to the original `%eval%` intrinsic function object.
    let callee_expr = self.get_expr(body, call.callee)?;
    let callee_is_parenthesized = self.expr_is_parenthesized(callee_expr)?;
    let direct_eval_syntax = !call.is_new
      && !call.optional
      && matches!(
        &callee_expr.kind,
        hir_js::ExprKind::Ident(name_id)
          if self.hir().names.resolve(*name_id) == Some("eval")
      )
      && !callee_is_parenthesized;

    // `super(...args)` in derived class constructors.
    if !call.is_new && matches!(callee_expr.kind, hir_js::ExprKind::Super) {
      if call.optional {
        return Err(VmError::Unimplemented("optional chaining super call"));
      }

      let Some(class_ctor) = self.class_constructor else {
        return Err(VmError::Unimplemented("super call outside of class constructor"));
      };
      if !self.derived_constructor {
        return Err(throw_reference_error(
          self.vm,
          scope,
          "super() is not allowed in base class constructors",
        )?);
      }
      if self.this_initialized {
        return Err(throw_reference_error(
          self.vm,
          scope,
          "super() can only be called once in a derived constructor",
        )?);
      }

      // Resolve the superclass constructor from the class constructor's hidden `extends` slot.
      let super_value = crate::class_fields::class_constructor_super_value(scope, class_ctor)?;
      let super_ctor = match super_value {
        Value::Object(o) => o,
        Value::Null => {
          return Err(throw_type_error(
            self.vm,
            scope,
            "Class extends value is not a constructor",
          )?)
        }
        Value::Undefined => {
          return Err(VmError::InvariantViolation(
            "derived constructor attempted super() call with undefined superclass",
          ))
        }
        _ => {
          return Err(VmError::InvariantViolation(
            "class constructor super slot is not undefined, null, or object",
          ))
        }
      };

      // Root callee/new_target for the duration of argument evaluation + construction.
      let mut call_scope = scope.reborrow();
      call_scope.push_roots(&[Value::Object(super_ctor), self.new_target])?;

      let args = self.eval_call_arguments(&mut call_scope, body, call.args.as_slice())?;

      let this_value = self.vm.construct_with_host_and_hooks(
        &mut *self.host,
        &mut call_scope,
        &mut *self.hooks,
        Value::Object(super_ctor),
        args.as_slice(),
        self.new_target,
      )?;
      let Value::Object(this_obj) = this_value else {
        return Err(VmError::InvariantViolation(
          "super constructor returned non-object from Construct",
        ));
      };

      // Bind `this` and keep it rooted for the remainder of constructor evaluation.
      let this_root_idx = self.this_root_idx.ok_or(VmError::InvariantViolation(
        "derived constructor missing this root slot",
      ))?;
      self.this = this_value;
      self.this_initialized = true;
      call_scope.heap_mut().root_stack[this_root_idx] = this_value;

      // Initialize derived instance fields immediately after `super()` returns.
      crate::class_fields::initialize_instance_fields_with_host_and_hooks(
        self.vm,
        &mut call_scope,
        &mut *self.host,
        &mut *self.hooks,
        this_obj,
        class_ctor,
      )?;

      return Ok(OptionalChainEval::Value(this_value));
    }

    let mut scope = scope.reborrow();

    if call.is_new {
      // `new callee(...args)` evaluates the callee as a value (no method-call `this` binding) and
      // invokes `[[Construct]]` with `newTarget = callee` (best-effort; `Reflect.construct` sets
      // `newTarget` explicitly).
      let callee_value = match self.eval_chain_base(&mut scope, body, call.callee)? {
        OptionalChainEval::Value(v) => v,
        OptionalChainEval::ShortCircuit => return Ok(OptionalChainEval::ShortCircuit),
      };
      if call.optional && matches!(callee_value, Value::Null | Value::Undefined) {
        return Ok(OptionalChainEval::ShortCircuit);
      }

      // Root callee while evaluating args.
      scope.push_root(callee_value)?;

      let args = self.eval_call_arguments(&mut scope, body, call.args.as_slice())?;

      // For `new F(...)`, the `newTarget` is `F` itself.
      return Ok(OptionalChainEval::Value(self.vm.construct_with_host_and_hooks(
        &mut *self.host,
        &mut scope,
        &mut *self.hooks,
        callee_value,
        args.as_slice(),
        callee_value,
      )?));
    }

    // Method call detection: `obj.prop(...)` uses `this = obj`.
    let (callee_value, this_value) = match &callee_expr.kind {
      hir_js::ExprKind::Member(member) => {
        match self.eval_chain_base(&mut scope, body, member.object)? {
          OptionalChainEval::Value(base) => {
            if member.optional && matches!(base, Value::Null | Value::Undefined) {
              return Ok(OptionalChainEval::ShortCircuit);
            }
            // Root base across `ToObject` + key evaluation + `[[Get]]` in case any step allocates /
            // triggers GC.
            scope.push_root(base)?;

            let key = self.eval_object_key(&mut scope, body, &member.property)?;
            root_property_key(&mut scope, key)?;

            let obj = match scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base) {
              Ok(obj) => obj,
              Err(VmError::TypeError(msg)) => return Err(throw_type_error(self.vm, &mut scope, msg)?),
              Err(err) => return Err(err),
            };
            scope.push_root(Value::Object(obj))?;

            let func =
              scope.get_with_host_and_hooks(self.vm, &mut *self.host, &mut *self.hooks, obj, key, base)?;
            (func, base)
          }
          OptionalChainEval::ShortCircuit => {
            if callee_is_parenthesized {
              (Value::Undefined, Value::Undefined)
            } else {
              return Ok(OptionalChainEval::ShortCircuit);
            }
          }
        }
      }
      _ => {
        let callee_value = match self.eval_chain_base(&mut scope, body, call.callee)? {
          OptionalChainEval::Value(v) => v,
          OptionalChainEval::ShortCircuit => return Ok(OptionalChainEval::ShortCircuit),
        };
        (callee_value, Value::Undefined)
      }
    };

    if call.optional && matches!(callee_value, Value::Null | Value::Undefined) {
      return Ok(OptionalChainEval::ShortCircuit);
    }

    // Root callee/this while evaluating args.
    scope.push_roots(&[callee_value, this_value])?;

    let args = self.eval_call_arguments(&mut scope, body, call.args.as_slice())?;

    if direct_eval_syntax {
      let intr = self
        .vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
      if callee_value == Value::Object(intr.eval()) {
        // Direct eval: execute in the caller's lexical environment (with strictness propagation).
        let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);
        let out = match arg0 {
          Value::String(s) => perform_direct_eval_with_host_and_hooks(
            self.vm,
            &mut scope,
            &mut *self.host,
            &mut *self.hooks,
            self.env,
            self.strict,
            self.this,
            self.new_target,
            s,
          )?,
          other => other,
        };
        return Ok(OptionalChainEval::Value(out));
      }
    }

    Ok(OptionalChainEval::Value(self.vm.call_with_host_and_hooks(
      &mut *self.host,
      &mut scope,
      &mut *self.hooks,
      callee_value,
      this_value,
      args.as_slice(),
    )?))
  }

  fn eval_class_expr(
    &mut self,
    scope: &mut Scope<'_>,
    body_id: hir_js::BodyId,
    name: Option<&str>,
    inferred_name: Option<PropertyKey>,
  ) -> Result<Value, VmError> {
    // Avoid borrowing the body through `self` across calls that mutably borrow `self`.
    let hir = self.script.hir.clone();
    let class_body = hir
      .body(body_id)
      .ok_or(VmError::InvariantViolation("hir body id missing from compiled script"))?;
    if class_body.kind != hir_js::BodyKind::Class {
      return Err(VmError::InvariantViolation("class expr body is not a class"));
    }
    if class_body.class.is_none() {
      return Err(VmError::InvariantViolation("class body missing class metadata"));
    }

    let outer = self.env.lexical_env();
    if let Some(name) = name {
      // Named class expressions introduce an inner immutable binding for the class name.
      let class_env = scope.env_create(Some(outer))?;
      self.env.set_lexical_env(scope.heap_mut(), class_env);

      let result = self.eval_class(scope, class_body, Some(name), name, None);
      self.env.set_lexical_env(scope.heap_mut(), outer);
      Ok(Value::Object(result?))
    } else {
      let result = self.eval_class(scope, class_body, None, "", inferred_name);
      Ok(Value::Object(result?))
    }
  }

  fn eval_class(
    &mut self,
    scope: &mut Scope<'_>,
    class_body: &hir_js::Body,
    binding_name: Option<&str>,
    func_name: &str,
    inferred_name: Option<PropertyKey>,
  ) -> Result<GcObject, VmError> {
    // Per ECMA-262, class definitions are always strict mode code.
    let saved_strict = self.strict;
    self.strict = true;

     let result = (|| {
        // Avoid borrowing the HIR through `self` across calls that mutably borrow `self` during class
        // member evaluation (e.g. evaluating class static blocks).
        let hir = self.script.hir.clone();

        let Some(class_meta) = class_body.class.as_ref() else {
          return Err(VmError::InvariantViolation("class body missing class metadata"));
        };

       let class_env = self.env.lexical_env();

       // Ensure the requested class binding exists before evaluating `extends` or creating any
       // class element closures.
       //
       // The binding must exist (but be uninitialized) so a class can observe TDZ semantics when
       // referencing its own name in the `extends` clause (e.g. `class C extends C {}` should throw
       // a ReferenceError, not consult outer bindings).
       if let Some(name) = binding_name {
         if scope.heap().env_has_binding(class_env, name)? {
           return Err(VmError::InvariantViolation(
             "class binding already exists in class environment",
           ));
         }
         scope.env_create_immutable_binding(class_env, name)?;
       }

        // Evaluate `extends` (class heritage), if present.
        //
        // - `undefined` => no `extends` (base class)
        // - `null` => `extends null`
        // - object => superclass constructor
        let super_value = match class_meta.extends {
          None => Value::Undefined,
          Some(extends_expr_id) => {
            let v = self.eval_expr(scope, class_body, extends_expr_id)?;
            match v {
              Value::Null => Value::Null,
              Value::Object(_) => {
                if !scope.heap().is_constructor(v)? {
                  return Err(throw_type_error(
                    self.vm,
                    scope,
                    "Class extends value is not a constructor",
                  )?);
                }
                v
              }
              _ => {
                return Err(throw_type_error(
                  self.vm,
                  scope,
                  "Class extends value is not a constructor",
                )?)
              }
            }
          }
        };

        // Keep the superclass value alive across subsequent allocations/GC until it becomes
        // reachable from the class constructor object (via its native `super` slot).
        let super_root_len = scope.heap().root_stack.len();
        scope.push_root(super_value)?;
        let _super_root_guard = RootStackTruncateGuard::new(scope.heap_mut(), super_root_len);
      // Find an explicit constructor, if present.
      let mut ctor_member: Option<&hir_js::ClassMember> = None;
      for member in class_meta.members.iter() {
        self.vm.tick()?;
        if member.static_ {
          continue;
        }
        if matches!(member.kind, hir_js::ClassMemberKind::Constructor { .. }) {
          if ctor_member.is_some() {
            return Err(VmError::TypeError("A class may only have one constructor"));
          }
          ctor_member = Some(member);
        }
      }

      // Allocate the optional hidden constructable constructor body.
      let mut ctor_length: u32 = 0;
      let mut ctor_body_inner_func: Option<GcObject> = None;
      let ctor_body_func = if let Some(member) = ctor_member {
        let hir_js::ClassMemberKind::Constructor { body, .. } = &member.kind else {
          return Err(VmError::InvariantViolation(
            "expected constructor member kind for class constructor",
          ));
        };
        if let Some(body_id) = *body {
          // Allocate the compiled function object for the constructor body.
           let body_func =
             self.alloc_user_function_object(
               scope,
               body_id,
               "constructor",
               /* is_arrow */ false,
               /* is_constructable */ true,
              /* name_binding */ None,
              EcmaFunctionKind::ClassMember,
            )?;
          ctor_body_inner_func = Some(body_func);
          ctor_length = scope.heap().get_function(body_func)?.length;

          // Wrap it in a constructable native function so `class_constructor_construct` can delegate
          // via `vm.construct`.
          let intr = self
            .vm
            .intrinsics()
            .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

          // Root the body function across wrapper allocation in case it triggers GC.
          let mut wrapper_scope = scope.reborrow();
          wrapper_scope.push_root(Value::Object(body_func))?;
          let wrapper_name = wrapper_scope.alloc_string("constructor")?;
          wrapper_scope.push_root(Value::String(wrapper_name))?;

          let construct_id = self
            .vm
            .register_native_construct(compiled_constructor_body_construct)?;
          let wrapper = wrapper_scope.alloc_native_function_with_slots(
            intr.class_constructor_call(),
            Some(construct_id),
            wrapper_name,
            ctor_length,
            &[Value::Object(body_func)],
          )?;

          // Best-effort `[[Prototype]]` / `[[Realm]]` metadata.
          wrapper_scope.push_root(Value::Object(wrapper))?;
          wrapper_scope
            .heap_mut()
            .object_set_prototype(wrapper, Some(intr.function_prototype()))?;
          wrapper_scope
            .heap_mut()
            .set_function_realm(wrapper, self.env.global_object())?;
          if let Some(realm) = self.vm.current_realm() {
            wrapper_scope.heap_mut().set_function_job_realm(wrapper, realm)?;
          }
          if let Some(script_or_module) = self.vm.get_active_script_or_module() {
            let token = self.vm.intern_script_or_module(script_or_module)?;
            wrapper_scope
              .heap_mut()
              .set_function_script_or_module_token(wrapper, Some(token))?;
          }
          Some(wrapper)
        } else {
          None
        }
       } else {
         None
       };

        let func_obj = self.create_class_constructor_object(
          scope,
          func_name,
          ctor_length,
          ctor_body_func,
          super_value,
          /* instance_field_count */ 0,
        )?;

        // `NamedEvaluation` assigns inferred names to anonymous class expressions in specific syntactic
        // positions (e.g. `{ key: class {} }`).
       //
       // This must happen *before* defining class elements: a class can define a `static name() {}`
       // method which should override the constructor's initial `"name"` property. Setting the name
       // after class evaluation would overwrite the method.
        if func_name.is_empty() {
          if let Some(name_key) = inferred_name {
            let mut name_scope = scope.reborrow();
            name_scope.push_root(Value::Object(func_obj))?;
            root_property_key(&mut name_scope, name_key)?;
            crate::function_properties::set_function_name(&mut name_scope, func_obj, name_key, None)?;
          }
        }

        // If the class has an explicit `constructor(...) { ... }` body, annotate that hidden function
        // object so `[[Construct]]` can implement derived `super()` semantics (and, in particular, so
        // derived constructors that never initialize `this` throw the correct ReferenceError).
        if let Some(body_func) = ctor_body_func {
          scope.heap_mut().set_function_data(
            body_func,
            FunctionData::ClassConstructorBody {
              class_constructor: func_obj,
            },
          )?;
        }

       // Initialize the requested binding now that the class constructor object exists.
       if let Some(name) = binding_name {
         let mut init_scope = scope.reborrow();
         init_scope.push_root(Value::Object(func_obj))?;
        init_scope
          .heap_mut()
          .env_initialize_binding(class_env, name, Value::Object(func_obj))?;
      }

      // Extract the prototype object created by `make_constructor`.
      let mut class_scope = scope.reborrow();
      class_scope.push_root(Value::Object(func_obj))?;
      let prototype_key_s = class_scope.alloc_string("prototype")?;
      class_scope.push_root(Value::String(prototype_key_s))?;
      let prototype_key = PropertyKey::from_string(prototype_key_s);
      let Some(prototype_desc) = class_scope.heap().get_own_property(func_obj, prototype_key)? else {
        return Err(VmError::InvariantViolation(
          "class constructor missing prototype property",
        ));
      };
      let crate::property::PropertyKind::Data { value, .. } = prototype_desc.kind else {
        return Err(VmError::InvariantViolation(
          "class constructor prototype property is not a data property",
        ));
      };
      let Value::Object(prototype_obj) = value else {
        return Err(VmError::InvariantViolation(
          "class constructor prototype property is not an object",
        ));
      };
      class_scope.push_root(Value::Object(prototype_obj))?;

      if let Some(body_func) = ctor_body_inner_func {
        class_scope
          .heap_mut()
          .set_function_home_object(body_func, Some(prototype_obj))?;
      }

      // Per ECMAScript, class constructors have a non-writable `prototype` property.
      class_scope.define_property_or_throw(
        func_obj,
        prototype_key,
        PropertyDescriptorPatch {
          writable: Some(false),
          ..Default::default()
         },
       )?;

       // Wire the instance prototype chain for derived classes.
       //
       // - base class: `prototype.[[Prototype]] = %Object.prototype%`
       // - `extends null`: `prototype.[[Prototype]] = null`
       // - derived class: `prototype.[[Prototype]] = super.prototype`
       let intr = self
         .vm
         .intrinsics()
         .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
       match super_value {
         Value::Undefined => {
           class_scope
             .heap_mut()
             .object_set_prototype(prototype_obj, Some(intr.object_prototype()))?;
         }
         Value::Null => {
           class_scope.heap_mut().object_set_prototype(prototype_obj, None)?;
         }
         Value::Object(super_ctor) => {
           let proto_parent = {
             // `Get(superCtor, "prototype")` (Proxy-aware / accessor-aware).
             let mut proto_scope = class_scope.reborrow();
             proto_scope.push_root(Value::Object(super_ctor))?;
             let proto_key_s = proto_scope.alloc_string("prototype")?;
             proto_scope.push_root(Value::String(proto_key_s))?;
             let proto_key = PropertyKey::from_string(proto_key_s);
             let proto_value = proto_scope.ordinary_get_with_host_and_hooks(
               self.vm,
               &mut *self.host,
               &mut *self.hooks,
               super_ctor,
               proto_key,
               Value::Object(super_ctor),
             )?;
             match proto_value {
               Value::Object(o) => Some(o),
               Value::Null => None,
               _ => {
                 return Err(throw_type_error(
                   self.vm,
                   &mut proto_scope,
                   "Class extends value does not have a valid prototype property",
                 )?)
               }
             }
           };
           class_scope
             .heap_mut()
             .object_set_prototype(prototype_obj, proto_parent)?;
         }
         _ => {
           return Err(VmError::InvariantViolation(
             "class constructor super value is not undefined, null, or object",
           ))
         }
       }

        // Define prototype and static methods/accessors.
        let mut static_blocks: Vec<hir_js::BodyId> = Vec::new();
        for member in class_meta.members.iter() {
         self.vm.tick()?;

         match &member.kind {
           hir_js::ClassMemberKind::Constructor { .. } => {
            // The actual `constructor(...) { ... }` body is represented by the class constructor
            // object itself (and its hidden body function).
            continue;
          }
           hir_js::ClassMemberKind::Field { .. } => {
             return Err(VmError::Unimplemented("class fields"));
           }
           hir_js::ClassMemberKind::StaticBlock { body, .. } => {
             static_blocks.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
             static_blocks.push(*body);
             continue;
           }
           hir_js::ClassMemberKind::Method {
             body,
             key,
             kind,
            ..
          } => {
            let target_obj = if member.static_ { func_obj } else { prototype_obj };

            let mut member_scope = class_scope.reborrow();
            member_scope.push_root(Value::Object(target_obj))?;

            let key = self.eval_class_member_key(&mut member_scope, class_body, key)?;
            root_property_key(&mut member_scope, key)?;

            let Some(body_id) = body else {
              return Err(VmError::Unimplemented(
                "class methods without bodies (hir-js compiled path)",
              ));
            };

            // Allocate the method function object (non-constructable), and apply `SetFunctionName`
            // based on the property key. This matches interpreter semantics and handles getter/setter
            // prefixes and Symbol keys.
            let func_obj_member = self.alloc_user_function_object(
              &mut member_scope,
              *body_id,
              /* name */ "",
              /* is_arrow */ false,
              /* is_constructable */ false,
              /* name_binding */ None,
              EcmaFunctionKind::ClassMember,
            )?;
            member_scope
              .heap_mut()
              .set_function_home_object(func_obj_member, Some(target_obj))?;

            match kind {
              hir_js::ClassMethodKind::Method => {
                crate::function_properties::set_function_name(&mut member_scope, func_obj_member, key, None)?;
                member_scope.define_property_or_throw(
                  target_obj,
                  key,
                  PropertyDescriptorPatch {
                    value: Some(Value::Object(func_obj_member)),
                    writable: Some(true),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                  },
                )?;
              }
              hir_js::ClassMethodKind::Getter => {
                crate::function_properties::set_function_name(
                  &mut member_scope,
                  func_obj_member,
                  key,
                  Some("get"),
                )?;
                member_scope.define_property_or_throw(
                  target_obj,
                  key,
                  PropertyDescriptorPatch {
                    get: Some(Value::Object(func_obj_member)),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                  },
                )?;
              }
              hir_js::ClassMethodKind::Setter => {
                crate::function_properties::set_function_name(
                  &mut member_scope,
                  func_obj_member,
                  key,
                  Some("set"),
                )?;
                member_scope.define_property_or_throw(
                  target_obj,
                  key,
                  PropertyDescriptorPatch {
                    set: Some(Value::Object(func_obj_member)),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                  },
                )?;
              }
            }
           }
         }
       }

       // Evaluate class static blocks after defining methods/accessors, in source order.
       //
       // This matches ECMA-262 `ClassDefinitionEvaluation`, where static initialization elements run
       // in a second pass after the element definition pass.
       for body_id in static_blocks {
         self.vm.tick()?;
         let block_body = hir
           .body(body_id)
           .ok_or(VmError::InvariantViolation("hir body id missing from compiled script"))?;
         self.eval_class_static_block_hir(&mut class_scope, func_obj, block_body)?;
       }

       Ok(func_obj)
     })();

     self.strict = saved_strict;
     result
  }

  fn eval_class_static_block_hir(
    &mut self,
    scope: &mut Scope<'_>,
    receiver: GcObject,
    block_body: &hir_js::Body,
  ) -> Result<(), VmError> {
    // Static blocks are evaluated as strict-mode "method" bodies with `this` bound to the class
    // constructor object and `new.target` set to `undefined`.
    //
    // We implement this by:
    // - creating a fresh var environment whose outer is the current class lexical environment,
    // - creating a function-body lexical environment whose outer is that var environment,
    // - instantiating the statement list, then evaluating it.
    //
    // This mirrors `exec.rs::eval_class_static_block` and prevents `var` declarations inside static
    // blocks from leaking to the surrounding global/function VariableEnvironment.
    let mut block_scope = scope.reborrow();
    block_scope.push_root(Value::Object(receiver))?;

    let saved_this = self.this;
    let saved_this_initialized = self.this_initialized;
    let saved_new_target = self.new_target;
    let saved_home_object = self.home_object;
    let saved_lex = self.env.lexical_env();
    let saved_var_env = self.env.var_env();

    let res: Result<Flow, VmError> = (|| {
      self.this = Value::Object(receiver);
      self.this_initialized = true;
      self.new_target = Value::Undefined;
      self.home_object = Some(receiver);

      let var_env = block_scope.env_create(Some(saved_lex))?;
      let body_lex = block_scope.env_create(Some(var_env))?;
      self.env.set_var_env(VarEnv::Env(var_env));
      self.env.set_lexical_env(block_scope.heap_mut(), body_lex);

      // Some early errors are still checked at runtime during instantiation so invalid declarations
      // do not partially pollute the static block environments.
      self.early_error_missing_initializers_in_stmt_list(block_body, block_body.root_stmts.as_slice())?;
      self.instantiate_var_decls(&mut block_scope, block_body, block_body.root_stmts.as_slice())?;
      // Class bodies (including static blocks) are always strict mode, so Annex B block-function
      // hoisting does not apply here.
      self.instantiate_function_decls(
        &mut block_scope,
        block_body,
        block_body.root_stmts.as_slice(),
        /* annex_b */ false,
      )?;
      self.instantiate_lexical_decls(
        &mut block_scope,
        block_body,
        block_body.root_stmts.as_slice(),
        self.env.lexical_env(),
      )?;

      self.eval_stmt_list(&mut block_scope, block_body, block_body.root_stmts.as_slice())
    })();

    // Restore the surrounding class evaluation context regardless of how the block completes.
    self.env.set_lexical_env(block_scope.heap_mut(), saved_lex);
    self.env.set_var_env(saved_var_env);
    self.this = saved_this;
    self.this_initialized = saved_this_initialized;
    self.new_target = saved_new_target;
    self.home_object = saved_home_object;

    match res? {
      Flow::Normal(_) => Ok(()),
      Flow::Return(_) => Err(VmError::InvariantViolation(
        "class static block produced Return flow (early errors should prevent this)",
      )),
      Flow::Break(..) => Err(VmError::InvariantViolation(
        "class static block produced Break flow (early errors should prevent this)",
      )),
      Flow::Continue(..) => Err(VmError::InvariantViolation(
        "class static block produced Continue flow (early errors should prevent this)",
      )),
    }
  }

  fn eval_class_member_key(
    &mut self,
    scope: &mut Scope<'_>,
    class_body: &hir_js::Body,
    key: &hir_js::ClassMemberKey,
  ) -> Result<PropertyKey, VmError> {
    match key {
      hir_js::ClassMemberKey::Ident(name_id) => {
        let name = self.resolve_name(*name_id)?;
        Ok(PropertyKey::from_string(scope.alloc_string(name.as_str())?))
      }
      hir_js::ClassMemberKey::String(s) => Ok(PropertyKey::from_string(scope.alloc_string(s)?)),
      hir_js::ClassMemberKey::Number(raw) => {
        // Numeric literal property names are canonicalized in ECMAScript:
        // `ToPropertyKey(NumericLiteral)` uses `ToString(ToNumber(literal))`.
        //
        // HIR stores the source text of numeric literal property names (e.g. `0x10`, `1_0`) so the
        // compiled path must perform this canonicalization at runtime.
        //
        // Note: `parse_js::num::JsNumber::from_literal` is not tick-aware, so charge fuel based on
        // input length before parsing to avoid long uninterruptible work for pathological literals.
        if raw.len() > crate::tick::DEFAULT_TICK_EVERY {
          let mut i = crate::tick::DEFAULT_TICK_EVERY;
          while i < raw.len() {
            self.vm.tick()?;
            i = i.saturating_add(crate::tick::DEFAULT_TICK_EVERY);
          }
        }

        let n = JsNumber::from_literal(raw).map(|n| n.0).unwrap_or(f64::NAN);
        let key_s = scope.heap_mut().to_string(Value::Number(n))?;
        Ok(PropertyKey::from_string(key_s))
      }
      hir_js::ClassMemberKey::Computed(expr_id) => {
        let v = self.eval_expr(scope, class_body, *expr_id)?;
        // `ToPropertyKey` can invoke user code via `ToPrimitive`, so root the intermediate value
        // across conversion.
        let mut key_scope = scope.reborrow();
        key_scope.push_root(v)?;
        Ok(key_scope.to_property_key(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          v,
        )?)
      }
      hir_js::ClassMemberKey::Private(_) => Err(VmError::Unimplemented("class private elements")),
    }
  }

  fn create_class_constructor_object(
    &mut self,
    scope: &mut Scope<'_>,
    name: &str,
    length: u32,
    constructor_body: Option<GcObject>,
    super_value: Value,
    instance_field_count: usize,
  ) -> Result<GcObject, VmError> {
    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

    // Root the optional constructor body function and superclass value before allocating any
    // strings/function objects in case those allocations trigger GC.
    let mut init_scope = scope.reborrow();
    if let Some(body) = constructor_body {
      init_scope.push_root(Value::Object(body))?;
    }
    init_scope.push_root(super_value)?;

    let name_s = init_scope.alloc_string(name)?;
    let slots_len = crate::class_fields::CLASS_CTOR_SLOT_INSTANCE_FIELDS_START
      .saturating_add(instance_field_count.saturating_mul(2));
    let mut slots_vec: Vec<Value> = Vec::new();
    slots_vec
      .try_reserve_exact(slots_len)
      .map_err(|_| VmError::OutOfMemory)?;
    slots_vec.push(constructor_body.map(Value::Object).unwrap_or(Value::Undefined));
    slots_vec.push(super_value);
    slots_vec.resize(slots_len, Value::Undefined);

    let func_obj = init_scope.alloc_native_function_with_slots(
      intr.class_constructor_call(),
      Some(intr.class_constructor_construct()),
      name_s,
      length,
      &slots_vec,
    )?;
    init_scope.push_root(Value::Object(func_obj))?;

    // Per ECMA-262 `ClassDefinitionEvaluation`:
    // - base classes: `F.[[Prototype]] = %Function.prototype%`
    // - derived classes: `F.[[Prototype]] = superCtor` (static inheritance)
    let proto_parent = match super_value {
      Value::Object(super_ctor) => super_ctor,
      Value::Undefined | Value::Null => intr.function_prototype(),
      _ => {
        return Err(VmError::InvariantViolation(
          "class constructor super value is not undefined, null, or object",
        ))
      }
    };
    init_scope
      .heap_mut()
      .object_set_prototype(func_obj, Some(proto_parent))?;
    init_scope
      .heap_mut()
      .set_function_realm(func_obj, self.env.global_object())?;
    if let Some(realm) = self.vm.current_realm() {
      init_scope.heap_mut().set_function_job_realm(func_obj, realm)?;
    }
    if let Some(script_or_module) = self.vm.get_active_script_or_module() {
      let token = self.vm.intern_script_or_module(script_or_module)?;
      init_scope
        .heap_mut()
        .set_function_script_or_module_token(func_obj, Some(token))?;
    }
    Ok(func_obj)
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

fn bigint_shift_count(value: &crate::JsBigInt) -> (bool, u64) {
  match value.try_to_i128() {
    Some(shift_i) => {
      // `(-i128::MIN)` overflows, so handle it explicitly.
      let shift_mag: u128 = if shift_i == i128::MIN {
        1u128 << 127
      } else if shift_i < 0 {
        (-shift_i) as u128
      } else {
        shift_i as u128
      };
      let shift = u64::try_from(shift_mag).unwrap_or(u64::MAX);
      (shift_i < 0, shift)
    }
    // If the shift count does not fit into an i128, it is either extremely large or extremely
    // negative. Saturate to `u64::MAX` so:
    // - huge right shifts quickly produce 0/-1,
    // - huge left shifts attempt to allocate and surface OOM.
    None => (value.is_negative(), u64::MAX),
  }
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
  this_initialized: bool,
  new_target: Value,
  home_object: Option<GcObject>,
  args: &[Value],
  class_constructor: Option<GcObject>,
  derived_constructor: bool,
  this_root_idx: Option<usize>,
) -> Result<Value, VmError> {
  env.set_source_info(func.script.source.clone(), 0, 0);

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
    // `Vm::call_user_function` skips `RuntimeEnv::teardown` for async functions so the compiled
    // executor can eventually support suspension by transferring ownership of the env root to an
    // async continuation. Until async compiled functions are implemented, this early-return must
    // still tear down the env root to avoid leaking persistent roots.
    env.teardown(scope.heap_mut());
    return Err(VmError::Unimplemented("async functions (hir-js compiled path)"));
  }

  let mut evaluator = HirEvaluator {
    vm,
    host,
    hooks,
    env,
    strict,
    this,
    this_initialized,
    class_constructor,
    derived_constructor,
    this_root_idx,
    new_target,
    home_object,
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
  env.set_source_info(script.source.clone(), 0, 0);

  let global_object = env.global_object();
  let mut evaluator = HirEvaluator {
    vm,
    host,
    hooks,
    env,
    // Best-effort strict detection.
    strict: false,
    this: Value::Object(global_object),
    this_initialized: true,
    class_constructor: None,
    derived_constructor: false,
    this_root_idx: None,
    new_target: Value::Undefined,
    home_object: None,
    script: script.clone(),
  };

  let hir = script.hir.as_ref();
  let body = hir
    .body(hir.root_body())
    .ok_or(VmError::InvariantViolation("compiled script root body not found"))?;

  evaluator.strict = evaluator.detect_use_strict_directive(body)?;

  // Some early errors are still checked at runtime during instantiation so invalid declarations do
  // not partially pollute the global environment.
  evaluator.early_error_missing_initializers_in_stmt_list(body, body.root_stmts.as_slice())?;

  // Hoist `var` declarations so lookups before declaration see `undefined` instead of throwing
  // ReferenceError.
  evaluator.instantiate_var_decls(scope, body, body.root_stmts.as_slice())?;

  // Hoist function declarations so they can be called before their declaration statement.
  evaluator.instantiate_function_decls(scope, body, body.root_stmts.as_slice(), /* annex_b */ false)?;

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

/// Execute a pre-compiled module body (HIR) in an already-instantiated module environment.
///
/// This is analogous to `exec::run_module` for the compiled executor: it evaluates the module's
/// statement list in strict mode with `this = undefined` and an active `ScriptOrModule::Module`
/// execution context so `import.meta` and dynamic `import()` can resolve module-scoped state.
///
/// Note: the compiled executor does **not** currently support top-level await; callers must fall
/// back to the AST async evaluator for modules with `[[HasTLA]] = true`.
pub(crate) fn run_compiled_module(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  global_object: GcObject,
  realm_id: RealmId,
  module_id: ModuleId,
  module_env: GcEnv,
  script: Arc<CompiledScript>,
) -> Result<(), VmError> {
  let exec_ctx = ExecutionContext {
    realm: realm_id,
    script_or_module: Some(ScriptOrModule::Module(module_id)),
  };
  vm.push_execution_context(exec_ctx)?;

  let result = (|| -> Result<(), VmError> {
    let mut env =
      RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, module_env, module_env)?;
    env.set_source_info(script.source.clone(), 0, 0);

    let result = (|| -> Result<(), VmError> {
      let source = script.source.clone();
      let (line, col) = source.line_col(0);
      let frame = StackFrame {
        function: None,
        source: source.name.clone(),
        line,
        col,
      };
      let mut vm_frame = vm.enter_frame(frame)?;

      let mut evaluator = HirEvaluator {
        vm: &mut *vm_frame,
        host,
        hooks,
        env: &mut env,
        // Modules are always strict mode.
        strict: true,
        // Per ECMA-262, module top-level `this` is `undefined`.
        this: Value::Undefined,
        this_initialized: true,
        class_constructor: None,
        derived_constructor: false,
        this_root_idx: None,
        new_target: Value::Undefined,
        home_object: None,
        script: script.clone(),
      };

      let hir = script.hir.as_ref();
      let body = hir
        .body(hir.root_body())
        .ok_or(VmError::InvariantViolation("compiled module root body not found"))?;

      let eval_res = evaluator.eval_stmt_list(scope, body, body.root_stmts.as_slice());
      match eval_res {
        Ok(Flow::Normal(_)) => Ok(()),
        Ok(Flow::Return(_)) => Err(VmError::InvariantViolation(
          "module evaluation produced Return completion (early errors should prevent this)",
        )),
        Ok(Flow::Break(..)) => Err(VmError::InvariantViolation(
          "module evaluation produced Break completion (early errors should prevent this)",
        )),
        Ok(Flow::Continue(..)) => Err(VmError::InvariantViolation(
          "module evaluation produced Continue completion (early errors should prevent this)",
        )),
        Err(err) if err.is_throw_completion() => {
          // Coerce internal helper errors into a JS throw value when intrinsics exist, so module
          // evaluation failures can be represented as thrown values with a captured stack.
          let err = crate::vm::coerce_error_to_throw(&*vm_frame, scope, err);
          match err {
            VmError::Throw(value) => Err(VmError::ThrowWithStack {
              value,
              stack: vm_frame.capture_stack(),
            }),
            VmError::ThrowWithStack { .. } => Err(err),
            other => Err(other),
          }
        }
        Err(err) => Err(err),
      }
    })();

    env.teardown(scope.heap_mut());
    result
  })();

  let popped = vm.pop_execution_context();
  debug_assert_eq!(popped, Some(exec_ctx));
  debug_assert!(popped.is_some(), "module execution popped no execution context");

  result
}

#[cfg(test)]
mod async_function_ast_fallback_tests {
  use crate::function::CallHandler;
  use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};
  use std::sync::Arc;

  #[test]
  fn compiled_script_with_async_function_falls_back_to_ast_executor() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let mut script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        async function f() { return 1; }
        f;
      "#,
    )?;
    // The compiled (HIR) execution path does not yet support executing async function bodies, so
    // `CompiledScript` conservatively opts into AST fallback when it sees `async function` syntax.
    // For this unit test we only care about *allocation* of the async function object during HIR
    // execution, so force the compiled path.
    Arc::get_mut(&mut script)
      .expect("compiled script Arc should be uniquely owned in this unit test")
      .requires_ast_fallback = false;

    assert!(script.contains_async_functions);
    assert!(!script.contains_generators);
    assert!(!script.contains_async_generators);
    assert!(
      script.requires_ast_fallback,
      "async function bodies are not yet supported by the compiled (HIR) executor, so compiled scripts must fall back to the AST interpreter"
    );

    let result = rt.exec_compiled_script(script)?;
    let Value::Object(func_obj) = result else {
      panic!("expected async function object, got {result:?}");
    };

    let call_handler = rt.heap.get_function_call_handler(func_obj)?;
    assert!(
      matches!(call_handler, CallHandler::Ecma(_)),
      "expected async function to be allocated as an interpreter-backed ECMAScript function when falling back from compiled scripts, got {call_handler:?}"
    );
    Ok(())
  }
}
