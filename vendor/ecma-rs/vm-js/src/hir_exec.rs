use crate::code::{CompiledFunctionRef, CompiledScript};
use crate::conversion_ops::ToPrimitiveHint;
use crate::exec::{
  perform_direct_eval_with_host_and_hooks, AsyncContinuation, ModuleTlaStepResult, ResolvedBinding,
  RuntimeEnv, VarEnv,
};
use crate::fallible_format;
use crate::function::FunctionData;
use crate::function::ThisMode;
use crate::meta_properties::MetaPropertyContext;
use crate::for_in::ForInEnumerator;
use crate::iterator;
use crate::module_loading;
use crate::property::{PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind};
use crate::tick::vec_try_extend_from_slice_with_ticks;
use crate::vm::{EcmaFunctionKind, VmAsyncContinuation};
use crate::{
  EnvBinding, ExecutionContext, GcBigInt, GcEnv, GcObject, ModuleId, RealmId, RootId, Scope,
  ScriptOrModule, StackFrame, Value, Vm, VmError, VmHost, VmHostHooks,
};
use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};
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

  let (func_ref, is_strict, realm, outer, home_object, meta_property_context) = {
    let call_handler = scope.heap().get_function_call_handler(body_func)?;
    let crate::function::CallHandler::User(func_ref) = call_handler else {
      return Err(VmError::InvariantViolation(
        "compiled constructor body slot is not a compiled user function",
      ));
    };
    let f = scope.heap().get_function(body_func)?;
    (
      func_ref,
      f.is_strict,
      f.realm,
      f.closure_env,
      f.home_object,
      f.meta_property_context,
    )
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
    let mut env =
      RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, func_env, func_env)?;
    env.set_meta_property_context(meta_property_context);

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
    // Represent `this` as a heap-owned shared state object so nested arrow functions and direct
    // eval code can observe initialization when `super()` is called.
    //
    // Root inputs across env creation and body execution in case either triggers GC.
    let mut scope = scope.reborrow();
    scope.push_roots(&[Value::Object(body_func), new_target])?;
    let class_ctor = class_constructor.ok_or(VmError::InvariantViolation(
      "derived constructor missing containing class constructor reference",
    ))?;
    // Derived constructors have an uninitialized `this` binding until `super()` returns.
    //
    // Represent `this` as a shared heap state object so nested arrow functions and direct eval code
    // can observe initialization when `super()` is called (even across the compiled/AST boundary).
    let state_obj = scope.alloc_derived_constructor_state(class_ctor)?;
    scope.push_root(Value::Object(state_obj))?;

    let func_env = scope.env_create(outer)?;
    let mut env =
      RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, func_env, func_env)?;
    env.set_meta_property_context(meta_property_context);

    let result = run_compiled_function(
      vm,
      &mut scope,
      host,
      hooks,
      &mut env,
      func_ref,
      is_strict,
      Value::Object(state_obj),
      /* this_initialized */ false,
      new_target,
      home_object,
      args,
      class_constructor,
      /* derived_constructor */ true,
      /* this_root_idx */ None,
    );

    env.teardown(scope.heap_mut());

      match result? {
        // If the derived constructor explicitly returns an object, that becomes the result of
        // construction (even if `super()` was never called).
        Value::Object(o) => Ok(Value::Object(o)),
        // `return;` / no explicit return (or an explicit `return undefined`): yield `this`.
        //
        // If `this` was never initialized via `super()`, this must throw a ReferenceError.
        Value::Undefined => {
        let state = scope.heap().get_derived_constructor_state(state_obj)?;
        match state.this_value {
          Some(this_obj) => Ok(Value::Object(this_obj)),
          None => Err(throw_reference_error(
            vm,
            &mut scope,
            "Derived constructor did not initialize `this` via super()",
          )?),
        }
      }
      // ECMA-262: derived constructors that return a non-object *other than `undefined`* must throw
      // a TypeError rather than falling back to `this`.
      _ => Err(VmError::TypeError(
        "Derived constructors may only return an object or undefined",
      )),
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
  /// A `super` property reference (`super.prop` or `super[expr]`).
  ///
  /// Super references differ from ordinary property references in that the `[[Get]]`/`[[Set]]`
  /// operation is performed on the prototype of the current function's `[[HomeObject]]`, but with
  /// the receiver (`this` value) set to the current `this` binding.
  ///
  /// `super_base` is optional to preserve spec evaluation order:
  /// - Reference evaluation computes `super_base = Object.getPrototypeOf(home_object)` and can
  ///   observe `null` (e.g. `class C extends null { ... }`).
  /// - For `super.prop = rhs`, `rhs` is evaluated before the `[[Set]]` is attempted; if
  ///   `super_base` is `null`, the TypeError is thrown *after* evaluating `rhs`.
  SuperProperty {
    super_base: Option<GcObject>,
    receiver: Value,
    key: PropertyKey,
  },
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

fn throw_syntax_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let value = crate::new_syntax_error_object(scope, &intr, message)?;
  Ok(VmError::Throw(value))
}

#[inline]
fn global_var_binding_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: false,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

#[inline]
fn patch_stack_top_frame_best_effort(
  stack: &mut Vec<StackFrame>,
  source: Arc<str>,
  line: u32,
  col: u32,
) {
  if let Some(top) = stack.first_mut() {
    top.source = source;
    top.line = line;
    top.col = col;
    return;
  }

  // Avoid aborting the process on allocator OOM: reserve fallibly.
  if stack.try_reserve(1).is_ok() {
    stack.push(StackFrame {
      function: None,
      source,
      line,
      col,
    });
  }
}

fn finalize_throw_with_stack_at_source_offset(
  vm: &Vm,
  scope: &mut Scope<'_>,
  source: &crate::SourceText,
  offset: u32,
  err: VmError,
) -> VmError {
  if !err.is_throw_completion() {
    return err;
  }

  let (line, col) = source.line_col(offset);
  let err = crate::vm::coerce_error_to_throw(vm, scope, err);
  match err {
    VmError::Throw(value) => {
      let mut stack = vm.capture_stack();
      patch_stack_top_frame_best_effort(&mut stack, source.name.clone(), line, col);
      crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
      VmError::ThrowWithStack { value, stack }
    }
    VmError::ThrowWithStack { value, mut stack } => {
      if stack.first().is_none() || stack.first().is_some_and(|top| top.line == 0) {
        patch_stack_top_frame_best_effort(&mut stack, source.name.clone(), line, col);
      }
      crate::error_object::attach_stack_property_for_throw(scope, value, &stack);
      VmError::ThrowWithStack { value, stack }
    }
    other => other,
  }
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
  /// For derived constructors, the compiled executor represents `this` as a heap-owned shared cell
  /// (`DerivedConstructorState`) so nested arrow functions and direct eval code can observe
  /// initialization across the compiled/AST boundary. This root slot keeps that cell alive for the
  /// duration of the constructor body.
  this_root_idx: Option<usize>,
  new_target: Value,
  /// Whether direct eval code is permitted to contain the `new.target` meta property.
  ///
  /// Per ECMAScript, `new.target` is only syntactically valid in direct eval when the direct eval
  /// call occurs in **non-arrow** function code.
  ///
  /// This is separate from the runtime `new_target` value: even if an arrow function captures a
  /// lexical `new.target`, `eval("new.target")` must still throw a SyntaxError.
  allow_new_target_in_eval: bool,
  home_object: Option<GcObject>,
  script: Arc<CompiledScript>,
}

impl<'vm> HirEvaluator<'vm> {
  fn hir(&self) -> &hir_js::LowerResult {
    self.script.hir.as_ref()
  }

  fn is_default_export_anonymous_class_decl(&self, def_id: hir_js::DefId) -> bool {
    self.script.hir.hir.exports.iter().any(|export| {
      let hir_js::ExportKind::Default(default) = &export.kind else {
        return false;
      };
      matches!(
        &default.value,
        hir_js::ExportDefaultValue::Class { def, name: None, .. } if *def == def_id
      )
    })
  }

  fn is_default_export_anonymous_function_decl(&self, def_id: hir_js::DefId) -> bool {
    self.script.hir.hir.exports.iter().any(|export| {
      let hir_js::ExportKind::Default(default) = &export.kind else {
        return false;
      };
      matches!(
        &default.value,
        hir_js::ExportDefaultValue::Function { def, name: None, .. } if *def == def_id
      )
    })
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

  fn super_base_value(&mut self, scope: &mut Scope<'_>) -> Result<Value, VmError> {
    let Some(home_object) = self.home_object else {
      return Err(VmError::InvariantViolation(
        "super property reference missing [[HomeObject]]",
      ));
    };
    // Root the home object across prototype lookup in case host hooks allocate / trigger GC.
    scope.push_root(Value::Object(home_object))?;
    let proto = scope.get_prototype_of_with_host_and_hooks(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      home_object,
    )?;
    Ok(proto.map(Value::Object).unwrap_or(Value::Null))
  }

  /// Returns the current `this` value for evaluation/receiver purposes.
  ///
  /// In derived class constructors (and arrow/eval code lexically nested within them), `this` is
  /// uninitialized until `super()` returns. The compiled executor represents that shared state as a
  /// heap-owned [`crate::heap::DerivedConstructorState`] cell so it can be observed across
  /// boundaries (e.g. `eval("super()")` inside a derived constructor body).
  ///
  /// When `self.this` is such a cell, this method unwraps the initialized `this` object (or throws
  /// a ReferenceError if it is still uninitialized).
  fn resolve_this_binding(&mut self, scope: &mut Scope<'_>) -> Result<Value, VmError> {
    if let Value::Object(obj) = self.this {
      if scope.heap().is_derived_constructor_state(obj) {
        let state = scope.heap().get_derived_constructor_state(obj)?;
        if let Some(this_obj) = state.this_value {
          return Ok(Value::Object(this_obj));
        }
        return Err(throw_reference_error(
          self.vm,
          scope,
          "Must call super constructor in derived class before accessing 'this'",
        )?);
      }
    }

    if self.derived_constructor && !self.this_initialized {
      return Err(throw_reference_error(
        self.vm,
        scope,
        "Must call super constructor in derived class before accessing 'this'",
      )?);
    }
    Ok(self.this)
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

  fn hir_body_has_await_suspension(&self, body_id: hir_js::BodyId) -> Result<bool, VmError> {
    let mut visited: HashSet<hir_js::BodyId> = HashSet::new();
    self.hir_body_has_await_suspension_inner(body_id, &mut visited)
  }

  fn hir_body_has_await_suspension_inner(
    &self,
    body_id: hir_js::BodyId,
    visited: &mut HashSet<hir_js::BodyId>,
  ) -> Result<bool, VmError> {
    if !visited.insert(body_id) {
      return Ok(false);
    }

    let body = self.get_body(body_id)?;
    match body.kind {
      hir_js::BodyKind::Function => {
        let Some(func_meta) = body.function.as_ref() else {
          return Ok(false);
        };
        match &func_meta.body {
          hir_js::FunctionBody::Expr(expr_id) => self.hir_expr_has_await_suspension(body, *expr_id, visited),
          hir_js::FunctionBody::Block(stmts) => self.hir_stmt_list_has_await_suspension(body, stmts.as_slice(), visited),
        }
      }
      // In non-function bodies, conservatively scan the root statement list.
      hir_js::BodyKind::TopLevel | hir_js::BodyKind::Initializer => {
        self.hir_stmt_list_has_await_suspension(body, body.root_stmts.as_slice(), visited)
      }
      hir_js::BodyKind::Class => {
        if self.hir_stmt_list_has_await_suspension(body, body.root_stmts.as_slice(), visited)? {
          return Ok(true);
        }

        let Some(class_meta) = body.class.as_ref() else {
          return Ok(false);
        };

        if let Some(extends) = class_meta.extends {
          if self.hir_expr_has_await_suspension(body, extends, visited)? {
            return Ok(true);
          }
        }

        for member in class_meta.members.iter() {
          if self.hir_class_member_has_await_suspension(body, member, visited)? {
            return Ok(true);
          }
        }

        Ok(false)
      }
      hir_js::BodyKind::Unknown => Ok(false),
    }
  }

  fn hir_stmt_list_has_await_suspension(
    &self,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
    visited: &mut HashSet<hir_js::BodyId>,
  ) -> Result<bool, VmError> {
    for stmt_id in stmts.iter() {
      if self.hir_stmt_has_await_suspension(body, *stmt_id, visited)? {
        return Ok(true);
      }
    }
    Ok(false)
  }

  fn hir_stmt_has_await_suspension(
    &self,
    body: &hir_js::Body,
    stmt_id: hir_js::StmtId,
    visited: &mut HashSet<hir_js::BodyId>,
  ) -> Result<bool, VmError> {
    let stmt = self.get_stmt(body, stmt_id)?;
    match &stmt.kind {
      hir_js::StmtKind::Expr(expr_id) => self.hir_expr_has_await_suspension(body, *expr_id, visited),
      hir_js::StmtKind::Decl(def_id) => {
        let def = self
          .hir()
          .def(*def_id)
          .ok_or(VmError::InvariantViolation("hir def id missing from compiled script"))?;
        let Some(decl_body_id) = def.body else {
          return Ok(false);
        };
        let decl_body = self.get_body(decl_body_id)?;
        match decl_body.kind {
          // Function declarations evaluate to function objects; their bodies are not executed here.
          hir_js::BodyKind::Function => Ok(false),
          // Class declarations (and some synthetic declarations like `export default <expr>`) execute
          // their statement lists when evaluated.
          hir_js::BodyKind::Class | hir_js::BodyKind::TopLevel | hir_js::BodyKind::Initializer => {
            self.hir_body_has_await_suspension_inner(decl_body_id, visited)
          }
          hir_js::BodyKind::Unknown => Ok(false),
        }
      }
      hir_js::StmtKind::Return(expr) => match expr {
        Some(expr_id) => self.hir_expr_has_await_suspension(body, *expr_id, visited),
        None => Ok(false),
      },
      hir_js::StmtKind::Block(stmts) => self.hir_stmt_list_has_await_suspension(body, stmts.as_slice(), visited),
      hir_js::StmtKind::If {
        test,
        consequent,
        alternate,
      } => {
        if self.hir_expr_has_await_suspension(body, *test, visited)? {
          return Ok(true);
        }
        if self.hir_stmt_has_await_suspension(body, *consequent, visited)? {
          return Ok(true);
        }
        if let Some(alt) = alternate {
          if self.hir_stmt_has_await_suspension(body, *alt, visited)? {
            return Ok(true);
          }
        }
        Ok(false)
      }
      hir_js::StmtKind::While { test, body: inner } => {
        Ok(
          self.hir_expr_has_await_suspension(body, *test, visited)?
            || self.hir_stmt_has_await_suspension(body, *inner, visited)?,
        )
      }
      hir_js::StmtKind::DoWhile { test, body: inner } => {
        Ok(
          self.hir_stmt_has_await_suspension(body, *inner, visited)?
            || self.hir_expr_has_await_suspension(body, *test, visited)?,
        )
      }
      hir_js::StmtKind::For {
        init,
        test,
        update,
        body: inner,
      } => {
        if let Some(init) = init {
          match init {
            hir_js::ForInit::Expr(expr_id) => {
              if self.hir_expr_has_await_suspension(body, *expr_id, visited)? {
                return Ok(true);
              }
            }
            hir_js::ForInit::Var(var_decl) => {
              if self.hir_var_decl_has_await_suspension(body, var_decl, visited)? {
                return Ok(true);
              }
            }
          }
        }
        if let Some(test) = test {
          if self.hir_expr_has_await_suspension(body, *test, visited)? {
            return Ok(true);
          }
        }
        if let Some(update) = update {
          if self.hir_expr_has_await_suspension(body, *update, visited)? {
            return Ok(true);
          }
        }
        self.hir_stmt_has_await_suspension(body, *inner, visited)
      }
      hir_js::StmtKind::ForIn {
        left,
        right,
        body: inner,
        await_,
        ..
      } => {
        // `for await (...)` loops always suspend (async iteration).
        if *await_ {
          return Ok(true);
        }
        if self.hir_for_head_has_await_suspension(body, left, visited)? {
          return Ok(true);
        }
        if self.hir_expr_has_await_suspension(body, *right, visited)? {
          return Ok(true);
        }
        self.hir_stmt_has_await_suspension(body, *inner, visited)
      }
      hir_js::StmtKind::Switch { discriminant, cases } => {
        if self.hir_expr_has_await_suspension(body, *discriminant, visited)? {
          return Ok(true);
        }
        for case in cases {
          if let Some(test) = case.test {
            if self.hir_expr_has_await_suspension(body, test, visited)? {
              return Ok(true);
            }
          }
          if self.hir_stmt_list_has_await_suspension(body, case.consequent.as_slice(), visited)? {
            return Ok(true);
          }
        }
        Ok(false)
      }
      hir_js::StmtKind::Try {
        block,
        catch,
        finally_block,
      } => {
        if self.hir_stmt_has_await_suspension(body, *block, visited)? {
          return Ok(true);
        }
        if let Some(catch) = catch {
          if let Some(param) = catch.param {
            if self.hir_pat_has_await_suspension(body, param, visited)? {
              return Ok(true);
            }
          }
          if self.hir_stmt_has_await_suspension(body, catch.body, visited)? {
            return Ok(true);
          }
        }
        if let Some(finally) = finally_block {
          if self.hir_stmt_has_await_suspension(body, *finally, visited)? {
            return Ok(true);
          }
        }
        Ok(false)
      }
      hir_js::StmtKind::Throw(expr_id) => self.hir_expr_has_await_suspension(body, *expr_id, visited),
      hir_js::StmtKind::Var(var_decl) => self.hir_var_decl_has_await_suspension(body, var_decl, visited),
      hir_js::StmtKind::Labeled { body: inner, .. } => {
        self.hir_stmt_has_await_suspension(body, *inner, visited)
      }
      hir_js::StmtKind::With { object, body: inner } => {
        Ok(
          self.hir_expr_has_await_suspension(body, *object, visited)?
            || self.hir_stmt_has_await_suspension(body, *inner, visited)?,
        )
      }
      hir_js::StmtKind::Break(_)
      | hir_js::StmtKind::Continue(_)
      | hir_js::StmtKind::Debugger
      | hir_js::StmtKind::Empty => Ok(false),
    }
  }

  fn hir_for_head_has_await_suspension(
    &self,
    body: &hir_js::Body,
    head: &hir_js::ForHead,
    visited: &mut HashSet<hir_js::BodyId>,
  ) -> Result<bool, VmError> {
    match head {
      hir_js::ForHead::Pat(pat) => self.hir_pat_has_await_suspension(body, *pat, visited),
      hir_js::ForHead::Var(var_decl) => self.hir_var_decl_has_await_suspension(body, var_decl, visited),
    }
  }

  fn hir_var_decl_has_await_suspension(
    &self,
    body: &hir_js::Body,
    var_decl: &hir_js::VarDecl,
    visited: &mut HashSet<hir_js::BodyId>,
  ) -> Result<bool, VmError> {
    if matches!(var_decl.kind, hir_js::VarDeclKind::AwaitUsing) {
      return Ok(true);
    }
    for decl in var_decl.declarators.iter() {
      if self.hir_pat_has_await_suspension(body, decl.pat, visited)? {
        return Ok(true);
      }
      if let Some(init) = decl.init {
        if self.hir_expr_has_await_suspension(body, init, visited)? {
          return Ok(true);
        }
      }
    }
    Ok(false)
  }

  fn hir_pat_has_await_suspension(
    &self,
    body: &hir_js::Body,
    pat_id: hir_js::PatId,
    visited: &mut HashSet<hir_js::BodyId>,
  ) -> Result<bool, VmError> {
    let pat = self.get_pat(body, pat_id)?;
    match &pat.kind {
      hir_js::PatKind::Ident(_) => Ok(false),
      hir_js::PatKind::Array(arr) => {
        for elem in arr.elements.iter().flatten() {
          if self.hir_pat_has_await_suspension(body, elem.pat, visited)? {
            return Ok(true);
          }
          if let Some(default_value) = elem.default_value {
            if self.hir_expr_has_await_suspension(body, default_value, visited)? {
              return Ok(true);
            }
          }
        }
        if let Some(rest) = arr.rest {
          if self.hir_pat_has_await_suspension(body, rest, visited)? {
            return Ok(true);
          }
        }
        Ok(false)
      }
      hir_js::PatKind::Object(obj) => {
        for prop in obj.props.iter() {
          if self.hir_object_key_has_await_suspension(body, &prop.key, visited)? {
            return Ok(true);
          }
          if self.hir_pat_has_await_suspension(body, prop.value, visited)? {
            return Ok(true);
          }
          if let Some(default_value) = prop.default_value {
            if self.hir_expr_has_await_suspension(body, default_value, visited)? {
              return Ok(true);
            }
          }
        }
        if let Some(rest) = obj.rest {
          if self.hir_pat_has_await_suspension(body, rest, visited)? {
            return Ok(true);
          }
        }
        Ok(false)
      }
      hir_js::PatKind::Rest(inner) => self.hir_pat_has_await_suspension(body, **inner, visited),
      hir_js::PatKind::Assign {
        target,
        default_value,
      } => {
        Ok(
          self.hir_pat_has_await_suspension(body, *target, visited)?
            || self.hir_expr_has_await_suspension(body, *default_value, visited)?,
        )
      }
      hir_js::PatKind::AssignTarget(expr_id) => self.hir_expr_has_await_suspension(body, *expr_id, visited),
    }
  }

  fn hir_object_key_has_await_suspension(
    &self,
    body: &hir_js::Body,
    key: &hir_js::ObjectKey,
    visited: &mut HashSet<hir_js::BodyId>,
  ) -> Result<bool, VmError> {
    match key {
      hir_js::ObjectKey::Computed(expr_id) => self.hir_expr_has_await_suspension(body, *expr_id, visited),
      _ => Ok(false),
    }
  }

  fn hir_class_member_key_has_await_suspension(
    &self,
    body: &hir_js::Body,
    key: &hir_js::ClassMemberKey,
    visited: &mut HashSet<hir_js::BodyId>,
  ) -> Result<bool, VmError> {
    match key {
      hir_js::ClassMemberKey::Computed(expr_id) => self.hir_expr_has_await_suspension(body, *expr_id, visited),
      _ => Ok(false),
    }
  }

  fn hir_class_member_has_await_suspension(
    &self,
    body: &hir_js::Body,
    member: &hir_js::ClassMember,
    visited: &mut HashSet<hir_js::BodyId>,
  ) -> Result<bool, VmError> {
    match &member.kind {
      hir_js::ClassMemberKind::Constructor { .. } => Ok(false),
      hir_js::ClassMemberKind::Method { key, .. } => self.hir_class_member_key_has_await_suspension(body, key, visited),
      hir_js::ClassMemberKind::Field { key, initializer, .. } => {
        if self.hir_class_member_key_has_await_suspension(body, key, visited)? {
          return Ok(true);
        }
        if let Some(init_body) = initializer {
          if self.hir_body_has_await_suspension_inner(*init_body, visited)? {
            return Ok(true);
          }
        }
        Ok(false)
      }
      hir_js::ClassMemberKind::StaticBlock { body: block_body, .. } => {
        self.hir_body_has_await_suspension_inner(*block_body, visited)
      }
    }
  }

  fn hir_jsx_container_has_await_suspension(
    &self,
    body: &hir_js::Body,
    container: &hir_js::JsxExprContainer,
    visited: &mut HashSet<hir_js::BodyId>,
  ) -> Result<bool, VmError> {
    match container.expr {
      Some(expr_id) => self.hir_expr_has_await_suspension(body, expr_id, visited),
      None => Ok(false),
    }
  }

  fn hir_expr_has_await_suspension(
    &self,
    body: &hir_js::Body,
    expr_id: hir_js::ExprId,
    visited: &mut HashSet<hir_js::BodyId>,
  ) -> Result<bool, VmError> {
    let expr = self.get_expr(body, expr_id)?;
    match &expr.kind {
      hir_js::ExprKind::Await { .. } => Ok(true),
      hir_js::ExprKind::Unary { op, .. } if matches!(op, hir_js::UnaryOp::Await) => Ok(true),

      hir_js::ExprKind::Unary { expr, .. }
      | hir_js::ExprKind::Update { expr, .. }
      | hir_js::ExprKind::Instantiation { expr, .. }
      | hir_js::ExprKind::TypeAssertion { expr, .. }
      | hir_js::ExprKind::NonNull { expr, .. }
      | hir_js::ExprKind::Satisfies { expr, .. } => self.hir_expr_has_await_suspension(body, *expr, visited),

      hir_js::ExprKind::Binary { left, right, .. } => {
        Ok(
          self.hir_expr_has_await_suspension(body, *left, visited)?
            || self.hir_expr_has_await_suspension(body, *right, visited)?,
        )
      }
      hir_js::ExprKind::Assignment { target, value, .. } => {
        Ok(
          self.hir_pat_has_await_suspension(body, *target, visited)?
            || self.hir_expr_has_await_suspension(body, *value, visited)?,
        )
      }
      hir_js::ExprKind::Call(call) => {
        if self.hir_expr_has_await_suspension(body, call.callee, visited)? {
          return Ok(true);
        }
        for arg in call.args.iter() {
          if self.hir_expr_has_await_suspension(body, arg.expr, visited)? {
            return Ok(true);
          }
        }
        Ok(false)
      }
      hir_js::ExprKind::Member(member) => {
        Ok(
          self.hir_expr_has_await_suspension(body, member.object, visited)?
            || self.hir_object_key_has_await_suspension(body, &member.property, visited)?,
        )
      }
      hir_js::ExprKind::Conditional {
        test,
        consequent,
        alternate,
      } => {
        Ok(
          self.hir_expr_has_await_suspension(body, *test, visited)?
            || self.hir_expr_has_await_suspension(body, *consequent, visited)?
            || self.hir_expr_has_await_suspension(body, *alternate, visited)?,
        )
      }
      hir_js::ExprKind::Array(arr) => {
        for elem in arr.elements.iter() {
          let expr_id = match elem {
            hir_js::ArrayElement::Expr(expr_id) | hir_js::ArrayElement::Spread(expr_id) => *expr_id,
            hir_js::ArrayElement::Empty => continue,
          };
          if self.hir_expr_has_await_suspension(body, expr_id, visited)? {
            return Ok(true);
          }
        }
        Ok(false)
      }
      hir_js::ExprKind::Object(obj) => {
        for prop in obj.properties.iter() {
          match prop {
            hir_js::ObjectProperty::KeyValue { key, value, .. } => {
              if self.hir_object_key_has_await_suspension(body, key, visited)? {
                return Ok(true);
              }
              if self.hir_expr_has_await_suspension(body, *value, visited)? {
                return Ok(true);
              }
            }
            hir_js::ObjectProperty::Getter { key, .. } | hir_js::ObjectProperty::Setter { key, .. } => {
              if self.hir_object_key_has_await_suspension(body, key, visited)? {
                return Ok(true);
              }
            }
            hir_js::ObjectProperty::Spread(expr_id) => {
              if self.hir_expr_has_await_suspension(body, *expr_id, visited)? {
                return Ok(true);
              }
            }
          }
        }
        Ok(false)
      }
      hir_js::ExprKind::ClassExpr { body: class_body, .. } => {
        self.hir_body_has_await_suspension_inner(*class_body, visited)
      }
      hir_js::ExprKind::Template(tpl) => {
        for span in tpl.spans.iter() {
          if self.hir_expr_has_await_suspension(body, span.expr, visited)? {
            return Ok(true);
          }
        }
        Ok(false)
      }
      hir_js::ExprKind::TaggedTemplate { tag, template } => {
        if self.hir_expr_has_await_suspension(body, *tag, visited)? {
          return Ok(true);
        }
        for span in template.spans.iter() {
          if self.hir_expr_has_await_suspension(body, span.expr, visited)? {
            return Ok(true);
          }
        }
        Ok(false)
      }
      hir_js::ExprKind::ImportCall { argument, attributes } => {
        if self.hir_expr_has_await_suspension(body, *argument, visited)? {
          return Ok(true);
        }
        if let Some(attrs) = attributes {
          if self.hir_expr_has_await_suspension(body, *attrs, visited)? {
            return Ok(true);
          }
        }
        Ok(false)
      }
      hir_js::ExprKind::Jsx(el) => {
        for attr in el.attributes.iter() {
          match attr {
            hir_js::JsxAttr::Named { value, .. } => {
              if let Some(value) = value {
                if let hir_js::JsxAttrValue::Expression(container) = value {
                  if self.hir_jsx_container_has_await_suspension(body, container, visited)? {
                    return Ok(true);
                  }
                }
              }
            }
            hir_js::JsxAttr::Spread { expr, .. } => {
              if self.hir_expr_has_await_suspension(body, *expr, visited)? {
                return Ok(true);
              }
            }
          }
        }
        for child in el.children.iter() {
          match child {
            hir_js::JsxChild::Element(expr) => {
              if self.hir_expr_has_await_suspension(body, *expr, visited)? {
                return Ok(true);
              }
            }
            hir_js::JsxChild::Expr(container) => {
              if self.hir_jsx_container_has_await_suspension(body, container, visited)? {
                return Ok(true);
              }
            }
            hir_js::JsxChild::Text(_) => {}
          }
        }
        Ok(false)
      }

      // Expressions that either cannot contain `await`, or whose nested bodies are not evaluated
      // when the expression is evaluated (nested function bodies, etc.).
      hir_js::ExprKind::Missing
      | hir_js::ExprKind::Ident(_)
      | hir_js::ExprKind::This
      | hir_js::ExprKind::Super
      | hir_js::ExprKind::Literal(_)
      | hir_js::ExprKind::FunctionExpr { .. }
      | hir_js::ExprKind::Yield { .. }
      | hir_js::ExprKind::ImportMeta
      | hir_js::ExprKind::NewTarget => Ok(false),
    }
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

    // The compiled (HIR) executor does not yet support generator / async-generator bodies
    // (`yield`, `yield*`). Allocate generator functions as interpreter-backed ECMAScript functions
    // so calling them can still execute via the AST interpreter.
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

    let meta_property_context = if is_arrow {
      MetaPropertyContext::for_arrow(self.env.meta_property_context())
    } else {
      match kind {
        EcmaFunctionKind::Decl | EcmaFunctionKind::Expr => MetaPropertyContext::FUNCTION,
        EcmaFunctionKind::ClassFieldInitializer
        | EcmaFunctionKind::ObjectMember
        | EcmaFunctionKind::ClassMember => MetaPropertyContext::METHOD,
      }
    };
    scope
      .heap_mut()
      .set_function_meta_property_context(func_obj, meta_property_context)?;

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

    // Async functions currently execute via the AST interpreter at call-time (see
    // `Vm::call_user_function`).
    //
    // For "trivial" async functions that do not contain any `await`/`for await..of` suspension
    // points, eagerly tag them as `FunctionData::AsyncEcmaFallback` so calls can dispatch directly
    // to the interpreter without re-scanning the compiled HIR.
    if is_async && !is_generator && !self.hir_body_has_await_suspension(body_id)? {
      let code_id = self.vm.register_ecma_function(
        self.env.source(),
        def_span.start,
        def_span.end,
        kind,
      )?;
      scope
        .heap_mut()
        .set_function_data(func_obj, FunctionData::AsyncEcmaFallback { code_id })?;
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
        } else if is_async {
          intr.async_function_prototype()
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

      // `arguments[@@iterator]` is `%Array.prototype.values%` (ECMA-262).
      if let Some(intr) = intr {
        scope.define_property(
          args_obj,
          PropertyKey::Symbol(intr.well_known_symbols().iterator),
          PropertyDescriptor {
            enumerable: false,
            configurable: true,
            kind: PropertyKind::Data {
              value: Value::Object(intr.array_prototype_values()),
              writable: true,
            },
          },
        )?;
      }

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
    // Create lexical bindings (`let`/`const`/`using`/`await using`) for the entire function body
    // statement list up-front so TDZ + shadowing semantics are correct.
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
      // `const x;`, `using x;`, and `await using x;` are syntax errors (missing initializer).
      hir_js::VarDeclKind::Const | hir_js::VarDeclKind::Using | hir_js::VarDeclKind::AwaitUsing => {
        for declarator in &decl.declarators {
          self.vm.tick()?;
          if declarator.init.is_some() {
            continue;
          }

          let pat = self.get_pat(body, declarator.pat)?;
          let message = match decl.kind {
            hir_js::VarDeclKind::Const => "Missing initializer in const declaration",
            hir_js::VarDeclKind::Using => "Missing initializer in using declaration",
            hir_js::VarDeclKind::AwaitUsing => "Missing initializer in await using declaration",
            _ => unreachable!(),
          };
          let diag = diagnostics::Diagnostic::error(
            "VMJS0002",
            message,
            diagnostics::Span {
              file: diagnostics::FileId(0),
              range: pat.span,
            },
          );
          return Err(VmError::Syntax(vec![diag]));
        }
      }
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

  fn has_restricted_global_property(
    &mut self,
    scope: &mut Scope<'_>,
    global_object: GcObject,
    name: &str,
  ) -> Result<bool, VmError> {
    // GlobalEnvironmentRecord.HasRestrictedGlobalProperty (ECMA-262).
    //
    // Returns true iff the global object has an own property `name` that is non-configurable.
    let mut key_scope = scope.reborrow();
    key_scope.push_root(Value::Object(global_object))?;
    let key = PropertyKey::from_string(key_scope.alloc_string(name)?);
    let existing = key_scope
      .heap()
      .object_get_own_property_with_tick(global_object, &key, || self.vm.tick())?;
    Ok(existing.is_some_and(|d| !d.configurable))
  }

  fn instantiate_module_hoisted_function_decl_bindings(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
    env: GcEnv,
  ) -> Result<(), VmError> {
    // Collect names of top-level function declarations that `instantiate_function_decls` will hoist
    // in strict mode (modules are always strict).
    //
    // Note: do not recurse into blocks; block function declarations are block-scoped in modules.
    let mut names: HashSet<String> = HashSet::new();
    for stmt_id in stmts {
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

      let name = self.resolve_name(def.name)?;

      // `export default function() {}` is represented as a function declaration whose name is the
      // sentinel `"<anonymous>"`. Do not create a binding for that string.
      if def.is_default_export && name == "<anonymous>" {
        continue;
      }

      names.insert(name);
    }

    // Create+initialize the bindings so hoisting can safely assign via `set_var`/`SetMutableBinding`.
    for name in names {
      self.vm.tick()?;

      if !scope.heap().env_has_binding(env, name.as_str())? {
        scope.env_create_mutable_binding(env, name.as_str())?;
        scope
          .heap_mut()
          .env_initialize_binding(env, name.as_str(), Value::Undefined)?;
        continue;
      }

      // Binding exists: ensure it is initialized, since `env_set_mutable_binding` rejects TDZ
      // bindings.
      match scope
        .heap()
        .env_get_binding_value(env, name.as_str(), /* strict */ false)
      {
        Ok(_) => {}
        // TDZ sentinel from `Heap::env_get_binding_value`.
        Err(VmError::Throw(Value::Null)) => {
          scope
            .heap_mut()
            .env_initialize_binding(env, name.as_str(), Value::Undefined)?;
        }
        // Keep the engine robust against malformed env records: if the binding lookup failed even
        // though `env_has_binding` returned true, fall back to creating it.
        Err(VmError::Unimplemented("unbound identifier")) => {
          scope.env_create_mutable_binding(env, name.as_str())?;
          scope
            .heap_mut()
            .env_initialize_binding(env, name.as_str(), Value::Undefined)?;
        }
        Err(err) => return Err(err),
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

  fn collect_var_declared_names(
    &mut self,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
    out: &mut HashSet<String>,
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
                out.insert(self.resolve_name(name_id)?);
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
                  out.insert(self.resolve_name(name_id)?);
                }
              }
            }
          }
          self.collect_var_declared_names(body, std::slice::from_ref(inner), out)?;
        }
        hir_js::StmtKind::ForIn { left, body: inner, .. } => {
          if let hir_js::ForHead::Var(decl) = left {
            if decl.kind == hir_js::VarDeclKind::Var {
              for declarator in &decl.declarators {
                self.vm.tick()?;
                let mut names: Vec<hir_js::NameId> = Vec::new();
                self.collect_pat_idents(body, declarator.pat, &mut names)?;
                for name_id in names {
                  out.insert(self.resolve_name(name_id)?);
                }
              }
            }
          }
          self.collect_var_declared_names(body, std::slice::from_ref(inner), out)?;
        }
        hir_js::StmtKind::Block(inner) => {
          self.collect_var_declared_names(body, inner.as_slice(), out)?;
        }
        hir_js::StmtKind::If {
          consequent,
          alternate,
          ..
        } => {
          self.collect_var_declared_names(body, std::slice::from_ref(consequent), out)?;
          if let Some(alt) = alternate {
            self.collect_var_declared_names(body, std::slice::from_ref(alt), out)?;
          }
        }
        hir_js::StmtKind::While { body: inner, .. }
        | hir_js::StmtKind::DoWhile { body: inner, .. }
        | hir_js::StmtKind::Labeled { body: inner, .. }
        | hir_js::StmtKind::With { body: inner, .. } => {
          self.collect_var_declared_names(body, std::slice::from_ref(inner), out)?;
        }
        hir_js::StmtKind::Try {
          block,
          catch,
          finally_block,
        } => {
          self.collect_var_declared_names(body, std::slice::from_ref(block), out)?;
          if let Some(catch) = catch {
            self.collect_var_declared_names(body, std::slice::from_ref(&catch.body), out)?;
          }
          if let Some(finally_block) = finally_block {
            self.collect_var_declared_names(body, std::slice::from_ref(finally_block), out)?;
          }
        }
        hir_js::StmtKind::Switch { cases, .. } => {
          for case in cases {
            self.collect_var_declared_names(body, case.consequent.as_slice(), out)?;
          }
        }
        _ => {}
      }
    }
    Ok(())
  }

  fn collect_global_lexical_decl_names(
    &mut self,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
    out: &mut HashSet<String>,
  ) -> Result<(), VmError> {
    // GlobalDeclarationInstantiation only considers lexically-scoped declarations in the global
    // statement list (not those nested inside blocks/loops).
    for stmt_id in stmts {
      self.vm.tick()?;
      let stmt = self.get_stmt(body, *stmt_id)?;
      match &stmt.kind {
        hir_js::StmtKind::Var(decl) => {
          if matches!(
            decl.kind,
            hir_js::VarDeclKind::Let
              | hir_js::VarDeclKind::Const
              | hir_js::VarDeclKind::Using
              | hir_js::VarDeclKind::AwaitUsing
          ) {
            for declarator in &decl.declarators {
              self.vm.tick()?;
              let mut names: Vec<hir_js::NameId> = Vec::new();
              self.collect_pat_idents(body, declarator.pat, &mut names)?;
              for name_id in names {
                out.insert(self.resolve_name(name_id)?);
              }
            }
          }
        }
        hir_js::StmtKind::Decl(def_id) => {
          let def = self
            .hir()
            .def(*def_id)
            .ok_or(VmError::InvariantViolation(
              "hir def id missing from compiled script",
            ))?;
          let Some(body_id) = def.body else {
            continue;
          };
          let decl_body = self.get_body(body_id)?;
          if decl_body.kind != hir_js::BodyKind::Class {
            continue;
          }
          let name = self.resolve_name(def.name)?;
          if name.as_str() == "<anonymous>" {
            // `export default class {}` uses the engine-internal `*default*` binding created during
            // module linking. Do not create/check a binding named `"<anonymous>"`.
            continue;
          }
          out.insert(name);
        }
        _ => {}
      }
    }
    Ok(())
  }

  fn validate_global_lexical_decls(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
  ) -> Result<(), VmError> {
    // GlobalDeclarationInstantiation runtime checks for global lexical declarations:
    // - reject collisions with existing global var/function declarations (`[[VarNames]]`),
    // - reject redeclaring existing global lexical bindings,
    // - reject creating a lexical binding when the global object has a restricted (non-configurable)
    //   property.
    let global_object = self.env.global_object();
    let global_lex = self.env.lexical_env();

    let mut names: HashSet<String> = HashSet::new();
    self.collect_global_lexical_decl_names(body, stmts, &mut names)?;

    for name in names {
      self.vm.tick()?;
      if self.vm.global_var_names_contains(name.as_str()) {
        return Err(throw_syntax_error(
          self.vm,
          scope,
          "Identifier has already been declared",
        )?);
      }
      if scope.heap().env_has_binding(global_lex, name.as_str())? {
        return Err(throw_syntax_error(
          self.vm,
          scope,
          "Identifier has already been declared",
        )?);
      }
      if self.has_restricted_global_property(scope, global_object, name.as_str())? {
        return Err(throw_syntax_error(
          self.vm,
          scope,
          "Identifier has already been declared",
        )?);
      }
    }
    Ok(())
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
          // `export default function() {}` uses an engine-internal `*default*` binding created by
          // module linking. The lowered `DefData.name` is `"<anonymous>"` and must not create a
          // concrete var binding.
          if self.is_default_export_anonymous_function_decl(*def_id) {
            let func_obj = self.alloc_user_function_object(
              scope,
              body_id,
              "default",
              /* is_arrow */ false,
              /* is_constructable */ true,
              /* name_binding */ None,
              EcmaFunctionKind::Decl,
            )?;

            let mut init_scope = scope.reborrow();
            init_scope.push_root(Value::Object(func_obj))?;
            let binding_env = self.env.lexical_env();
            let binding_name = "*default*";
            if !init_scope.heap().env_has_binding(binding_env, binding_name)? {
              return Err(VmError::InvariantViolation(
                "export default function declaration missing *default* binding",
              ));
            }
            init_scope.heap_mut().env_initialize_binding(
              binding_env,
              binding_name,
              Value::Object(func_obj),
            )?;
            continue;
          }
          let name = self.resolve_name(def.name)?;
          if name.as_str() == "<anonymous>" {
            // `export default function() {}` is represented by `hir-js` as a default-exported
            // function declaration with name `"<anonymous>"`.
            //
            // Module linking pre-creates an immutable `*default*` binding; module instantiation is
            // responsible for creating the function object and initializing that binding.
            if !def.is_default_export {
              return Err(VmError::Unimplemented("anonymous function declaration (hir-js compiled path)"));
            }
            let func_obj = self.alloc_user_function_object(
              scope,
              body_id,
              // Match typical JS behavior: the default export function has name "default".
              "default",
              /* is_arrow */ false,
              /* is_constructable */ true,
              /* name_binding */ None,
              EcmaFunctionKind::Decl,
            )?;
            // Root the function object while initializing the module's `*default*` binding.
            let mut init_scope = scope.reborrow();
            init_scope.push_root(Value::Object(func_obj))?;
            let binding_env = self.env.lexical_env();
            if !init_scope.heap().env_has_binding(binding_env, "*default*")? {
              return Err(VmError::InvariantViolation(
                "export default function declaration missing *default* binding",
              ));
            }
            init_scope.heap_mut().env_initialize_binding(binding_env, "*default*", Value::Object(func_obj))?;
            continue;
          }
          if annex_b {
            // Annex B: block-level ordinary function declarations create a var binding, but do not
            // initialize it unless the declaration is actually executed.
            //
            // This ensures:
            // - `if (false) { function g(){} } typeof g` yields `"undefined"`, and
            // - executing the block/case later updates the var binding.
            //
            // The declaration statement performs initialization in `eval_stmt_labelled`.
            self.env.declare_var(self.vm, scope, name.as_str())?;
            continue;
          }
          let func_obj = self.alloc_user_function_object(
            scope,
            body_id,
            name.as_str(),
            /* is_arrow */ false,
            /* is_constructable */ true,
            /* name_binding */ None,
            EcmaFunctionKind::Decl,
          )?;
          // Global function declarations in classic scripts must create a non-deletable
          // (non-configurable) global property, even when an existing configurable property is
          // present (ECMA-262 `CreateGlobalFunctionBinding`).
          if matches!(self.env.var_env(), VarEnv::GlobalObject) {
            let global_object = self.env.global_object();
            // Root the function object across key allocation + property definition.
            let mut def_scope = scope.reborrow();
            def_scope.push_root(Value::Object(func_obj))?;
            let key = PropertyKey::from_string(def_scope.alloc_string(name.as_str())?);
            def_scope.define_property(
              global_object,
              key,
              global_var_binding_desc(Value::Object(func_obj)),
            )?;
          } else {
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
          // `export default class {}` uses an engine-internal `*default*` binding created by module
          // linking. The lowered `DefData.name` is `"<anonymous>"` and must not create a concrete
          // lexical binding during instantiation.
          if self.is_default_export_anonymous_class_decl(*def_id) {
            continue;
          }
          let name = self.resolve_name(def.name)?;
          if name.as_str() == "<anonymous>" {
            // `export default class {}` uses the engine-internal `*default*` binding created during
            // module linking. Do not create a binding named `"<anonymous>"`.
            if !def.is_default_export {
              return Err(VmError::Unimplemented("anonymous class declaration (hir-js compiled path)"));
            }
            continue;
          }
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
      hir_js::VarDeclKind::Let
      | hir_js::VarDeclKind::Const
      | hir_js::VarDeclKind::Using
      | hir_js::VarDeclKind::AwaitUsing => {}
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
          hir_js::VarDeclKind::Const
          | hir_js::VarDeclKind::Using
          | hir_js::VarDeclKind::AwaitUsing => scope.env_create_immutable_binding(env, name.as_str())?,
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

  fn initialize_annex_b_function_decls_in_stmt_list(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
  ) -> Result<(), VmError> {
    if self.strict {
      return Ok(());
    }

    for stmt_id in stmts {
      // Tick per statement list entry so large blocks of function declarations cannot be
      // initialized without consuming fuel.
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

      // Annex B block-level function semantics apply only to ordinary (non-async, non-generator)
      // functions. Async/generator function declarations are always block-scoped, even in non-strict
      // mode.
      if func_meta.async_ || func_meta.generator {
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

      // Root the function object while assigning into the var environment.
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
    self.eval_stmt_list_with_root(scope, body, stmts, /* root */ false)
  }

  fn eval_root_stmt_list(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
  ) -> Result<Flow, VmError> {
    self.eval_stmt_list_with_root(scope, body, stmts, /* root */ true)
  }

  fn eval_stmt_list_with_root(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmts: &[hir_js::StmtId],
    root: bool,
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
        let flow = if root {
          self.eval_root_stmt(scope, body, *stmt_id)?
        } else {
          self.eval_stmt(scope, body, *stmt_id)?
        };
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
    self.eval_stmt_labelled(scope, body, stmt_id, &[], /* in_root_stmt_list */ false)
  }

  fn eval_root_stmt(
    &mut self,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    stmt_id: hir_js::StmtId,
  ) -> Result<Flow, VmError> {
    self.eval_stmt_labelled(scope, body, stmt_id, &[], /* in_root_stmt_list */ true)
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
    in_root_stmt_list: bool,
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
          self.initialize_annex_b_function_decls_in_stmt_list(scope, body, stmts.as_slice())?;
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
            if matches!(
              decl.kind,
              hir_js::VarDeclKind::Let
                | hir_js::VarDeclKind::Const
                | hir_js::VarDeclKind::Using
                | hir_js::VarDeclKind::AwaitUsing
            ) =>
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
                  hir_js::VarDeclKind::Const
                  | hir_js::VarDeclKind::Using
                  | hir_js::VarDeclKind::AwaitUsing => {
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
          return Err(VmError::InvariantViolation(
            "for-await-of executed in synchronous HIR evaluator",
          ));
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

          // Per-iteration lexical environments for lexical head declarations
          // (`let`/`const`/`using`/`await using`).
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
              if matches!(
                var_decl.kind,
                hir_js::VarDeclKind::Let
                  | hir_js::VarDeclKind::Const
                  | hir_js::VarDeclKind::Using
                  | hir_js::VarDeclKind::AwaitUsing
              ) {
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

          // Per-iteration lexical environments for lexical head declarations
          // (`let`/`const`/`using`/`await using`).
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
              if matches!(
                var_decl.kind,
                hir_js::VarDeclKind::Let
                  | hir_js::VarDeclKind::Const
                  | hir_js::VarDeclKind::Using
                  | hir_js::VarDeclKind::AwaitUsing
              ) {
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
              self.initialize_annex_b_function_decls_in_stmt_list(
                &mut switch_scope,
                body,
                case.consequent.as_slice(),
              )?;
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
        // Function declarations are usually handled during instantiation.
        //
        // However, in non-strict mode Annex B requires *block-level* ordinary function
        // declarations to update the var binding only when the declaration is actually executed.
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
          if !self.strict && !in_root_stmt_list {
            let Some(func_meta) = decl_body.function.as_ref() else {
              return Err(VmError::InvariantViolation("function body missing function metadata"));
            };
            // Annex B only applies to ordinary (non-async, non-generator) functions.
            if !func_meta.async_ && !func_meta.generator {
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
          }
          Ok(Flow::empty())
        } else if decl_body.kind == hir_js::BodyKind::Class {
          let is_default_anon = self.is_default_export_anonymous_class_decl(*def_id);
          let name = if is_default_anon {
            None
          } else {
            Some(self.resolve_name(def.name)?)
          };
          let (binding_name, func_name, inner_binding): (&str, &str, Option<&str>) = if is_default_anon {
            ("*default*", "default", None)
          } else {
            let name = name
              .as_deref()
              .ok_or(VmError::InvariantViolation("class name resolution failed"))?;
            (name, name, Some(name))
          };

          // Per ECMAScript, class declarations are evaluated within a fresh lexical environment whose
          // outer is the surrounding lexical environment. That environment contains an immutable
          // binding for the class name so class element functions can reference the class even if the
          // outer binding is later reassigned.
          let outer = self.env.lexical_env();
          let class_env = scope.env_create(Some(outer))?;
          self.env.set_lexical_env(scope.heap_mut(), class_env);

          // Evaluate the class definition with (optional) inner immutable name binding.
          let result = self.eval_class(scope, decl_body, inner_binding, func_name, None);
          // Restore the outer environment regardless of how class evaluation completes.
          self.env.set_lexical_env(scope.heap_mut(), outer);
          let func_obj = result?;

          // Initialize the outer (mutable) class binding in the surrounding environment.
          //
          // Root the class constructor object first: creating the binding may allocate and trigger
          // GC.
          let mut init_scope = scope.reborrow();
          init_scope.push_root(Value::Object(func_obj))?;

          if !init_scope.heap().env_has_binding(outer, binding_name)? {
            // Non-block statement contexts may not have performed lexical hoisting yet.
            if binding_name == "*default*" {
              init_scope.env_create_immutable_binding(outer, binding_name)?;
            } else {
              init_scope.env_create_mutable_binding(outer, binding_name)?;
            }
          }
          init_scope
            .heap_mut()
            .env_initialize_binding(outer, binding_name, Value::Object(func_obj))?;

          Ok(Flow::empty())
        } else if decl_body.kind == hir_js::BodyKind::TopLevel && def.is_default_export {
          // `hir-js` lowers `export default <expr>;` as a synthetic "declaration" whose body contains
          // the export expression as a statement list. At runtime, module evaluation must:
          // - evaluate the expression in source order,
          // - and initialize the module's `*default*` binding with the resulting value.
          //
          // Module linking pre-creates the immutable `*default*` binding (see `ModuleGraph::link`).
          let expr_stmt_id = decl_body
            .root_stmts
            .last()
            .copied()
            .ok_or(VmError::InvariantViolation(
              "export default expression missing statement list",
            ))?;
          let export_expr_id = match &self.get_stmt(decl_body, expr_stmt_id)?.kind {
            hir_js::StmtKind::Expr(expr_id) => *expr_id,
            _ => {
              return Err(VmError::InvariantViolation(
                "export default expression missing expression statement",
              ))
            }
          };

          // Evaluate the statement list in source order so the exported expression observes
          // preceding side effects.
          if decl_body.root_stmts.len() > 1 {
            let prefix = &decl_body.root_stmts[..decl_body.root_stmts.len().saturating_sub(1)];
            let prefix_result = self.eval_stmt_list(scope, decl_body, prefix)?;
            match prefix_result {
              Flow::Normal(_) => {}
              Flow::Return(_) => {
                return Err(VmError::InvariantViolation(
                  "export default expression produced Return completion (early errors should prevent this)",
                ))
              }
              Flow::Break(..) => {
                return Err(VmError::InvariantViolation(
                  "export default expression produced Break completion (early errors should prevent this)",
                ))
              }
              Flow::Continue(..) => {
                return Err(VmError::InvariantViolation(
                  "export default expression produced Continue completion (early errors should prevent this)",
                ))
              }
            }
          }

          // Implement the observable behaviour of `ExportDefaultDeclaration` `SetFunctionName`.
          //
          // This must use spec-ish `NamedEvaluation` for anonymous class expressions so the inferred
          // name is applied **during** class construction (allowing a `static name() {}` element to
          // override the constructor's initial `"name"` property).
          self.vm.tick()?;
          let is_anonymous_function_or_class = match &self.get_expr(decl_body, export_expr_id)?.kind {
            hir_js::ExprKind::FunctionExpr { name, is_arrow, .. } => *is_arrow || name.is_none(),
            hir_js::ExprKind::ClassExpr { name, .. } => name.is_none(),
            _ => false,
          };
          let value = if is_anonymous_function_or_class {
            let mut name_scope = scope.reborrow();
            let default_s = name_scope.alloc_string("default")?;
            name_scope.push_root(Value::String(default_s))?;
            self.eval_expr_named(
              &mut name_scope,
              decl_body,
              export_expr_id,
              PropertyKey::String(default_s),
            )?
          } else {
            self.eval_expr(scope, decl_body, export_expr_id)?
          };

          let binding_env = self.env.lexical_env();
          if !scope.heap().env_has_binding(binding_env, "*default*")? {
            return Err(VmError::InvariantViolation(
              "export default expression missing *default* binding",
            ));
          }
          scope
            .heap_mut()
            .env_initialize_binding(binding_env, "*default*", value)?;

          // `export default <expr>` is a statement that produces an empty completion.
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
        let flow = self.eval_stmt_labelled(
          scope,
          body,
          *inner,
          new_label_set.as_slice(),
          /* in_root_stmt_list */ false,
        )?;
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
      hir_js::StmtKind::Empty => {
        Ok(Flow::empty())
      }
      hir_js::StmtKind::Debugger => Ok(Flow::empty()),
    };

    let Err(err) = res else {
      return res;
    };
    Err(finalize_throw_with_stack_at_source_offset(
      &*self.vm,
      scope,
      self.script.source.as_ref(),
      stmt.span.start,
      err,
    ))
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
          hir_js::VarDeclKind::Var
            | hir_js::VarDeclKind::Let
            | hir_js::VarDeclKind::Const
            | hir_js::VarDeclKind::Using
            | hir_js::VarDeclKind::AwaitUsing
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

        // For lexical head bindings (`let`/`const`/`using`/`await using`, including destructuring
        // patterns), create all bound names in TDZ before binding initialization. This ensures
        // defaults like `for (let {x = x} of xs) {}`
        // correctly throw a ReferenceError instead of resolving `x` from an outer scope.
        if matches!(
          var_decl.kind,
          hir_js::VarDeclKind::Let
            | hir_js::VarDeclKind::Const
            | hir_js::VarDeclKind::Using
            | hir_js::VarDeclKind::AwaitUsing
        ) {
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
              hir_js::VarDeclKind::Const
              | hir_js::VarDeclKind::Using
              | hir_js::VarDeclKind::AwaitUsing => scope.env_create_immutable_binding(env_rec, name.as_str())?,
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

      // Explicit Resource Management (tc39/proposal-explicit-resource-management):
      // `using` and `await using` declarations must throw a TypeError if the initializer value is
      // not an object, `null`, or `undefined`.
      //
      // Note: this mirrors the AST interpreter's `check_disposable_resource_value`.
      if matches!(
        decl.kind,
        hir_js::VarDeclKind::Using | hir_js::VarDeclKind::AwaitUsing
      ) {
        match value {
          Value::Null | Value::Undefined | Value::Object(_) => {}
          _ => return Err(VmError::TypeError("Using declaration initializer must be an object")),
        }
      }
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
      // Explicit Resource Management:
      //
      // `using` and `await using` declarations introduce immutable lexical bindings (like `const`).
      //
      // Note: The AST interpreter currently treats `using` bindings in `for..in/of` heads as
      // ordinary immutable bindings (no disposable-resource bookkeeping). Keep this helper focused
      // on binding initialization; disposable-resource type checks are handled in `eval_var_decl`.
      hir_js::VarDeclKind::Using | hir_js::VarDeclKind::AwaitUsing => {
        if init_missing {
          // Should have been caught as a syntax error, but keep the engine robust.
          let message = match kind {
            hir_js::VarDeclKind::Using => "Missing initializer in using declaration",
            hir_js::VarDeclKind::AwaitUsing => "Missing initializer in await using declaration",
            _ => unreachable!(),
          };
          return Err(VmError::TypeError(message));
        }
        self.bind_pattern(scope, body, pat_id, value, PatBindingKind::Const)
      }
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
        self.resolve_this_binding(scope)
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
        // Standalone `super` is a syntax error in ECMAScript. The compiled executor should only see
        // `ExprKind::Super` as part of `super.prop`, `super[expr]`, or `super()` evaluation (all of
        // which are handled in context).
        hir_js::ExprKind::Super => VmError::InvariantViolation(
          "standalone super expression should be unreachable in compiled executor",
        ),
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
        let object_expr = self.get_expr(body, member.object)?;
        if matches!(object_expr.kind, hir_js::ExprKind::Super) {
          if member.optional {
            return Err(VmError::InvariantViolation(
              "optional chaining used in super property tagged template",
            ));
          }
 
          // `super` property references require an initialized `this` binding.
          //
          // Root `this` (possibly a derived-constructor state cell) + `[[HomeObject]]` across the
          // resolution/key evaluation steps.
          let raw_this = self.this;
          scope.push_root(raw_this)?;
          if let Some(home) = self.home_object {
            scope.push_root(Value::Object(home))?;
          }
 
          let receiver = self.resolve_this_binding(&mut scope)?;
          scope.push_root(receiver)?;
          let home_object = self
            .home_object
            .ok_or(VmError::InvariantViolation("super reference missing [[HomeObject]]"))?;
 
          scope.push_roots(&[receiver, Value::Object(home_object)])?;

          let key = self.eval_object_key(&mut scope, body, &member.property)?;
          root_property_key(&mut scope, key)?;

          let super_base = scope.object_get_prototype(home_object)?;
          let Some(super_base_obj) = super_base else {
            return Err(throw_type_error(
              self.vm,
              &mut scope,
              "Cannot read a super property from a null prototype",
            )?);
          };
          scope.push_root(Value::Object(super_base_obj))?;

          let func = scope.get_with_host_and_hooks(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            super_base_obj,
            key,
            receiver,
          )?;
          (func, receiver)
        } else {
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
              // Evaluating a `super` property reference requires an initialized `this` binding.
              // In derived constructors (and nested arrow/eval contexts) before `super()`, this must
              // throw before any of the `delete`-specific semantics apply.
              let _ = self.resolve_this_binding(scope)?;
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
        // `super.prop++` / `super[expr]--`.
        let object_expr = self.get_expr(body, member.object)?;
        if matches!(object_expr.kind, hir_js::ExprKind::Super) {
          if member.optional {
            return Err(VmError::InvariantViolation(
              "optional chaining used in update target",
            ));
          }
          let raw_this = self.this;
          let mut update_scope = scope.reborrow();
          update_scope.push_root(raw_this)?;
          if let Some(home) = self.home_object {
            update_scope.push_root(Value::Object(home))?;
          }

          let this_value = self.resolve_this_binding(&mut update_scope)?;
          update_scope.push_root(this_value)?;

          let key = self.eval_object_key(&mut update_scope, body, &member.property)?;
          root_property_key(&mut update_scope, key)?;

          let base = self.super_base_value(&mut update_scope)?;
          update_scope.push_root(base)?;
          let obj = update_scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base)?;
          update_scope.push_root(Value::Object(obj))?;

          let old_value = update_scope.get_with_host_and_hooks(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            obj,
            key,
            this_value,
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
            this_value,
          )?;
          if !ok && self.strict {
            return Err(VmError::TypeError("Cannot assign to read-only property"));
          }

          if prefix {
            return Ok(new_value);
          } else {
            return Ok(old_out);
          }
        }

        // Ordinary property update (`obj.x++`).
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
        // `#x in obj` is a private brand check, not a normal property-name `in` operation.
        //
        // In HIR, the LHS is lowered as an `Ident` expression containing the raw private identifier
        // text (including the leading `#`), but it must *not* be evaluated as an identifier
        // reference.
        let private_name_id = match self.get_expr(body, left)?.kind {
          hir_js::ExprKind::Ident(name_id) => match self.hir().names.resolve(name_id) {
            Some(s) if s.starts_with('#') => Some(name_id),
            _ => None,
          },
          _ => None,
        };

        if let Some(private_name_id) = private_name_id {
          let r = self.eval_expr(scope, body, right)?;
          let Value::Object(obj) = r else {
            return Err(VmError::TypeError("Right-hand side of 'in' should be an object"));
          };

          // Root the RHS object across private-name resolution and own-property lookup.
          let mut rhs_scope = scope.reborrow();
          rhs_scope.push_root(Value::Object(obj))?;

          let private_name = self
            .hir()
            .names
            .resolve(private_name_id)
            .ok_or(VmError::InvariantViolation(
              "hir name id missing from interner",
            ))?;

          let sym = rhs_scope
            .heap()
            .resolve_private_name_symbol(self.env.lexical_env(), private_name)?
            .ok_or(VmError::InvariantViolation("unresolved private name"))?;

          // Private elements are not accessible through Proxy objects; `#x in proxy` always fails
          // the brand check without consulting any traps.
          if rhs_scope.heap().is_proxy_object(obj) {
            return Ok(Value::Bool(false));
          }

          rhs_scope.push_root(Value::Symbol(sym))?;
          let key = PropertyKey::from_symbol(sym);
          let has = rhs_scope.heap().get_own_property(obj, key)?.is_some();
          return Ok(Value::Bool(has));
        }

        // Ordinary `in` operator: evaluate LHS expression, convert it to a property key, and
        // perform `[[HasProperty]]` (which can invoke proxy traps and user code).
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

    // `super.prop` / `super[expr]` assignment targets.
    let object_expr = self.get_expr(body, member.object)?;
    if matches!(object_expr.kind, hir_js::ExprKind::Super) {
      // Root receiver + home object while evaluating the key.
      let raw_this = self.this;
      let mut scope = scope.reborrow();
      scope.push_root(raw_this)?;
      if let Some(home) = self.home_object {
        scope.push_root(Value::Object(home))?;
      }

      // Super property references require an initialized `this` binding.
      let receiver = self.resolve_this_binding(&mut scope)?;
      scope.push_root(receiver)?;
      let home_object = self
        .home_object
        .ok_or(VmError::InvariantViolation("super reference missing [[HomeObject]]"))?;

      let key = self.eval_object_key(&mut scope, body, &member.property)?;
      root_property_key(&mut scope, key)?;

      // `GetSuperBase` (ECMA-262) returns the prototype of `home_object`, which may be `null`.
      let super_base = scope.object_get_prototype(home_object)?;

      return Ok(AssignmentReference::SuperProperty {
        super_base,
        receiver,
        key,
      });
    }

    // Ordinary property assignment target.
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
    // Root both the receiver/base and key together so `push_roots` can treat them as extra roots if
    // growing the root stack triggers a GC.
    match reference {
      AssignmentReference::Binding(_) => Ok(()),
      AssignmentReference::Property { base, key } => {
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
      AssignmentReference::SuperProperty {
        super_base,
        receiver,
        key,
      } => {
        let key_root = match key {
          PropertyKey::String(s) => Value::String(*s),
          PropertyKey::Symbol(s) => Value::Symbol(*s),
        };
        if let Some(super_base) = super_base {
          let roots = [*receiver, key_root, Value::Object(*super_base)];
          scope.push_roots(&roots)?;
        } else {
          let roots = [*receiver, key_root];
          scope.push_roots(&roots)?;
        }
        Ok(())
      }
    }
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
      AssignmentReference::SuperProperty {
        super_base,
        receiver,
        key,
      } => {
        let mut set_scope = scope.reborrow();
        self.root_assignment_reference(&mut set_scope, reference)?;
        set_scope.push_root(value)?;

        let Some(super_base_obj) = *super_base else {
          return Err(throw_type_error(
            self.vm,
            &mut set_scope,
            "Cannot assign to a super property on a null prototype",
          )?);
        };

        let ok = crate::spec_ops::internal_set_with_host_and_hooks(
          self.vm,
          &mut set_scope,
          &mut *self.host,
          &mut *self.hooks,
          super_base_obj,
          *key,
          value,
          *receiver,
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

  fn get_value_from_assignment_reference(
    &mut self,
    scope: &mut Scope<'_>,
    reference: &AssignmentReference,
  ) -> Result<Value, VmError> {
    match reference {
      AssignmentReference::Binding(reference) => {
        self.get_value_from_resolved_binding(scope, reference.as_resolved_binding())
      }
      AssignmentReference::Property { base, key } => {
        let mut get_scope = scope.reborrow();
        self.root_assignment_reference(&mut get_scope, reference)?;
        let obj = get_scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, *base)?;
        get_scope.push_root(Value::Object(obj))?;
        let receiver = *base;
        get_scope.get_with_host_and_hooks(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          obj,
          *key,
          receiver,
        )
      }
      AssignmentReference::SuperProperty {
        super_base,
        receiver,
        key,
      } => {
        let mut get_scope = scope.reborrow();
        self.root_assignment_reference(&mut get_scope, reference)?;
        let Some(super_base_obj) = *super_base else {
          return Err(throw_type_error(
            self.vm,
            &mut get_scope,
            "Cannot read a super property from a null prototype",
          )?);
        };
        get_scope.get_with_host_and_hooks(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          super_base_obj,
          *key,
          *receiver,
        )
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
      AssignmentReference::Property { key, .. }
      | AssignmentReference::SuperProperty { key, .. } => *key,
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
            // `super.prop += rhs` / `super[expr] *= rhs` compound assignment.
            let object_expr = self.get_expr(body, member.object)?;
            if matches!(object_expr.kind, hir_js::ExprKind::Super) {
              if member.optional {
                return Err(VmError::InvariantViolation(
                  "optional chaining used in assignment target",
                ));
              }
              let raw_this = self.this;

              let mut scope = scope.reborrow();
              scope.push_root(raw_this)?;
              if let Some(home) = self.home_object {
                scope.push_root(Value::Object(home))?;
              }

              let receiver = self.resolve_this_binding(&mut scope)?;
              scope.push_root(receiver)?;
              let home_object = self
                .home_object
                .ok_or(VmError::InvariantViolation("super reference missing [[HomeObject]]"))?;

              let key = self.eval_object_key(&mut scope, body, &member.property)?;
              root_property_key(&mut scope, key)?;

              let super_base = scope.object_get_prototype(home_object)?;
              let Some(super_base_obj) = super_base else {
                return Err(throw_type_error(
                  self.vm,
                  &mut scope,
                  "Cannot read a super property from a null prototype",
                )?);
              };
              scope.push_root(Value::Object(super_base_obj))?;

              let left = scope.get_with_host_and_hooks(
                self.vm,
                &mut *self.host,
                &mut *self.hooks,
                super_base_obj,
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
                super_base_obj,
                key,
                out,
                receiver,
              )?;
              if !ok && self.strict {
                return Err(VmError::TypeError("Cannot assign to read-only property"));
              }
              return Ok(out);
            }

            // Ordinary property compound assignment (`obj.x += rhs`).
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

            // `super.prop ||= rhs` / `super[expr] &&= rhs` logical assignment.
            let object_expr = self.get_expr(body, member.object)?;
            if matches!(object_expr.kind, hir_js::ExprKind::Super) {
              let raw_this = self.this;

              let mut scope = scope.reborrow();
              scope.push_root(raw_this)?;
              if let Some(home) = self.home_object {
                scope.push_root(Value::Object(home))?;
              }

              let receiver = self.resolve_this_binding(&mut scope)?;
              scope.push_root(receiver)?;
              let home_object = self
                .home_object
                .ok_or(VmError::InvariantViolation("super reference missing [[HomeObject]]"))?;

              let key = self.eval_object_key(&mut scope, body, &member.property)?;
              root_property_key(&mut scope, key)?;

              let super_base = scope.object_get_prototype(home_object)?;
              let Some(super_base_obj) = super_base else {
                return Err(throw_type_error(
                  self.vm,
                  &mut scope,
                  "Cannot read a super property from a null prototype",
                )?);
              };
              scope.push_root(Value::Object(super_base_obj))?;

              let left = scope.get_with_host_and_hooks(
                self.vm,
                &mut *self.host,
                &mut *self.hooks,
                super_base_obj,
                key,
                receiver,
              )?;
              if !should_assign(&scope, left)? {
                return Ok(left);
              }

              scope.push_root(left)?;
              let right = self.eval_expr(&mut scope, body, value)?;
              scope.push_root(right)?;
              let reference = AssignmentReference::SuperProperty {
                super_base: Some(super_base_obj),
                receiver,
                key,
              };
              self.maybe_set_anonymous_function_name_for_assignment(&mut scope, &reference, right)?;

              let ok = crate::spec_ops::internal_set_with_host_and_hooks(
                self.vm,
                &mut scope,
                &mut *self.host,
                &mut *self.hooks,
                super_base_obj,
                key,
                right,
                receiver,
              )?;
              if !ok && self.strict {
                return Err(VmError::TypeError("Cannot assign to read-only property"));
              }
              return Ok(right);
            }

            // Ordinary property logical assignment (`obj.x ||= rhs`).
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
        SuperMember {
          super_base: Option<GcObject>,
          receiver: Value,
          key: PropertyKey,
        },
        SuperComputedMember {
          super_base: Option<GcObject>,
          receiver: Value,
          key_value: Value,
        },
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
                let object_expr = self.get_expr(body, member.object)?;
                if matches!(object_expr.kind, hir_js::ExprKind::Super) {
                  let raw_this = self.this;
                  let home_object = self
                    .home_object
                    .ok_or(VmError::InvariantViolation("super reference missing [[HomeObject]]"))?;
                  prop_scope.push_roots(&[raw_this, Value::Object(home_object)])?;

                  // In derived constructors before `super()`, `super[expr]` must throw before
                  // evaluating `expr` if the `this` binding is still uninitialized.
                  let receiver = self.resolve_this_binding(&mut prop_scope)?;
                  let receiver = prop_scope.push_root(receiver)?;

                  match &member.property {
                    hir_js::ObjectKey::Computed(expr_id) => {
                      let key_value = self.eval_expr(&mut prop_scope, body, *expr_id)?;
                      let key_value = prop_scope.push_root(key_value)?;
                      let super_base = prop_scope.object_get_prototype(home_object)?;
                      if let Some(super_base_obj) = super_base {
                        prop_scope.push_root(Value::Object(super_base_obj))?;
                      }
                      target = PropTarget::SuperComputedMember {
                        super_base,
                        receiver,
                        key_value,
                      };
                    }
                    other => {
                      let member_key = self.eval_object_key(&mut prop_scope, body, other)?;
                      root_property_key(&mut prop_scope, member_key)?;
                      let super_base = prop_scope.object_get_prototype(home_object)?;
                      if let Some(super_base_obj) = super_base {
                        prop_scope.push_root(Value::Object(super_base_obj))?;
                      }
                      target = PropTarget::SuperMember {
                        super_base,
                        receiver,
                        key: member_key,
                      };
                    }
                  }
                } else {
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
        PropTarget::SuperMember {
          super_base,
          receiver,
          key,
        } => {
          let reference = AssignmentReference::SuperProperty {
            super_base,
            receiver,
            key,
          };
          self.put_value_to_assignment_reference(&mut prop_scope, &reference, prop_value)?;
        }
        PropTarget::SuperComputedMember {
          super_base,
          receiver,
          key_value,
        } => {
          let key = prop_scope.to_property_key(self.vm, &mut *self.host, &mut *self.hooks, key_value)?;
          root_property_key(&mut prop_scope, key)?;
          let reference = AssignmentReference::SuperProperty {
            super_base,
            receiver,
            key,
          };
          self.put_value_to_assignment_reference(&mut prop_scope, &reference, prop_value)?;
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
      SuperMember {
        super_base: Option<GcObject>,
        receiver: Value,
        key: PropertyKey,
      },
      SuperComputedMember {
        super_base: Option<GcObject>,
        receiver: Value,
        key_value: Value,
      },
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
              let object_expr = self.get_expr(body, member.object)?;
              if matches!(object_expr.kind, hir_js::ExprKind::Super) {
                let raw_this = self.this;
                let home_object = self
                  .home_object
                  .ok_or(VmError::InvariantViolation("super reference missing [[HomeObject]]"))?;
                scope.push_roots(&[raw_this, Value::Object(home_object)])?;

                // In derived constructors before `super()`, `super[expr]` must throw before
                // evaluating `expr` if the `this` binding is still uninitialized.
                let receiver = self.resolve_this_binding(scope)?;
                let receiver = scope.push_root(receiver)?;

                match &member.property {
                  hir_js::ObjectKey::Computed(expr_id) => {
                    let key_value = self.eval_expr(scope, body, *expr_id)?;
                    let key_value = scope.push_root(key_value)?;
                    let super_base = scope.object_get_prototype(home_object)?;
                    if let Some(super_base_obj) = super_base {
                      scope.push_root(Value::Object(super_base_obj))?;
                    }
                    rest_target = RestTarget::SuperComputedMember {
                      super_base,
                      receiver,
                      key_value,
                    };
                  }
                  other => {
                    let member_key = self.eval_object_key(scope, body, other)?;
                    root_property_key(scope, member_key)?;
                    let super_base = scope.object_get_prototype(home_object)?;
                    if let Some(super_base_obj) = super_base {
                      scope.push_root(Value::Object(super_base_obj))?;
                    }
                    rest_target = RestTarget::SuperMember {
                      super_base,
                      receiver,
                      key: member_key,
                    };
                  }
                }
              } else {
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
      RestTarget::SuperMember {
        super_base,
        receiver,
        key,
      } => {
        let reference = AssignmentReference::SuperProperty {
          super_base,
          receiver,
          key,
        };
        self.put_value_to_assignment_reference(scope, &reference, Value::Object(rest_obj))
      }
      RestTarget::SuperComputedMember {
        super_base,
        receiver,
        key_value,
      } => {
        let key = scope.to_property_key(self.vm, &mut *self.host, &mut *self.hooks, key_value)?;
        root_property_key(scope, key)?;
        let reference = AssignmentReference::SuperProperty {
          super_base,
          receiver,
          key,
        };
        self.put_value_to_assignment_reference(scope, &reference, Value::Object(rest_obj))
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
        SuperMember {
          super_base: Option<GcObject>,
          receiver: Value,
          key: PropertyKey,
        },
        SuperComputedMember {
          super_base: Option<GcObject>,
          receiver: Value,
          key_value: Value,
        },
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
              let object_expr = self.get_expr(body, member.object)?;
              if matches!(object_expr.kind, hir_js::ExprKind::Super) {
                  let raw_this = self.this;
                  let home_object = self
                    .home_object
                    .ok_or(VmError::InvariantViolation("super reference missing [[HomeObject]]"))?;
                  elem_scope.push_roots(&[raw_this, Value::Object(home_object)])?;

                  // In derived constructors before `super()`, `super[expr]` must throw before
                  // evaluating `expr` if the `this` binding is still uninitialized.
                  let receiver = self.resolve_this_binding(&mut elem_scope)?;
                  let receiver = elem_scope.push_root(receiver)?;

                  match &member.property {
                    hir_js::ObjectKey::Computed(expr_id) => {
                      let key_value = self.eval_expr(&mut elem_scope, body, *expr_id)?;
                      let key_value = elem_scope.push_root(key_value)?;
                      let super_base = elem_scope.object_get_prototype(home_object)?;
                      if let Some(super_base_obj) = super_base {
                        elem_scope.push_root(Value::Object(super_base_obj))?;
                      }
                      target = ElemTarget::SuperComputedMember {
                        super_base,
                        receiver,
                        key_value,
                      };
                    }
                    other => {
                      let member_key = self.eval_object_key(&mut elem_scope, body, other)?;
                      root_property_key(&mut elem_scope, member_key)?;
                      let super_base = elem_scope.object_get_prototype(home_object)?;
                      if let Some(super_base_obj) = super_base {
                        elem_scope.push_root(Value::Object(super_base_obj))?;
                      }
                      target = ElemTarget::SuperMember {
                        super_base,
                        receiver,
                        key: member_key,
                      };
                    }
                  }
                } else {
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
        ElemTarget::SuperMember {
          super_base,
          receiver,
          key,
        } => {
          let reference = AssignmentReference::SuperProperty {
            super_base,
            receiver,
            key,
          };
          self.put_value_to_assignment_reference(&mut elem_scope, &reference, item)
        }
        ElemTarget::SuperComputedMember {
          super_base,
          receiver,
          key_value,
        } => {
          let key = match elem_scope.to_property_key(self.vm, &mut *self.host, &mut *self.hooks, key_value) {
            Ok(key) => key,
            Err(err) => return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err),
          };
          if let Err(err) = root_property_key(&mut elem_scope, key) {
            return self.iterator_close_on_err(&mut elem_scope, &iterator_record, err);
          }
          let reference = AssignmentReference::SuperProperty {
            super_base,
            receiver,
            key,
          };
          self.put_value_to_assignment_reference(&mut elem_scope, &reference, item)
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
      SuperMember {
        super_base: Option<GcObject>,
        receiver: Value,
        key: PropertyKey,
      },
      SuperComputedMember {
        super_base: Option<GcObject>,
        receiver: Value,
        key_value: Value,
      },
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
              let object_expr = self.get_expr(body, member.object)?;
              if matches!(object_expr.kind, hir_js::ExprKind::Super) {
                let raw_this = self.this;
                let home_object = self
                  .home_object
                  .ok_or(VmError::InvariantViolation("super reference missing [[HomeObject]]"))?;
                scope.push_roots(&[raw_this, Value::Object(home_object)])?;

                // In derived constructors before `super()`, `super[expr]` must throw before
                // evaluating `expr` if the `this` binding is still uninitialized.
                let receiver = self.resolve_this_binding(scope)?;
                let receiver = scope.push_root(receiver)?;

                match &member.property {
                  hir_js::ObjectKey::Computed(expr_id) => {
                    let key_value = self.eval_expr(scope, body, *expr_id)?;
                    let key_value = scope.push_root(key_value)?;
                    let super_base = scope.object_get_prototype(home_object)?;
                    if let Some(super_base_obj) = super_base {
                      scope.push_root(Value::Object(super_base_obj))?;
                    }
                    rest_target = RestTarget::SuperComputedMember {
                      super_base,
                      receiver,
                      key_value,
                    };
                  }
                  other => {
                    let member_key = self.eval_object_key(scope, body, other)?;
                    root_property_key(scope, member_key)?;
                    let super_base = scope.object_get_prototype(home_object)?;
                    if let Some(super_base_obj) = super_base {
                      scope.push_root(Value::Object(super_base_obj))?;
                    }
                    rest_target = RestTarget::SuperMember {
                      super_base,
                      receiver,
                      key: member_key,
                    };
                  }
                }
              } else {
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
      RestTarget::SuperMember {
        super_base,
        receiver,
        key,
      } => {
        let reference = AssignmentReference::SuperProperty {
          super_base,
          receiver,
          key,
        };
        self.put_value_to_assignment_reference(scope, &reference, Value::Object(rest_arr))
      }
      RestTarget::SuperComputedMember {
        super_base,
        receiver,
        key_value,
      } => {
        let key = match scope.to_property_key(self.vm, &mut *self.host, &mut *self.hooks, key_value) {
          Ok(k) => k,
          Err(err) => return self.iterator_close_on_err(scope, &iterator_record, err),
        };
        if let Err(err) = root_property_key(scope, key) {
          return self.iterator_close_on_err(scope, &iterator_record, err);
        }
        let reference = AssignmentReference::SuperProperty {
          super_base,
          receiver,
          key,
        };
        self.put_value_to_assignment_reference(scope, &reference, Value::Object(rest_arr))
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
    // `super.prop` / `super[expr]` property access.
    //
    // These are not normal member expressions: the `[[Get]]` is performed on the prototype of the
    // current function's `[[HomeObject]]`, but with the receiver set to the current `this` binding.
    let object_expr = self.get_expr(body, member.object)?;
    if matches!(object_expr.kind, hir_js::ExprKind::Super) {
      if member.optional {
        return Err(VmError::InvariantViolation(
          "optional chaining used in super property access",
        ));
      }
      let raw_this = self.this;

      // Root `this` + home object across receiver resolution, key evaluation, prototype lookup, and
      // property access.
      let mut get_scope = scope.reborrow();
      get_scope.push_root(raw_this)?;
      if let Some(home) = self.home_object {
        get_scope.push_root(Value::Object(home))?;
      }
      // Resolve the `this` binding before evaluating the computed key expression. In derived
      // constructors before `super()`, `GetThisBinding` throws a ReferenceError and must happen
      // before any side effects from evaluating the key expression.
      let receiver = self.resolve_this_binding(&mut get_scope)?;
      get_scope.push_root(receiver)?;
      let key = self.eval_object_key(&mut get_scope, body, &member.property)?;
      root_property_key(&mut get_scope, key)?;

      let base = self.super_base_value(&mut get_scope)?;
      get_scope.push_root(base)?;

      let obj = match get_scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base) {
        Ok(obj) => obj,
        Err(VmError::TypeError(msg)) => return Err(throw_type_error(self.vm, &mut get_scope, msg)?),
        Err(err) => return Err(err),
      };
      get_scope.push_root(Value::Object(obj))?;

      return Ok(OptionalChainEval::Value(get_scope.get_with_host_and_hooks(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        obj,
        key,
        receiver,
      )?));
    }

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

    // `super.prop = value` / `super[expr] = value` used as a destructuring assignment target.
    let object_expr = self.get_expr(body, member.object)?;
    if matches!(object_expr.kind, hir_js::ExprKind::Super) {
      let raw_this = self.this;

      let mut scope = scope.reborrow();
      // Root receiver + RHS across receiver resolution, key evaluation, prototype lookup, and
      // `[[Set]]`.
      scope.push_roots(&[raw_this, value])?;
      if let Some(home) = self.home_object {
        scope.push_root(Value::Object(home))?;
      }

      // Resolve the `this` binding before evaluating the computed key expression. In derived
      // constructors before `super()`, `GetThisBinding` throws a ReferenceError and must happen
      // before any side effects from evaluating the key expression.
      let receiver = self.resolve_this_binding(&mut scope)?;
      scope.push_root(receiver)?;
      let key = self.eval_object_key(&mut scope, body, &member.property)?;
      root_property_key(&mut scope, key)?;

      let base = self.super_base_value(&mut scope)?;
      scope.push_root(base)?;

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
        receiver,
      )?;
      if ok {
        return Ok(());
      }
      if self.strict {
        return Err(throw_type_error(
          self.vm,
          &mut scope,
          "Cannot assign to read-only property",
        )?);
      }
      return Ok(());
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
              } => {
                let func_obj = self.alloc_user_function_object(
                  &mut member_scope,
                  *func_body,
                  "",
                  *is_arrow,
                  /* is_constructable */ false,
                  /* name_binding */ None,
                  EcmaFunctionKind::ObjectMember,
                )?;
                member_scope
                  .heap_mut()
                  .set_function_home_object(func_obj, Some(obj_val))?;
                Value::Object(func_obj)
              }
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

      // Derived constructor `super()` semantics must be visible to nested arrow functions and eval
      // code. Those contexts capture the enclosing constructor's `this` as a shared heap object.
      if let Value::Object(state_obj) = self.this {
        if scope.heap().is_derived_constructor_state(state_obj) {
          let (class_ctor, already_initialized) = {
            let state = scope.heap().get_derived_constructor_state(state_obj)?;
            (state.class_constructor, state.this_value.is_some())
          };
          if already_initialized {
            return Err(throw_reference_error(
              self.vm,
              scope,
              "super() can only be called once in a derived constructor",
            )?);
          }

          // Resolve the superclass constructor from the class constructor's hidden `extends` slot.
          let super_value =
            crate::class_fields::class_constructor_super_value(scope, class_ctor)?;
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

          // Root callee/new_target and the derived-constructor state for the duration of argument
          // evaluation + construction.
          let mut call_scope = scope.reborrow();
          call_scope.push_roots(&[
            Value::Object(super_ctor),
            self.new_target,
            Value::Object(state_obj),
            Value::Object(class_ctor),
          ])?;

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

          // Initialize the enclosing derived constructor's `this` binding exactly once.
          call_scope
            .heap_mut()
            .get_derived_constructor_state_mut(state_obj)?
            .this_value = Some(this_obj);

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
        // `super.prop(...)` / `super[expr](...)` calls use the current `this` binding as the call
        // receiver, not the super base object.
        let object_expr = self.get_expr(body, member.object)?;
        if matches!(object_expr.kind, hir_js::ExprKind::Super) {
          if member.optional {
            return Err(VmError::InvariantViolation(
              "optional chaining used in super property call",
            ));
          }
          let raw_this = self.this;
          // Root `this` + home object across receiver resolution, key evaluation, prototype lookup,
          // and `[[Get]]`.
          scope.push_root(raw_this)?;
          if let Some(home) = self.home_object {
            scope.push_root(Value::Object(home))?;
          }
          // Resolve the `this` binding before evaluating the computed key expression. In derived
          // constructors before `super()`, `GetThisBinding` throws a ReferenceError and must happen
          // before any side effects from evaluating the key expression.
          let receiver = self.resolve_this_binding(&mut scope)?;
          scope.push_root(receiver)?;
          let key = self.eval_object_key(&mut scope, body, &member.property)?;
          root_property_key(&mut scope, key)?;

          let base = self.super_base_value(&mut scope)?;
          scope.push_root(base)?;

          let obj = match scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, base) {
            Ok(obj) => obj,
            Err(VmError::TypeError(msg)) => return Err(throw_type_error(self.vm, &mut scope, msg)?),
            Err(err) => return Err(err),
          };
          scope.push_root(Value::Object(obj))?;

          let func = scope.get_with_host_and_hooks(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            obj,
            key,
            receiver,
          )?;
          (func, receiver)
        } else {
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
        let allow_new_target = self.env.meta_property_context().allow_new_target();
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
            self.home_object,
            allow_new_target,
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

        // Count instance **public** fields so the class constructor wrapper can preallocate its
        // hidden native-slot storage.
        let mut instance_field_count: usize = 0;
      // Find an explicit constructor, if present.
      let mut ctor_member: Option<&hir_js::ClassMember> = None;
      for member in class_meta.members.iter() {
        self.vm.tick()?;
        if member.static_ {
          continue;
        }

        match &member.kind {
          hir_js::ClassMemberKind::Constructor { .. } => {
            if ctor_member.is_some() {
              return Err(VmError::TypeError("A class may only have one constructor"));
            }
            ctor_member = Some(member);
          }
          hir_js::ClassMemberKind::Field { key, .. } => {
            if matches!(key, hir_js::ClassMemberKey::Private(_)) {
              continue;
            }
            instance_field_count = instance_field_count
              .checked_add(1)
              .ok_or(VmError::OutOfMemory)?;
          }
          _ => {}
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
          let body_func = self.alloc_user_function_object(
            scope,
            body_id,
            "constructor",
            /* is_arrow */ false,
            /* is_constructable */ true,
            /* name_binding */ None,
            EcmaFunctionKind::ClassMember,
          )?;
          // Class constructor bodies are a special case: they are parsed as class members (so they
          // always allow `super.prop`), but only *derived* class constructors permit `super()` calls.
          //
          // Direct eval within a derived constructor must therefore be parsed with
          // `AllowSuperCall=true`, matching the caller context.
          let meta_property_context = if matches!(super_value, Value::Undefined) {
            MetaPropertyContext::METHOD
          } else {
            MetaPropertyContext::DERIVED_CONSTRUCTOR
          };
          scope
            .heap_mut()
            .set_function_meta_property_context(body_func, meta_property_context)?;
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
          instance_field_count,
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

        // If the class has an explicit `constructor(...) { ... }` body, annotate the hidden body
        // function (and its wrapper, if one was created) so `[[Construct]]` can implement derived
        // `super()` semantics (and, in particular, so derived constructors that never initialize
        // `this` throw the correct ReferenceError).
        if let Some(body_func) = ctor_body_inner_func {
          scope.heap_mut().set_function_data(
            body_func,
            FunctionData::ClassConstructorBody {
              class_constructor: func_obj,
            },
          )?;
        }
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
        enum StaticInitElement {
          Block(hir_js::BodyId),
          Field { key: Value, initializer: Value },
        }

        let mut static_inits: Vec<StaticInitElement> = Vec::new();
        let mut instance_field_idx: usize = 0;
        for member in class_meta.members.iter() {
          self.vm.tick()?;

          match &member.kind {
            hir_js::ClassMemberKind::Constructor { .. } => {
              // The actual `constructor(...) { ... }` body is represented by the class constructor
              // object itself (and its hidden body function).
              continue;
            }
            hir_js::ClassMemberKind::StaticBlock { body, .. } => {
              static_inits.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
              static_inits.push(StaticInitElement::Block(*body));
              continue;
            }
            hir_js::ClassMemberKind::Field { initializer, key, .. } => {
              let target_obj = if member.static_ { func_obj } else { prototype_obj };

              let mut member_scope = class_scope.reborrow();
              member_scope.push_root(Value::Object(target_obj))?;

              let key = self.eval_class_member_key(&mut member_scope, class_body, key)?;
              root_property_key(&mut member_scope, key)?;

              let key_value = match key {
                PropertyKey::String(s) => Value::String(s),
                PropertyKey::Symbol(s) => Value::Symbol(s),
              };

              // Create a function object for `= <expr>` initializers so they can be evaluated later
              // with the correct `this` value.
              let init_value = match initializer {
                Some(init_body_id) => {
                  let init_body = hir
                    .body(*init_body_id)
                    .ok_or(VmError::InvariantViolation("hir body id missing from compiled script"))?;
                  let init_func =
                    self.alloc_class_field_initializer_function(&mut member_scope, init_body.span)?;
                  member_scope
                    .heap_mut()
                    .set_function_home_object(init_func, Some(target_obj))?;
                  Value::Object(init_func)
                }
                None => Value::Undefined,
              };

              if member.static_ {
                // Static field: defer initialization until after the element definition pass.
                // Drop `member_scope` early so we can push persistent roots onto `class_scope`.
                drop(member_scope);
                class_scope.push_root(key_value)?;
                class_scope.push_root(init_value)?;
                static_inits.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
                static_inits.push(StaticInitElement::Field {
                  key: key_value,
                  initializer: init_value,
                });
              } else {
                // Instance field: store as `(key, initializer)` in the class constructor's native
                // slots so `[[Construct]]` can initialize them per instance.
                let slot_base = crate::class_fields::CLASS_CTOR_SLOT_INSTANCE_FIELDS_START
                  .saturating_add(instance_field_idx.saturating_mul(2));
                member_scope
                  .heap_mut()
                  .set_function_native_slot(func_obj, slot_base, key_value)?;
                member_scope.heap_mut().set_function_native_slot(
                  func_obj,
                  slot_base.saturating_add(1),
                  init_value,
                )?;
                instance_field_idx = instance_field_idx
                  .checked_add(1)
                  .ok_or(VmError::OutOfMemory)?;
              }
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
              // based on the property key. This matches interpreter semantics and handles
              // getter/setter prefixes and Symbol keys.
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
                  crate::function_properties::set_function_name(
                    &mut member_scope,
                    func_obj_member,
                    key,
                    None,
                  )?;
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

        // Evaluate class static initialization elements (public static fields and static blocks) in
        // source order.
        //
        // This matches ECMA-262 `ClassDefinitionEvaluation`, where static initialization elements run
        // in a second pass after the element definition pass.
        for init in static_inits {
          self.vm.tick()?;
          match init {
            StaticInitElement::Block(body_id) => {
              let block_body = hir
                .body(body_id)
                .ok_or(VmError::InvariantViolation("hir body id missing from compiled script"))?;
              self.eval_class_static_block_hir(&mut class_scope, func_obj, block_body)?;
            }
            StaticInitElement::Field { key, initializer } => {
              let key = match key {
                Value::String(s) => PropertyKey::from_string(s),
                Value::Symbol(s) => PropertyKey::from_symbol(s),
                Value::Undefined => {
                  return Err(VmError::InvariantViolation("static field key is undefined"))
                }
                _ => {
                  return Err(VmError::InvariantViolation(
                    "static field key is not a string or symbol",
                  ))
                }
              };
              let value = match initializer {
                Value::Object(func) => self.vm.call_with_host_and_hooks(
                  &mut *self.host,
                  &mut class_scope,
                  &mut *self.hooks,
                  Value::Object(func),
                  Value::Object(func_obj),
                  &[],
                )?,
                Value::Undefined => Value::Undefined,
                _ => {
                  return Err(VmError::InvariantViolation(
                    "static field initializer is not a function or undefined",
                  ))
                }
              };
              class_scope.push_root(value)?;
              class_scope.create_data_property_or_throw(func_obj, key, value)?;
            }
          }
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
    let saved_meta_property_context = self.env.meta_property_context();

    let res: Result<Flow, VmError> = (|| {
      self.this = Value::Object(receiver);
      self.this_initialized = true;
      self.new_target = Value::Undefined;
      // Static blocks use the class constructor object as their `[[HomeObject]]` so `super.prop`
      // resolves against `Object.getPrototypeOf(classConstructor)` with receiver = `classConstructor`.
      self.home_object = Some(receiver);
      self
        .env
        .set_meta_property_context(MetaPropertyContext::METHOD);

      let var_env = block_scope.env_create(Some(saved_lex))?;
      let body_lex = block_scope.env_create(Some(var_env))?;
      self.env.set_var_env(VarEnv::Env(var_env));
      self.env.set_lexical_env(block_scope.heap_mut(), body_lex);

      // Some early errors are still checked at runtime during instantiation so invalid declarations
      // do not partially pollute the static block environments.
      self.early_error_missing_initializers_in_stmt_list(
        block_body,
        block_body.root_stmts.as_slice(),
      )?;
      self.instantiate_var_decls(&mut block_scope, block_body, block_body.root_stmts.as_slice())?;
      // Class bodies (including static initialization blocks) are always strict mode, so legacy
      // Annex B block-level function hoisting does not apply.
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

      self.eval_root_stmt_list(&mut block_scope, block_body, block_body.root_stmts.as_slice())
    })();

    // Restore the surrounding class evaluation context regardless of how the block completes.
    self.env.set_lexical_env(block_scope.heap_mut(), saved_lex);
    self.env.set_var_env(saved_var_env);
    self
      .env
      .set_meta_property_context(saved_meta_property_context);
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

  fn alloc_class_field_initializer_function(
    &mut self,
    scope: &mut Scope<'_>,
    expr_span: diagnostics::TextRange,
  ) -> Result<GcObject, VmError> {
    // Mirror `exec.rs::eval_class`:
    // class field initializers are represented as ordinary strict-mode callable functions created
    // by parsing `(class extends null { m() { return <expr>; } })` snippets (see
    // `EcmaFunctionKind::ClassFieldInitializer`).
    //
    // This allows both static field initialization and per-instance field initialization to share the
    // same `vm.call` machinery and ensures `eval()` inside initializers uses a function-scoped
    // variable environment (rather than polluting the surrounding class/module env).
    let closure_env = self.env.lexical_env();

    // Root the class lexical environment across allocation in case it triggers GC.
    let mut init_scope = scope.reborrow();
    init_scope.push_env_root(closure_env)?;

    // Compute a stable span key matching interpreter semantics (env prefix/base offsets).
    let rel_start = expr_span.start.saturating_sub(self.env.prefix_len());
    let rel_end = expr_span.end.saturating_sub(self.env.prefix_len());
    let span_start = self.env.base_offset().saturating_add(rel_start);
    let span_end = self.env.base_offset().saturating_add(rel_end);

    let code_id = self.vm.register_ecma_function(
      self.env.source(),
      span_start,
      span_end,
      EcmaFunctionKind::ClassFieldInitializer,
    )?;

    // Field initializer functions are always strict mode and have `length = 0`.
    let name_s = init_scope.alloc_string("")?;
    init_scope.push_root(Value::String(name_s))?;
    let func_obj = init_scope.alloc_ecma_function(
      code_id,
      /* is_constructable */ false,
      name_s,
      0,
      ThisMode::Strict,
      /* is_strict */ true,
      Some(closure_env),
    )?;
    init_scope.push_root(Value::Object(func_obj))?;
    // Field initializer functions are parsed/evaluated as class *methods* so they have a lexical
    // `super` binding (for `super.prop`, not `super()`), and so direct eval inherits the enclosing
    // meta-property context.
    init_scope
      .heap_mut()
      .set_function_meta_property_context(func_obj, MetaPropertyContext::METHOD)?;

    // Best-effort `[[Prototype]]` / `[[Realm]]` metadata.
    if let Some(intr) = self.vm.intrinsics() {
      init_scope
        .heap_mut()
        .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
    }
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

#[derive(Debug)]
pub(crate) enum HirAsyncResult {
  CompleteOk(Value),
  CompleteThrow(Value),
  Await {
    kind: crate::exec::AsyncSuspendKind,
    await_value: Value,
  },
}

#[derive(Debug)]
struct RootedFlow {
  kind: RootedFlowKind,
}

#[derive(Debug)]
enum RootedFlowKind {
  Normal(Option<RootId>),
  Return(RootId),
  Break(Option<hir_js::NameId>, Option<RootId>),
  Continue(Option<hir_js::NameId>, Option<RootId>),
}

impl RootedFlow {
  fn new(scope: &mut Scope<'_>, flow: Flow) -> Result<Self, VmError> {
    let mut root_value = |v: Value| -> Result<RootId, VmError> {
      let mut root_scope = scope.reborrow();
      root_scope.push_root(v)?;
      root_scope.heap_mut().add_root(v)
    };
    let kind = match flow {
      Flow::Normal(v) => RootedFlowKind::Normal(v.map(&mut root_value).transpose()?),
      Flow::Return(v) => RootedFlowKind::Return(root_value(v)?),
      Flow::Break(label, v) => RootedFlowKind::Break(label, v.map(&mut root_value).transpose()?),
      Flow::Continue(label, v) => RootedFlowKind::Continue(label, v.map(&mut root_value).transpose()?),
    };
    Ok(Self { kind })
  }

  fn to_flow(&self, heap: &crate::Heap) -> Result<Flow, VmError> {
    let get_opt = |id: Option<RootId>| -> Result<Option<Value>, VmError> {
      let Some(id) = id else {
        return Ok(None);
      };
      Ok(Some(
        heap
          .get_root(id)
          .ok_or(VmError::InvariantViolation("missing rooted flow value"))?,
      ))
    };
    Ok(match &self.kind {
      RootedFlowKind::Normal(v) => Flow::Normal(get_opt(*v)?),
      RootedFlowKind::Return(v) => Flow::Return(
        heap
          .get_root(*v)
          .ok_or(VmError::InvariantViolation("missing rooted flow return value"))?,
      ),
      RootedFlowKind::Break(label, v) => Flow::Break(*label, get_opt(*v)?),
      RootedFlowKind::Continue(label, v) => Flow::Continue(*label, get_opt(*v)?),
    })
  }

  fn teardown(&mut self, heap: &mut crate::Heap) {
    let mut remove_opt = |id: &mut Option<RootId>| {
      if let Some(id) = id.take() {
        heap.remove_root(id);
      }
    };
    match &mut self.kind {
      RootedFlowKind::Normal(v) => remove_opt(v),
      RootedFlowKind::Return(id) => heap.remove_root(*id),
      RootedFlowKind::Break(_, v) => remove_opt(v),
      RootedFlowKind::Continue(_, v) => remove_opt(v),
    }
  }
}

#[derive(Debug)]
struct RootedThrow {
  value_root: RootId,
}

impl RootedThrow {
  fn new(scope: &mut Scope<'_>, value: Value) -> Result<Self, VmError> {
    let mut root_scope = scope.reborrow();
    root_scope.push_root(value)?;
    let value_root = root_scope.heap_mut().add_root(value)?;
    Ok(Self { value_root })
  }

  fn to_error(&self, heap: &crate::Heap) -> Result<VmError, VmError> {
    let value = heap
      .get_root(self.value_root)
      .ok_or(VmError::InvariantViolation("missing rooted throw value"))?;
    Ok(VmError::Throw(value))
  }

  fn teardown(&mut self, heap: &mut crate::Heap) {
    heap.remove_root(self.value_root);
  }
}

#[derive(Debug)]
enum RootedPending {
  Flow(RootedFlow),
  Throw(RootedThrow),
}

impl RootedPending {
  fn is_throw(&self) -> bool {
    matches!(self, RootedPending::Throw(_))
  }

  fn teardown(&mut self, heap: &mut crate::Heap) {
    match self {
      RootedPending::Flow(flow) => flow.teardown(heap),
      RootedPending::Throw(thrown) => thrown.teardown(heap),
    }
  }
}

#[derive(Debug)]
enum ForAwaitOfStage {
  Init,
  AwaitRhs,
  AwaitNext,
  ClosingAwait { pending: Option<RootedPending> },
}

#[derive(Debug)]
struct ForAwaitOfState {
  left: hir_js::ForHead,
  right: hir_js::ExprId,
  body_stmt: hir_js::StmtId,
  label_set: Box<[hir_js::NameId]>,
  outer_lex: Option<GcEnv>,
  v_root: Option<RootId>,
  iterator_root: Option<RootId>,
  next_method_root: Option<RootId>,
  stage: ForAwaitOfStage,
}

#[derive(Debug)]
enum ForAwaitOfPoll {
  Complete(Flow),
  Await {
    kind: crate::exec::AsyncSuspendKind,
    await_value: Value,
  },
}

impl ForAwaitOfState {
  fn start_from_iterable(
    &mut self,
    evaluator: &mut HirEvaluator<'_>,
    scope: &mut Scope<'_>,
    iterable: Value,
  ) -> Result<ForAwaitOfPoll, VmError> {
    // Root iterable during iterator acquisition.
    let mut iter_scope = scope.reborrow();
    iter_scope.push_root(iterable)?;

    let iterator_record = match iterator::get_async_iterator(
      evaluator.vm,
      &mut *evaluator.host,
      &mut *evaluator.hooks,
      &mut iter_scope,
      iterable,
    ) {
      Ok(r) => r,
      Err(err) => {
        let err = crate::vm::coerce_error_to_throw(&*evaluator.vm, &mut iter_scope, err);
        self.cleanup_roots(iter_scope.heap_mut());
        return Err(err);
      }
    };

    // Root iterator + next method while registering persistent roots.
    let iterator_value = iterator_record.iterator;
    let next_method_value = iterator_record.next_method;

    let (v_root, iterator_root, next_method_root) = {
      let mut root_scope = iter_scope.reborrow();
      root_scope.push_roots(&[iterator_value, next_method_value])?;
      let v_root = root_scope.heap_mut().add_root(Value::Undefined)?;
      let iterator_root = root_scope.heap_mut().add_root(iterator_value)?;
      let next_method_root = root_scope.heap_mut().add_root(next_method_value)?;
      (v_root, iterator_root, next_method_root)
    };

    self.v_root = Some(v_root);
    self.iterator_root = Some(iterator_root);
    self.next_method_root = Some(next_method_root);

    // First iteration: await next().
    evaluator.vm.tick()?;
    let record = self.iterator_record(iter_scope.heap())?;
    let next_value = match iterator::async_iterator_next(
      evaluator.vm,
      &mut *evaluator.host,
      &mut *evaluator.hooks,
      &mut iter_scope,
      &record,
    ) {
      Ok(v) => v,
      Err(err) => {
        // Spec: do not AsyncIteratorClose on step errors.
        let err = crate::vm::coerce_error_to_throw(&*evaluator.vm, &mut iter_scope, err);
        self.cleanup_roots(iter_scope.heap_mut());
        return Err(err);
      }
    };

    self.stage = ForAwaitOfStage::AwaitNext;
    Ok(ForAwaitOfPoll::Await {
      kind: crate::exec::AsyncSuspendKind::Await,
      await_value: next_value,
    })
  }

  fn new(
    left: hir_js::ForHead,
    right: hir_js::ExprId,
    body_stmt: hir_js::StmtId,
    label_set: &[hir_js::NameId],
  ) -> Result<Self, VmError> {
    let mut labels: Vec<hir_js::NameId> = Vec::new();
    labels
      .try_reserve_exact(label_set.len())
      .map_err(|_| VmError::OutOfMemory)?;
    labels.extend_from_slice(label_set);
    Ok(Self {
      left,
      right,
      body_stmt,
      label_set: labels.into_boxed_slice(),
      outer_lex: None,
      v_root: None,
      iterator_root: None,
      next_method_root: None,
      stage: ForAwaitOfStage::Init,
    })
  }

  fn iterator_record(&self, heap: &crate::Heap) -> Result<iterator::AsyncIteratorRecord, VmError> {
    let iterator_root = self
      .iterator_root
      .ok_or(VmError::InvariantViolation("missing for-await-of iterator root"))?;
    let next_method_root = self
      .next_method_root
      .ok_or(VmError::InvariantViolation("missing for-await-of next method root"))?;
    let iterator = heap
      .get_root(iterator_root)
      .ok_or(VmError::InvariantViolation("missing for-await-of iterator root value"))?;
    let next_method = heap
      .get_root(next_method_root)
      .ok_or(VmError::InvariantViolation("missing for-await-of next method root value"))?;
    Ok(iterator::AsyncIteratorRecord {
      iterator,
      next_method,
      done: false,
    })
  }

  fn cleanup_roots(&mut self, heap: &mut crate::Heap) {
    if let Some(id) = self.v_root.take() {
      if heap.get_root(id).is_some() {
        heap.remove_root(id);
      }
    }
    if let Some(id) = self.iterator_root.take() {
      if heap.get_root(id).is_some() {
        heap.remove_root(id);
      }
    }
    if let Some(id) = self.next_method_root.take() {
      if heap.get_root(id).is_some() {
        heap.remove_root(id);
      }
    }
  }

  fn teardown(&mut self, heap: &mut crate::Heap) {
    if let ForAwaitOfStage::ClosingAwait { pending } = &mut self.stage {
      if let Some(pending) = pending.as_mut() {
        pending.teardown(heap);
      }
    }
    self.cleanup_roots(heap);
  }

  fn poll(
    &mut self,
    evaluator: &mut HirEvaluator<'_>,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    resume_value: Option<Result<Value, VmError>>,
  ) -> Result<ForAwaitOfPoll, VmError> {
    match &mut self.stage {
      ForAwaitOfStage::Init => {
        if resume_value.is_some() {
          return Err(VmError::InvariantViolation(
            "for-await-of init received resume value",
          ));
        }

        let outer_lex = evaluator.env.lexical_env();
        self.outer_lex = Some(outer_lex);

        let rhs = evaluator.get_expr(body, self.right)?;
        if let hir_js::ExprKind::Await { expr: awaited_expr } = &rhs.kind {
          // The compiled synchronous evaluator does not yet support `await` expressions. However,
          // `for await..of` must still be able to suspend while evaluating the RHS expression.
          //
          // Evaluate the await argument under the same TDZ environment semantics as normal
          // `ForIn/OfHeadEvaluation`, but *do not* restore the outer lexical environment until the
          // await has resolved: the continuation after `await` is still part of RHS evaluation.
          let old_lex = evaluator.env.lexical_env();
          let mut tdz_env_created = false;
          if let hir_js::ForHead::Var(var_decl) = &self.left {
            if matches!(
              var_decl.kind,
              hir_js::VarDeclKind::Let
                | hir_js::VarDeclKind::Const
                | hir_js::VarDeclKind::Using
                | hir_js::VarDeclKind::AwaitUsing
            ) {
              let tdz_env = scope.env_create(Some(old_lex))?;
              for declarator in &var_decl.declarators {
                evaluator.vm.tick()?;
                let mut names: Vec<hir_js::NameId> = Vec::new();
                evaluator.collect_pat_idents(body, declarator.pat, &mut names)?;
                for name_id in names {
                  let name = evaluator.resolve_name(name_id)?;
                  if scope.heap().env_has_binding(tdz_env, name.as_str())? {
                    continue;
                  }
                  match var_decl.kind {
                    hir_js::VarDeclKind::Let => scope.env_create_mutable_binding(tdz_env, name.as_str())?,
                    hir_js::VarDeclKind::Const
                    | hir_js::VarDeclKind::Using
                    | hir_js::VarDeclKind::AwaitUsing => {
                      scope.env_create_immutable_binding(tdz_env, name.as_str())?
                    }
                    _ => {
                      return Err(VmError::InvariantViolation(
                        "unexpected VarDeclKind in for-await-of head TDZ environment creation",
                      ));
                    }
                  }
                }
              }
              evaluator.env.set_lexical_env(scope.heap_mut(), tdz_env);
              tdz_env_created = true;
            }
          }

          // Evaluate the await argument value. If this throws, the loop has not acquired an
          // iterator yet, so we can propagate the error directly.
          let await_value = match evaluator.eval_expr(scope, body, *awaited_expr) {
            Ok(v) => v,
            Err(err) => {
              if tdz_env_created {
                evaluator.env.set_lexical_env(scope.heap_mut(), old_lex);
              }
              return Err(err);
            }
          };

          self.stage = ForAwaitOfStage::AwaitRhs;
          return Ok(ForAwaitOfPoll::Await {
            kind: crate::exec::AsyncSuspendKind::Await,
            await_value,
          });
        }

        // Evaluate RHS with TDZ env semantics for lexical loop heads.
        let iterable = evaluator.eval_for_in_of_rhs_with_tdz_env(scope, body, &self.left, self.right)?;
        self.start_from_iterable(evaluator, scope, iterable)
      }

      ForAwaitOfStage::AwaitRhs => {
        let Some(resume_value) = resume_value else {
          return Err(VmError::InvariantViolation(
            "for-await-of awaiting rhs missing resume value",
          ));
        };

        let outer_lex = self
          .outer_lex
          .ok_or(VmError::InvariantViolation("missing for-await-of outer lex env"))?;

        // Once the await has resolved (or rejected), RHS evaluation is complete and the TDZ env
        // should be popped before iterator acquisition.
        evaluator.env.set_lexical_env(scope.heap_mut(), outer_lex);

        let iterable = match resume_value {
          Ok(v) => v,
          Err(err) => {
            self.cleanup_roots(scope.heap_mut());
            return Err(err);
          }
        };

        self.start_from_iterable(evaluator, scope, iterable)
      }

      ForAwaitOfStage::AwaitNext => {
        let Some(resume_value) = resume_value else {
          return Err(VmError::InvariantViolation(
            "for-await-of awaiting next missing resume value",
          ));
        };

        let outer_lex = self
          .outer_lex
          .ok_or(VmError::InvariantViolation("missing for-await-of outer lex env"))?;
        let v_root = self
          .v_root
          .ok_or(VmError::InvariantViolation("missing for-await-of loop value root"))?;

        // Awaiting `next()` can re-enter as a thrown completion.
        let next_result = match resume_value {
          Ok(v) => v,
          Err(err) => {
            self.cleanup_roots(scope.heap_mut());
            return Err(err);
          }
        };

        let mut step_scope = scope.reborrow();
        step_scope.push_root(next_result)?;

        let done = match iterator::iterator_complete(
          evaluator.vm,
          &mut *evaluator.host,
          &mut *evaluator.hooks,
          &mut step_scope,
          next_result,
        ) {
          Ok(done) => done,
          Err(err) => {
            // Spec: do not AsyncIteratorClose on step errors.
            let err = crate::vm::coerce_error_to_throw(&*evaluator.vm, &mut step_scope, err);
            self.cleanup_roots(step_scope.heap_mut());
            return Err(err);
          }
        };

        let v = step_scope
          .heap()
          .get_root(v_root)
          .ok_or(VmError::InvariantViolation("missing for-await-of loop value root"))?;

        if done {
          self.cleanup_roots(step_scope.heap_mut());
          return Ok(ForAwaitOfPoll::Complete(Flow::Normal(Some(v))));
        }

        let iter_value = match iterator::iterator_value(
          evaluator.vm,
          &mut *evaluator.host,
          &mut *evaluator.hooks,
          &mut step_scope,
          next_result,
        ) {
          Ok(v) => v,
          Err(err) => {
            let err = crate::vm::coerce_error_to_throw(&*evaluator.vm, &mut step_scope, err);
            self.cleanup_roots(step_scope.heap_mut());
            return Err(err);
          }
        };

        // Root iterated value across binding + body evaluation.
        let mut iter_scope = step_scope.reborrow();
        iter_scope.push_root(iter_value)?;

        // Per-iteration lexical env for lexical head declarations (`let`/`const`/`using`/`await using`).
        let mut iter_env_created: bool = false;
        if let hir_js::ForHead::Var(var_decl) = &self.left {
          if matches!(
            var_decl.kind,
            hir_js::VarDeclKind::Let
              | hir_js::VarDeclKind::Const
              | hir_js::VarDeclKind::Using
              | hir_js::VarDeclKind::AwaitUsing
          ) {
            let env = iter_scope.env_create(Some(outer_lex))?;
            evaluator.env.set_lexical_env(iter_scope.heap_mut(), env);
            iter_env_created = true;
            if let Err(err) = evaluator.instantiate_lexical_decl(&mut iter_scope, body, var_decl, env) {
              evaluator.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
              self.cleanup_roots(iter_scope.heap_mut());
              return Err(err);
            }
          } else if !matches!(var_decl.kind, hir_js::VarDeclKind::Var) {
            self.cleanup_roots(iter_scope.heap_mut());
            return Err(VmError::Unimplemented(
              "for-await-of loop variable declaration kind (hir-js compiled path)",
            ));
          }
        }

        let bind_res = evaluator.bind_for_in_of_head(&mut iter_scope, body, &self.left, iter_value);
        if let Err(err) = bind_res {
          if iter_env_created {
            evaluator.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
          }
          let err = crate::vm::coerce_error_to_throw(&*evaluator.vm, &mut iter_scope, err);
          return self.start_close_from_error(evaluator, &mut iter_scope, err);
        }

        let body_flow = match evaluator.eval_stmt(&mut iter_scope, body, self.body_stmt) {
          Ok(flow) => flow,
          Err(err) => {
            if iter_env_created {
              evaluator.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
            }
            let err = crate::vm::coerce_error_to_throw(&*evaluator.vm, &mut iter_scope, err);
            return self.start_close_from_error(evaluator, &mut iter_scope, err);
          }
        };

        if iter_env_created {
          evaluator.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
        }

        // Loop continues?
        let continue_value: Option<Option<Value>> = match body_flow {
          Flow::Normal(value) => Some(value),
          Flow::Continue(None, value) => Some(value),
          Flow::Continue(Some(label), value) => {
            if self.label_set.iter().any(|l| *l == label) {
              Some(value)
            } else {
              let out_flow = Flow::Continue(Some(label), value).update_empty(Some(v));
              return self.start_close_from_flow(evaluator, &mut iter_scope, out_flow);
            }
          }

          Flow::Break(None, break_value) => {
            let out = break_value.unwrap_or(v);
            return self.start_close_from_flow(evaluator, &mut iter_scope, Flow::Normal(Some(out)));
          }

          Flow::Break(Some(label), break_value) => {
            let out_flow = Flow::Break(Some(label), break_value).update_empty(Some(v));
            return self.start_close_from_flow(evaluator, &mut iter_scope, out_flow);
          }

          Flow::Return(ret) => return self.start_close_from_flow(evaluator, &mut iter_scope, Flow::Return(ret)),
        };

        if let Some(value) = continue_value {
          if let Some(value) = value {
            iter_scope.heap_mut().set_root(v_root, value);
          }

          evaluator.vm.tick()?;
          let record = self.iterator_record(iter_scope.heap())?;
          let next_value = match iterator::async_iterator_next(
            evaluator.vm,
            &mut *evaluator.host,
            &mut *evaluator.hooks,
            &mut iter_scope,
            &record,
          ) {
            Ok(v) => v,
            Err(err) => {
              let err = crate::vm::coerce_error_to_throw(&*evaluator.vm, &mut iter_scope, err);
              self.cleanup_roots(iter_scope.heap_mut());
              return Err(err);
            }
          };
          return Ok(ForAwaitOfPoll::Await {
            kind: crate::exec::AsyncSuspendKind::Await,
            await_value: next_value,
          });
        }

        unreachable!("match body_flow should either return or continue loop");
      }

      ForAwaitOfStage::ClosingAwait { pending } => {
        let Some(resume_value) = resume_value else {
          return Err(VmError::InvariantViolation(
            "for-await-of closing missing resume value",
          ));
        };

        let pending = pending
          .take()
          .ok_or(VmError::InvariantViolation("for-await-of close missing pending completion"))?;
        let pending_is_throw = pending.is_throw();
        let pending = pending;

        // Iterator roots are no longer needed once `return` has settled.
        self.cleanup_roots(scope.heap_mut());

        let out = match pending {
          RootedPending::Flow(mut flow) => {
            let flow_value = flow.to_flow(scope.heap())?;
            flow.teardown(scope.heap_mut());
            Ok(flow_value)
          }
          RootedPending::Throw(mut thrown) => {
            let err = thrown.to_error(scope.heap())?;
            thrown.teardown(scope.heap_mut());
            Err(err)
          }
        };

        match resume_value {
          Ok(v) => {
            if !matches!(v, Value::Object(_)) && !pending_is_throw {
              let err =
                throw_type_error(&*evaluator.vm, scope, "AsyncIteratorClose: return value is not an object")?;
              match err {
                VmError::Throw(_) | VmError::ThrowWithStack { .. } => return Err(err),
                other => return Err(other),
              }
            }
            match out {
              Ok(flow) => Ok(ForAwaitOfPoll::Complete(flow)),
              Err(err) => Err(err),
            }
          }
          Err(err) => {
            if pending_is_throw && err.is_throw_completion() {
              match out {
                Ok(flow) => Ok(ForAwaitOfPoll::Complete(flow)),
                Err(err) => Err(err),
              }
            } else {
              Err(err)
            }
          }
        }
      }
    }
  }

  fn start_close_from_error(
    &mut self,
    evaluator: &mut HirEvaluator<'_>,
    scope: &mut Scope<'_>,
    err: VmError,
  ) -> Result<ForAwaitOfPoll, VmError> {
    let (VmError::Throw(value) | VmError::ThrowWithStack { value, .. }) = err else {
      self.cleanup_roots(scope.heap_mut());
      return Err(err);
    };
    let pending = RootedPending::Throw(RootedThrow::new(scope, value)?);
    self.start_close(evaluator, scope, pending)
  }

  fn start_close_from_flow(
    &mut self,
    evaluator: &mut HirEvaluator<'_>,
    scope: &mut Scope<'_>,
    flow: Flow,
  ) -> Result<ForAwaitOfPoll, VmError> {
    let pending = RootedPending::Flow(RootedFlow::new(scope, flow)?);
    self.start_close(evaluator, scope, pending)
  }

  fn start_close(
    &mut self,
    evaluator: &mut HirEvaluator<'_>,
    scope: &mut Scope<'_>,
    pending: RootedPending,
  ) -> Result<ForAwaitOfPoll, VmError> {
    // Exiting loop: `V` is no longer needed.
    if let Some(v_root) = self.v_root.take() {
      scope.heap_mut().remove_root(v_root);
    }

    let completion_is_throw = pending.is_throw();

    let iterator_root = match self.iterator_root {
      Some(root) => root,
      None => {
        let mut pending = pending;
        pending.teardown(scope.heap_mut());
        self.cleanup_roots(scope.heap_mut());
        return Err(VmError::InvariantViolation("missing for-await-of iterator root"));
      }
    };

    let iterator_value = scope
      .heap()
      .get_root(iterator_root)
      .ok_or(VmError::InvariantViolation("missing for-await-of iterator root value"))?;

    // `AsyncIteratorClose`: GetMethod(iterator, "return").
    let close_value: Result<Option<Value>, VmError> = (|| {
      let mut close_scope = scope.reborrow();
      close_scope.push_root(iterator_value)?;
      let return_key_s = close_scope.alloc_string("return")?;
      close_scope.push_root(Value::String(return_key_s))?;
      let return_key = PropertyKey::from_string(return_key_s);
      let return_method = crate::spec_ops::get_method_with_host_and_hooks(
        evaluator.vm,
        &mut close_scope,
        &mut *evaluator.host,
        &mut *evaluator.hooks,
        iterator_value,
        return_key,
      )
      .map_err(|err| crate::vm::coerce_error_to_throw(&*evaluator.vm, &mut close_scope, err))?;

      let Some(return_method) = return_method else {
        return Ok(None);
      };

      close_scope.push_root(return_method)?;
      let return_result = evaluator
        .vm
        .call_with_host_and_hooks(
          &mut *evaluator.host,
          &mut close_scope,
          &mut *evaluator.hooks,
          return_method,
          iterator_value,
          &[],
        )
        .map_err(|err| crate::vm::coerce_error_to_throw(&*evaluator.vm, &mut close_scope, err))?;

      // Await the `return` result using the same Promise resolution semantics as `Await`:
      // - observe `.constructor` for Promise objects,
      // - but do not wrap promise subclasses (no derived promise / no @@species side effects).
      close_scope.push_root(return_result)?;
      let awaited = crate::promise_ops::promise_resolve_for_await_with_host_and_hooks(
        evaluator.vm,
        &mut close_scope,
        &mut *evaluator.host,
        &mut *evaluator.hooks,
        return_result,
      )
      .map_err(|err| crate::vm::coerce_error_to_throw(&*evaluator.vm, &mut close_scope, err))?;

      Ok(Some(awaited))
    })();

    let close_value = match close_value {
      Ok(v) => v,
      Err(err) => {
        if completion_is_throw && err.is_throw_completion() {
          self.cleanup_roots(scope.heap_mut());
          return match pending {
            RootedPending::Flow(mut flow) => {
              let flow_value = flow.to_flow(scope.heap())?;
              flow.teardown(scope.heap_mut());
              Ok(ForAwaitOfPoll::Complete(flow_value))
            }
            RootedPending::Throw(mut thrown) => {
              let err = thrown.to_error(scope.heap())?;
              thrown.teardown(scope.heap_mut());
              Err(err)
            }
          };
        }
        let mut pending = pending;
        pending.teardown(scope.heap_mut());
        self.cleanup_roots(scope.heap_mut());
        return Err(err);
      }
    };

    let Some(close_value) = close_value else {
      self.cleanup_roots(scope.heap_mut());
      return match pending {
        RootedPending::Flow(mut flow) => {
          let flow_value = flow.to_flow(scope.heap())?;
          flow.teardown(scope.heap_mut());
          Ok(ForAwaitOfPoll::Complete(flow_value))
        }
        RootedPending::Throw(mut thrown) => {
          let err = thrown.to_error(scope.heap())?;
          thrown.teardown(scope.heap_mut());
          Err(err)
        }
      };
    };

    // Await the return result.
    self.stage = ForAwaitOfStage::ClosingAwait {
      pending: Some(pending),
    };
    Ok(ForAwaitOfPoll::Await {
      kind: crate::exec::AsyncSuspendKind::AwaitResolved,
      await_value: close_value,
    })
  }
}

#[derive(Debug)]
enum ForTripleAwaitStage {
  Init,
  /// Awaiting a direct `await <expr>` in the init position.
  AwaitInitExpr,
  /// Awaiting `x = await <expr>` in the init position.
  AwaitInitAssign,
  Test,
  AwaitTest,
  Body,
  Update,
  /// Awaiting a direct `await <expr>` in the update position.
  AwaitUpdateExpr,
  /// Awaiting `x = await <expr>` in the update position.
  AwaitUpdateAssign,
}

#[derive(Debug)]
struct PendingAssignment {
  reference: AssignmentReference,
  base_root: Option<RootId>,
  key_root: Option<RootId>,
}

impl PendingAssignment {
  fn teardown(&mut self, heap: &mut crate::Heap) {
    if let Some(id) = self.base_root.take() {
      if heap.get_root(id).is_some() {
        heap.remove_root(id);
      }
    }
    if let Some(id) = self.key_root.take() {
      if heap.get_root(id).is_some() {
        heap.remove_root(id);
      }
    }
  }
}

#[derive(Debug)]
struct ForTripleAwaitState {
  init: Option<hir_js::ForInit>,
  test: Option<hir_js::ExprId>,
  update: Option<hir_js::ExprId>,
  body_stmt: hir_js::StmtId,
  label_set: Box<[hir_js::NameId]>,

  outer_lex: Option<GcEnv>,
  iter_env: Option<GcEnv>,
  v_root: Option<RootId>,
  pending_assign: Option<PendingAssignment>,
  stage: ForTripleAwaitStage,
}

#[derive(Debug)]
enum ForTripleAwaitPoll {
  Complete(Flow),
  Await {
    kind: crate::exec::AsyncSuspendKind,
    await_value: Value,
  },
}

impl ForTripleAwaitState {
  fn new(
    init: Option<hir_js::ForInit>,
    test: Option<hir_js::ExprId>,
    update: Option<hir_js::ExprId>,
    body_stmt: hir_js::StmtId,
    label_set: &[hir_js::NameId],
  ) -> Result<Self, VmError> {
    let mut labels: Vec<hir_js::NameId> = Vec::new();
    labels
      .try_reserve_exact(label_set.len())
      .map_err(|_| VmError::OutOfMemory)?;
    labels.extend_from_slice(label_set);
    Ok(Self {
      init,
      test,
      update,
      body_stmt,
      label_set: labels.into_boxed_slice(),
      outer_lex: None,
      iter_env: None,
      v_root: None,
      pending_assign: None,
      stage: ForTripleAwaitStage::Init,
    })
  }

  fn cleanup_roots(&mut self, heap: &mut crate::Heap) {
    if let Some(id) = self.v_root.take() {
      if heap.get_root(id).is_some() {
        heap.remove_root(id);
      }
    }
    if let Some(pending) = self.pending_assign.as_mut() {
      pending.teardown(heap);
    }
    self.pending_assign = None;
  }

  fn teardown(&mut self, heap: &mut crate::Heap) {
    self.cleanup_roots(heap);
  }

  fn poll(
    &mut self,
    evaluator: &mut HirEvaluator<'_>,
    scope: &mut Scope<'_>,
    body: &hir_js::Body,
    mut resume_value: Option<Result<Value, VmError>>,
  ) -> Result<ForTripleAwaitPoll, VmError> {
    let result: Result<ForTripleAwaitPoll, VmError> = (|| {
      loop {
        match self.stage {
          ForTripleAwaitStage::Init => {
          if resume_value.is_some() {
            return Err(VmError::InvariantViolation(
              "for-triple init received resume value",
            ));
          }

            // Match the synchronous evaluator's "one tick per statement evaluation" budget.
            evaluator.vm.tick()?;

          if self.outer_lex.is_none() {
            self.outer_lex = Some(evaluator.env.lexical_env());
          }
          let outer_lex = self
            .outer_lex
            .ok_or(VmError::InvariantViolation("missing for-triple outer lex env"))?;

            if self.v_root.is_none() {
              // Root the loop completion value across suspensions.
              let v_root = scope.heap_mut().add_root(Value::Undefined)?;
              self.v_root = Some(v_root);
            }

          // Lexically-declared `for` loops require per-iteration environments so closures capture
          // the correct binding value (ECMA-262 `CreatePerIterationEnvironment`).
          let lexical_init = match self.init.as_ref() {
            Some(hir_js::ForInit::Var(decl))
              if matches!(
                decl.kind,
                hir_js::VarDeclKind::Let
                  | hir_js::VarDeclKind::Const
                  | hir_js::VarDeclKind::Using
                  | hir_js::VarDeclKind::AwaitUsing
              ) =>
            {
              Some(decl)
            }
            _ => None,
          };

            if let Some(init_decl) = lexical_init {
            // Create a loop-scoped declarative environment for the lexical declaration and evaluate
            // the initializer with TDZ semantics.
            let loop_env = scope.env_create(Some(outer_lex))?;
            evaluator.env.set_lexical_env(scope.heap_mut(), loop_env);

            // Bind names in TDZ before evaluating initializers.
            for declarator in &init_decl.declarators {
              evaluator.vm.tick()?;
              let mut names: Vec<hir_js::NameId> = Vec::new();
              evaluator.collect_pat_idents(body, declarator.pat, &mut names)?;
              for name_id in names {
                let name = evaluator.resolve_name(name_id)?;
                if scope.heap().env_has_binding(loop_env, name.as_str())? {
                  continue;
                }
                match init_decl.kind {
                  hir_js::VarDeclKind::Let => scope.env_create_mutable_binding(loop_env, name.as_str())?,
                  hir_js::VarDeclKind::Const
                  | hir_js::VarDeclKind::Using
                  | hir_js::VarDeclKind::AwaitUsing => scope.env_create_immutable_binding(loop_env, name.as_str())?,
                  _ => {
                    return Err(VmError::InvariantViolation(
                      "unexpected VarDeclKind in lexical for-loop initialization (hir async)",
                    ));
                  }
                }
              }
            }

            // Evaluate initializer(s) and initialize the bindings.
            evaluator.eval_var_decl(scope, body, init_decl)?;

            // Enter the first per-iteration environment.
            let iter_env = evaluator.create_for_triple_per_iteration_env(scope, outer_lex, loop_env)?;
            evaluator.env.set_lexical_env(scope.heap_mut(), iter_env);
            self.iter_env = Some(iter_env);

              self.stage = ForTripleAwaitStage::Test;
              continue;
            }

          // Non-lexical init.
          if let Some(init) = &self.init {
            match init {
              hir_js::ForInit::Expr(expr_id) => {
                let expr = evaluator.get_expr(body, *expr_id)?;
                match &expr.kind {
                  hir_js::ExprKind::Await { expr: awaited_expr } => {
                    // Budget once for the await expression itself.
                    evaluator.vm.tick()?;
                    let await_value = evaluator.eval_expr(scope, body, *awaited_expr)?;
                    self.stage = ForTripleAwaitStage::AwaitInitExpr;
                    return Ok(ForTripleAwaitPoll::Await {
                      kind: crate::exec::AsyncSuspendKind::Await,
                      await_value,
                    });
                  }
                  hir_js::ExprKind::Assignment {
                    op: hir_js::AssignOp::Assign,
                    target,
                    value,
                  } => {
                    let rhs = evaluator.get_expr(body, *value)?;
                    if let hir_js::ExprKind::Await { expr: awaited_expr } = &rhs.kind {
                      // Budget once for the init expression and once for the await expression node.
                      evaluator.vm.tick()?;
                      evaluator.vm.tick()?;

                      let reference = evaluator.eval_assignment_reference(scope, body, *target)?;

                      let mut await_scope = scope.reborrow();
                      evaluator.root_assignment_reference(&mut await_scope, &reference)?;
                      let await_value = evaluator.eval_expr(&mut await_scope, body, *awaited_expr)?;
                      await_scope.push_root(await_value)?;

                      let mut base_root = None;
                      let mut key_root = None;
                      match &reference {
                        AssignmentReference::Binding(_) => {}
                        AssignmentReference::Property { base, key } => {
                          let key_value = match key {
                            PropertyKey::String(s) => Value::String(*s),
                            PropertyKey::Symbol(s) => Value::Symbol(*s),
                          };
                          // Root base+key across root registration.
                          await_scope.push_roots(&[*base, key_value])?;
                          base_root = Some(await_scope.heap_mut().add_root(*base)?);
                          key_root = Some(await_scope.heap_mut().add_root(key_value)?);
                        }
                        AssignmentReference::SuperProperty { super_base, key, .. } => {
                          let key_value = match key {
                            PropertyKey::String(s) => Value::String(*s),
                            PropertyKey::Symbol(s) => Value::Symbol(*s),
                          };
                          let base_value = (*super_base).map(Value::Object).unwrap_or(Value::Null);
                          await_scope.push_roots(&[base_value, key_value])?;
                          base_root = Some(await_scope.heap_mut().add_root(base_value)?);
                          key_root = Some(await_scope.heap_mut().add_root(key_value)?);
                        }
                      }

                      self.pending_assign = Some(PendingAssignment {
                        reference,
                        base_root,
                        key_root,
                      });
                      self.stage = ForTripleAwaitStage::AwaitInitAssign;
                      return Ok(ForTripleAwaitPoll::Await {
                        kind: crate::exec::AsyncSuspendKind::Await,
                        await_value,
                      });
                    }
                    // No await in RHS; fall through to synchronous eval below.
                    let _ = evaluator.eval_expr(scope, body, *expr_id)?;
                  }
                  _ => {
                    let _ = evaluator.eval_expr(scope, body, *expr_id)?;
                  }
                }
              }
              hir_js::ForInit::Var(decl) => {
                evaluator.eval_var_decl(scope, body, decl)?;
              }
            }
          }

            self.stage = ForTripleAwaitStage::Test;
            continue;
          }

        ForTripleAwaitStage::AwaitInitExpr => {
          let Some(resume) = resume_value.take() else {
            return Err(VmError::InvariantViolation(
              "for-triple init await missing resume value",
            ));
          };
          match resume {
            Ok(_) => {}
            Err(err) => {
              self.restore_outer_lex(evaluator, scope);
              self.cleanup_roots(scope.heap_mut());
              return Err(err);
            }
          }
            self.stage = ForTripleAwaitStage::Test;
            continue;
        }

        ForTripleAwaitStage::AwaitInitAssign => {
          let Some(resume) = resume_value.take() else {
            return Err(VmError::InvariantViolation(
              "for-triple init assignment await missing resume value",
            ));
          };

          let resumed_value = match resume {
            Ok(v) => v,
            Err(err) => {
              self.restore_outer_lex(evaluator, scope);
              self.cleanup_roots(scope.heap_mut());
              return Err(err);
            }
          };

          let mut assign_scope = scope.reborrow();
          let mut pending = self
            .pending_assign
            .take()
            .ok_or(VmError::InvariantViolation("missing pending init assignment"))?;
          evaluator.root_assignment_reference(&mut assign_scope, &pending.reference)?;
          assign_scope.push_root(resumed_value)?;
          evaluator.maybe_set_anonymous_function_name_for_assignment(
            &mut assign_scope,
            &pending.reference,
            resumed_value,
          )?;
          evaluator.put_value_to_assignment_reference(&mut assign_scope, &pending.reference, resumed_value)?;
          pending.teardown(assign_scope.heap_mut());

            self.stage = ForTripleAwaitStage::Test;
            continue;
        }

        ForTripleAwaitStage::Test => {
          if resume_value.is_some() {
            return Err(VmError::InvariantViolation(
              "for-triple test received unexpected resume value",
            ));
          }

          // Tick once per iteration so `for (...) {}` is budgeted even when body is empty.
          evaluator.vm.tick()?;

          if let Some(test_id) = self.test {
            let test_expr = evaluator.get_expr(body, test_id)?;
            if let hir_js::ExprKind::Await { expr: awaited_expr } = &test_expr.kind {
              // Budget once for the await expression node (the awaited subexpression is budgeted by
              // `eval_expr`).
              evaluator.vm.tick()?;
              let await_value = evaluator.eval_expr(scope, body, *awaited_expr)?;
              self.stage = ForTripleAwaitStage::AwaitTest;
              return Ok(ForTripleAwaitPoll::Await {
                kind: crate::exec::AsyncSuspendKind::Await,
                await_value,
              });
            }

            let test_value = evaluator.eval_expr(scope, body, test_id)?;
            if !scope.heap().to_boolean(test_value)? {
              let v = self.get_loop_value(scope.heap())?;
              self.restore_outer_lex(evaluator, scope);
              self.cleanup_roots(scope.heap_mut());
              return Ok(ForTripleAwaitPoll::Complete(Flow::Normal(Some(v))));
            }
          }

            self.stage = ForTripleAwaitStage::Body;
            continue;
        }

        ForTripleAwaitStage::AwaitTest => {
          let Some(resume) = resume_value.take() else {
            return Err(VmError::InvariantViolation(
              "for-triple test await missing resume value",
            ));
          };

          let test_value = match resume {
            Ok(v) => v,
            Err(err) => {
              let v = self.get_loop_value(scope.heap())?;
              let _ = v;
              self.restore_outer_lex(evaluator, scope);
              self.cleanup_roots(scope.heap_mut());
              return Err(err);
            }
          };

          if !scope.heap().to_boolean(test_value)? {
            let v = self.get_loop_value(scope.heap())?;
            self.restore_outer_lex(evaluator, scope);
            self.cleanup_roots(scope.heap_mut());
            return Ok(ForTripleAwaitPoll::Complete(Flow::Normal(Some(v))));
          }

            self.stage = ForTripleAwaitStage::Body;
            continue;
        }

        ForTripleAwaitStage::Body => {
          if resume_value.is_some() {
            return Err(VmError::InvariantViolation(
              "for-triple body received unexpected resume value",
            ));
          }

          let flow = evaluator.eval_stmt(scope, body, self.body_stmt)?;
          let v_root = self
            .v_root
            .ok_or(VmError::InvariantViolation("missing for-triple loop value root"))?;
          let v = scope
            .heap()
            .get_root(v_root)
            .ok_or(VmError::InvariantViolation("missing for-triple loop value root"))?;

            match flow {
              Flow::Normal(value) => {
                if let Some(value) = value {
                  scope.heap_mut().set_root(v_root, value);
                }
              }
              Flow::Continue(None, value) => {
                if let Some(value) = value {
                  scope.heap_mut().set_root(v_root, value);
                }
              }
              Flow::Continue(Some(label), value) if self.label_set.iter().any(|l| *l == label) => {
                if let Some(value) = value {
                  scope.heap_mut().set_root(v_root, value);
                }
              }
            Flow::Continue(Some(label), value) => {
              self.restore_outer_lex(evaluator, scope);
              self.cleanup_roots(scope.heap_mut());
              return Ok(ForTripleAwaitPoll::Complete(
                Flow::Continue(Some(label), value).update_empty(Some(v)),
              ));
            }
            Flow::Break(None, break_value) => {
              let out = break_value.unwrap_or(v);
              scope.heap_mut().set_root(v_root, out);
              self.restore_outer_lex(evaluator, scope);
              self.cleanup_roots(scope.heap_mut());
              return Ok(ForTripleAwaitPoll::Complete(Flow::Normal(Some(out))));
            }
            Flow::Break(Some(label), break_value) => {
              self.restore_outer_lex(evaluator, scope);
              self.cleanup_roots(scope.heap_mut());
              return Ok(ForTripleAwaitPoll::Complete(
                Flow::Break(Some(label), break_value).update_empty(Some(v)),
              ));
            }
            Flow::Return(v) => {
              self.restore_outer_lex(evaluator, scope);
              self.cleanup_roots(scope.heap_mut());
              return Ok(ForTripleAwaitPoll::Complete(Flow::Return(v)));
            }
          }

            self.stage = ForTripleAwaitStage::Update;
            continue;
        }

        ForTripleAwaitStage::Update => {
          if resume_value.is_some() {
            return Err(VmError::InvariantViolation(
              "for-triple update received unexpected resume value",
            ));
          }

          // Create the next per-iteration environment *before* evaluating the update expression for
          // lexically-declared loops, matching the synchronous evaluator.
          if let Some(iter_env) = self.iter_env {
            let outer_lex = self
              .outer_lex
              .ok_or(VmError::InvariantViolation("missing for-triple outer lex env"))?;
            let next_env = evaluator.create_for_triple_per_iteration_env(scope, outer_lex, iter_env)?;
            evaluator.env.set_lexical_env(scope.heap_mut(), next_env);
            self.iter_env = Some(next_env);
          }

          if let Some(update_id) = self.update {
            let update_expr = evaluator.get_expr(body, update_id)?;
            match &update_expr.kind {
              hir_js::ExprKind::Await { expr: awaited_expr } => {
                evaluator.vm.tick()?;
                let await_value = evaluator.eval_expr(scope, body, *awaited_expr)?;
                self.stage = ForTripleAwaitStage::AwaitUpdateExpr;
                return Ok(ForTripleAwaitPoll::Await {
                  kind: crate::exec::AsyncSuspendKind::Await,
                  await_value,
                });
              }
              hir_js::ExprKind::Assignment {
                op: hir_js::AssignOp::Assign,
                target,
                value,
              } => {
                let rhs = evaluator.get_expr(body, *value)?;
                if let hir_js::ExprKind::Await { expr: awaited_expr } = &rhs.kind {
                  // Budget once for the update expression and once for the await expression node.
                  evaluator.vm.tick()?;
                  evaluator.vm.tick()?;

                  let reference = evaluator.eval_assignment_reference(scope, body, *target)?;

                  let mut await_scope = scope.reborrow();
                  evaluator.root_assignment_reference(&mut await_scope, &reference)?;
                  let await_value = evaluator.eval_expr(&mut await_scope, body, *awaited_expr)?;
                  await_scope.push_root(await_value)?;

                  let mut base_root = None;
                  let mut key_root = None;
                  match &reference {
                    AssignmentReference::Binding(_) => {}
                    AssignmentReference::Property { base, key } => {
                      let key_value = match key {
                        PropertyKey::String(s) => Value::String(*s),
                        PropertyKey::Symbol(s) => Value::Symbol(*s),
                      };
                      await_scope.push_roots(&[*base, key_value])?;
                      base_root = Some(await_scope.heap_mut().add_root(*base)?);
                      key_root = Some(await_scope.heap_mut().add_root(key_value)?);
                    }
                    AssignmentReference::SuperProperty { super_base, key, .. } => {
                      let key_value = match key {
                        PropertyKey::String(s) => Value::String(*s),
                        PropertyKey::Symbol(s) => Value::Symbol(*s),
                      };
                      let base_value = (*super_base).map(Value::Object).unwrap_or(Value::Null);
                      await_scope.push_roots(&[base_value, key_value])?;
                      base_root = Some(await_scope.heap_mut().add_root(base_value)?);
                      key_root = Some(await_scope.heap_mut().add_root(key_value)?);
                    }
                  }

                  self.pending_assign = Some(PendingAssignment {
                    reference,
                    base_root,
                    key_root,
                  });
                  self.stage = ForTripleAwaitStage::AwaitUpdateAssign;
                  return Ok(ForTripleAwaitPoll::Await {
                    kind: crate::exec::AsyncSuspendKind::Await,
                    await_value,
                  });
                }
                // No await in RHS; fall through to synchronous eval below.
                let _ = evaluator.eval_expr(scope, body, update_id)?;
              }
              _ => {
                let _ = evaluator.eval_expr(scope, body, update_id)?;
              }
            }
          }

            self.stage = ForTripleAwaitStage::Test;
            continue;
        }

        ForTripleAwaitStage::AwaitUpdateExpr => {
          let Some(resume) = resume_value.take() else {
            return Err(VmError::InvariantViolation(
              "for-triple update await missing resume value",
            ));
          };
          match resume {
            Ok(_) => {}
            Err(err) => {
              self.restore_outer_lex(evaluator, scope);
              self.cleanup_roots(scope.heap_mut());
              return Err(err);
            }
          }
          self.stage = ForTripleAwaitStage::Test;
          continue;
        }

        ForTripleAwaitStage::AwaitUpdateAssign => {
          let Some(resume) = resume_value.take() else {
            return Err(VmError::InvariantViolation(
              "for-triple update assignment await missing resume value",
            ));
          };

          let resumed_value = match resume {
            Ok(v) => v,
            Err(err) => {
              self.restore_outer_lex(evaluator, scope);
              self.cleanup_roots(scope.heap_mut());
              return Err(err);
            }
          };

          let mut assign_scope = scope.reborrow();
          let mut pending = self
            .pending_assign
            .take()
            .ok_or(VmError::InvariantViolation("missing pending update assignment"))?;
          evaluator.root_assignment_reference(&mut assign_scope, &pending.reference)?;
          assign_scope.push_root(resumed_value)?;
          evaluator.maybe_set_anonymous_function_name_for_assignment(
            &mut assign_scope,
            &pending.reference,
            resumed_value,
          )?;
          evaluator.put_value_to_assignment_reference(&mut assign_scope, &pending.reference, resumed_value)?;
          pending.teardown(assign_scope.heap_mut());

            self.stage = ForTripleAwaitStage::Test;
            continue;
        }
      }
      }
    })();

    match result {
      Ok(v) => Ok(v),
      Err(err) => {
        // Ensure loop-scoped envs/roots do not leak when unwinding on an error. The caller will
        // clear `self.active` before returning the completion.
        self.restore_outer_lex(evaluator, scope);
        self.cleanup_roots(scope.heap_mut());
        Err(err)
      }
    }
  }

  fn restore_outer_lex(&self, evaluator: &mut HirEvaluator<'_>, scope: &mut Scope<'_>) {
    let is_lexical = matches!(
      self.init.as_ref(),
      Some(hir_js::ForInit::Var(decl))
        if matches!(
          decl.kind,
          hir_js::VarDeclKind::Let
            | hir_js::VarDeclKind::Const
            | hir_js::VarDeclKind::Using
            | hir_js::VarDeclKind::AwaitUsing
        )
    );
    if is_lexical {
      if let Some(outer) = self.outer_lex {
        evaluator.env.set_lexical_env(scope.heap_mut(), outer);
      }
    }
  }

  fn get_loop_value(&self, heap: &crate::Heap) -> Result<Value, VmError> {
    let v_root = self
      .v_root
      .ok_or(VmError::InvariantViolation("missing for-triple loop value root"))?;
    heap
      .get_root(v_root)
      .ok_or(VmError::InvariantViolation("missing for-triple loop value root"))
  }
}

#[derive(Debug)]
enum HirAsyncBodyKind {
  Block { stmts: Box<[hir_js::StmtId]> },
  Expr { expr: hir_js::ExprId },
}

#[derive(Debug)]
enum HirAsyncActive {
  ForAwaitOf(ForAwaitOfState),
  ForTriple(ForTripleAwaitState),
  /// Suspended at a direct `await <expr>;` expression statement.
  AwaitExprStmt { next_stmt_index: usize },
  /// Suspended at a `return await <expr>;`.
  AwaitReturn,
  /// Suspended at a `throw await <expr>;`.
  AwaitThrow,
  /// Suspended at a `var`/`let`/`const` declarator initializer of the form `await <expr>`.
  AwaitVarDecl {
    stmt_index: usize,
    declarator_index: usize,
  },
}

impl HirAsyncActive {
  fn teardown(&mut self, heap: &mut crate::Heap) {
    match self {
      HirAsyncActive::ForAwaitOf(state) => state.teardown(heap),
      HirAsyncActive::ForTriple(state) => state.teardown(heap),
      HirAsyncActive::AwaitExprStmt { .. }
      | HirAsyncActive::AwaitReturn
      | HirAsyncActive::AwaitThrow
      | HirAsyncActive::AwaitVarDecl { .. } => {}
    }
  }
}

#[derive(Debug)]
pub(crate) struct HirAsyncState {
  script: Arc<CompiledScript>,
  body_id: hir_js::BodyId,
  allow_new_target_in_eval: bool,
  await_stmt_offset: u32,
  body_kind: HirAsyncBodyKind,
  in_root_stmt_list: bool,
  next_stmt_index: usize,
  active: Option<HirAsyncActive>,
}

impl HirAsyncState {
  pub(crate) fn script_source(&self) -> Arc<crate::SourceText> {
    self.script.source.clone()
  }

  pub(crate) fn await_stmt_offset(&self) -> u32 {
    self.await_stmt_offset
  }

  pub(crate) fn new(script: Arc<CompiledScript>, body_id: hir_js::BodyId) -> Result<Self, VmError> {
    let body = script
      .hir
      .body(body_id)
      .ok_or(VmError::InvariantViolation("hir body id missing from compiled script"))?;
    let (allow_new_target_in_eval, body_kind, in_root_stmt_list) = match body.kind {
      // Root script/module body.
      hir_js::BodyKind::TopLevel => {
        let mut cloned: Vec<hir_js::StmtId> = Vec::new();
        cloned
          .try_reserve_exact(body.root_stmts.len())
          .map_err(|_| VmError::OutOfMemory)?;
        cloned.extend_from_slice(body.root_stmts.as_slice());
        (
          /* allow_new_target_in_eval */ false,
          HirAsyncBodyKind::Block {
            stmts: cloned.into_boxed_slice(),
          },
          /* in_root_stmt_list */ true,
        )
      }
      // Async function body (block or expression-bodied arrow).
      hir_js::BodyKind::Function => {
        let Some(func_meta) = body.function.as_ref() else {
          return Err(VmError::InvariantViolation("function body missing metadata"));
        };
        let allow_new_target_in_eval = !func_meta.is_arrow;
        let body_kind = match &func_meta.body {
          hir_js::FunctionBody::Block(stmts) => {
            let mut cloned: Vec<hir_js::StmtId> = Vec::new();
            cloned
              .try_reserve_exact(stmts.len())
              .map_err(|_| VmError::OutOfMemory)?;
            cloned.extend_from_slice(stmts);
            HirAsyncBodyKind::Block {
              stmts: cloned.into_boxed_slice(),
            }
          }
          hir_js::FunctionBody::Expr(expr) => HirAsyncBodyKind::Expr { expr: *expr },
        };
        (allow_new_target_in_eval, body_kind, /* in_root_stmt_list */ false)
      }
      other => {
        return Err(VmError::InvariantViolation(match other {
          hir_js::BodyKind::Class => "compiled async body is a class body",
          hir_js::BodyKind::Initializer => "compiled async body is an initializer body",
          hir_js::BodyKind::Unknown => "compiled async body is an unknown body kind",
          hir_js::BodyKind::TopLevel | hir_js::BodyKind::Function => unreachable!(),
        }))
      }
    };
    Ok(Self {
      script,
      body_id,
      allow_new_target_in_eval,
      await_stmt_offset: 0,
      body_kind,
      in_root_stmt_list,
      next_stmt_index: 0,
      active: None,
    })
  }

  pub(crate) fn teardown(&mut self, heap: &mut crate::Heap) {
    if let Some(active) = &mut self.active {
      active.teardown(heap);
    }
  }

  pub(crate) fn start(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    env: &mut RuntimeEnv,
    strict: bool,
    this: Value,
    new_target: Value,
    home_object: Option<GcObject>,
  ) -> Result<HirAsyncResult, VmError> {
    let mut evaluator = HirEvaluator {
      vm,
      host,
      hooks,
      env,
      strict,
      this,
      this_initialized: true,
      class_constructor: None,
      derived_constructor: false,
      this_root_idx: None,
      new_target,
      allow_new_target_in_eval: self.allow_new_target_in_eval,
      home_object,
      script: self.script.clone(),
    };
    self.drive(&mut evaluator, scope, None)
  }

  pub(crate) fn resume(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    env: &mut RuntimeEnv,
    strict: bool,
    this: Value,
    new_target: Value,
    home_object: Option<GcObject>,
    resume_value: Result<Value, VmError>,
  ) -> Result<HirAsyncResult, VmError> {
    let mut evaluator = HirEvaluator {
      vm,
      host,
      hooks,
      env,
      strict,
      this,
      this_initialized: true,
      class_constructor: None,
      derived_constructor: false,
      this_root_idx: None,
      new_target,
      allow_new_target_in_eval: self.allow_new_target_in_eval,
      home_object,
      script: self.script.clone(),
    };
    self.drive(&mut evaluator, scope, Some(resume_value))
  }

  fn drive(
    &mut self,
    evaluator: &mut HirEvaluator<'_>,
    scope: &mut Scope<'_>,
    mut resume_value: Option<Result<Value, VmError>>,
  ) -> Result<HirAsyncResult, VmError> {
    let body = self
      .script
      .hir
      .body(self.body_id)
      .ok_or(VmError::InvariantViolation("hir body id missing from compiled script"))?;

    loop {
      if let Some(active) = &mut self.active {
        match active {
          HirAsyncActive::ForAwaitOf(state) => {
            let poll = state.poll(evaluator, scope, body, resume_value.take());
            match poll {
              Ok(ForAwaitOfPoll::Await { kind, await_value }) => {
                let stmt_offset = match &self.body_kind {
                  HirAsyncBodyKind::Block { stmts } => stmts
                    .get(self.next_stmt_index)
                    .and_then(|stmt_id| evaluator.get_stmt(body, *stmt_id).ok())
                    .map(|stmt| stmt.span.start)
                    .unwrap_or(0),
                  HirAsyncBodyKind::Expr { .. } => 0,
                };
                self.await_stmt_offset = stmt_offset;
                return Ok(HirAsyncResult::Await { kind, await_value });
              }
              Ok(ForAwaitOfPoll::Complete(flow)) => {
                self.active = None;
                match flow {
                  Flow::Normal(_) => {
                    self.next_stmt_index = self.next_stmt_index.saturating_add(1);
                    continue;
                  }
                  Flow::Return(v) => return Ok(HirAsyncResult::CompleteOk(v)),
                  Flow::Break(..) | Flow::Continue(..) => {
                    return Err(VmError::InvariantViolation(
                      "async compiled function body produced break/continue completion",
                    ))
                  }
                }
              }
              Err(err) => {
                self.active = None;
                let stmt_offset = match &self.body_kind {
                  HirAsyncBodyKind::Block { stmts } => stmts
                    .get(self.next_stmt_index)
                    .and_then(|stmt_id| evaluator.get_stmt(body, *stmt_id).ok())
                    .map(|stmt| stmt.span.start)
                    .unwrap_or(0),
                  HirAsyncBodyKind::Expr { .. } => 0,
                };
                let err = finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  scope,
                  evaluator.script.source.as_ref(),
                  stmt_offset,
                  err,
                );
              match err {
                VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                  return Ok(HirAsyncResult::CompleteThrow(value))
                }
                  other => return Err(other),
                }
              }
            }
          }
          HirAsyncActive::ForTriple(state) => {
            let poll = state.poll(evaluator, scope, body, resume_value.take());
            match poll {
              Ok(ForTripleAwaitPoll::Await { kind, await_value }) => {
                return Ok(HirAsyncResult::Await { kind, await_value });
              }
              Ok(ForTripleAwaitPoll::Complete(flow)) => {
                self.active = None;
                match flow {
                  Flow::Normal(_) => {
                    self.next_stmt_index = self.next_stmt_index.saturating_add(1);
                    continue;
                  }
                  Flow::Return(v) => return Ok(HirAsyncResult::CompleteOk(v)),
                  Flow::Break(..) | Flow::Continue(..) => {
                    return Err(VmError::InvariantViolation(
                      "async compiled function body produced break/continue completion",
                    ))
                  }
                }
              }
              Err(err) => {
                self.active = None;
                let stmt_offset = match &self.body_kind {
                  HirAsyncBodyKind::Block { stmts } => stmts
                    .get(self.next_stmt_index)
                    .and_then(|stmt_id| evaluator.get_stmt(body, *stmt_id).ok())
                    .map(|stmt| stmt.span.start)
                    .unwrap_or(0),
                  HirAsyncBodyKind::Expr { .. } => 0,
                };
                let err = finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  scope,
                  evaluator.script.source.as_ref(),
                  stmt_offset,
                  err,
                );
                match err {
                  VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                    return Ok(HirAsyncResult::CompleteThrow(value))
                  }
                  other => return Err(other),
                }
              }
            }
          }

          HirAsyncActive::AwaitExprStmt { next_stmt_index } => {
            let next_stmt_index = *next_stmt_index;
            let Some(resume) = resume_value.take() else {
              return Err(VmError::InvariantViolation(
                "hir async await expr statement missing resume value",
              ));
            };
            self.active = None;
            match resume {
              Ok(_) => {
                self.next_stmt_index = next_stmt_index;
                continue;
              }
              Err(err) => {
                let await_stmt_index = next_stmt_index.saturating_sub(1);
                let stmt_offset = match &self.body_kind {
                  HirAsyncBodyKind::Block { stmts } => stmts
                    .get(await_stmt_index)
                    .and_then(|stmt_id| evaluator.get_stmt(body, *stmt_id).ok())
                    .map(|stmt| stmt.span.start)
                    .unwrap_or(0),
                  HirAsyncBodyKind::Expr { .. } => 0,
                };
                let err = finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  scope,
                  evaluator.script.source.as_ref(),
                  stmt_offset,
                  err,
                );
                match err {
                  VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                    return Ok(HirAsyncResult::CompleteThrow(value))
                  }
                  other => return Err(other),
                }
              }
            }
          }
          HirAsyncActive::AwaitReturn => {
            let Some(resume) = resume_value.take() else {
              return Err(VmError::InvariantViolation(
                "hir async await return missing resume value",
              ));
            };
            self.active = None;
            match resume {
              Ok(v) => return Ok(HirAsyncResult::CompleteOk(v)),
              Err(err) => {
                let stmt_offset = match &self.body_kind {
                  HirAsyncBodyKind::Block { stmts } => stmts
                    .get(self.next_stmt_index)
                    .and_then(|stmt_id| evaluator.get_stmt(body, *stmt_id).ok())
                    .map(|stmt| stmt.span.start)
                    .unwrap_or(0),
                  HirAsyncBodyKind::Expr { .. } => 0,
                };
                let err = finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  scope,
                  evaluator.script.source.as_ref(),
                  stmt_offset,
                  err,
                );
                match err {
                  VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                    return Ok(HirAsyncResult::CompleteThrow(value))
                  }
                  other => return Err(other),
                }
              }
            }
          }

          HirAsyncActive::AwaitThrow => {
            let Some(resume) = resume_value.take() else {
              return Err(VmError::InvariantViolation(
                "hir async await throw missing resume value",
              ));
            };
            self.active = None;
            let stmt_offset = match &self.body_kind {
              HirAsyncBodyKind::Block { stmts } => stmts
                .get(self.next_stmt_index)
                .and_then(|stmt_id| evaluator.get_stmt(body, *stmt_id).ok())
                .map(|stmt| stmt.span.start)
                .unwrap_or(0),
              HirAsyncBodyKind::Expr { .. } => 0,
            };
            match resume {
              Ok(v) => {
                // `throw await <expr>;` must behave like a normal `throw` statement once the awaited
                // value has resolved: capture a stack trace and attach the `stack` property at the
                // throw site so user code can inspect it.
                let err = finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  scope,
                  evaluator.script.source.as_ref(),
                  stmt_offset,
                  VmError::Throw(v),
                );
                match err {
                  VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                    return Ok(HirAsyncResult::CompleteThrow(value))
                  }
                  other => return Err(other),
                }
              }
              Err(err) => {
                let err = finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  scope,
                  evaluator.script.source.as_ref(),
                  stmt_offset,
                  err,
                );
                match err {
                  VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                    return Ok(HirAsyncResult::CompleteThrow(value))
                  }
                  other => return Err(other),
                }
              }
            }
          }
          HirAsyncActive::AwaitVarDecl {
            stmt_index,
            declarator_index,
          } => {
            let stmt_index = *stmt_index;
            let declarator_index = *declarator_index;
            let Some(resume) = resume_value.take() else {
              return Err(VmError::InvariantViolation(
                "hir async await var decl missing resume value",
              ));
            };

            self.active = None;

            // Rejection from the awaited promise becomes a throw at the await site.
            let resumed_value = match resume {
              Ok(v) => v,
              Err(err) => {
                let stmt_offset = match &self.body_kind {
                  HirAsyncBodyKind::Block { stmts } => stmts
                    .get(stmt_index)
                    .and_then(|stmt_id| evaluator.get_stmt(body, *stmt_id).ok())
                    .map(|stmt| stmt.span.start)
                    .unwrap_or(0),
                  HirAsyncBodyKind::Expr { .. } => 0,
                };
                let err = finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  scope,
                  evaluator.script.source.as_ref(),
                  stmt_offset,
                  err,
                );
                match err {
                  VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                    return Ok(HirAsyncResult::CompleteThrow(value))
                  }
                  other => return Err(other),
                }
              }
            };
            // This resume point can only exist for block-bodied async functions.
            let HirAsyncBodyKind::Block { stmts } = &self.body_kind else {
              return Err(VmError::InvariantViolation(
                "hir async var decl resume used for non-block body",
              ));
            };
            let stmt_id = *stmts.get(stmt_index).ok_or(VmError::InvariantViolation(
              "hir async var decl resume stmt index out of bounds",
            ))?;
            let stmt = evaluator.get_stmt(body, stmt_id)?;
            let stmt_offset = stmt.span.start;
            let hir_js::StmtKind::Var(var_decl) = &stmt.kind else {
              return Err(VmError::InvariantViolation(
                "hir async var decl resume target is not a var declaration",
              ));
            };
            let declarator = var_decl.declarators.get(declarator_index).ok_or(
              VmError::InvariantViolation("hir async var decl resume declarator index out of bounds"),
            )?;

            // Initialize the awaited declarator binding with the resumed value.
            if let Err(err) = evaluator.bind_var_decl_pat(
              scope,
              body,
              declarator.pat,
              var_decl.kind,
              /* init_missing */ false,
              resumed_value,
            ) {
              let err = finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                scope,
                evaluator.script.source.as_ref(),
                stmt_offset,
                err,
              );
              match err {
                VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                  return Ok(HirAsyncResult::CompleteThrow(value))
                }
                other => return Err(other),
              }
            }

            // Continue evaluating subsequent declarators in the same declaration.
            for (j, declarator) in var_decl
              .declarators
              .iter()
              .enumerate()
              .skip(declarator_index.saturating_add(1))
            {
              evaluator.vm.tick()?;
              let init_missing = declarator.init.is_none();
              if let Some(init) = declarator.init {
                let init_expr = evaluator.get_expr(body, init)?;
                if let hir_js::ExprKind::Await { expr: awaited_expr } = init_expr.kind {
                  // Budget once for the await expression itself, matching synchronous evaluation.
                  evaluator.vm.tick()?;
                  let await_value = match evaluator.eval_expr(scope, body, awaited_expr) {
                    Ok(v) => v,
                    Err(err) => {
                      let err = finalize_throw_with_stack_at_source_offset(
                        &*evaluator.vm,
                        scope,
                        evaluator.script.source.as_ref(),
                        stmt_offset,
                        err,
                      );
                      match err {
                        VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                          return Ok(HirAsyncResult::CompleteThrow(value))
                        }
                        other => return Err(other),
                      }
                    }
                  };
                  self.active = Some(HirAsyncActive::AwaitVarDecl {
                    stmt_index,
                    declarator_index: j,
                  });
                  self.next_stmt_index = stmt_index;
                  self.await_stmt_offset = stmt_offset;
                  return Ok(HirAsyncResult::Await {
                    kind: crate::exec::AsyncSuspendKind::Await,
                    await_value,
                  });
                }
              }

              let value = match declarator.init {
                Some(init) => match evaluator.eval_expr(scope, body, init) {
                  Ok(v) => v,
                  Err(err) => {
                    let err = finalize_throw_with_stack_at_source_offset(
                      &*evaluator.vm,
                      scope,
                      evaluator.script.source.as_ref(),
                      stmt_offset,
                      err,
                    );
                    match err {
                      VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                        return Ok(HirAsyncResult::CompleteThrow(value))
                      }
                      other => return Err(other),
                    }
                  }
                },
                None => Value::Undefined,
              };

              if let Err(err) = evaluator.bind_var_decl_pat(
                scope,
                body,
                declarator.pat,
                var_decl.kind,
                init_missing,
                value,
              ) {
                let err = finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  scope,
                  evaluator.script.source.as_ref(),
                  stmt_offset,
                  err,
                );
                match err {
                  VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                    return Ok(HirAsyncResult::CompleteThrow(value))
                  }
                  other => return Err(other),
                }
              }
            }

            // Variable statement completes; continue with the next statement.
            self.next_stmt_index = stmt_index.saturating_add(1);
            continue;
          }
        }
      }

      let HirAsyncBodyKind::Block { stmts } = &self.body_kind else {
        let HirAsyncBodyKind::Expr { expr } = &self.body_kind else {
          return Err(VmError::InvariantViolation("missing compiled async body kind"));
        };
        let expr_node = evaluator.get_expr(body, *expr)?;
        let expr_span_start = expr_node.span.start;

        // Fast-path expression-bodied async arrow functions whose body is an AwaitExpression
        // (`async () => await <expr>`). This form is equivalent to `return await <expr>;`.
        if let hir_js::ExprKind::Await { expr: awaited_expr } = &expr_node.kind {
          // Budget once for evaluating the `await` expression itself; the awaited subexpression is
          // budgeted by `eval_expr`.
          evaluator.vm.tick()?;
          let await_value = match evaluator.eval_expr(scope, body, *awaited_expr) {
            Ok(v) => v,
            Err(err) => {
              let err = finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                scope,
                evaluator.script.source.as_ref(),
                expr_span_start,
                err,
              );
              return match err {
                VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                  Ok(HirAsyncResult::CompleteThrow(value))
                }
                other => Err(other),
              };
            }
          };
          self.active = Some(HirAsyncActive::AwaitReturn);
          self.await_stmt_offset = expr_span_start;
          return Ok(HirAsyncResult::Await {
            kind: crate::exec::AsyncSuspendKind::Await,
            await_value,
          });
        }

        let expr_res = evaluator.eval_expr(scope, body, *expr);
        return match expr_res {
          Ok(v) => Ok(HirAsyncResult::CompleteOk(v)),
          Err(err) => {
            let err = finalize_throw_with_stack_at_source_offset(
              &*evaluator.vm,
              scope,
              evaluator.script.source.as_ref(),
              expr_span_start,
              err,
            );
            match err {
              VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                Ok(HirAsyncResult::CompleteThrow(value))
              }
              other => Err(other),
            }
          }
        };
      };

      if self.next_stmt_index >= stmts.len() {
        return Ok(HirAsyncResult::CompleteOk(Value::Undefined));
      }
      let stmt_id = stmts[self.next_stmt_index];
      let stmt = evaluator.get_stmt(body, stmt_id)?;
      let stmt_offset = stmt.span.start;
      if let hir_js::StmtKind::ForIn {
        left,
        right,
        body: inner,
        is_for_of: true,
        await_: true,
      } = &stmt.kind
      {
        // Budget once for the statement entry (matching `eval_stmt_labelled`).
        evaluator.vm.tick()?;
        self.active = Some(HirAsyncActive::ForAwaitOf(ForAwaitOfState::new(
          left.clone(),
          *right,
          *inner,
          &[],
        )?));
        continue;
      }
      if let hir_js::StmtKind::For {
        init,
        test,
        update,
        body: inner,
      } = &stmt.kind
      {
        let mut has_await = false;
        if let Some(init) = init {
          match init {
            hir_js::ForInit::Expr(expr_id) => {
              let expr = evaluator.get_expr(body, *expr_id)?;
              match &expr.kind {
                hir_js::ExprKind::Await { .. } => has_await = true,
                hir_js::ExprKind::Assignment {
                  op: hir_js::AssignOp::Assign,
                  value,
                  ..
                } => {
                  let rhs = evaluator.get_expr(body, *value)?;
                  if matches!(rhs.kind, hir_js::ExprKind::Await { .. }) {
                    has_await = true;
                  }
                }
                _ => {}
              }
            }
            hir_js::ForInit::Var(decl) => {
              for declarator in &decl.declarators {
                if let Some(init) = declarator.init {
                  let init_expr = evaluator.get_expr(body, init)?;
                  if matches!(init_expr.kind, hir_js::ExprKind::Await { .. }) {
                    has_await = true;
                    break;
                  }
                }
              }
            }
          }
        }
        if !has_await {
          if let Some(test) = test {
            let test_expr = evaluator.get_expr(body, *test)?;
            if matches!(test_expr.kind, hir_js::ExprKind::Await { .. }) {
              has_await = true;
            }
          }
        }
        if !has_await {
          if let Some(update) = update {
            let update_expr = evaluator.get_expr(body, *update)?;
            match &update_expr.kind {
              hir_js::ExprKind::Await { .. } => has_await = true,
              hir_js::ExprKind::Assignment {
                op: hir_js::AssignOp::Assign,
                value,
                ..
              } => {
                let rhs = evaluator.get_expr(body, *value)?;
                if matches!(rhs.kind, hir_js::ExprKind::Await { .. }) {
                  has_await = true;
                }
              }
              _ => {}
            }
          }
        }
        if has_await {
          self.active = Some(HirAsyncActive::ForTriple(ForTripleAwaitState::new(
            init.clone(),
            *test,
            *update,
            *inner,
            &[],
          )?));
          continue;
        }
      }

      // Fast-path direct `await` statement forms without touching the synchronous evaluator (which
      // does not support `ExprKind::Await`).
      //
      // Note: this only supports *direct* awaits at the statement level (not nested `await` in
      // arbitrary expressions). This is sufficient to cover common patterns like:
      // - `await expr;`
      // - `return await expr;`
      // - `throw await expr;`
      // - `const/let/var x = await expr;`

      // `await <expr>;`
      if let hir_js::StmtKind::Expr(expr_id) = &stmt.kind {
        let expr = evaluator.get_expr(body, *expr_id)?;
        if let hir_js::ExprKind::Await { expr: awaited_expr } = &expr.kind {
          // Budget once for the statement and once for the await expression itself.
          evaluator.vm.tick()?;
          evaluator.vm.tick()?;
          let await_value = match evaluator.eval_expr(scope, body, *awaited_expr) {
            Ok(v) => v,
            Err(err) => {
              let err = finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                scope,
                evaluator.script.source.as_ref(),
                stmt_offset,
                err,
              );
              return match err {
                VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                  Ok(HirAsyncResult::CompleteThrow(value))
                }
                other => Err(other),
              };
            }
          };
          self.active = Some(HirAsyncActive::AwaitExprStmt {
            next_stmt_index: self.next_stmt_index.saturating_add(1),
          });
          self.await_stmt_offset = stmt_offset;
          return Ok(HirAsyncResult::Await {
            kind: crate::exec::AsyncSuspendKind::Await,
            await_value,
          });
        }
      }

      // `return await <expr>;`
      if let hir_js::StmtKind::Return(Some(expr_id)) = &stmt.kind {
        let expr = evaluator.get_expr(body, *expr_id)?;
        if let hir_js::ExprKind::Await { expr: awaited_expr } = &expr.kind {
          evaluator.vm.tick()?;
          evaluator.vm.tick()?;
          let await_value = match evaluator.eval_expr(scope, body, *awaited_expr) {
            Ok(v) => v,
            Err(err) => {
              let err = finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                scope,
                evaluator.script.source.as_ref(),
                stmt_offset,
                err,
              );
              return match err {
                VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                  Ok(HirAsyncResult::CompleteThrow(value))
                }
                other => Err(other),
              };
            }
          };
          self.active = Some(HirAsyncActive::AwaitReturn);
          self.await_stmt_offset = stmt_offset;
          return Ok(HirAsyncResult::Await {
            kind: crate::exec::AsyncSuspendKind::Await,
            await_value,
          });
        }
      }

      // `throw await <expr>;`
      if let hir_js::StmtKind::Throw(expr_id) = &stmt.kind {
        let expr = evaluator.get_expr(body, *expr_id)?;
        if let hir_js::ExprKind::Await { expr: awaited_expr } = &expr.kind {
          evaluator.vm.tick()?;
          evaluator.vm.tick()?;
          let await_value = match evaluator.eval_expr(scope, body, *awaited_expr) {
            Ok(v) => v,
            Err(err) => {
              let err = finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                scope,
                evaluator.script.source.as_ref(),
                stmt_offset,
                err,
              );
              return match err {
                VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                  Ok(HirAsyncResult::CompleteThrow(value))
                }
                other => Err(other),
              };
            }
          };
          self.active = Some(HirAsyncActive::AwaitThrow);
          self.await_stmt_offset = stmt_offset;
          return Ok(HirAsyncResult::Await {
            kind: crate::exec::AsyncSuspendKind::Await,
            await_value,
          });
        }
      }

      // `var`/`let`/`const` declarations with direct `await` initializers.
      //
      // We handle this statement kind manually so we can suspend after evaluating the awaited
      // subexpression and resume by initializing the declarator binding.
      if let hir_js::StmtKind::Var(var_decl) = &stmt.kind {
        // Budget once for the statement itself, matching `eval_stmt_labelled`.
        evaluator.vm.tick()?;
        for (j, declarator) in var_decl.declarators.iter().enumerate() {
          // Match `eval_var_decl`'s per-declarator tick.
          evaluator.vm.tick()?;
          let init_missing = declarator.init.is_none();
          if let Some(init) = declarator.init {
            let init_expr = evaluator.get_expr(body, init)?;
            if let hir_js::ExprKind::Await { expr: awaited_expr } = &init_expr.kind {
              // Budget once for the await expression itself.
              evaluator.vm.tick()?;
              let await_value = match evaluator.eval_expr(scope, body, *awaited_expr) {
                Ok(v) => v,
                Err(err) => {
                  let err = finalize_throw_with_stack_at_source_offset(
                    &*evaluator.vm,
                    scope,
                    evaluator.script.source.as_ref(),
                    stmt_offset,
                    err,
                  );
                  return match err {
                    VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                      Ok(HirAsyncResult::CompleteThrow(value))
                    }
                    other => Err(other),
                  };
                }
              };
              self.active = Some(HirAsyncActive::AwaitVarDecl {
                stmt_index: self.next_stmt_index,
                declarator_index: j,
              });
              self.await_stmt_offset = stmt_offset;
              return Ok(HirAsyncResult::Await {
                kind: crate::exec::AsyncSuspendKind::Await,
                await_value,
              });
            }
          }

          let value = match declarator.init {
            Some(init) => match evaluator.eval_expr(scope, body, init) {
              Ok(v) => v,
              Err(err) => {
                let err = finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  scope,
                  evaluator.script.source.as_ref(),
                  stmt_offset,
                  err,
                );
                return match err {
                  VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                    Ok(HirAsyncResult::CompleteThrow(value))
                  }
                  other => Err(other),
                };
              }
            },
            None => Value::Undefined,
          };
          if let Err(err) = evaluator.bind_var_decl_pat(
            scope,
            body,
            declarator.pat,
            var_decl.kind,
            init_missing,
            value,
          ) {
            let err = finalize_throw_with_stack_at_source_offset(
              &*evaluator.vm,
              scope,
              evaluator.script.source.as_ref(),
              stmt_offset,
              err,
            );
            return match err {
              VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
                Ok(HirAsyncResult::CompleteThrow(value))
              }
              other => Err(other),
            };
          }
        }

        // The var statement completes; continue with the next statement.
        self.next_stmt_index = self.next_stmt_index.saturating_add(1);
        continue;
      }

      // Evaluate non-awaiting statements synchronously.
      let stmt_result = if self.in_root_stmt_list {
        evaluator.eval_root_stmt(scope, body, stmt_id)
      } else {
        evaluator.eval_stmt(scope, body, stmt_id)
      };
      match stmt_result {
        Ok(flow) => match flow {
          Flow::Normal(_) => {
            self.next_stmt_index = self.next_stmt_index.saturating_add(1);
          }
          Flow::Return(v) => return Ok(HirAsyncResult::CompleteOk(v)),
          Flow::Break(..) | Flow::Continue(..) => {
            return Err(VmError::InvariantViolation(
              "async compiled function body produced break/continue completion",
            ))
          }
        },
        Err(err) => {
          let err = finalize_throw_with_stack_at_source_offset(
            &*evaluator.vm,
            scope,
            evaluator.script.source.as_ref(),
            stmt_offset,
            err,
          );
          return match err {
            VmError::Throw(value) | VmError::ThrowWithStack { value, .. } => {
              Ok(HirAsyncResult::CompleteThrow(value))
            }
            other => Err(other),
          };
        }
      }
    }
  }
}

fn run_compiled_async_function(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  env: &mut RuntimeEnv,
  strict: bool,
  this: Value,
  new_target: Value,
  home_object: Option<GcObject>,
  script: Arc<CompiledScript>,
  body_id: hir_js::BodyId,
) -> Result<Value, VmError> {
  let cap = crate::promise_ops::new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = cap.promise;

  let mut state = HirAsyncState::new(script, body_id)?;

  let body_result = state.start(vm, scope, host, hooks, env, strict, this, new_target, home_object);
  let body_result = match body_result {
    Ok(v) => v,
    Err(err) => {
      state.teardown(scope.heap_mut());
      env.teardown(scope.heap_mut());
      return Err(err);
    }
  };

  match body_result {
    HirAsyncResult::CompleteOk(v) => {
      let mut call_scope = scope.reborrow();
      if let Err(err) = call_scope.push_roots(&[cap.resolve, v]) {
        env.teardown(call_scope.heap_mut());
        return Err(err);
      }
      let res = vm.call_with_host_and_hooks(host, &mut call_scope, hooks, cap.resolve, Value::Undefined, &[v]);
      env.teardown(call_scope.heap_mut());
      res.map(|_| promise)
    }
    HirAsyncResult::CompleteThrow(reason) => {
      let mut call_scope = scope.reborrow();
      if let Err(err) = call_scope.push_roots(&[cap.reject, reason]) {
        env.teardown(call_scope.heap_mut());
        return Err(err);
      }
      let res = vm.call_with_host_and_hooks(host, &mut call_scope, hooks, cap.reject, Value::Undefined, &[reason]);
      env.teardown(call_scope.heap_mut());
      res.map(|_| promise)
    }
    HirAsyncResult::Await { kind, await_value } => {
      let this_at_suspend = this;
      let new_target_at_suspend = new_target;
      let home_object_at_suspend = home_object
        .map(Value::Object)
        .unwrap_or(Value::Undefined);

      // Root all GC-managed values while we schedule the resumption.
      let mut root_scope = scope.reborrow();
      if let Err(err) = root_scope.push_roots(&[
        promise,
        cap.resolve,
        cap.reject,
        this_at_suspend,
        new_target_at_suspend,
        home_object_at_suspend,
        await_value,
      ]) {
        state.teardown(root_scope.heap_mut());
        env.teardown(root_scope.heap_mut());
        return Err(err);
      }

      let awaited_promise_res: Result<Value, VmError> = match kind {
        crate::exec::AsyncSuspendKind::Await => {
          // Implement `Await`: PromiseResolve(%Promise%, value) while avoiding Promise species side effects
          // for Promise objects (see `promise_resolve_for_await_with_host_and_hooks`).
          crate::promise_ops::promise_resolve_for_await_with_host_and_hooks(vm, &mut root_scope, host, hooks, await_value)
            .map_err(|err| crate::vm::coerce_error_to_throw(&*vm, &mut root_scope, err))
        }
        crate::exec::AsyncSuspendKind::AwaitResolved => Ok(await_value),
        crate::exec::AsyncSuspendKind::Yield => Err(VmError::InvariantViolation(
          "unexpected async generator yield suspension in compiled async function",
        )),
      };

      let awaited_promise = match awaited_promise_res {
        Ok(p) => p,
        Err(err) if err.is_throw_completion() => {
          let err = finalize_throw_with_stack_at_source_offset(
            &*vm,
            &mut root_scope,
            state.script.source.as_ref(),
            state.await_stmt_offset,
            err,
          );
          let reason = match err {
            VmError::Throw(reason) | VmError::ThrowWithStack { value: reason, .. } => reason,
            other => {
              state.teardown(root_scope.heap_mut());
              env.teardown(root_scope.heap_mut());
              return Err(other);
            }
          };
          root_scope.push_root(reason)?;
          let reject_result =
            vm.call_with_host_and_hooks(host, &mut root_scope, hooks, cap.reject, Value::Undefined, &[reason]);
          state.teardown(root_scope.heap_mut());
          env.teardown(root_scope.heap_mut());
          return reject_result.map(|_| promise);
        }
        Err(e) => {
          state.teardown(root_scope.heap_mut());
          env.teardown(root_scope.heap_mut());
          return Err(e);
        }
      };

      if let Err(err) = root_scope.push_root(awaited_promise) {
        state.teardown(root_scope.heap_mut());
        env.teardown(root_scope.heap_mut());
        return Err(err);
      }

      // Reserve async continuation capacity before we create roots and move `state` into the
      // continuation so insertion cannot fail and leak `HirAsyncState`-owned roots.
      if let Err(err) = vm.reserve_async_continuations(1) {
        state.teardown(root_scope.heap_mut());
        env.teardown(root_scope.heap_mut());
        return Err(err);
      }

      // Create persistent roots for the async continuation.
      let values = [
        this_at_suspend,
        new_target_at_suspend,
        home_object_at_suspend,
        promise,
        cap.resolve,
        cap.reject,
        awaited_promise,
      ];
      let mut roots: Vec<RootId> = Vec::new();
      if roots.try_reserve_exact(values.len()).is_err() {
        state.teardown(root_scope.heap_mut());
        env.teardown(root_scope.heap_mut());
        return Err(VmError::OutOfMemory);
      }
      for &value in &values {
        match root_scope.heap_mut().add_root(value) {
          Ok(id) => roots.push(id),
          Err(e) => {
            for id in roots.drain(..) {
              root_scope.heap_mut().remove_root(id);
            }
            state.teardown(root_scope.heap_mut());
            env.teardown(root_scope.heap_mut());
            return Err(e);
          }
        }
      }

      let this_root = roots[0];
      let new_target_root = roots[1];
      let home_object_root = roots[2];
      let promise_root = roots[3];
      let resolve_root = roots[4];
      let reject_root = roots[5];
      let awaited_root = roots[6];

      let mut frames: VecDeque<crate::exec::AsyncFrame> = VecDeque::new();
      if frames.try_reserve(1).is_err() {
        for id in roots.drain(..) {
          root_scope.heap_mut().remove_root(id);
        }
        state.teardown(root_scope.heap_mut());
        env.teardown(root_scope.heap_mut());
        return Err(VmError::OutOfMemory);
      }
      let allow_new_target_in_eval = state.allow_new_target_in_eval;
      frames.push_back(crate::exec::AsyncFrame::HirAsync { state });

      let cont = AsyncContinuation {
        env: env.clone(),
        strict,
        allow_new_target_in_eval,
        exec_ctx: None,
        script_ast: None,
        script_ast_memory: None,
        this_root,
        new_target_root,
        home_object_root,
        promise_root,
        resolve_root,
        reject_root,
        awaited_promise_root: Some(awaited_root),
        frames,
      };

      let id = vm.insert_async_continuation_reserved(VmAsyncContinuation::Ast(cont));

      let schedule_res: Result<(), VmError> = (|| {
        let call_id = vm.async_resume_call_id()?;
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let global_object = env.global_object();
        let job_realm = vm.current_realm();
        let script_or_module_token = match vm.get_active_script_or_module() {
          Some(sm) => Some(vm.intern_script_or_module(sm)?),
          None => None,
        };

        let name = root_scope.alloc_string("")?;
        let slots_fulfill = [Value::Number(id as f64), Value::Bool(false)];
        let on_fulfilled = root_scope.alloc_native_function_with_slots(call_id, None, name, 1, &slots_fulfill)?;
        root_scope.push_root(Value::Object(on_fulfilled))?;

        let name = root_scope.alloc_string("")?;
        let slots_reject = [Value::Number(id as f64), Value::Bool(true)];
        let on_rejected = root_scope.alloc_native_function_with_slots(call_id, None, name, 1, &slots_reject)?;
        root_scope.push_root(Value::Object(on_rejected))?;

        for cb in [on_fulfilled, on_rejected] {
          root_scope
            .heap_mut()
            .object_set_prototype(cb, Some(intr.function_prototype()))?;
          root_scope
            .heap_mut()
            .set_function_realm(cb, global_object)?;
          if let Some(realm) = job_realm {
            root_scope.heap_mut().set_function_job_realm(cb, realm)?;
          }
          if let Some(token) = script_or_module_token {
            root_scope
              .heap_mut()
              .set_function_script_or_module_token(cb, Some(token))?;
          }
        }

        let _ = crate::promise_ops::perform_promise_then_with_result_capability_with_host_and_hooks(
          vm,
          &mut root_scope,
          host,
          hooks,
          awaited_promise,
          Value::Object(on_fulfilled),
          Value::Object(on_rejected),
          None,
        )?;
        Ok(())
      })();

      if let Err(err) = schedule_res {
        if let Some(cont) = vm.take_async_continuation(id) {
          match cont {
            VmAsyncContinuation::Ast(cont) => {
              crate::exec::async_teardown_continuation(&mut root_scope, cont)
            }
            VmAsyncContinuation::Hir(cont) => hir_async_teardown_continuation(&mut root_scope, cont),
          }
        } else {
          for id in roots.drain(..) {
            root_scope.heap_mut().remove_root(id);
          }
          env.teardown(root_scope.heap_mut());
        }
        return Err(err);
      }

      Ok(promise)
    }
  }
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
  let allow_new_target_in_eval = !func_meta.is_arrow;
  if func_meta.generator {
    return Err(VmError::Unimplemented(if func_meta.async_ {
      "async generator functions"
    } else {
      "generator functions"
    }));
  }
  if func_meta.async_ {
    // Instantiate the function's environment and parameter bindings synchronously before
    // executing the async body.
    let inst_res: Result<(), VmError> = {
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
        allow_new_target_in_eval,
        home_object,
        script: func.script.clone(),
      };
      evaluator.instantiate_function_body(scope, body, args)
    };
    if let Err(err) = inst_res {
      // Async compiled functions transfer ownership of `env` to the async continuation when they
      // suspend. If instantiation fails before the continuation is created, ensure we tear the
      // environment down here so callers can unconditionally skip teardown for async bodies.
      env.teardown(scope.heap_mut());
      return Err(err);
    }
    return run_compiled_async_function(
      vm,
      scope,
      host,
      hooks,
      env,
      strict,
      this,
      new_target,
      home_object,
      func.script.clone(),
      func.body,
    );
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
    allow_new_target_in_eval,
    home_object,
    script: func.script.clone(),
  };

  evaluator.instantiate_function_body(scope, body, args)?;

  // Base class instance fields are initialized immediately after `this` is created (before running
  // the user-defined constructor body). Derived constructors initialize instance fields after
  // `super()` returns (handled by the `super(...args)` call path).
  if let Some(class_ctor) = evaluator.class_constructor {
    if !evaluator.derived_constructor {
      let Value::Object(this_obj) = evaluator.this else {
        return Err(VmError::InvariantViolation(
          "base class constructor `this` is not an object",
        ));
      };
      crate::class_fields::initialize_instance_fields_with_host_and_hooks(
        evaluator.vm,
        scope,
        &mut *evaluator.host,
        &mut *evaluator.hooks,
        this_obj,
        class_ctor,
      )?;
    }
  }

  match &func_meta.body {
    hir_js::FunctionBody::Expr(expr_id) => evaluator.eval_expr(scope, body, *expr_id),
    hir_js::FunctionBody::Block(stmts) => match evaluator.eval_root_stmt_list(scope, body, stmts.as_slice())? {
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

  if script.contains_top_level_await {
    return run_compiled_script_async(vm, scope, host, hooks, env, script);
  }

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
    allow_new_target_in_eval: false,
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

  // GlobalDeclarationInstantiation runtime checks for global lexical declarations must run before
  // we create any bindings so invalid programs do not partially pollute the global environment.
  //
  // Only apply these checks when executing in the realm's global lexical environment (not e.g.
  // direct eval, which uses a nested lexical environment with `VarEnv::GlobalObject`).
  let is_global_script_env = matches!(evaluator.env.var_env(), VarEnv::GlobalObject)
    && scope.heap().env_outer(evaluator.env.lexical_env())?.is_none();
  let mut global_var_names_to_insert: Vec<String> = Vec::new();
  if is_global_script_env {
    evaluator.validate_global_lexical_decls(scope, body, body.root_stmts.as_slice())?;

    // Pre-compute var/function names for:
    // - collision checks against existing global lexical bindings, and
    // - best-effort `[[VarNames]]` tracking.
    let mut var_declared_names: HashSet<String> = HashSet::new();
    evaluator.collect_var_declared_names(body, body.root_stmts.as_slice(), &mut var_declared_names)?;

    let mut function_declared_names: HashSet<String> = HashSet::new();
    for stmt_id in body.root_stmts.as_slice() {
      evaluator.vm.tick()?;
      let stmt = evaluator.get_stmt(body, *stmt_id)?;
      let hir_js::StmtKind::Decl(def_id) = stmt.kind else {
        continue;
      };
      let def = evaluator
        .hir()
        .def(def_id)
        .ok_or(VmError::InvariantViolation("hir def id missing from compiled script"))?;
      let Some(body_id) = def.body else {
        continue;
      };
      let decl_body = evaluator.get_body(body_id)?;
      if decl_body.kind != hir_js::BodyKind::Function {
        continue;
      }
      let name = evaluator.resolve_name(def.name)?;
      if name.as_str() == "<anonymous>" {
        continue;
      }
      function_declared_names.insert(name);
    }

    // Step 6-ish: reject var/function names that collide with existing global lexical declarations.
    let global_lex = evaluator.env.lexical_env();
    for name in var_declared_names.iter().chain(function_declared_names.iter()) {
      evaluator.vm.tick()?;
      if scope.heap().env_has_binding(global_lex, name.as_str())? {
        return Err(throw_syntax_error(
          evaluator.vm,
          scope,
          "Identifier has already been declared",
        )?);
      }
    }

    // Best-effort `[[VarNames]]` tracking: only extend the set for `var` bindings that actually
    // create a new non-deletable global property (i.e. the global object did not already have an
    // own property for that name).
    //
    // This matches V8: `Object.defineProperty(globalThis, "x", { configurable: true, ... }); var x;`
    // does not prevent a later `let x;` in a separate script.
    global_var_names_to_insert
      .try_reserve(var_declared_names.len().saturating_add(function_declared_names.len()))
      .map_err(|_| VmError::OutOfMemory)?;
    for name in &var_declared_names {
      evaluator.vm.tick()?;
      let existed = {
        let mut key_scope = scope.reborrow();
        key_scope.push_root(Value::Object(global_object))?;
        let key = PropertyKey::from_string(key_scope.alloc_string(name.as_str())?);
        key_scope
          .heap()
          .object_get_own_property_with_tick(global_object, &key, || evaluator.vm.tick())?
          .is_some()
      };
      if !existed {
        global_var_names_to_insert.push(name.clone());
      }
    }
    for name in &function_declared_names {
      global_var_names_to_insert.push(name.clone());
    }
  }

  // Hoist `var` declarations so lookups before declaration see `undefined` instead of throwing
  // ReferenceError.
  evaluator.instantiate_var_decls(scope, body, body.root_stmts.as_slice())?;

  // Hoist function declarations so they can be called before their declaration statement.
  evaluator.instantiate_function_decls(scope, body, body.root_stmts.as_slice(), /* annex_b */ false)?;

  // Create lexical bindings (`let`/`const`/`using`/`await using`) up-front in the global lexical
  // environment so TDZ + shadowing semantics are correct.
  evaluator.instantiate_lexical_decls(
    scope,
    body,
    body.root_stmts.as_slice(),
    evaluator.env.lexical_env(),
  )?;

  if is_global_script_env && !global_var_names_to_insert.is_empty() {
    evaluator.vm.global_var_names_insert_all(global_var_names_to_insert)?;
  }

  match evaluator.eval_root_stmt_list(scope, body, body.root_stmts.as_slice())? {
    Flow::Normal(v) => Ok(v.unwrap_or(Value::Undefined)),
    Flow::Return(_) => Err(VmError::Unimplemented("return outside of function")),
    Flow::Break(..) => Err(VmError::Unimplemented("break outside of loop")),
    Flow::Continue(..) => Err(VmError::Unimplemented("continue outside of loop")),
  }
}

/// Execute a pre-compiled module body (HIR) in an already-instantiated module environment.
///
/// This is analogous to [`crate::exec::run_module`] for the compiled executor: it evaluates the
/// module's statement list in strict mode with `this = undefined` and an active
/// `ScriptOrModule::Module` execution context so `import.meta` and dynamic `import()` can resolve
/// module-scoped state.
///
/// For modules with `[[HasTLA]] = true`, use [`start_compiled_module_tla_evaluation`] instead so
/// execution can suspend/resume without falling back to the async AST evaluator.
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
  debug_assert!(
    !script.contains_async_generators,
    "run_compiled_module cannot execute async-generator modules; ModuleGraph must fall back to the AST evaluator"
  );
  // Ensure module execution reports an active ScriptOrModule so `import.meta` can consult it.
  let exec_ctx = ExecutionContext {
    realm: realm_id,
    script_or_module: Some(ScriptOrModule::Module(module_id)),
  };
  vm.push_execution_context(exec_ctx)?;

  let result = (|| -> Result<(), VmError> {
    let mut env = RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, module_env, module_env)?;
    env.set_source_info(script.source.clone(), 0, 0);

    let result = (|| -> Result<(), VmError> {
      let (line, col) = script.source.line_col(0);
      let frame = StackFrame {
        function: None,
        source: script.source.name.clone(),
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
        // Per ECMAScript, module top-level `this` is `undefined`.
        this: Value::Undefined,
        this_initialized: true,
        class_constructor: None,
        derived_constructor: false,
        this_root_idx: None,
        new_target: Value::Undefined,
        allow_new_target_in_eval: false,
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

/// Starts async evaluation of a compiled module body (HIR) until completion or the first supported
/// top-level `await` boundary.
///
/// This mirrors [`crate::exec::start_module_tla_evaluation`], but uses the compiled/HIR executor and
/// suspends/resumes using HIR ids (no raw `parse-js` AST pointers in continuation frames).
///
/// Callers must conservatively ensure the module's top-level await shapes are supported by the HIR
/// async evaluator (see [`CompiledScript::top_level_await_requires_ast_fallback`]) before invoking
/// this function.
pub(crate) fn start_compiled_module_tla_evaluation(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  global_object: GcObject,
  realm_id: RealmId,
  module_id: ModuleId,
  module_env: GcEnv,
  script: Arc<CompiledScript>,
) -> Result<ModuleTlaStepResult, VmError> {
  // Ensure module execution reports an active ScriptOrModule so `import.meta` can consult it.
  let exec_ctx = ExecutionContext {
    realm: realm_id,
    script_or_module: Some(ScriptOrModule::Module(module_id)),
  };
  vm.push_execution_context(exec_ctx)?;
  let prev_state = vm.load_realm_state(scope.heap_mut(), realm_id)?;

  let result = (|| -> Result<ModuleTlaStepResult, VmError> {
    let mut env = RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, module_env, module_env)?;
    env.set_source_info(script.source.clone(), 0, 0);

    let source = script.source.clone();
    let (line, col) = source.line_col(0);
    let frame = StackFrame {
      function: None,
      source: source.name.clone(),
      line,
      col,
    };
    let mut vm_frame = vm.enter_frame(frame)?;

    let mut state = HirAsyncState::new(script.clone(), script.hir.root_body())?;
    let mut next = state.start(
      &mut *vm_frame,
      scope,
      host,
      hooks,
      &mut env,
      /* strict */ true,
      /* this */ Value::Undefined,
      /* new_target */ Value::Undefined,
      /* home_object */ None,
    );

    // If `PromiseResolve(%Promise%, awaitValue)` throws, treat it as a rejection at the await site
    // (i.e. resume immediately with a throw completion so `try/catch` around `await` can observe it).
    loop {
      let next_res = match next {
        Ok(r) => r,
        Err(err) => {
          state.teardown(scope.heap_mut());
          env.teardown(scope.heap_mut());
          return Err(err);
        }
      };

      match next_res {
        HirAsyncResult::CompleteOk(_) => {
          state.teardown(scope.heap_mut());
          env.teardown(scope.heap_mut());
          return Ok(ModuleTlaStepResult::Completed);
        }
        HirAsyncResult::CompleteThrow(reason) => {
          state.teardown(scope.heap_mut());
          env.teardown(scope.heap_mut());
          return Err(VmError::ThrowWithStack {
            value: reason,
            stack: vm_frame.capture_stack(),
          });
        }
        HirAsyncResult::Await { kind, await_value } => {
          // Resolve the awaited value to an awaited promise using the same Promise resolution
          // semantics as `Await`.
          let awaited_promise_res = {
            let mut promise_scope = scope.reborrow();
            promise_scope.push_root(await_value)?;
            match kind {
              crate::exec::AsyncSuspendKind::Await => {
                let res = crate::promise_ops::promise_resolve_for_await_with_host_and_hooks(
                  &mut *vm_frame,
                  &mut promise_scope,
                  host,
                  hooks,
                  await_value,
                );
                res.map_err(|err| {
                  crate::exec::coerce_error_to_throw_for_async(&*vm_frame, &mut promise_scope, err)
                })
              }
              crate::exec::AsyncSuspendKind::AwaitResolved => Ok(await_value),
              crate::exec::AsyncSuspendKind::Yield => Err(VmError::InvariantViolation(
                "unexpected async generator yield suspension in compiled module TLA",
              )),
            }
          };

          let awaited_promise = match awaited_promise_res {
            Ok(p) => p,
            Err(VmError::Throw(reason) | VmError::ThrowWithStack { value: reason, .. }) => {
              // Resume immediately with a throw completion.
              next = state.resume(
                &mut *vm_frame,
                scope,
                host,
                hooks,
                &mut env,
                /* strict */ true,
                /* this */ Value::Undefined,
                /* new_target */ Value::Undefined,
                /* home_object */ None,
                Err(VmError::Throw(reason)),
              );
              continue;
            }
            Err(err) => {
              state.teardown(scope.heap_mut());
              env.teardown(scope.heap_mut());
              return Err(err);
            }
          };

          // Root all GC-managed values while we create persistent roots and register the continuation.
          //
          // Module top-level `this` and `new.target` are always `undefined`, and there is no
          // `[[HomeObject]]` by default. We still store dummy roots to satisfy `AsyncContinuation`'s
          // fields and to match `start_module_tla_evaluation`'s layout.
          let mut root_scope = scope.reborrow();
          let values = [
            Value::Undefined, // this
            Value::Undefined, // new.target
            Value::Undefined, // home_object
            Value::Undefined, // promise (unused)
            Value::Undefined, // resolve (unused)
            Value::Undefined, // reject (unused)
            awaited_promise,
          ];
          root_scope.push_roots(&values)?;

          let mut roots: Vec<RootId> = Vec::new();
          roots
            .try_reserve_exact(values.len())
            .map_err(|_| VmError::OutOfMemory)?;
          for &value in &values {
            match root_scope.heap_mut().add_root(value) {
              Ok(id) => roots.push(id),
              Err(e) => {
                for id in roots.drain(..) {
                  root_scope.heap_mut().remove_root(id);
                }
                state.teardown(root_scope.heap_mut());
                env.teardown(root_scope.heap_mut());
                return Err(e);
              }
            }
          }

          // Ensure async continuation insertion cannot fail after consuming the state/env, so we
          // don't leak persistent roots if the continuation storage allocation fails.
          if let Err(err) = vm_frame.reserve_async_continuations(1) {
            for id in roots.drain(..) {
              root_scope.heap_mut().remove_root(id);
            }
            state.teardown(root_scope.heap_mut());
            env.teardown(root_scope.heap_mut());
            return Err(err);
          }

          let mut frames: VecDeque<crate::exec::AsyncFrame> = VecDeque::new();
          if frames.try_reserve(1).is_err() {
            for id in roots.drain(..) {
              root_scope.heap_mut().remove_root(id);
            }
            state.teardown(root_scope.heap_mut());
            env.teardown(root_scope.heap_mut());
            return Err(VmError::OutOfMemory);
          }
          frames.push_back(crate::exec::AsyncFrame::HirAsync { state });

          let cont = AsyncContinuation {
            env: env.clone(),
            strict: true,
            allow_new_target_in_eval: false,
            exec_ctx: Some(exec_ctx),
            script_ast: None,
            script_ast_memory: None,
            this_root: roots[0],
            new_target_root: roots[1],
            home_object_root: roots[2],
            promise_root: roots[3],
            resolve_root: roots[4],
            reject_root: roots[5],
            awaited_promise_root: Some(roots[6]),
            frames,
          };

          let continuation_id =
            vm_frame.insert_async_continuation_reserved(VmAsyncContinuation::Ast(cont));

          // Do not tear down `env`: its env root is now owned by the continuation.
          return Ok(ModuleTlaStepResult::Await {
            promise: awaited_promise,
            continuation_id,
          });
        }
      }
    }
  })();

  let popped = vm.pop_execution_context();
  debug_assert_eq!(popped, Some(exec_ctx));
  debug_assert!(
    popped.is_some(),
    "module execution popped no execution context"
  );
  let restore_res = vm.restore_realm_state(scope.heap_mut(), prev_state);
  match (result, restore_res) {
    (Ok(v), Ok(())) => Ok(v),
    (Err(err), Ok(())) => Err(err),
    (Ok(_), Err(err)) => Err(err),
    (Err(err), Err(_)) => Err(err),
  }
}

pub(crate) fn instantiate_compiled_module_decls(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  global_object: GcObject,
  module_id: ModuleId,
  module_env: GcEnv,
  script: Arc<CompiledScript>,
) -> Result<(), VmError> {
  debug_assert!(
    !script.contains_async_generators,
    "instantiate_compiled_module_decls cannot instantiate async-generator modules; ModuleGraph must fall back to the AST evaluator"
  );

  // Module instantiation creates bindings and (for function declarations) pre-creates function
  // objects. Those function objects must capture the instantiating module as `[[ScriptOrModule]]`
  // so nested operations like dynamic `import()` can correctly determine their referrer.
  //
  // `HirEvaluator::alloc_user_function_object` consults `Vm::get_active_script_or_module` at
  // creation time, so establish a temporary module execution context while instantiating.
  if let Some(realm_id) = vm.current_realm().or_else(|| vm.intrinsics_realm()) {
    let exec_ctx = ExecutionContext {
      realm: realm_id,
      script_or_module: Some(ScriptOrModule::Module(module_id)),
    };
    let mut vm_ctx = vm.execution_context_guard(exec_ctx)?;
    let prev_state = vm_ctx.load_realm_state(scope.heap_mut(), realm_id)?;

    let result = {
      let vm_inner = &mut *vm_ctx;
      instantiate_compiled_module_decls_inner(vm_inner, scope, global_object, module_env, script)
    };

    drop(vm_ctx);
    let restore_res = vm.restore_realm_state(scope.heap_mut(), prev_state);
    return match (result, restore_res) {
      (Ok(()), Ok(())) => Ok(()),
      (Err(err), Ok(())) => Err(err),
      (Ok(_), Err(err)) => Err(err),
      (Err(err), Err(_)) => Err(err),
    };
  }

  // Best-effort fallback: allow module instantiation to proceed even when no realm has been
  // initialized. In this mode, function objects will not capture `[[JobRealm]]`/`[[ScriptOrModule]]`
  // metadata, so host features like dynamic `import()` may observe a missing referrer.
  instantiate_compiled_module_decls_inner(vm, scope, global_object, module_env, script)
}

fn instantiate_compiled_module_decls_inner(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  global_object: GcObject,
  module_env: GcEnv,
  script: Arc<CompiledScript>,
) -> Result<(), VmError> {
  let mut env = RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, module_env, module_env)?;
  env.set_source_info(script.source.clone(), 0, 0);

  // Module instantiation does not execute code, but reuses the evaluator's hoisting/instantiation
  // logic to create bindings and pre-create function objects.
  let mut dummy_host = ();
  let mut dummy_hooks = crate::MicrotaskQueue::new();
  let result = {
    let mut evaluator = HirEvaluator {
      vm,
      host: &mut dummy_host,
      hooks: &mut dummy_hooks,
      env: &mut env,
      // Modules are always strict mode.
      strict: true,
      this: Value::Undefined,
      this_initialized: true,
      class_constructor: None,
      derived_constructor: false,
      this_root_idx: None,
      new_target: Value::Undefined,
      allow_new_target_in_eval: false,
      home_object: None,
      script: script.clone(),
    };

    let hir = script.hir.as_ref();
    let body = hir
      .body(hir.root_body())
      .ok_or(VmError::InvariantViolation("compiled module root body not found"))?;

    // Some early errors are still checked at runtime during instantiation so invalid declarations do
    // not partially pollute the module environment.
    evaluator.early_error_missing_initializers_in_stmt_list(body, body.root_stmts.as_slice())?;

    // Hoist `var` declarations so lookups before declaration see `undefined` instead of throwing
    // ReferenceError.
    evaluator.instantiate_var_decls(scope, body, body.root_stmts.as_slice())?;

    // Create lexical bindings (`let`/`const`/`using`/`await using`/`class`) up-front in the module
    // environment so TDZ + shadowing semantics are correct.
    evaluator.instantiate_lexical_decls(scope, body, body.root_stmts.as_slice(), module_env)?;

    // Pre-create+initialize bindings for hoisted top-level function declarations so
    // `instantiate_function_decls` can assign into the module environment.
    evaluator.instantiate_module_hoisted_function_decl_bindings(
      scope,
      body,
      body.root_stmts.as_slice(),
      module_env,
    )?;

    // Hoist function declarations so they can be called before their declaration statement.
    evaluator.instantiate_function_decls(scope, body, body.root_stmts.as_slice(), /* annex_b */ false)
  };

  env.teardown(scope.heap_mut());
  result
}

#[cfg(test)]
mod async_function_ast_fallback_tests {
  use crate::function::{CallHandler, FunctionData};
  use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

  #[test]
  fn compiled_script_async_function_uses_call_time_ast_fallback() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        async function f() { return 1; }
        f;
      "#,
    )?;
    assert!(script.contains_async_functions);
    assert!(!script.contains_generators);
    assert!(!script.contains_async_generators);
    assert!(
      !script.requires_ast_fallback,
      "compiled scripts should only require full AST fallback for generators or top-level await"
    );
    let result = rt.exec_compiled_script(script)?;
    let Value::Object(func_obj) = result else {
      panic!("expected async function object, got {result:?}");
    };

    let call_handler = rt.heap.get_function_call_handler(func_obj)?;
    assert!(
      matches!(call_handler, CallHandler::User(_)),
      "expected async function to be allocated as a compiled user function, got {call_handler:?}"
    );

    let func_data = rt.heap.get_function_data(func_obj)?;
    assert!(
      matches!(func_data, FunctionData::AsyncEcmaFallback { .. }),
      "expected async function to carry FunctionData::AsyncEcmaFallback metadata, got {func_data:?}"
    );

    // Calling the async function should execute via the AST interpreter and produce a resolved
    // Promise.
    let promise = {
      let mut scope = rt.heap.scope();
      rt.vm
        .call_without_host(&mut scope, Value::Object(func_obj), Value::Undefined, &[])?
    };
    let promise_root = rt.heap.add_root(promise)?;

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected async function call to return a Promise object");
    };
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    assert_eq!(
      rt.heap.promise_result(promise_obj)?,
      Some(Value::Number(1.0))
    );
    rt.heap.remove_root(promise_root);
    Ok(())
  }
}

#[cfg(test)]
mod compiled_hir_async_await_semantics_tests {
  use crate::function::{CallHandler, FunctionData};
  use crate::property::{PropertyKey, PropertyKind};
  use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};
 
  #[derive(Clone, Copy, Debug)]
  enum ExpectedValue {
    Bool(bool),
    Number(f64),
    String(&'static str),
  }
 
  fn get_global_data_property(rt: &mut JsRuntime, name: &str) -> Result<Value, VmError> {
    let global = rt.realm().global_object();
    let mut scope = rt.heap.scope();
    let key_s = scope.alloc_string(name)?;
    let key = PropertyKey::from_string(key_s);
    let desc = scope
      .heap()
      .get_own_property(global, key)?
      .unwrap_or_else(|| panic!("expected global property {name}"));
    let PropertyKind::Data { value, .. } = desc.kind else {
      panic!("expected global property {name} to be a data property");
    };
    Ok(value)
  }
 
  fn run_compiled_async_fn_case(script_src: &str, expected: ExpectedValue) -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;
 
    let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", script_src)?;
    assert!(
      !script.requires_ast_fallback,
      "async/await regression tests must execute in the compiled (HIR) script path"
    );
 
    let result = rt.exec_compiled_script(script)?;
    let Value::Object(returned_promise) = result else {
      panic!("expected script to evaluate to a Promise object, got {result:?}");
    };
    assert!(
      rt.heap.is_promise_object(returned_promise),
      "expected script to evaluate to a Promise object, got {result:?}"
    );
 
    // Ensure the async function was allocated as a compiled user function by the HIR script path.
    //
    // Note: async functions may still execute via the call-time AST fallback
    // (`FunctionData::AsyncEcmaFallback`) until the compiled async evaluator supports all `await`
    // forms.
    let func_value = get_global_data_property(&mut rt, "__f")?;
    let Value::Object(func_obj) = func_value else {
      panic!("expected __f to be a function object, got {func_value:?}");
    };
 
    let call_handler = rt.heap.get_function_call_handler(func_obj)?;
    assert!(
      matches!(call_handler, CallHandler::User(_)),
      "expected __f to be a compiled user function, got {call_handler:?}"
    );

    let func_data = rt.heap.get_function_data(func_obj)?;
    assert!(
      matches!(func_data, FunctionData::None | FunctionData::AsyncEcmaFallback { .. }),
      "expected __f to be a plain user function, got {func_data:?}"
    );

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
 
    let promise_value = get_global_data_property(&mut rt, "__p")?;
    let Value::Object(promise_obj) = promise_value else {
      panic!("expected __p to be a Promise object, got {promise_value:?}");
    };
    assert!(
      rt.heap.is_promise_object(promise_obj),
      "expected __p to be a Promise object, got {promise_value:?}"
    );
    assert_eq!(
      promise_obj, returned_promise,
      "expected script completion value to equal global __p promise"
    );
 
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    let resolved = rt
      .heap
      .promise_result(promise_obj)?
      .expect("fulfilled Promise missing [[PromiseResult]]");
 
    match expected {
      ExpectedValue::Bool(b) => {
        assert!(
          matches!(resolved, Value::Bool(m) if m == b),
          "expected Promise to fulfill with {b}, got {resolved:?}"
        );
      }
      ExpectedValue::Number(n) => {
        assert!(
          matches!(resolved, Value::Number(m) if m == n),
          "expected Promise to fulfill with {n}, got {resolved:?}"
        );
      }
      ExpectedValue::String(s) => {
        let Value::String(str_obj) = resolved else {
          panic!("expected Promise to fulfill with a String, got {resolved:?}");
        };
        assert_eq!(rt.heap.get_string(str_obj)?.to_utf8_lossy(), s);
      }
    }
 
    Ok(())
  }
 
  #[test]
  fn compiled_async_arrow_expr_body_await() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        let f = async () => (await Promise.resolve(1)) + 2;
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(3.0),
    )
  }
 
  #[test]
  fn compiled_async_nested_await() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() { return await (await Promise.resolve(1)); }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(1.0),
    )
  }
 
  #[test]
  fn compiled_async_await_in_computed_member_key_assignment() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() {
          let o = {};
          o[await Promise.resolve('k')] = 1;
          return o.k;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(1.0),
    )
  }
 
  #[test]
  fn compiled_async_await_in_destructuring_default() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() {
          let { x = await Promise.resolve(1) } = {};
          return x;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(1.0),
    )
  }
  
  #[test]
  fn compiled_async_await_in_while_condition() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() {
          let i = 0;
          while (await Promise.resolve(i < 3)) {
            i++;
          }
          return i;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(3.0),
    )
  }
  
  #[test]
  fn compiled_async_await_in_for_triple_init() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let i = await Promise.resolve(0);
          let out='';
          for (i = await Promise.resolve(0); i < 2; i++) { out += i; }
          return out;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::String("01"),
    )
  }

  #[test]
  fn compiled_async_await_in_for_triple_test() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let i=0;
          while(false){}
          for (; await Promise.resolve(i<3); i++) {}
          return i;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(3.0),
    )
  }

  #[test]
  fn compiled_async_await_in_for_triple_update() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let i=0;
          for (; i<3; i = await Promise.resolve(i+1)) {}
          return i;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(3.0),
    )
  }

  #[test]
  fn compiled_async_for_triple_await_break_continue() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let out='';
          for (let i=0; await Promise.resolve(i<3); i = await Promise.resolve(i+1)) {
            if (i===1) continue;
            if (i===2) break;
            out += i;
          }
          return out;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::String("0"),
    )
  }

  #[test]
  fn compiled_async_try_finally_ordering() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() {
          let log = '';
          try {
            await Promise.resolve(0);
            log += 't';
          } finally {
            log += 'f';
          }
          return log;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::String("tf"),
    )
  }

  #[test]
  fn compiled_async_for_await_of_with_await_in_body() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() {
          let s = 0;
          for await (const x of [Promise.resolve(1), Promise.resolve(2)]) {
            s += await Promise.resolve(x);
          }
          return s;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(3.0),
    )
  }

  #[test]
  fn compiled_async_for_of_with_await_in_rhs() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let out='';
          for (const x of await Promise.resolve([1,2])) {
            out += x;
          }
          return out;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::String("12"),
    )
  }

  #[test]
  fn compiled_async_for_in_with_await_in_rhs() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let out='';
          for (const k in await Promise.resolve({a:1,b:2})) {
            out += k;
          }
          // order is not guaranteed; sort
          return out.split('').sort().join('');
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::String("ab"),
    )
  }

  #[test]
  fn compiled_async_for_await_of_with_await_in_rhs() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let out='';
          for await (const x of await Promise.resolve([Promise.resolve(1), Promise.resolve(2)])) {
            out += x;
          }
          return out;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::String("12"),
    )
  }

  #[test]
  fn compiled_async_for_await_of_with_await_in_rhs_and_body() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let out='';
          for await (const x of await Promise.resolve([Promise.resolve(1), Promise.resolve(2)])) {
            out += await Promise.resolve(x);
          }
          return out;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::String("12"),
    )
  }
  
  #[test]
  fn compiled_async_await_in_member_base_simple_assignment() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let obj = {x: 1};
          (await Promise.resolve(obj)).x = 2;
          return obj.x;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(2.0),
    )
  }
  
  #[test]
  fn compiled_async_await_in_member_base_compound_assignment() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let obj = {x: 1};
          (await Promise.resolve(obj)).x += 1;
          return obj.x;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(2.0),
    )
  }
  
  #[test]
  fn compiled_async_await_in_member_base_update_expression() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let obj = {x: 1};
          (await Promise.resolve(obj)).x++;
          return obj.x;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(2.0),
    )
  }
  
  #[test]
  fn compiled_async_comma_operator_multiple_awaits() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          return (await Promise.resolve(1), await Promise.resolve(2));
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(2.0),
    )
  }
  
  #[test]
  fn compiled_async_in_operator_await_on_rhs() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          return ('x' in (await Promise.resolve({x:1}))) === true;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Bool(true),
    )
  }
  
  #[test]
  fn compiled_async_instanceof_operator_await_on_lhs() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          return (await Promise.resolve([])) instanceof Array;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Bool(true),
    )
  }
  
  #[test]
  fn compiled_async_unary_operators_on_awaited_values() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let a = +(await Promise.resolve('1'));
          let b = !(await Promise.resolve(false));
          let c = typeof (await Promise.resolve(undefined));
          return a === 1 && b === true && c === 'undefined';
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Bool(true),
    )
  }

  #[test]
  fn compiled_async_await_in_call_callee_position() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() {
          let fn = await Promise.resolve(() => 3);
          return (await Promise.resolve(fn))();
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(3.0),
    )
  }

  #[test]
  fn compiled_async_await_in_call_args() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() {
          function add(a, b) { return a + b; }
          return add(await Promise.resolve(1), await Promise.resolve(2));
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(3.0),
    )
  }

  #[test]
  fn compiled_async_await_in_new_callee_position() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() {
          let C = await Promise.resolve(class { constructor() { this.v = 7; } });
          return new C().v;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(7.0),
    )
  }

  #[test]
  fn compiled_async_await_in_array_spread() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() {
          let arr = [0, ...await Promise.resolve([1, 2]), 3];
          return arr.join(',');
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::String("0,1,2,3"),
    )
  }

  #[test]
  fn compiled_async_await_in_object_spread_value() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() {
          let o = { a: 0, ...(await Promise.resolve({ b: 1 })), c: 2 };
          return o.a + o.b + o.c;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::Number(3.0),
    )
  }

  #[test]
  fn compiled_async_await_in_template_literal_substitution() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() {
          return `x${await Promise.resolve('y')}z`;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::String("xyz"),
    )
  }

  #[test]
  fn compiled_async_await_in_tagged_template_tag_and_substitution() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f() {
          function tag(strings, v) { return strings[0] + v + strings[1]; }
          return (await Promise.resolve(tag))`a${await Promise.resolve('b')}c`;
        }
        this.__f = f;
        this.__p = f();
        this.__p;
      "#,
      ExpectedValue::String("abc"),
    )
  }

  #[test]
  fn compiled_top_level_await_script_resolves_completion_value() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;
 
    let script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        let x = await Promise.resolve(1);
        x + 1;
      "#,
    )?;
    assert!(script.contains_top_level_await);
    assert!(!script.requires_ast_fallback);
 
    let value = rt.exec_compiled_script(script)?;
    let Value::Object(promise_obj) = value else {
      panic!("expected top-level await script to evaluate to a Promise object, got {value:?}");
    };
    assert!(rt.heap.is_promise_object(promise_obj));
 
    // Root the completion promise across the microtask checkpoint so it remains valid even if the
    // checkpoint triggers GC after the script settles.
    let promise_root = rt.heap.add_root(Value::Object(promise_obj))?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;
 
    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected rooted completion Promise to remain live");
    };
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    assert_eq!(rt.heap.promise_result(promise_obj)?, Some(Value::Number(2.0)));
 
    rt.heap.remove_root(promise_root);
    Ok(())
  }
}

#[cfg(test)]
mod hir_async_await_in_pattern_binding_regression_tests {
  use crate::function::CallHandler;
  use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

  #[derive(Clone, Copy, Debug)]
  enum ExpectedValue {
    Number(f64),
    String(&'static str),
  }

  fn run_compiled_async_fn_case(script_src: &str, expected: ExpectedValue) -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", script_src)?;
    assert!(
      !script.requires_ast_fallback,
      "async/await regression tests must execute in the compiled (HIR) script path"
    );

    let func_value = rt.exec_compiled_script(script)?;
    let Value::Object(func_obj) = func_value else {
      panic!("expected script to evaluate to a function object, got {func_value:?}");
    };

    let call_handler = rt.heap.get_function_call_handler(func_obj)?;
    assert!(
      matches!(call_handler, CallHandler::User(_)),
      "expected async function to be a compiled user function, got {call_handler:?}"
    );

    // Async function bodies may still execute via the AST interpreter at call-time
    // (`FunctionData::EcmaFallback`). Once compiled async/await execution is enabled, this test will
    // automatically exercise the compiled async evaluator instead.
    let _func_data = rt.heap.get_function_data(func_obj)?;

    // Calling the async function should produce a Promise.
    let promise = {
      let mut scope = rt.heap.scope();
      rt.vm
        .call_without_host(&mut scope, Value::Object(func_obj), Value::Undefined, &[])?
    };
    let promise_root = rt.heap.add_root(promise)?;

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected async function call to return a Promise object");
    };
    let state = rt.heap.promise_state(promise_obj)?;
    assert_eq!(
      state,
      PromiseState::Fulfilled,
      "expected Promise to be fulfilled, got {state:?} with result {:?}",
      rt.heap.promise_result(promise_obj)?
    );
    let resolved = rt
      .heap
      .promise_result(promise_obj)?
      .expect("fulfilled Promise missing [[PromiseResult]]");

    match expected {
      ExpectedValue::Number(n) => {
        assert!(
          matches!(resolved, Value::Number(m) if m == n),
          "expected Promise to fulfill with {n}, got {resolved:?}"
        );
      }
      ExpectedValue::String(s) => {
        let Value::String(str_obj) = resolved else {
          panic!("expected Promise to fulfill with a String, got {resolved:?}");
        };
        assert_eq!(rt.heap.get_string(str_obj)?.to_utf8_lossy(), s);
      }
    }

    rt.heap.remove_root(promise_root);
    Ok(())
  }

  #[test]
  fn destructuring_assignment_default_with_await() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let x;
          ({x = await Promise.resolve(1)} = {});
          return x;
        }
        f;
      "#,
      ExpectedValue::Number(1.0),
    )
  }

  #[test]
  fn destructuring_assignment_computed_key_with_await() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let x;
          ({[await Promise.resolve('k')]: x} = {k: 2});
          return x;
        }
        f;
      "#,
      ExpectedValue::Number(2.0),
    )
  }

  #[test]
  fn for_of_head_destructuring_default_with_await() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let out = '';
          for (const {x = await Promise.resolve(1)} of [{}, {}]) {
            out += x;
          }
          return out;
        }
        f;
      "#,
      ExpectedValue::String("11"),
    )
  }

  #[test]
  fn for_await_of_head_destructuring_default_with_await() -> Result<(), VmError> {
    run_compiled_async_fn_case(
      r#"
        async function f(){
          let out='';
          for await (const {x = await Promise.resolve(1)} of [Promise.resolve({}), Promise.resolve({})]) {
            out += x;
          }
          return out;
        }
        f;
      "#,
      ExpectedValue::String("11"),
    )
  }
}
 
#[cfg(test)]
mod async_for_await_of_async_iterator_close_tests {
  use crate::function::{CallHandler, FunctionData};
  use crate::property::{PropertyKey, PropertyKind};
  use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

  fn get_global_data_property(rt: &mut JsRuntime, name: &str) -> Result<Value, VmError> {
    let global = rt.realm().global_object();
    let mut scope = rt.heap.scope();
    let key_s = scope.alloc_string(name)?;
    let key = PropertyKey::from_string(key_s);
    let desc = scope
      .heap()
      .get_own_property(global, key)?
      .unwrap_or_else(|| panic!("expected global property {name}"));
    let PropertyKind::Data { value, .. } = desc.kind else {
      panic!("expected global property {name} to be a data property");
    };
    Ok(value)
  }

  #[test]
  fn compiled_async_for_await_of_break_awaits_async_iterator_close() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        async function f() {
          let log = '';
          let iter = {
            i: 0,
            [Symbol.asyncIterator]() { return this; },
            next() {
              this.i++;
              if (this.i === 1) return Promise.resolve({ value: 1, done: false });
              return Promise.resolve({ value: undefined, done: true });
            },
            return() {
              return Promise.resolve().then(() => { log += 'R'; return { done: true }; });
            }
          };
          for await (const x of iter) {
            break;
          }
          log += 'A';
          return log;
        }
        f();
      "#,
    )?;
    assert!(
      !script.requires_ast_fallback,
      "script should be eligible for compiled execution"
    );

    let promise = rt.exec_compiled_script(script)?;
    let promise_root = rt.heap.add_root(promise)?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected script evaluation to produce a Promise object");
    };
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    let result = rt
      .heap
      .promise_result(promise_obj)?
      .ok_or(VmError::InvariantViolation("missing promise result"))?;
    let Value::String(result_s) = result else {
      panic!("expected promise result to be a string, got {result:?}");
    };
    assert_eq!(rt.heap.get_string(result_s)?.to_utf8_lossy(), "RA");

    rt.heap.remove_root(promise_root);
    Ok(())
  }

  #[test]
  fn compiled_hir_async_fn_for_await_of_break_awaits_async_iterator_close() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        var __resolveClose;
        async function f() {
          let log = '';
          let iter = {
            i: 0,
            [Symbol.asyncIterator]() { return this; },
            next() {
              this.i++;
              if (this.i === 1) return Promise.resolve({ value: 1, done: false });
              return Promise.resolve({ value: undefined, done: true });
            },
            return() {
              return new Promise(resolve => {
                __resolveClose = () => { log += 'R'; resolve({ done: true }); };
              });
            }
          };
          for await (const x of iter) {
            break;
          }
          log += 'A';
          return log;
        }
        f;
      "#,
    )?;
    assert!(
      !script.requires_ast_fallback,
      "script should be eligible for compiled execution"
    );

    let func_value = rt.exec_compiled_script(script)?;
    let Value::Object(func_obj) = func_value else {
      panic!("expected script to evaluate to a function object, got {func_value:?}");
    };

    let call_handler = rt.heap.get_function_call_handler(func_obj)?;
    assert!(
      matches!(call_handler, CallHandler::User(_)),
      "expected async function to be a compiled user function, got {call_handler:?}"
    );

    // Force the async function to execute via the compiled (HIR) async evaluator by clearing the
    // call-time AST fallback marker.
    if matches!(
      rt.heap.get_function_data(func_obj)?,
      FunctionData::AsyncEcmaFallback { .. }
    ) {
      rt.heap.set_function_data(func_obj, FunctionData::None)?;
    }

    // Call the async function and drive it to the AsyncIteratorClose await boundary.
    let promise = {
      let mut scope = rt.heap.scope();
      scope.push_root(Value::Object(func_obj))?;
      rt.vm
        .call_without_host(&mut scope, Value::Object(func_obj), Value::Undefined, &[])?
    };
    let promise_root = rt.heap.add_root(promise)?;

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected async function call to return a Promise object");
    };
    assert_eq!(
      rt.heap.promise_state(promise_obj)?,
      PromiseState::Pending,
      "async function should remain pending until AsyncIteratorClose awaits iterator.return()",
    );

    let resolve_close = get_global_data_property(&mut rt, "__resolveClose")?;
    let Value::Object(resolve_obj) = resolve_close else {
      panic!("expected __resolveClose to be a function object, got {resolve_close:?}");
    };
    {
      let mut scope = rt.heap.scope();
      scope.push_root(Value::Object(resolve_obj))?;
      rt.vm
        .call_without_host(&mut scope, Value::Object(resolve_obj), Value::Undefined, &[])?;
    }

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected Promise root");
    };
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    let result = rt
      .heap
      .promise_result(promise_obj)?
      .ok_or(VmError::InvariantViolation("missing promise result"))?;
    let Value::String(result_s) = result else {
      panic!("expected promise result to be a string, got {result:?}");
    };
    assert_eq!(rt.heap.get_string(result_s)?.to_utf8_lossy(), "RA");

    rt.heap.remove_root(promise_root);
    Ok(())
  }

  #[test]
  fn compiled_top_level_for_await_of_break_awaits_async_iterator_close() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        var log = '';
        var __resolveClose;
        var iter = {
          i: 0,
          [Symbol.asyncIterator]() { return this; },
          next() {
            this.i++;
            if (this.i === 1) return Promise.resolve({ value: 1, done: false });
            return Promise.resolve({ value: undefined, done: true });
          },
          return() {
            return new Promise(resolve => {
              __resolveClose = () => { log += 'R'; resolve({ done: true }); };
            });
          }
        };

        for await (const x of iter) {
          break;
        }
        log += 'A';
        log;
      "#,
    )?;
    assert!(script.contains_top_level_await);
    assert!(!script.requires_ast_fallback);
    assert!(
      !script.top_level_await_requires_ast_fallback,
      "top-level for-await-of script should be eligible for compiled async execution",
    );

    let promise = rt.exec_compiled_script(script)?;
    let promise_root = rt.heap.add_root(promise)?;

    // Drive the async script until it suspends on the iterator `return()` promise.
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected script evaluation to produce a Promise object");
    };
    assert_eq!(
      rt.heap.promise_state(promise_obj)?,
      PromiseState::Pending,
      "script promise should remain pending until AsyncIteratorClose awaits iterator.return()",
    );

    // `AsyncIteratorClose` should have invoked `return()` and stored the resolver on the global.
    let resolve_close = get_global_data_property(&mut rt, "__resolveClose")?;
    let Value::Object(resolve_obj) = resolve_close else {
      panic!("expected __resolveClose to be a function object, got {resolve_close:?}");
    };
    {
      let mut scope = rt.heap.scope();
      scope.push_root(Value::Object(resolve_obj))?;
      rt.vm
        .call_without_host(&mut scope, Value::Object(resolve_obj), Value::Undefined, &[])?;
    }

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected script promise root");
    };
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    let result = rt
      .heap
      .promise_result(promise_obj)?
      .ok_or(VmError::InvariantViolation("missing promise result"))?;
    let Value::String(result_s) = result else {
      panic!("expected promise result to be a string, got {result:?}");
    };
    assert_eq!(rt.heap.get_string(result_s)?.to_utf8_lossy(), "RA");

    rt.heap.remove_root(promise_root);
    Ok(())
  }

  #[test]
  fn compiled_async_for_await_of_throw_awaits_async_iterator_close_before_catch() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        async function f() {
          let log = '';
          let iter = {
            i: 0,
            [Symbol.asyncIterator]() { return this; },
            next() {
              this.i++;
              if (this.i === 1) return Promise.resolve({ value: 1, done: false });
              return Promise.resolve({ value: undefined, done: true });
            },
            return() {
              return Promise.resolve().then(() => { log += 'R'; return { done: true }; });
            }
          };

          try {
            for await (const x of iter) {
              throw 'x';
            }
          } catch (e) {
            log += 'C';
            return log + e;
          }
        }
        f();
      "#,
    )?;
    assert!(
      !script.requires_ast_fallback,
      "script should be eligible for compiled execution"
    );

    let promise = rt.exec_compiled_script(script)?;
    let promise_root = rt.heap.add_root(promise)?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected script evaluation to produce a Promise object");
    };
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    let result = rt
      .heap
      .promise_result(promise_obj)?
      .ok_or(VmError::InvariantViolation("missing promise result"))?;
    let Value::String(result_s) = result else {
      panic!("expected promise result to be a string, got {result:?}");
    };
    assert_eq!(rt.heap.get_string(result_s)?.to_utf8_lossy(), "RCx");

    rt.heap.remove_root(promise_root);
    Ok(())
  }

  #[test]
  fn compiled_hir_async_fn_for_await_of_throw_awaits_async_iterator_close_before_rejecting() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        var __resolveClose;
        async function f() {
          let iter = {
            i: 0,
            [Symbol.asyncIterator]() { return this; },
            next() {
              this.i++;
              if (this.i === 1) return Promise.resolve({ value: 1, done: false });
              return Promise.resolve({ value: undefined, done: true });
            },
            return() {
              return new Promise(resolve => {
                __resolveClose = () => { resolve({ done: true }); };
              });
            }
          };
          for await (const x of iter) {
            throw 'x';
          }
        }
        f;
      "#,
    )?;
    assert!(
      !script.requires_ast_fallback,
      "script should be eligible for compiled execution"
    );

    let func_value = rt.exec_compiled_script(script)?;
    let Value::Object(func_obj) = func_value else {
      panic!("expected script to evaluate to a function object, got {func_value:?}");
    };

    let call_handler = rt.heap.get_function_call_handler(func_obj)?;
    assert!(
      matches!(call_handler, CallHandler::User(_)),
      "expected async function to be a compiled user function, got {call_handler:?}"
    );

    // Force compiled async/await semantics (see above).
    if matches!(
      rt.heap.get_function_data(func_obj)?,
      FunctionData::AsyncEcmaFallback { .. }
    ) {
      rt.heap.set_function_data(func_obj, FunctionData::None)?;
    }

    let promise = {
      let mut scope = rt.heap.scope();
      scope.push_root(Value::Object(func_obj))?;
      rt.vm
        .call_without_host(&mut scope, Value::Object(func_obj), Value::Undefined, &[])?
    };
    let promise_root = rt.heap.add_root(promise)?;

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected async function call to return a Promise object");
    };
    assert_eq!(
      rt.heap.promise_state(promise_obj)?,
      PromiseState::Pending,
      "async function should remain pending until AsyncIteratorClose awaits iterator.return()",
    );

    let resolve_close = get_global_data_property(&mut rt, "__resolveClose")?;
    let Value::Object(resolve_obj) = resolve_close else {
      panic!("expected __resolveClose to be a function object, got {resolve_close:?}");
    };
    {
      let mut scope = rt.heap.scope();
      scope.push_root(Value::Object(resolve_obj))?;
      rt.vm
        .call_without_host(&mut scope, Value::Object(resolve_obj), Value::Undefined, &[])?;
    }

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected Promise root");
    };
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);
    let reason = rt
      .heap
      .promise_result(promise_obj)?
      .ok_or(VmError::InvariantViolation("missing promise rejection reason"))?;
    let Value::String(reason_s) = reason else {
      panic!("expected rejection reason to be a string, got {reason:?}");
    };
    assert_eq!(rt.heap.get_string(reason_s)?.to_utf8_lossy(), "x");

    rt.heap.remove_root(promise_root);
    Ok(())
  }

  #[test]
  fn compiled_hir_async_fn_for_await_of_return_awaits_async_iterator_close() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        var __resolveClose;
        async function f() {
          let iter = {
            i: 0,
            [Symbol.asyncIterator]() { return this; },
            next() {
              this.i++;
              if (this.i === 1) return Promise.resolve({ value: 1, done: false });
              return Promise.resolve({ value: undefined, done: true });
            },
            return() {
              return new Promise(resolve => {
                __resolveClose = () => { resolve({ done: true }); };
              });
            }
          };
          for await (const x of iter) {
            return 'A';
          }
          return 'unreachable';
        }
        f;
      "#,
    )?;
    assert!(
      !script.requires_ast_fallback,
      "script should be eligible for compiled execution"
    );

    let func_value = rt.exec_compiled_script(script)?;
    let Value::Object(func_obj) = func_value else {
      panic!("expected script to evaluate to a function object, got {func_value:?}");
    };

    let call_handler = rt.heap.get_function_call_handler(func_obj)?;
    assert!(
      matches!(call_handler, CallHandler::User(_)),
      "expected async function to be a compiled user function, got {call_handler:?}"
    );

    if matches!(
      rt.heap.get_function_data(func_obj)?,
      FunctionData::AsyncEcmaFallback { .. }
    ) {
      rt.heap.set_function_data(func_obj, FunctionData::None)?;
    }

    let promise = {
      let mut scope = rt.heap.scope();
      scope.push_root(Value::Object(func_obj))?;
      rt.vm
        .call_without_host(&mut scope, Value::Object(func_obj), Value::Undefined, &[])?
    };
    let promise_root = rt.heap.add_root(promise)?;

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected async function call to return a Promise object");
    };
    assert_eq!(
      rt.heap.promise_state(promise_obj)?,
      PromiseState::Pending,
      "async function should remain pending until AsyncIteratorClose awaits iterator.return()",
    );

    let resolve_close = get_global_data_property(&mut rt, "__resolveClose")?;
    let Value::Object(resolve_obj) = resolve_close else {
      panic!("expected __resolveClose to be a function object, got {resolve_close:?}");
    };
    {
      let mut scope = rt.heap.scope();
      scope.push_root(Value::Object(resolve_obj))?;
      rt.vm
        .call_without_host(&mut scope, Value::Object(resolve_obj), Value::Undefined, &[])?;
    }

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected Promise root");
    };
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    let result = rt
      .heap
      .promise_result(promise_obj)?
      .ok_or(VmError::InvariantViolation("missing promise result"))?;
    let Value::String(result_s) = result else {
      panic!("expected promise result to be a string, got {result:?}");
    };
    assert_eq!(rt.heap.get_string(result_s)?.to_utf8_lossy(), "A");

    rt.heap.remove_root(promise_root);
    Ok(())
  }

  #[test]
  fn compiled_top_level_for_await_of_throw_awaits_async_iterator_close_before_rejecting() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        var log = '';
        var __resolveClose;
        var iter = {
          i: 0,
          [Symbol.asyncIterator]() { return this; },
          next() {
            this.i++;
            if (this.i === 1) return Promise.resolve({ value: 1, done: false });
            return Promise.resolve({ value: undefined, done: true });
          },
          return() {
            return new Promise(resolve => {
              __resolveClose = () => { log += 'R'; resolve({ done: true }); };
            });
          }
        };

        for await (const x of iter) {
          throw 'x';
        }
      "#,
    )?;
    assert!(script.contains_top_level_await);
    assert!(!script.requires_ast_fallback);
    assert!(
      !script.top_level_await_requires_ast_fallback,
      "top-level for-await-of script should be eligible for compiled async execution",
    );

    let promise = rt.exec_compiled_script(script)?;
    let promise_root = rt.heap.add_root(promise)?;

    // Drive the async script until it suspends on the iterator `return()` promise.
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected script evaluation to produce a Promise object");
    };
    assert_eq!(
      rt.heap.promise_state(promise_obj)?,
      PromiseState::Pending,
      "script promise should remain pending until AsyncIteratorClose awaits iterator.return()",
    );

    let resolve_close = get_global_data_property(&mut rt, "__resolveClose")?;
    let Value::Object(resolve_obj) = resolve_close else {
      panic!("expected __resolveClose to be a function object, got {resolve_close:?}");
    };
    {
      let mut scope = rt.heap.scope();
      scope.push_root(Value::Object(resolve_obj))?;
      rt.vm
        .call_without_host(&mut scope, Value::Object(resolve_obj), Value::Undefined, &[])?;
    }

    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected script promise root");
    };
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);
    let reason = rt
      .heap
      .promise_result(promise_obj)?
      .ok_or(VmError::InvariantViolation("missing promise rejection reason"))?;
    let Value::String(reason_s) = reason else {
      panic!("expected promise rejection reason to be a string, got {reason:?}");
    };
    assert_eq!(rt.heap.get_string(reason_s)?.to_utf8_lossy(), "x");

    rt.heap.remove_root(promise_root);
    Ok(())
  }
}

#[derive(Debug, Clone, Copy)]
enum HirAsyncResumePoint {
  /// Resume statement-list evaluation after completing a top-level `await` *expression statement*.
  ///
  /// The resumed value becomes the statement-list completion value (ECMA-262 `UpdateEmpty`).
  ExprStmt { next_stmt_index: usize },
  /// Resume a `var`/`let`/`const` statement after awaiting a declarator initializer of the form
  /// `await <expr>`.
  VarDecl {
    stmt_index: usize,
    declarator_index: usize,
  },
  /// Resume an assignment expression statement whose RHS was a direct `await` expression
  /// (`x = await <expr>;` / `x += await <expr>;`).
  Assignment { next_stmt_index: usize },
  /// Resume a top-level `for await..of` statement.
  ///
  /// The continuation stores a `ForAwaitOfState` state machine that drives the loop across
  /// suspensions. When the loop completes, evaluation continues from `next_stmt_index`.
  ///
  /// If `break_label` is `Some`, this `for await..of` was wrapped in a single top-level label
  /// statement (`label: for await (...) { ... }`). In that case, a `break label;` completion from
  /// the loop must be consumed by the label statement and treated as a normal completion.
  ForAwaitOf {
    next_stmt_index: usize,
    break_label: Option<hir_js::NameId>,
  },
}

#[derive(Debug)]
pub(crate) struct HirAsyncContinuation {
  env: RuntimeEnv,
  strict: bool,
  exec_ctx: Option<ExecutionContext>,
  script: Arc<CompiledScript>,
  this_root: RootId,
  new_target_root: RootId,
  promise_root: RootId,
  resolve_root: RootId,
  reject_root: RootId,
  awaited_promise_root: Option<RootId>,
  resume: HirAsyncResumePoint,
  for_await_of_state: Option<ForAwaitOfState>,
  /// Assignment reference captured when suspending on `x = await <expr>;` or `x += await <expr>;`.
  assign_reference: Option<AssignmentReference>,
  /// Assignment operator to apply after resumption (compound assignments only).
  assign_op: Option<hir_js::AssignOp>,
  /// Persistent root for the assignment reference base value (member/super assignments).
  assign_base_root: Option<RootId>,
  /// Persistent root for the assignment reference property key (member/super assignments).
  assign_key_root: Option<RootId>,
  /// Persistent root for the pre-await LHS value for compound assignments.
  assign_left_root: Option<RootId>,
  /// Rooted running completion value for statement-list evaluation (ECMA-262 `UpdateEmpty`).
  last_value_root: RootId,
  last_value_is_set: bool,
}

pub(crate) fn hir_async_teardown_continuation(scope: &mut Scope<'_>, mut cont: HirAsyncContinuation) {
  if let Some(mut state) = cont.for_await_of_state.take() {
    state.teardown(scope.heap_mut());
  }
  cont.env.teardown(scope.heap_mut());
  scope.heap_mut().remove_root(cont.this_root);
  scope.heap_mut().remove_root(cont.new_target_root);
  scope.heap_mut().remove_root(cont.promise_root);
  scope.heap_mut().remove_root(cont.resolve_root);
  scope.heap_mut().remove_root(cont.reject_root);
  scope.heap_mut().remove_root(cont.last_value_root);
  if let Some(root) = cont.awaited_promise_root.take() {
    scope.heap_mut().remove_root(root);
  }
  if let Some(root) = cont.assign_base_root.take() {
    scope.heap_mut().remove_root(root);
  }
  if let Some(root) = cont.assign_key_root.take() {
    scope.heap_mut().remove_root(root);
  }
  if let Some(root) = cont.assign_left_root.take() {
    scope.heap_mut().remove_root(root);
  }
}

#[derive(Debug)]
enum HirAsyncEvalResult {
  Complete,
  Await {
    kind: crate::exec::AsyncSuspendKind,
    await_value: Value,
    resume: HirAsyncResumePoint,
    assign_reference: Option<AssignmentReference>,
    assign_op: Option<hir_js::AssignOp>,
    assign_left_value: Option<Value>,
    for_await_of_state: Option<ForAwaitOfState>,
  },
}

fn hir_eval_stmt_list_until_await(
  evaluator: &mut HirEvaluator<'_>,
  scope: &mut Scope<'_>,
  body: &hir_js::Body,
  stmts: &[hir_js::StmtId],
  start_index: usize,
  last_value_root: RootId,
  last_value_is_set: &mut bool,
) -> Result<HirAsyncEvalResult, VmError> {
  // Avoid borrowing `evaluator` immutably across `eval_expr` calls: we need mutable access to the
  // evaluator while executing.
  let source = evaluator.script.source.clone();

  for (i, stmt_id) in stmts.iter().enumerate().skip(start_index) {
    // Fast-path top-level `await` expression statements without touching the normal compiled
    // evaluator (which does not yet support `ExprKind::Await`).
    let stmt = evaluator.get_stmt(body, *stmt_id)?;
    let stmt_offset = stmt.span.start;
    if let hir_js::StmtKind::Expr(expr_id) = stmt.kind {
      let expr = evaluator.get_expr(body, expr_id)?;
      if let hir_js::ExprKind::Await { expr: awaited_expr } = expr.kind {
        // Budget once for the statement and once for the await expression itself.
        evaluator.vm.tick()?;
        evaluator.vm.tick()?;
        let await_value = match evaluator.eval_expr(scope, body, awaited_expr) {
          Ok(v) => v,
          Err(err) => {
            return Err(finalize_throw_with_stack_at_source_offset(
              &*evaluator.vm,
              scope,
              source.as_ref(),
              stmt_offset,
              err,
            ))
          }
        };
        return Ok(HirAsyncEvalResult::Await {
          kind: crate::exec::AsyncSuspendKind::Await,
          await_value,
          resume: HirAsyncResumePoint::ExprStmt {
            next_stmt_index: i.saturating_add(1),
          },
          assign_reference: None,
          assign_op: None,
          assign_left_value: None,
          for_await_of_state: None,
        });
      }

      // Fast-path assignments where the RHS is a direct await expression (e.g. `x = await foo();`).
      //
      // This covers common top-level await usage without requiring full async expression support in
      // the HIR executor.
      if let hir_js::ExprKind::Assignment {
        op: hir_js::AssignOp::Assign,
        target,
        value,
      } = &expr.kind
      {
        let rhs = evaluator.get_expr(body, *value)?;
        if let hir_js::ExprKind::Await { expr: awaited_expr } = rhs.kind {
          // Budget once for the statement and once for the await expression itself.
          evaluator.vm.tick()?;
          evaluator.vm.tick()?;

          // Evaluate the assignment reference (including computed keys) before evaluating the await
          // argument value, matching `eval_assignment` ordering.
          let reference = match evaluator.eval_assignment_reference(scope, body, *target) {
            Ok(r) => r,
            Err(err) => {
              return Err(finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                scope,
                source.as_ref(),
                stmt_offset,
                err,
              ))
            }
          };

          let mut scope = scope.reborrow();
          if let Err(err) = evaluator.root_assignment_reference(&mut scope, &reference) {
            return Err(finalize_throw_with_stack_at_source_offset(
              &*evaluator.vm,
              &mut scope,
              source.as_ref(),
              stmt_offset,
              err,
            ));
          }
          let await_value = match evaluator.eval_expr(&mut scope, body, awaited_expr) {
            Ok(v) => v,
            Err(err) => {
              return Err(finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                &mut scope,
                source.as_ref(),
                stmt_offset,
                err,
              ))
            }
          };
          return Ok(HirAsyncEvalResult::Await {
            kind: crate::exec::AsyncSuspendKind::Await,
            await_value,
            resume: HirAsyncResumePoint::Assignment {
              next_stmt_index: i.saturating_add(1),
            },
            assign_reference: Some(reference),
            assign_op: None,
            assign_left_value: None,
            for_await_of_state: None,
          });
        }
      }

      // Fast-path compound assignments where the RHS is a direct await expression (e.g.
      // `x += await foo();`).
      if let hir_js::ExprKind::Assignment { op, target, value } = &expr.kind {
        // Restrict to arithmetic/bitwise compound assignment operators; logical assignments require
        // short-circuiting semantics that the compiled executor does not yet support across an
        // `await` boundary.
        if matches!(
          op,
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
            | hir_js::AssignOp::BitXorAssign
        ) {
          let rhs = evaluator.get_expr(body, *value)?;
          if let hir_js::ExprKind::Await { expr: awaited_expr } = rhs.kind {
            // Budget once for the statement and once for the await expression itself.
            evaluator.vm.tick()?;
            evaluator.vm.tick()?;

            let reference = match evaluator.eval_assignment_reference(scope, body, *target) {
              Ok(r) => r,
              Err(err) => {
                return Err(finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  scope,
                  source.as_ref(),
                  stmt_offset,
                  err,
                ))
              }
            };

            let mut scope = scope.reborrow();
            if let Err(err) = evaluator.root_assignment_reference(&mut scope, &reference) {
              return Err(finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                &mut scope,
                source.as_ref(),
                stmt_offset,
                err,
              ));
            }
            let left = match evaluator.get_value_from_assignment_reference(&mut scope, &reference) {
              Ok(v) => v,
              Err(err) => {
                return Err(finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  &mut scope,
                  source.as_ref(),
                  stmt_offset,
                  err,
                ))
              }
            };
            if let Err(err) = scope.push_root(left) {
              return Err(finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                &mut scope,
                source.as_ref(),
                stmt_offset,
                err,
              ));
            }

            let await_value = match evaluator.eval_expr(&mut scope, body, awaited_expr) {
              Ok(v) => v,
              Err(err) => {
                return Err(finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  &mut scope,
                  source.as_ref(),
                  stmt_offset,
                  err,
                ))
              }
            };

            return Ok(HirAsyncEvalResult::Await {
              kind: crate::exec::AsyncSuspendKind::Await,
              await_value,
              resume: HirAsyncResumePoint::Assignment {
                next_stmt_index: i.saturating_add(1),
              },
              assign_reference: Some(reference),
              assign_op: Some(*op),
              assign_left_value: Some(left),
              for_await_of_state: None,
            });
          }
        }
      }
    }

    // Fast-path `var`/`let`/`const` declarations whose initializer expression is a direct `await`
    // expression (e.g. `const x = await foo();`).
    if let hir_js::StmtKind::Var(var_decl) = &stmt.kind {
      // Budget once for the statement itself, matching `eval_stmt`.
      evaluator.vm.tick()?;
      for (j, declarator) in var_decl.declarators.iter().enumerate() {
        // Match `eval_var_decl`'s per-declarator tick.
        evaluator.vm.tick()?;
        let init_missing = declarator.init.is_none();
        if let Some(init) = declarator.init {
          let init_expr = evaluator.get_expr(body, init)?;
          if let hir_js::ExprKind::Await { expr: awaited_expr } = init_expr.kind {
            // Budget once for the await expression itself.
            evaluator.vm.tick()?;
            let await_value = match evaluator.eval_expr(scope, body, awaited_expr) {
              Ok(v) => v,
              Err(err) => {
                return Err(finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  scope,
                  source.as_ref(),
                  stmt_offset,
                  err,
                ))
              }
            };
            return Ok(HirAsyncEvalResult::Await {
              kind: crate::exec::AsyncSuspendKind::Await,
              await_value,
              resume: HirAsyncResumePoint::VarDecl {
                stmt_index: i,
                declarator_index: j,
              },
              assign_reference: None,
              assign_op: None,
              assign_left_value: None,
              for_await_of_state: None,
            });
          }
        }
        let value = match declarator.init {
          Some(init) => match evaluator.eval_expr(scope, body, init) {
            Ok(v) => v,
            Err(err) => {
              return Err(finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                scope,
                source.as_ref(),
                stmt_offset,
                err,
              ))
            }
          },
          None => Value::Undefined,
        };
        if let Err(err) = evaluator.bind_var_decl_pat(
          scope,
          body,
          declarator.pat,
          var_decl.kind,
          init_missing,
          value,
        ) {
          return Err(finalize_throw_with_stack_at_source_offset(
            &*evaluator.vm,
            scope,
            source.as_ref(),
            stmt_offset,
            err,
          ));
        }
      }
      continue;
    }

    // Fast-path a single top-level label statement whose body is a `for await..of` loop.
    //
    // This supports patterns like:
    //   `outer: for await (const x of iterable) { break outer; }`
    //
    // This is intentionally narrow: nested label statements still fall back to the AST executor.
    if let hir_js::StmtKind::Labeled { label, body: labeled_body } = &stmt.kind {
      // Budget once for the label statement itself, matching `eval_stmt_labelled`.
      evaluator.vm.tick()?;
      let inner_stmt = evaluator.get_stmt(body, *labeled_body)?;
      if let hir_js::StmtKind::ForIn {
        left,
        right,
        body: inner,
        is_for_of: true,
        await_: true,
      } = &inner_stmt.kind
      {
        // Budget once for the loop statement itself, matching the `StmtKind::ForIn` branch in
        // `eval_stmt_labelled`.
        evaluator.vm.tick()?;
        let label_set = [*label];
        let mut state = ForAwaitOfState::new(left.clone(), *right, *inner, &label_set)?;
        match state.poll(evaluator, scope, body, None) {
          Ok(ForAwaitOfPoll::Await { kind, await_value }) => {
            return Ok(HirAsyncEvalResult::Await {
              kind,
              await_value,
              resume: HirAsyncResumePoint::ForAwaitOf {
                next_stmt_index: i.saturating_add(1),
                break_label: Some(*label),
              },
              assign_reference: None,
              assign_op: None,
              assign_left_value: None,
              for_await_of_state: Some(state),
            });
          }
          Ok(ForAwaitOfPoll::Complete(flow)) => {
            // Defensive cleanup: `poll(Init)` should return `Await`, but keep this robust.
            state.teardown(scope.heap_mut());
            let flow = match flow {
              Flow::Break(Some(target), value) if target == *label => Flow::Normal(value),
              other => other,
            };
            match flow {
              Flow::Normal(v) => {
                if let Some(v) = v {
                  *last_value_is_set = true;
                  scope.heap_mut().set_root(last_value_root, v);
                }
              }
              Flow::Return(_) => {
                return Err(VmError::InvariantViolation(
                  "script evaluation produced Return flow (early errors should prevent this)",
                ))
              }
              Flow::Break(..) => {
                return Err(VmError::InvariantViolation(
                  "script evaluation produced Break flow (early errors should prevent this)",
                ))
              }
              Flow::Continue(..) => {
                return Err(VmError::InvariantViolation(
                  "script evaluation produced Continue flow (early errors should prevent this)",
                ))
              }
            }
            continue;
          }
          Err(err) => {
            state.teardown(scope.heap_mut());
            return Err(finalize_throw_with_stack_at_source_offset(
              &*evaluator.vm,
              scope,
              source.as_ref(),
              inner_stmt.span.start,
              err,
            ));
          }
        }
      }
    }

    // Top-level `for await..of` evaluation is driven by `ForAwaitOfState` so we can suspend on the
    // implicit iterator `await`s without running the synchronous HIR evaluator (which does not yet
    // support `await_` loop heads).
    if let hir_js::StmtKind::ForIn {
      left,
      right,
      body: inner,
      is_for_of: true,
      await_: true,
    } = &stmt.kind
    {
      // Budget once for the statement itself, matching `eval_stmt`.
      evaluator.vm.tick()?;
      let mut state = ForAwaitOfState::new(left.clone(), *right, *inner, &[])?;
      match state.poll(evaluator, scope, body, None) {
        Ok(ForAwaitOfPoll::Await { kind, await_value }) => {
          return Ok(HirAsyncEvalResult::Await {
            kind,
            await_value,
            resume: HirAsyncResumePoint::ForAwaitOf {
              next_stmt_index: i.saturating_add(1),
              break_label: None,
            },
            assign_reference: None,
            assign_op: None,
            assign_left_value: None,
            for_await_of_state: Some(state),
          });
        }
        Ok(ForAwaitOfPoll::Complete(flow)) => {
          state.teardown(scope.heap_mut());
          match flow {
            Flow::Normal(v) => {
              if let Some(v) = v {
                *last_value_is_set = true;
                scope.heap_mut().set_root(last_value_root, v);
              }
            }
            Flow::Return(_) => {
              return Err(VmError::InvariantViolation(
                "script evaluation produced Return flow (early errors should prevent this)",
              ))
            }
            Flow::Break(..) => {
              return Err(VmError::InvariantViolation(
                "script evaluation produced Break flow (early errors should prevent this)",
              ))
            }
            Flow::Continue(..) => {
              return Err(VmError::InvariantViolation(
                "script evaluation produced Continue flow (early errors should prevent this)",
              ))
            }
          }
        }
        Err(err) => {
          // Ensure persistent roots held by the for-await-of state machine do not leak when
          // evaluation fails before we store the state in a continuation.
          state.teardown(scope.heap_mut());
          return Err(finalize_throw_with_stack_at_source_offset(
            &*evaluator.vm,
            scope,
            source.as_ref(),
            stmt_offset,
            err,
          ));
        }
      }
      continue;
    }

    match evaluator.eval_stmt(scope, body, *stmt_id)? {
      Flow::Normal(v) => {
        if let Some(v) = v {
          *last_value_is_set = true;
          scope.heap_mut().set_root(last_value_root, v);
        }
      }
      Flow::Return(_) => {
        return Err(VmError::InvariantViolation(
          "script evaluation produced Return flow (early errors should prevent this)",
        ))
      }
      Flow::Break(..) => {
        return Err(VmError::InvariantViolation(
          "script evaluation produced Break flow (early errors should prevent this)",
        ))
      }
      Flow::Continue(..) => {
        return Err(VmError::InvariantViolation(
          "script evaluation produced Continue flow (early errors should prevent this)",
        ))
      }
    }
  }
  Ok(HirAsyncEvalResult::Complete)
}

fn run_compiled_script_async(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  runtime_env: &mut RuntimeEnv,
  script: Arc<CompiledScript>,
) -> Result<Value, VmError> {
  // Async classic script execution: evaluate the statement list and return a Promise representing
  // completion.
  let cap = crate::promise_ops::new_promise_capability_with_host_and_hooks(vm, scope, host, hooks)?;
  let promise = cap.promise;

  let global_object = runtime_env.global_object();
  let exec_ctx = vm
    .current_realm()
    .map(|realm| ExecutionContext {
      realm,
      script_or_module: vm.get_active_script_or_module(),
    });

  // Use a distinct `RuntimeEnv` for async script evaluation. Async continuations tear down their
  // env roots on completion; classic scripts must not tear down the runtime's global env.
  let mut env = RuntimeEnv::new_with_lexical_env(scope.heap_mut(), global_object, runtime_env.lexical_env())?;
  env.set_source_info(script.source.clone(), 0, 0);

  // Root the running completion value so it can survive allocations during evaluation segments and
  // across suspensions.
  let last_value_root = scope.heap_mut().add_root(Value::Undefined)?;
  let mut last_value_is_set = false;

  // In classic scripts, top-level `this` is the global object (even in strict mode).
  let global_this = Value::Object(global_object);

  let hir = script.hir.as_ref();
  let body = hir
    .body(hir.root_body())
    .ok_or(VmError::InvariantViolation("compiled script root body not found"))?;
 
  // Execute the instantiation phase and the first evaluation segment while borrowing `vm`/`host`/`hooks`
  // through the evaluator. We need those borrows to end before we can schedule Promise reactions for
  // the first `await`.
  let (strict, eval) = match (|| -> Result<(bool, Result<HirAsyncEvalResult, VmError>), VmError> {
    let mut evaluator = HirEvaluator {
      vm,
      host,
      hooks,
      env: &mut env,
      // Best-effort strict detection.
      strict: false,
      this: global_this,
      this_initialized: true,
      class_constructor: None,
      derived_constructor: false,
      this_root_idx: None,
      new_target: Value::Undefined,
      allow_new_target_in_eval: false,
      home_object: None,
      script: script.clone(),
    };

    evaluator.strict = evaluator.detect_use_strict_directive(body)?;

    // Some early errors are still checked at runtime during instantiation so invalid declarations do
    // not partially pollute the global environment.
    evaluator.early_error_missing_initializers_in_stmt_list(body, body.root_stmts.as_slice())?;

    let is_global_script_env = matches!(evaluator.env.var_env(), VarEnv::GlobalObject)
      && scope.heap().env_outer(evaluator.env.lexical_env())?.is_none();
    let mut global_var_names_to_insert: Vec<String> = Vec::new();
    if is_global_script_env {
      evaluator.validate_global_lexical_decls(scope, body, body.root_stmts.as_slice())?;

      let mut var_declared_names: HashSet<String> = HashSet::new();
      evaluator.collect_var_declared_names(body, body.root_stmts.as_slice(), &mut var_declared_names)?;

      let mut function_declared_names: HashSet<String> = HashSet::new();
      for stmt_id in body.root_stmts.as_slice() {
        evaluator.vm.tick()?;
        let stmt = evaluator.get_stmt(body, *stmt_id)?;
        let hir_js::StmtKind::Decl(def_id) = stmt.kind else {
          continue;
        };
        let def = evaluator
          .hir()
          .def(def_id)
          .ok_or(VmError::InvariantViolation("hir def id missing from compiled script"))?;
        let Some(body_id) = def.body else {
          continue;
        };
        let decl_body = evaluator.get_body(body_id)?;
        if decl_body.kind != hir_js::BodyKind::Function {
          continue;
        }
        let name = evaluator.resolve_name(def.name)?;
        if name.as_str() == "<anonymous>" {
          continue;
        }
        function_declared_names.insert(name);
      }

      let global_lex = evaluator.env.lexical_env();
      for name in var_declared_names.iter().chain(function_declared_names.iter()) {
        evaluator.vm.tick()?;
        if scope.heap().env_has_binding(global_lex, name.as_str())? {
          return Err(throw_syntax_error(
            evaluator.vm,
            scope,
            "Identifier has already been declared",
          )?);
        }
      }

      global_var_names_to_insert
        .try_reserve(var_declared_names.len().saturating_add(function_declared_names.len()))
        .map_err(|_| VmError::OutOfMemory)?;
      for name in &var_declared_names {
        evaluator.vm.tick()?;
        let existed = {
          let mut key_scope = scope.reborrow();
          key_scope.push_root(Value::Object(global_object))?;
          let key = PropertyKey::from_string(key_scope.alloc_string(name.as_str())?);
          key_scope
            .heap()
            .object_get_own_property_with_tick(global_object, &key, || evaluator.vm.tick())?
            .is_some()
        };
        if !existed {
          global_var_names_to_insert.push(name.clone());
        }
      }
      for name in &function_declared_names {
        global_var_names_to_insert.push(name.clone());
      }
    }
    // Hoist `var` declarations so lookups before declaration see `undefined` instead of throwing
    // ReferenceError.
    evaluator.instantiate_var_decls(scope, body, body.root_stmts.as_slice())?;
    // Hoist function declarations so they can be called before their declaration statement.
    evaluator.instantiate_function_decls(scope, body, body.root_stmts.as_slice(), /* annex_b */ false)?;
    // Create lexical bindings (`let`/`const`/`using`/`await using`) up-front in the global lexical
    // environment so TDZ + shadowing semantics are correct.
    evaluator.instantiate_lexical_decls(
      scope,
      body,
      body.root_stmts.as_slice(),
      evaluator.env.lexical_env(),
    )?;

    if is_global_script_env && !global_var_names_to_insert.is_empty() {
      evaluator.vm.global_var_names_insert_all(global_var_names_to_insert)?;
    }

    let eval = hir_eval_stmt_list_until_await(
      &mut evaluator,
      scope,
      body,
      body.root_stmts.as_slice(),
      /* start_index */ 0,
      last_value_root,
      &mut last_value_is_set,
    );
    Ok((evaluator.strict, eval))
  })() {
    Ok(v) => v,
    Err(err) => {
      scope.heap_mut().remove_root(last_value_root);
      env.teardown(scope.heap_mut());
      return Err(err);
    }
  };

  match eval {
    Ok(HirAsyncEvalResult::Complete) => {
      // Complete synchronously: resolve the script-completion promise.
      let v = if last_value_is_set {
        scope
          .heap()
          .get_root(last_value_root)
          .ok_or(VmError::InvariantViolation("async script missing last value root"))?
      } else {
        Value::Undefined
      };

      let mut call_scope = scope.reborrow();
      if let Err(err) = call_scope.push_roots(&[cap.resolve, v]) {
        call_scope.heap_mut().remove_root(last_value_root);
        env.teardown(call_scope.heap_mut());
        return Err(err);
      }
      let res = vm.call_with_host_and_hooks(host, &mut call_scope, hooks, cap.resolve, Value::Undefined, &[v]);
      call_scope.heap_mut().remove_root(last_value_root);
      env.teardown(call_scope.heap_mut());
      res.map(|_| promise)
    }
    Ok(HirAsyncEvalResult::Await {
      kind,
      await_value,
      resume,
      assign_reference,
      assign_op,
      assign_left_value,
      for_await_of_state,
    }) => {
      let mut for_await_of_state = for_await_of_state;
      // Root all captured values while we create persistent roots and schedule the resumption.
      let mut root_scope = scope.reborrow();
      let push_res = match (assign_reference.as_ref(), assign_left_value) {
        (Some(AssignmentReference::Property { base, key }), Some(left)) => {
          let key_value = match key {
            PropertyKey::String(s) => Value::String(*s),
            PropertyKey::Symbol(s) => Value::Symbol(*s),
          };
          root_scope.push_roots(&[
            promise,
            cap.resolve,
            cap.reject,
            global_this,
            await_value,
            *base,
            key_value,
            left,
          ])
        }
        (Some(AssignmentReference::Property { base, key }), None) => {
          let key_value = match key {
            PropertyKey::String(s) => Value::String(*s),
            PropertyKey::Symbol(s) => Value::Symbol(*s),
          };
          root_scope.push_roots(&[
            promise,
            cap.resolve,
            cap.reject,
            global_this,
            await_value,
            *base,
            key_value,
          ])
        }
        (
          Some(AssignmentReference::SuperProperty {
            super_base,
            receiver,
            key,
          }),
          Some(left),
        ) => {
          let key_value = match key {
            PropertyKey::String(s) => Value::String(*s),
            PropertyKey::Symbol(s) => Value::Symbol(*s),
          };
          let base_value = (*super_base).map(Value::Object).unwrap_or(Value::Null);
          root_scope.push_roots(&[
            promise,
            cap.resolve,
            cap.reject,
            global_this,
            await_value,
            base_value,
            *receiver,
            key_value,
            left,
          ])
        }
        (
          Some(AssignmentReference::SuperProperty {
            super_base,
            receiver,
            key,
          }),
          None,
        ) => {
          let key_value = match key {
            PropertyKey::String(s) => Value::String(*s),
            PropertyKey::Symbol(s) => Value::Symbol(*s),
          };
          let base_value = (*super_base).map(Value::Object).unwrap_or(Value::Null);
          root_scope.push_roots(&[
            promise,
            cap.resolve,
            cap.reject,
            global_this,
            await_value,
            base_value,
            *receiver,
            key_value,
          ])
        }
        (_, Some(left)) => root_scope.push_roots(&[promise, cap.resolve, cap.reject, global_this, await_value, left]),
        (_, None) => root_scope.push_roots(&[promise, cap.resolve, cap.reject, global_this, await_value]),
      };
      if let Err(err) = push_res {
        if let Some(mut state) = for_await_of_state.take() {
          state.teardown(root_scope.heap_mut());
        }
        root_scope.heap_mut().remove_root(last_value_root);
        env.teardown(root_scope.heap_mut());
        return Err(err);
      }
      let await_stmt_offset = {
        let stmt_index = match resume {
          HirAsyncResumePoint::ExprStmt { next_stmt_index }
          | HirAsyncResumePoint::Assignment { next_stmt_index }
          | HirAsyncResumePoint::ForAwaitOf { next_stmt_index, .. } => next_stmt_index.saturating_sub(1),
          HirAsyncResumePoint::VarDecl { stmt_index, .. } => stmt_index,
        };
        let stmt_id = *body
          .root_stmts
          .get(stmt_index)
          .ok_or(VmError::InvariantViolation(
            "hir async script await stmt index out of bounds",
          ))?;
        body
          .stmts
          .get(stmt_id.0 as usize)
          .ok_or(VmError::InvariantViolation("hir stmt id out of bounds"))?
          .span
          .start
      };

      let awaited_promise_res: Result<Value, VmError> = match kind {
        crate::exec::AsyncSuspendKind::Await => crate::promise_ops::promise_resolve_for_await_with_host_and_hooks(
          vm,
          &mut root_scope,
          host,
          hooks,
          await_value,
        )
        .map_err(|err| crate::exec::coerce_error_to_throw_for_async(vm, &mut root_scope, err)),
        crate::exec::AsyncSuspendKind::AwaitResolved => {
          // `AwaitResolved` means the internal `PromiseResolve` step has already been performed
          // (e.g. by `AsyncIteratorClose` in `for await..of`). Do not call `PromiseResolve` again or
          // we'd observe `promise.constructor` twice.
          debug_assert!(
            matches!(await_value, Value::Object(obj) if root_scope.heap().is_promise_object(obj)),
            "AwaitResolved suspension must carry a Promise object"
          );
          Ok(await_value)
        }
        crate::exec::AsyncSuspendKind::Yield => Err(VmError::InvariantViolation(
          "unexpected async generator yield suspension in compiled async script",
        )),
      };

      let awaited_promise = match awaited_promise_res {
        Ok(p) => p,
        Err(err) if err.is_throw_completion() => {
          let err = finalize_throw_with_stack_at_source_offset(
            &*vm,
            &mut root_scope,
            script.source.as_ref(),
            await_stmt_offset,
            err,
          );
          let reason = match err {
            VmError::Throw(reason) | VmError::ThrowWithStack { value: reason, .. } => reason,
            other => {
              if let Some(mut state) = for_await_of_state.take() {
                state.teardown(root_scope.heap_mut());
              }
              root_scope.heap_mut().remove_root(last_value_root);
              env.teardown(root_scope.heap_mut());
              return Err(other);
            }
          };
          let mut call_scope = root_scope.reborrow();
          if let Err(err) = call_scope.push_roots(&[cap.reject, reason]) {
            if let Some(mut state) = for_await_of_state.take() {
              state.teardown(call_scope.heap_mut());
            }
            call_scope.heap_mut().remove_root(last_value_root);
            env.teardown(call_scope.heap_mut());
            return Err(err);
          }
          let res =
            vm.call_with_host_and_hooks(host, &mut call_scope, hooks, cap.reject, Value::Undefined, &[reason]);
          if let Some(mut state) = for_await_of_state.take() {
            state.teardown(call_scope.heap_mut());
          }
          call_scope.heap_mut().remove_root(last_value_root);
          env.teardown(call_scope.heap_mut());
          return res.map(|_| promise);
        }
        Err(err) => {
          if let Some(mut state) = for_await_of_state.take() {
            state.teardown(root_scope.heap_mut());
          }
          root_scope.heap_mut().remove_root(last_value_root);
          env.teardown(root_scope.heap_mut());
          return Err(err);
        }
      };

      if let Err(err) = root_scope.push_root(awaited_promise) {
        if let Some(mut state) = for_await_of_state.take() {
          state.teardown(root_scope.heap_mut());
        }
        root_scope.heap_mut().remove_root(last_value_root);
        env.teardown(root_scope.heap_mut());
        return Err(err);
      }

      // Create persistent roots for the async continuation.
      let values = [
        global_this,
        Value::Undefined, // new.target
        promise,
        cap.resolve,
        cap.reject,
        awaited_promise,
      ];
      let mut roots: Vec<RootId> = Vec::new();
      // `roots` is also used to track any additional assignment-reference roots so they can be
      // removed on early errors without relying on infallible `Vec` growth (which could abort on
      // allocator OOM).
      let extra_root_count = match assign_reference.as_ref() {
        Some(AssignmentReference::Property { .. } | AssignmentReference::SuperProperty { .. }) => 2,
        _ => 0,
      } + usize::from(assign_left_value.is_some());
      let required_capacity = match values.len().checked_add(extra_root_count) {
        Some(n) => n,
        None => {
          if let Some(mut state) = for_await_of_state.take() {
            state.teardown(root_scope.heap_mut());
          }
          root_scope.heap_mut().remove_root(last_value_root);
          env.teardown(root_scope.heap_mut());
          return Err(VmError::OutOfMemory);
        }
      };
      if roots.try_reserve_exact(required_capacity).is_err() {
        if let Some(mut state) = for_await_of_state.take() {
          state.teardown(root_scope.heap_mut());
        }
        root_scope.heap_mut().remove_root(last_value_root);
        env.teardown(root_scope.heap_mut());
        return Err(VmError::OutOfMemory);
      }
      for &value in &values {
        match root_scope.heap_mut().add_root(value) {
          Ok(id) => roots.push(id),
          Err(err) => {
            for id in roots.drain(..) {
              root_scope.heap_mut().remove_root(id);
            }
            if let Some(mut state) = for_await_of_state.take() {
              state.teardown(root_scope.heap_mut());
            }
            root_scope.heap_mut().remove_root(last_value_root);
            env.teardown(root_scope.heap_mut());
            return Err(err);
          }
        }
      }

      let this_root = roots[0];
      let new_target_root = roots[1];
      let promise_root = roots[2];
      let resolve_root = roots[3];
      let reject_root = roots[4];
      let awaited_root = roots[5];

      // If we're suspending on an assignment whose reference includes a base/key, keep those values
      // alive across the async boundary by registering persistent roots.
      let mut assign_base_root = None;
      let mut assign_key_root = None;
      let mut assign_left_root = None;
      if let Some(reference) = assign_reference.as_ref() {
        match reference {
          AssignmentReference::Binding(_) => {}
          AssignmentReference::Property { base, key } => {
            let key_value = match key {
              PropertyKey::String(s) => Value::String(*s),
              PropertyKey::Symbol(s) => Value::Symbol(*s),
            };
            let base_root = match root_scope.heap_mut().add_root(*base) {
              Ok(id) => id,
              Err(err) => {
                for id in roots.drain(..) {
                  root_scope.heap_mut().remove_root(id);
                }
                if let Some(mut state) = for_await_of_state.take() {
                  state.teardown(root_scope.heap_mut());
                }
                root_scope.heap_mut().remove_root(last_value_root);
                env.teardown(root_scope.heap_mut());
                return Err(err);
              }
            };
            let key_root = match root_scope.heap_mut().add_root(key_value) {
              Ok(id) => id,
              Err(err) => {
                root_scope.heap_mut().remove_root(base_root);
                for id in roots.drain(..) {
                  root_scope.heap_mut().remove_root(id);
                }
                if let Some(mut state) = for_await_of_state.take() {
                  state.teardown(root_scope.heap_mut());
                }
                root_scope.heap_mut().remove_root(last_value_root);
                env.teardown(root_scope.heap_mut());
                return Err(err);
              }
            };
            assign_base_root = Some(base_root);
            assign_key_root = Some(key_root);
            roots.push(base_root);
            roots.push(key_root);
          }
          AssignmentReference::SuperProperty { super_base, key, .. } => {
            let key_value = match key {
              PropertyKey::String(s) => Value::String(*s),
              PropertyKey::Symbol(s) => Value::Symbol(*s),
            };
            let base_value = (*super_base).map(Value::Object).unwrap_or(Value::Null);
            let base_root = match root_scope.heap_mut().add_root(base_value) {
              Ok(id) => id,
              Err(err) => {
                for id in roots.drain(..) {
                  root_scope.heap_mut().remove_root(id);
                }
                root_scope.heap_mut().remove_root(last_value_root);
                env.teardown(root_scope.heap_mut());
                return Err(err);
              }
            };
            let key_root = match root_scope.heap_mut().add_root(key_value) {
              Ok(id) => id,
              Err(err) => {
                root_scope.heap_mut().remove_root(base_root);
                for id in roots.drain(..) {
                  root_scope.heap_mut().remove_root(id);
                }
                root_scope.heap_mut().remove_root(last_value_root);
                env.teardown(root_scope.heap_mut());
                return Err(err);
              }
            };
            assign_base_root = Some(base_root);
            assign_key_root = Some(key_root);
            roots.push(base_root);
            roots.push(key_root);
          }
        }
      }
      if let Some(left) = assign_left_value {
        let left_root = match root_scope.heap_mut().add_root(left) {
          Ok(id) => id,
          Err(err) => {
            for id in roots.drain(..) {
              root_scope.heap_mut().remove_root(id);
            }
            if let Some(mut state) = for_await_of_state.take() {
              state.teardown(root_scope.heap_mut());
            }
            root_scope.heap_mut().remove_root(last_value_root);
            env.teardown(root_scope.heap_mut());
            return Err(err);
          }
        };
        assign_left_root = Some(left_root);
        roots.push(left_root);
      }

      if let Err(err) = vm.reserve_hir_async_continuations(1) {
        for id in roots.drain(..) {
          root_scope.heap_mut().remove_root(id);
        }
        if let Some(mut state) = for_await_of_state.take() {
          state.teardown(root_scope.heap_mut());
        }
        root_scope.heap_mut().remove_root(last_value_root);
        env.teardown(root_scope.heap_mut());
        return Err(err);
      }

      let cont = HirAsyncContinuation {
        env: env.clone(),
        strict,
        exec_ctx,
        script: script.clone(),
        this_root,
        new_target_root,
        promise_root,
        resolve_root,
        reject_root,
        awaited_promise_root: Some(awaited_root),
        resume,
        for_await_of_state,
        assign_reference,
        assign_op,
        assign_base_root,
        assign_key_root,
        assign_left_root,
        last_value_root,
        last_value_is_set,
      };
      let id = vm.insert_hir_async_continuation_reserved(cont);

      let schedule_res: Result<(), VmError> = (|| {
        let call_id = vm.async_resume_call_id()?;
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let job_realm = vm.current_realm();
        let script_or_module_token = match vm.get_active_script_or_module() {
          Some(sm) => Some(vm.intern_script_or_module(sm)?),
          None => None,
        };

        let name = root_scope.alloc_string("")?;
        let slots_fulfill = [Value::Number(id as f64), Value::Bool(false)];
        let on_fulfilled = root_scope.alloc_native_function_with_slots(call_id, None, name, 1, &slots_fulfill)?;
        root_scope.push_root(Value::Object(on_fulfilled))?;

        let name = root_scope.alloc_string("")?;
        let slots_reject = [Value::Number(id as f64), Value::Bool(true)];
        let on_rejected = root_scope.alloc_native_function_with_slots(call_id, None, name, 1, &slots_reject)?;
        root_scope.push_root(Value::Object(on_rejected))?;

        for cb in [on_fulfilled, on_rejected] {
          root_scope
            .heap_mut()
            .object_set_prototype(cb, Some(intr.function_prototype()))?;
          root_scope
            .heap_mut()
            .set_function_realm(cb, global_object)?;
          if let Some(realm) = job_realm {
            root_scope.heap_mut().set_function_job_realm(cb, realm)?;
          }
          if let Some(token) = script_or_module_token {
            root_scope
              .heap_mut()
              .set_function_script_or_module_token(cb, Some(token))?;
          }
        }

        crate::promise_ops::perform_promise_then_no_capability_with_host_and_hooks(
          vm,
          &mut root_scope,
          host,
          hooks,
          awaited_promise,
          Value::Object(on_fulfilled),
          Value::Object(on_rejected),
        )?;
        Ok(())
      })();

      if let Err(err) = schedule_res {
        if let Some(cont) = vm.take_hir_async_continuation(id) {
          hir_async_teardown_continuation(&mut root_scope, cont);
        }
        return Err(err);
      }

      Ok(promise)
    }
    Err(err) if err.is_throw_completion() => {
      // Synchronous throw before the first await: reject the completion promise.
      let reason = err
        .thrown_value()
        .ok_or(VmError::InvariantViolation("throw completion missing thrown value"))?;
      let mut call_scope = scope.reborrow();
      if let Err(err) = call_scope.push_roots(&[cap.reject, reason]) {
        call_scope.heap_mut().remove_root(last_value_root);
        env.teardown(call_scope.heap_mut());
        return Err(err);
      }
      let res =
        vm.call_with_host_and_hooks(host, &mut call_scope, hooks, cap.reject, Value::Undefined, &[reason]);
      call_scope.heap_mut().remove_root(last_value_root);
      env.teardown(call_scope.heap_mut());
      res.map(|_| promise)
    }
    Err(err) => {
      // Fatal error during evaluation: clean up roots/env to avoid leaks.
      scope.heap_mut().remove_root(last_value_root);
      env.teardown(scope.heap_mut());
      Err(err)
    }
  }
}

pub(crate) fn hir_async_resume_continuation(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  id: u32,
  mut cont: HirAsyncContinuation,
  is_reject: bool,
  arg0: Value,
) -> Result<Value, VmError> {
  // The awaited promise has settled; it no longer needs to be rooted by the continuation.
  if let Some(root) = cont.awaited_promise_root.take() {
    scope.heap_mut().remove_root(root);
  }

  let resolve = match scope.heap().get_root(cont.resolve_root) {
    Some(v) => v,
    None => {
      hir_async_teardown_continuation(scope, cont);
      return Err(VmError::InvariantViolation(
        "hir async continuation missing resolve root",
      ));
    }
  };
  let reject = match scope.heap().get_root(cont.reject_root) {
    Some(v) => v,
    None => {
      hir_async_teardown_continuation(scope, cont);
      return Err(VmError::InvariantViolation(
        "hir async continuation missing reject root",
      ));
    }
  };

  let this = match scope.heap().get_root(cont.this_root) {
    Some(v) => v,
    None => {
      hir_async_teardown_continuation(scope, cont);
      return Err(VmError::InvariantViolation(
        "hir async continuation missing this root",
      ));
    }
  };
  let new_target = match scope.heap().get_root(cont.new_target_root) {
    Some(v) => v,
    None => {
      hir_async_teardown_continuation(scope, cont);
      return Err(VmError::InvariantViolation(
        "hir async continuation missing new.target root",
      ));
    }
  };

  let resume_segment = |vm: &mut Vm,
                        scope: &mut Scope<'_>,
                        host: &mut dyn VmHost,
                        hooks: &mut dyn VmHostHooks,
                        mut cont: HirAsyncContinuation|
   -> Result<Value, VmError> {
      let global_object = cont.env.global_object();
      // Convert promise rejection into a throw completion so loop machinery (notably
      // `AsyncIteratorClose` in `for await..of`) can observe error-precedence semantics.
      let resume_value: Result<Value, VmError> = if is_reject {
        // Best-effort attach an own `Error.stack` to implicit engine errors that surface via awaited
        // promise rejection (e.g. `await <revoked Proxy>` where `PromiseResolve` rejects with a
        // freshly-allocated TypeError that does not yet have a stack trace).
        //
        // Under memory pressure we must still throw/reject with the original reason.
        let await_stmt_offset = match (|| -> Result<u32, VmError> {
          let hir = cont.script.hir.as_ref();
          let body = hir
            .body(hir.root_body())
            .ok_or(VmError::InvariantViolation("compiled script root body not found"))?;
          let stmt_index = match cont.resume {
            HirAsyncResumePoint::ExprStmt { next_stmt_index }
            | HirAsyncResumePoint::Assignment { next_stmt_index }
            | HirAsyncResumePoint::ForAwaitOf { next_stmt_index, .. } => next_stmt_index.saturating_sub(1),
            HirAsyncResumePoint::VarDecl { stmt_index, .. } => stmt_index,
          };
          let stmt_id = *body
            .root_stmts
            .get(stmt_index)
            .ok_or(VmError::InvariantViolation(
              "hir async script await stmt index out of bounds",
            ))?;
          Ok(
            body
              .stmts
              .get(stmt_id.0 as usize)
              .ok_or(VmError::InvariantViolation("hir stmt id out of bounds"))?
              .span
              .start,
          )
        })() {
          Ok(offset) => offset,
          Err(err) => {
            hir_async_teardown_continuation(scope, cont);
            return Err(err);
          }
        };

        // Root the rejection reason across stack attachment and any subsequent allocation/GC.
        let reason = match scope.push_root(arg0) {
          Ok(v) => v,
          Err(err) => {
            hir_async_teardown_continuation(scope, cont);
            return Err(err);
          }
        };

        Err(finalize_throw_with_stack_at_source_offset(
          &*vm,
          scope,
          cont.script.source.as_ref(),
          await_stmt_offset,
          VmError::Throw(reason),
        ))
      } else {
        Ok(arg0)
      };

    let mut evaluator = HirEvaluator {
      vm,
      host,
      hooks,
      env: &mut cont.env,
      strict: cont.strict,
      this,
      this_initialized: true,
      class_constructor: None,
      derived_constructor: false,
      this_root_idx: None,
      new_target,
      allow_new_target_in_eval: false,
      home_object: None,
      script: cont.script.clone(),
    };

    let hir = cont.script.hir.as_ref();
    let body = match hir.body(hir.root_body()) {
      Some(body) => body,
      None => {
        // Fatal internal error: ensure we do not leak persistent roots held by the continuation.
        hir_async_teardown_continuation(scope, cont);
        return Err(VmError::InvariantViolation("compiled script root body not found"));
      }
    };
    // Avoid borrowing `evaluator` immutably across `eval_expr` calls: we need mutable access to the
    // evaluator while executing.
    let source = evaluator.script.source.clone();

    let eval: Result<HirAsyncEvalResult, VmError> = (|resume_value: Result<Value, VmError>| {
      match cont.resume {
        HirAsyncResumePoint::ExprStmt { next_stmt_index } => {
          let resumed = resume_value?;
          // Complete the suspended `await` expression statement: its value becomes the statement-list
          // completion value.
          cont.last_value_is_set = true;
          scope.heap_mut().set_root(cont.last_value_root, resumed);
          hir_eval_stmt_list_until_await(
            &mut evaluator,
            scope,
            body,
            body.root_stmts.as_slice(),
            next_stmt_index,
            cont.last_value_root,
            &mut cont.last_value_is_set,
          )
        }
        HirAsyncResumePoint::VarDecl {
          stmt_index,
          declarator_index,
        } => {
          let resumed = resume_value?;
          // Resume a variable declaration where the initializer was `await <expr>`.
          let stmt_id = *body
            .root_stmts
            .get(stmt_index)
            .ok_or(VmError::InvariantViolation(
              "hir async var decl resume stmt index out of bounds",
            ))?;
          let stmt = evaluator.get_stmt(body, stmt_id)?;
          let stmt_offset = stmt.span.start;
          let hir_js::StmtKind::Var(var_decl) = &stmt.kind else {
            return Err(VmError::InvariantViolation(
              "hir async var decl resume target is not a var declaration",
            ));
          };
          let declarator = var_decl
            .declarators
            .get(declarator_index)
            .ok_or(VmError::InvariantViolation(
              "hir async var decl resume declarator index out of bounds",
            ))?;

          // Initialize the awaited declarator binding with the resumed value.
          if let Err(err) = evaluator.bind_var_decl_pat(
            scope,
            body,
            declarator.pat,
            var_decl.kind,
            /* init_missing */ false,
            resumed,
          ) {
            return Err(finalize_throw_with_stack_at_source_offset(
              &*evaluator.vm,
              scope,
              source.as_ref(),
              stmt_offset,
              err,
            ));
          }

          // Continue evaluating subsequent declarators in the same declaration.
          for (j, declarator) in var_decl
            .declarators
            .iter()
            .enumerate()
            .skip(declarator_index.saturating_add(1))
          {
            evaluator.vm.tick()?;
            let init_missing = declarator.init.is_none();
            if let Some(init) = declarator.init {
              let init_expr = evaluator.get_expr(body, init)?;
              if let hir_js::ExprKind::Await { expr: awaited_expr } = init_expr.kind {
                evaluator.vm.tick()?;
                let await_value = match evaluator.eval_expr(scope, body, awaited_expr) {
                  Ok(v) => v,
                  Err(err) => {
                    return Err(finalize_throw_with_stack_at_source_offset(
                      &*evaluator.vm,
                      scope,
                      source.as_ref(),
                      stmt_offset,
                      err,
                    ))
                  }
                };
                return Ok(HirAsyncEvalResult::Await {
                  kind: crate::exec::AsyncSuspendKind::Await,
                  await_value,
                  resume: HirAsyncResumePoint::VarDecl {
                    stmt_index,
                    declarator_index: j,
                  },
                  assign_reference: None,
                  assign_op: None,
                  assign_left_value: None,
                  for_await_of_state: None,
                });
              }
            }
            let value = match declarator.init {
              Some(init) => match evaluator.eval_expr(scope, body, init) {
                Ok(v) => v,
                Err(err) => {
                  return Err(finalize_throw_with_stack_at_source_offset(
                    &*evaluator.vm,
                    scope,
                    source.as_ref(),
                    stmt_offset,
                    err,
                  ))
                }
              },
              None => Value::Undefined,
            };
            if let Err(err) = evaluator.bind_var_decl_pat(
              scope,
              body,
              declarator.pat,
              var_decl.kind,
              init_missing,
              value,
            ) {
              return Err(finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                scope,
                source.as_ref(),
                stmt_offset,
                err,
              ));
            }
          }

          // The variable statement completes with an empty value; continue with the next root stmt.
          hir_eval_stmt_list_until_await(
            &mut evaluator,
            scope,
            body,
            body.root_stmts.as_slice(),
            stmt_index.saturating_add(1),
            cont.last_value_root,
            &mut cont.last_value_is_set,
          )
        }
        HirAsyncResumePoint::Assignment { next_stmt_index } => {
          let resumed = resume_value?;
          // Complete the suspended assignment expression statement (`x = await <expr>;` /
          // `x += await <expr>;`).
          let stmt_index = next_stmt_index.checked_sub(1).ok_or(VmError::InvariantViolation(
            "hir async assignment resume missing statement index",
          ))?;
          let stmt_id = *body
            .root_stmts
            .get(stmt_index)
            .ok_or(VmError::InvariantViolation(
              "hir async assignment resume stmt index out of bounds",
            ))?;
          let stmt = evaluator.get_stmt(body, stmt_id)?;
          let stmt_offset = stmt.span.start;

          let reference = cont.assign_reference.take().ok_or(VmError::InvariantViolation(
            "hir async assignment resume missing assignment reference",
          ))?;
          let assign_op = cont.assign_op.take();

          let assigned_value = if let Some(op) = assign_op {
            // Compound assignment: apply the operator using the pre-await LHS value, then assign the
            // result back into the same reference.
            let left_root = cont.assign_left_root.ok_or(VmError::InvariantViolation(
              "hir async compound assignment resume missing left value root",
            ))?;
            let left = scope
              .heap()
              .get_root(left_root)
              .ok_or(VmError::InvariantViolation(
                "hir async compound assignment missing left value root",
              ))?;

            let mut assign_scope = scope.reborrow();
            if let Err(err) = assign_scope.push_roots(&[left, resumed]) {
              return Err(finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                &mut assign_scope,
                source.as_ref(),
                stmt_offset,
                err,
              ));
            }
            let out = match evaluator.apply_compound_assignment_op(&mut assign_scope, op, left, resumed) {
              Ok(v) => v,
              Err(err) => {
                return Err(finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  &mut assign_scope,
                  source.as_ref(),
                  stmt_offset,
                  err,
                ))
              }
            };
            if let Err(err) = assign_scope.push_root(out) {
              return Err(finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                &mut assign_scope,
                source.as_ref(),
                stmt_offset,
                err,
              ));
            }
            if let Err(err) = evaluator.put_value_to_assignment_reference(&mut assign_scope, &reference, out) {
              return Err(finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                &mut assign_scope,
                source.as_ref(),
                stmt_offset,
                err,
              ));
            }

            // The left value root is no longer needed after we compute + assign the result.
            assign_scope.heap_mut().remove_root(left_root);
            cont.assign_left_root = None;

            out
          } else {
            // Simple assignment: assign the resumed value directly.
            {
              let mut assign_scope = scope.reborrow();
              // Root the resumed value across anonymous function naming + PutValue operations.
              assign_scope.push_root(resumed)?;
              if let Err(err) =
                evaluator.maybe_set_anonymous_function_name_for_assignment(&mut assign_scope, &reference, resumed)
              {
                return Err(finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  &mut assign_scope,
                  source.as_ref(),
                  stmt_offset,
                  err,
                ));
              }
              if let Err(err) = evaluator.put_value_to_assignment_reference(&mut assign_scope, &reference, resumed) {
                return Err(finalize_throw_with_stack_at_source_offset(
                  &*evaluator.vm,
                  &mut assign_scope,
                  source.as_ref(),
                  stmt_offset,
                  err,
                ));
              }
            }
            resumed
          };

          // The assignment expression evaluates to the assigned value and becomes the statement-list
          // completion value.
          cont.last_value_is_set = true;
          scope.heap_mut().set_root(cont.last_value_root, assigned_value);

          // The captured assignment base/key roots are no longer needed after PutValue completes.
          if let Some(root) = cont.assign_base_root.take() {
            scope.heap_mut().remove_root(root);
          }
          if let Some(root) = cont.assign_key_root.take() {
            scope.heap_mut().remove_root(root);
          }
          if let Some(root) = cont.assign_left_root.take() {
            scope.heap_mut().remove_root(root);
          }

          hir_eval_stmt_list_until_await(
            &mut evaluator,
            scope,
            body,
            body.root_stmts.as_slice(),
            next_stmt_index,
            cont.last_value_root,
            &mut cont.last_value_is_set,
          )
        }
        HirAsyncResumePoint::ForAwaitOf {
          next_stmt_index,
          break_label,
        } => {
          // Attach any throw stack to the `for await..of` statement span.
          let stmt_index = next_stmt_index.checked_sub(1).ok_or(VmError::InvariantViolation(
            "hir async for-await-of resume missing statement index",
          ))?;
          let stmt_id = *body
            .root_stmts
            .get(stmt_index)
            .ok_or(VmError::InvariantViolation(
              "hir async for-await-of resume stmt index out of bounds",
            ))?;
          let stmt = evaluator.get_stmt(body, stmt_id)?;
          let stmt_offset = stmt.span.start;

          let mut state = cont.for_await_of_state.take().ok_or(VmError::InvariantViolation(
            "hir async for-await-of resume missing state machine",
          ))?;
          match state.poll(&mut evaluator, scope, body, Some(resume_value)) {
            Ok(ForAwaitOfPoll::Await { kind, await_value }) => Ok(HirAsyncEvalResult::Await {
              kind,
              await_value,
              resume: HirAsyncResumePoint::ForAwaitOf {
                next_stmt_index,
                break_label,
              },
              assign_reference: None,
              assign_op: None,
              assign_left_value: None,
              for_await_of_state: Some(state),
            }),
            Ok(ForAwaitOfPoll::Complete(flow)) => {
              // `ForAwaitOfState::poll` is expected to have cleaned up its persistent roots before
              // returning `Complete`, but call `teardown` defensively so future changes to the state
              // machine cannot leak roots.
              state.teardown(scope.heap_mut());
              let flow = match (break_label, flow) {
                (Some(label), Flow::Break(Some(target), value)) if target == label => Flow::Normal(value),
                (_, other) => other,
              };
              match flow {
                Flow::Normal(v) => {
                  if let Some(v) = v {
                    cont.last_value_is_set = true;
                    scope.heap_mut().set_root(cont.last_value_root, v);
                  }
                }
                Flow::Return(_) => {
                  return Err(VmError::InvariantViolation(
                    "script evaluation produced Return flow (early errors should prevent this)",
                  ))
                }
                Flow::Break(..) => {
                  return Err(VmError::InvariantViolation(
                    "script evaluation produced Break flow (early errors should prevent this)",
                  ))
                }
                Flow::Continue(..) => {
                  return Err(VmError::InvariantViolation(
                    "script evaluation produced Continue flow (early errors should prevent this)",
                  ))
                }
              }

              hir_eval_stmt_list_until_await(
                &mut evaluator,
                scope,
                body,
                body.root_stmts.as_slice(),
                next_stmt_index,
                cont.last_value_root,
                &mut cont.last_value_is_set,
              )
            }
            Err(err) => {
              state.teardown(scope.heap_mut());
              Err(finalize_throw_with_stack_at_source_offset(
                &*evaluator.vm,
                scope,
                source.as_ref(),
                stmt_offset,
                err,
              ))
            }
          }
        }
      }
    })(resume_value);

    match eval {
      Ok(HirAsyncEvalResult::Complete) => {
        let v = if cont.last_value_is_set {
          scope
            .heap()
            .get_root(cont.last_value_root)
            .ok_or(VmError::InvariantViolation("hir async script missing last value root"))?
        } else {
          Value::Undefined
        };

        let mut call_scope = scope.reborrow();
        if let Err(err) = call_scope.push_roots(&[resolve, v]) {
          hir_async_teardown_continuation(&mut call_scope, cont);
          return Err(err);
        }
        let res = vm.call_with_host_and_hooks(host, &mut call_scope, hooks, resolve, Value::Undefined, &[v]);
        hir_async_teardown_continuation(&mut call_scope, cont);
        res.map(|_| Value::Undefined)
      }
      Ok(HirAsyncEvalResult::Await {
        kind,
        await_value,
        resume,
        assign_reference,
        assign_op,
        assign_left_value,
        for_await_of_state,
      }) => {
        cont.resume = resume;
        cont.assign_reference = assign_reference;
        cont.assign_op = assign_op;
        if let Some(mut state) = cont.for_await_of_state.take() {
          state.teardown(scope.heap_mut());
        }
        cont.for_await_of_state = for_await_of_state;

        // Drop any stale assignment roots before installing new ones.
        if let Some(root) = cont.assign_base_root.take() {
          scope.heap_mut().remove_root(root);
        }
        if let Some(root) = cont.assign_key_root.take() {
          scope.heap_mut().remove_root(root);
        }
        if let Some(root) = cont.assign_left_root.take() {
          scope.heap_mut().remove_root(root);
        }

        let mut await_scope = scope.reborrow();
        let push_res = match (cont.assign_reference.as_ref(), assign_left_value) {
          (Some(AssignmentReference::Property { base, key }), Some(left)) => {
            let key_value = match key {
              PropertyKey::String(s) => Value::String(*s),
              PropertyKey::Symbol(s) => Value::Symbol(*s),
            };
            await_scope.push_roots(&[await_value, *base, key_value, left])
          }
          (Some(AssignmentReference::Property { base, key }), None) => {
            let key_value = match key {
              PropertyKey::String(s) => Value::String(*s),
              PropertyKey::Symbol(s) => Value::Symbol(*s),
            };
            await_scope.push_roots(&[await_value, *base, key_value])
          }
          (
            Some(AssignmentReference::SuperProperty {
              super_base,
              receiver,
              key,
            }),
            Some(left),
          ) => {
            let key_value = match key {
              PropertyKey::String(s) => Value::String(*s),
              PropertyKey::Symbol(s) => Value::Symbol(*s),
            };
            let base_value = (*super_base).map(Value::Object).unwrap_or(Value::Null);
            await_scope.push_roots(&[await_value, base_value, *receiver, key_value, left])
          }
          (
            Some(AssignmentReference::SuperProperty {
              super_base,
              receiver,
              key,
            }),
            None,
          ) => {
            let key_value = match key {
              PropertyKey::String(s) => Value::String(*s),
              PropertyKey::Symbol(s) => Value::Symbol(*s),
            };
            let base_value = (*super_base).map(Value::Object).unwrap_or(Value::Null);
            await_scope.push_roots(&[await_value, base_value, *receiver, key_value])
          }
          (_, Some(left)) => await_scope.push_roots(&[await_value, left]),
          (_, None) => await_scope.push_root(await_value).map(|_| ()),
        };
        if let Err(err) = push_res {
          hir_async_teardown_continuation(&mut await_scope, cont);
          return Err(err);
        }

        // Register persistent roots for any captured assignment base/key so the reference survives
        // across the async boundary.
        if let Some(reference) = cont.assign_reference.as_ref() {
          match reference {
            AssignmentReference::Binding(_) => {}
            AssignmentReference::Property { base, key } => {
              let key_value = match key {
                PropertyKey::String(s) => Value::String(*s),
                PropertyKey::Symbol(s) => Value::Symbol(*s),
              };

              let base_root = match await_scope.heap_mut().add_root(*base) {
                Ok(id) => id,
                Err(err) => {
                  hir_async_teardown_continuation(&mut await_scope, cont);
                  return Err(err);
                }
              };
              let key_root = match await_scope.heap_mut().add_root(key_value) {
                Ok(id) => id,
                Err(err) => {
                  await_scope.heap_mut().remove_root(base_root);
                  hir_async_teardown_continuation(&mut await_scope, cont);
                  return Err(err);
                }
              };
              cont.assign_base_root = Some(base_root);
              cont.assign_key_root = Some(key_root);
            }
            AssignmentReference::SuperProperty { super_base, key, .. } => {
              let key_value = match key {
                PropertyKey::String(s) => Value::String(*s),
                PropertyKey::Symbol(s) => Value::Symbol(*s),
              };
              let base_value = (*super_base).map(Value::Object).unwrap_or(Value::Null);
 
              let base_root = match await_scope.heap_mut().add_root(base_value) {
                Ok(id) => id,
                Err(err) => {
                  hir_async_teardown_continuation(&mut await_scope, cont);
                  return Err(err);
                }
              };
              let key_root = match await_scope.heap_mut().add_root(key_value) {
                Ok(id) => id,
                Err(err) => {
                  await_scope.heap_mut().remove_root(base_root);
                  hir_async_teardown_continuation(&mut await_scope, cont);
                  return Err(err);
                }
              };
              cont.assign_base_root = Some(base_root);
              cont.assign_key_root = Some(key_root);
            }
          }
        }
        if let Some(left) = assign_left_value {
          let left_root = match await_scope.heap_mut().add_root(left) {
            Ok(id) => id,
            Err(err) => {
              hir_async_teardown_continuation(&mut await_scope, cont);
              return Err(err);
            }
          };
          cont.assign_left_root = Some(left_root);
        }

        let await_stmt_offset = match (|| -> Result<u32, VmError> {
          let stmt_index = match cont.resume {
            HirAsyncResumePoint::ExprStmt { next_stmt_index }
            | HirAsyncResumePoint::Assignment { next_stmt_index }
            | HirAsyncResumePoint::ForAwaitOf { next_stmt_index, .. } => next_stmt_index.saturating_sub(1),
            HirAsyncResumePoint::VarDecl { stmt_index, .. } => stmt_index,
          };
          let stmt_id = *body
            .root_stmts
            .get(stmt_index)
            .ok_or(VmError::InvariantViolation(
              "hir async script await stmt index out of bounds",
            ))?;
          Ok(
            body
              .stmts
              .get(stmt_id.0 as usize)
              .ok_or(VmError::InvariantViolation("hir stmt id out of bounds"))?
              .span
              .start,
          )
        })() {
          Ok(offset) => offset,
          Err(err) => {
            hir_async_teardown_continuation(&mut await_scope, cont);
            return Err(err);
          }
        };

        let awaited_promise_res: Result<Value, VmError> = match kind {
          crate::exec::AsyncSuspendKind::Await => crate::promise_ops::promise_resolve_for_await_with_host_and_hooks(
            vm,
            &mut await_scope,
            host,
            hooks,
            await_value,
          )
          .map_err(|err| crate::exec::coerce_error_to_throw_for_async(vm, &mut await_scope, err)),
          crate::exec::AsyncSuspendKind::AwaitResolved => {
            // See comment in `run_compiled_script_async`.
            debug_assert!(
              matches!(await_value, Value::Object(obj) if await_scope.heap().is_promise_object(obj)),
              "AwaitResolved suspension must carry a Promise object"
            );
            Ok(await_value)
          }
          crate::exec::AsyncSuspendKind::Yield => Err(VmError::InvariantViolation(
            "unexpected async generator yield suspension in compiled async script",
          )),
        };

        let awaited_promise = match awaited_promise_res {
          Ok(p) => p,
          Err(err) if err.is_throw_completion() => {
            let err = finalize_throw_with_stack_at_source_offset(
              &*vm,
              &mut await_scope,
              source.as_ref(),
              await_stmt_offset,
              err,
            );
            let reason = match err {
              VmError::Throw(reason) | VmError::ThrowWithStack { value: reason, .. } => reason,
              other => {
                hir_async_teardown_continuation(&mut await_scope, cont);
                return Err(other);
              }
            };
            let mut call_scope = await_scope.reborrow();
            if let Err(err) = call_scope.push_roots(&[reject, reason]) {
              hir_async_teardown_continuation(&mut call_scope, cont);
              return Err(err);
            }
            let res = vm.call_with_host_and_hooks(host, &mut call_scope, hooks, reject, Value::Undefined, &[reason]);
            hir_async_teardown_continuation(&mut call_scope, cont);
            return res.map(|_| Value::Undefined);
          }
          Err(err) => {
            hir_async_teardown_continuation(&mut await_scope, cont);
            return Err(err);
          }
        };

        if let Err(err) = await_scope.push_root(awaited_promise) {
          hir_async_teardown_continuation(&mut await_scope, cont);
          return Err(err);
        }

        let awaited_root = match await_scope.heap_mut().add_root(awaited_promise) {
          Ok(root) => root,
          Err(err) => {
            hir_async_teardown_continuation(&mut await_scope, cont);
            return Err(err);
          }
        };
        cont.awaited_promise_root = Some(awaited_root);

        // Reinsert continuation before scheduling any resumption callbacks.
        if let Err(err) = vm.reserve_hir_async_continuations(1) {
          hir_async_teardown_continuation(&mut await_scope, cont);
          return Err(err);
        }
        vm.replace_hir_async_continuation(id, cont)?;

        let then_res: Result<(), VmError> = (|| {
          let call_id = vm.async_resume_call_id()?;
          let intr = vm
            .intrinsics()
            .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
          let job_realm = vm.current_realm();
          let script_or_module_token = match vm.get_active_script_or_module() {
            Some(sm) => Some(vm.intern_script_or_module(sm)?),
            None => None,
          };

          let name = await_scope.alloc_string("")?;
          let slots_fulfill = [Value::Number(id as f64), Value::Bool(false)];
          let on_fulfilled = await_scope.alloc_native_function_with_slots(call_id, None, name, 1, &slots_fulfill)?;
          await_scope.push_root(Value::Object(on_fulfilled))?;

          let name = await_scope.alloc_string("")?;
          let slots_reject = [Value::Number(id as f64), Value::Bool(true)];
          let on_rejected = await_scope.alloc_native_function_with_slots(call_id, None, name, 1, &slots_reject)?;
          await_scope.push_root(Value::Object(on_rejected))?;

          for cb in [on_fulfilled, on_rejected] {
            await_scope
              .heap_mut()
              .object_set_prototype(cb, Some(intr.function_prototype()))?;
            await_scope
              .heap_mut()
              .set_function_realm(cb, global_object)?;
            if let Some(realm) = job_realm {
              await_scope.heap_mut().set_function_job_realm(cb, realm)?;
            }
            if let Some(token) = script_or_module_token {
              await_scope
                .heap_mut()
                .set_function_script_or_module_token(cb, Some(token))?;
            }
          }

          crate::promise_ops::perform_promise_then_no_capability_with_host_and_hooks(
            vm,
            &mut await_scope,
            host,
            hooks,
            awaited_promise,
            Value::Object(on_fulfilled),
            Value::Object(on_rejected),
          )?;
          Ok(())
        })();

        if let Err(err) = then_res {
          if let Some(cont) = vm.take_hir_async_continuation(id) {
            hir_async_teardown_continuation(&mut await_scope, cont);
          }
          return Err(err);
        }

        Ok(Value::Undefined)
      }
      Err(err) if err.is_throw_completion() => {
        let reason = err
          .thrown_value()
          .ok_or(VmError::InvariantViolation("throw completion missing thrown value"))?;
        let mut call_scope = scope.reborrow();
        if let Err(err) = call_scope.push_roots(&[reject, reason]) {
          hir_async_teardown_continuation(&mut call_scope, cont);
          return Err(err);
        }
        let res = vm.call_with_host_and_hooks(host, &mut call_scope, hooks, reject, Value::Undefined, &[reason]);
        hir_async_teardown_continuation(&mut call_scope, cont);
        res.map(|_| Value::Undefined)
      }
      Err(err) => {
        hir_async_teardown_continuation(scope, cont);
        Err(err)
      }
    }
  };

  if let Some(exec_ctx) = cont.exec_ctx {
    let mut vm_ctx = match vm.execution_context_guard(exec_ctx) {
      Ok(g) => g,
      Err(err) => {
        hir_async_teardown_continuation(scope, cont);
        return Err(err);
      }
    };
    return resume_segment(&mut *vm_ctx, scope, host, hooks, cont);
  }

  resume_segment(vm, scope, host, hooks, cont)
}

#[cfg(test)]
mod derived_constructor_eval_super_call_compiled_tests {
  use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

  #[test]
  fn derived_constructor_direct_eval_super_call_compiled() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        (() => {
          let ok = true;
          new class extends class {} {
            constructor() {
              ok = ok && (eval("super(); this") === this);
              ok = ok && (this === eval("this"));
              ok = ok && (this === (() => this)());
            }
          }();

          new class extends class {} {
            constructor() {
              (() => super())();
              ok = ok && (this === eval("this"));
              ok = ok && (this === (() => this)());
            }
          }();

          return ok;
        })()
      "#,
    )?;

    let value = rt.exec_compiled_script(script)?;
    assert!(matches!(value, Value::Bool(true)));
    Ok(())
  }

  #[test]
  fn derived_constructor_direct_eval_nested_super_call_compiled() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        (() => {
          let ok = true;

          new class extends class {} {
            constructor() {
              (() => eval("super()"))();
              ok = ok && (this === eval("this"));
              ok = ok && (this === (() => this)());
            }
          }();

          new class extends class {} {
            constructor() {
              (() => (() => super())())();
              ok = ok && (this === eval("this"));
              ok = ok && (this === (() => this)());
            }
          }();

          new class extends class {} {
            constructor() {
              eval("(() => super())()");
              ok = ok && (this === eval("this"));
              ok = ok && (this === (() => this)());
            }
          }();

          new class extends class {} {
            constructor() {
              eval("eval('super()')");
              ok = ok && (this === eval("this"));
              ok = ok && (this === (() => this)());
            }
          }();

          return ok;
        })()
      "#,
    )?;

    let value = rt.exec_compiled_script(script)?;
    assert!(matches!(value, Value::Bool(true)));
    Ok(())
  }

  #[test]
  fn direct_eval_super_call_outside_derived_constructor_is_syntax_error_compiled() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(
      &mut rt.heap,
      "<inline>",
      r#"
        (() => {
          try {
            eval("super()");
            return false;
          } catch (e) {
            return typeof e === "object" && e !== null && e.constructor === SyntaxError;
          }
        })()
      "#,
    )?;

    let value = rt.exec_compiled_script(script)?;
    assert!(matches!(value, Value::Bool(true)));
    Ok(())
  }
}

#[cfg(test)]
mod hir_async_await_try_catch_finally_compiled_tests {
  use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

  fn new_runtime() -> Result<JsRuntime, VmError> {
    let vm = Vm::new(VmOptions::default());
    // Keep this relatively small to exercise GC paths while leaving headroom for Promise/async
    // machinery (matching other async-focused unit tests).
    let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
    JsRuntime::new(vm, heap)
  }

  fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
    let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", source)?;
    assert!(
      !script.requires_ast_fallback,
      "expected test script to execute via compiled (HIR) path",
    );
    rt.exec_compiled_script(script)
  }

  fn assert_promise_fulfills(rt: &mut JsRuntime, promise: Value, expected: ExpectedValue) -> Result<(), VmError> {
    let promise_root = rt.heap.add_root(promise)?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected async call to return a Promise object");
    };
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    let result = rt
      .heap
      .promise_result(promise_obj)?
      .expect("expected fulfilled Promise to have [[PromiseResult]]");

    match expected {
      ExpectedValue::Number(n) => assert_eq!(result, Value::Number(n)),
      ExpectedValue::String(s) => {
        let Value::String(actual) = result else {
          panic!("expected promise to fulfill with a string, got {result:?}");
        };
        let actual = rt.heap.get_string(actual)?.to_utf8_lossy();
        assert_eq!(actual, s);
      }
    }

    rt.heap.remove_root(promise_root);
    Ok(())
  }

  enum ExpectedValue {
    Number(f64),
    String(&'static str),
  }

  #[test]
  fn await_in_finally_after_return_compiled() -> Result<(), VmError> {
    let mut rt = new_runtime()?;

    let promise = exec_compiled(
      &mut rt,
      r#"
        async function f(){
          let log = '';
          try {
            return 1;
          } finally {
            await Promise.resolve(0);
            log = 'finally';
            globalThis.finallyLog = log;
          }
        }
        f()
      "#,
    )?;
    assert_promise_fulfills(&mut rt, promise, ExpectedValue::Number(1.0))?;

    let finally_ok = exec_compiled(&mut rt, "globalThis.finallyLog === 'finally'")?;
    assert_eq!(finally_ok, Value::Bool(true));
    Ok(())
  }

  #[test]
  fn await_in_finally_after_throw_compiled() -> Result<(), VmError> {
    let mut rt = new_runtime()?;

    let promise = exec_compiled(
      &mut rt,
      r#"
        async function f(){
          try {
            try { throw 'x'; }
            finally { await Promise.resolve(0); }
          } catch(e) { return e; }
        }
        f()
      "#,
    )?;
    assert_promise_fulfills(&mut rt, promise, ExpectedValue::String("x"))?;
    Ok(())
  }

  #[test]
  fn await_in_catch_param_destructuring_default_compiled() -> Result<(), VmError> {
    let mut rt = new_runtime()?;

    let promise = exec_compiled(
      &mut rt,
      r#"
        async function f(){
          try { throw {}; }
          catch ({x = await Promise.resolve(1)}) { return x; }
        }
        f()
      "#,
    )?;
    assert_promise_fulfills(&mut rt, promise, ExpectedValue::Number(1.0))?;
    Ok(())
  }

  #[test]
  fn await_in_catch_param_computed_key_compiled() -> Result<(), VmError> {
    let mut rt = new_runtime()?;

    let promise = exec_compiled(
      &mut rt,
      r#"
        async function f(){
          try { throw {k: 2}; }
          catch ({[await Promise.resolve('k')]: x}) { return x; }
        }
        f()
      "#,
    )?;
    assert_promise_fulfills(&mut rt, promise, ExpectedValue::Number(2.0))?;
    Ok(())
  }
}

#[cfg(test)]
mod hir_async_await_eval_order_compiled_tests {
  use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

  fn assert_compiled_promise_fulfills_true(source: &str) -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", source)?;
    assert!(
      !script.requires_ast_fallback,
      "await evaluation order regression tests must execute via compiled HIR (not full AST fallback)"
    );
    let promise = rt.exec_compiled_script(script)?;
    let promise_root = rt.heap.add_root(promise)?;

    let res = (|| {
      rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

      let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
        panic!("expected script to return a Promise object, got {promise:?}");
      };

      assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
      assert_eq!(rt.heap.promise_result(promise_obj)?, Some(Value::Bool(true)));
      Ok(())
    })();

    rt.heap.remove_root(promise_root);
    res
  }

  #[test]
  fn logical_and_short_circuits_over_await_compiled() -> Result<(), VmError> {
    assert_compiled_promise_fulfills_true(
      r#"
        async function f() {
          let side = false;
          let v = false && await (side = true, Promise.resolve(1));
          return v === false && side === false;
        }
        f()
      "#,
    )
  }

  #[test]
  fn logical_or_short_circuits_over_await_compiled() -> Result<(), VmError> {
    assert_compiled_promise_fulfills_true(
      r#"
        async function f() {
          let side = false;
          let v = true || await (side = true, Promise.resolve(1));
          return v === true && side === false;
        }
        f()
      "#,
    )
  }

  #[test]
  fn nullish_coalescing_short_circuits_over_await_compiled() -> Result<(), VmError> {
    assert_compiled_promise_fulfills_true(
      r#"
        async function f() {
          let side = false;
          let a = 0 ?? await (side = true, Promise.resolve(1));
          return a === 0 && side === false;
        }
        f()
      "#,
    )
  }

  #[test]
  fn conditional_only_evaluates_selected_branch_with_await_compiled() -> Result<(), VmError> {
    assert_compiled_promise_fulfills_true(
      r#"
        async function f() {
          let side = '';
          let v = true
            ? await (side += 't', Promise.resolve(1))
            : await (side += 'f', Promise.resolve(2));
          return v === 1 && side === 't';
        }
        f()
      "#,
    )
  }

  #[test]
  fn assignment_computed_member_key_evaluated_before_rhs_await_compiled() -> Result<(), VmError> {
    assert_compiled_promise_fulfills_true(
      r#"
        async function f() {
          let order = '';
          let obj = {};
          function k(){ order += 'k'; return Promise.resolve('p'); }
          function v(){ order += 'v'; return Promise.resolve(1); }
          obj[await k()] = await v();
          return order === 'kv' && obj.p === 1;
        }
        f()
      "#,
    )
  }

  #[test]
  fn assignment_rhs_await_happens_before_setter_call_compiled() -> Result<(), VmError> {
    assert_compiled_promise_fulfills_true(
      r#"
        async function f() {
          let order = '';
          let seen = undefined;
          let obj = { set x(v) { order += 's'; seen = v; } };
          async function rhs() { order += 'r'; return 1; }
          obj.x = await rhs();
          return order === 'rs' && seen === 1;
        }
        f()
      "#,
    )
  }
}

#[cfg(test)]
mod hir_async_object_literal_and_optional_call_regressions {
  use crate::{CompiledScript, Heap, HeapLimits, JsRuntime, PromiseState, Value, Vm, VmError, VmOptions};

  fn new_runtime() -> Result<JsRuntime, VmError> {
    let vm = Vm::new(VmOptions::default());
    // Keep this relatively small to exercise GC paths while leaving headroom for Promise/async
    // machinery.
    let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
    JsRuntime::new(vm, heap)
  }

  fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
    let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", source)?;
    assert!(
      !script.requires_ast_fallback,
      "expected test script to execute via compiled (HIR) path",
    );
    rt.exec_compiled_script(script)
  }

  fn assert_promise_fulfills(rt: &mut JsRuntime, promise: Value, expected: Value) -> Result<(), VmError> {
    let Value::Object(promise_obj) = promise else {
      panic!("expected async call to return a Promise object, got {promise:?}");
    };

    let promise_root = rt.heap.add_root(Value::Object(promise_obj))?;
    rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

    let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
      panic!("expected Promise root");
    };
    assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    let result = rt
      .heap
      .promise_result(promise_obj)?
      .expect("expected fulfilled Promise to have [[PromiseResult]]");
    assert_eq!(result, expected);

    rt.heap.remove_root(promise_root);
    Ok(())
  }

  #[test]
  fn async_object_literal_computed_key_and_value_await_order() -> Result<(), VmError> {
    let mut rt = new_runtime()?;

    let promise = exec_compiled(
      &mut rt,
      r#"
        async function f(){
          let order='';
          let o = {
            [await (order += 'k', Promise.resolve('a'))]: await (order += 'v', Promise.resolve(1)),
          };
          return order === 'kv' && o.a === 1;
        }
        f()
      "#,
    )?;
    assert_promise_fulfills(&mut rt, promise, Value::Bool(true))?;
    Ok(())
  }

  #[test]
  fn async_object_literal_computed_key_method_name() -> Result<(), VmError> {
    let mut rt = new_runtime()?;

    let promise = exec_compiled(
      &mut rt,
      r#"
        async function f(){
          let o = { [await Promise.resolve('m')](){ return 2; } };
          return o.m();
        }
        f()
      "#,
    )?;
    assert_promise_fulfills(&mut rt, promise, Value::Number(2.0))?;
    Ok(())
  }

  #[test]
  fn async_object_literal_computed_key_getter() -> Result<(), VmError> {
    let mut rt = new_runtime()?;

    let promise = exec_compiled(
      &mut rt,
      r#"
        async function f(){
          let o = { get [await Promise.resolve('x')](){ return 3; } };
          return o.x;
        }
        f()
      "#,
    )?;
    assert_promise_fulfills(&mut rt, promise, Value::Number(3.0))?;
    Ok(())
  }

  #[test]
  fn async_optional_direct_call_short_circuits_without_evaluating_args() -> Result<(), VmError> {
    let mut rt = new_runtime()?;

    let promise = exec_compiled(
      &mut rt,
      r#"
        async function f(){
          let side=false;
          let fn = null;
          let v = fn?.(await (side=true, Promise.resolve(1)));
          return v === undefined && side === false;
        }
        f()
      "#,
    )?;
    assert_promise_fulfills(&mut rt, promise, Value::Bool(true))?;
    Ok(())
  }

  #[test]
  fn async_optional_member_call_short_circuits_without_evaluating_args() -> Result<(), VmError> {
    let mut rt = new_runtime()?;

    let promise = exec_compiled(
      &mut rt,
      r#"
        async function f(){
          let side=false;
          let obj = null;
          let v = obj?.m(await (side=true, Promise.resolve(1)));
          return v === undefined && side === false;
        }
        f()
      "#,
    )?;
    assert_promise_fulfills(&mut rt, promise, Value::Bool(true))?;
    Ok(())
  }
}
