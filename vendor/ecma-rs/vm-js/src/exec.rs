use crate::destructure::{bind_assignment_target, bind_pattern, BindingKind};
use crate::error_object::new_error;
use crate::iterator;
use crate::ops::{abstract_equality, to_number};
use crate::{
  EnvRootId, ExecutionContext, GcEnv, GcObject, GcString, Heap, JsBigInt, ModuleGraph, ModuleId,
  NativeCall, PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind, Realm,
  RealmId, RootId, Scope, ScriptOrModule, SourceText, StackFrame, Value, Vm, VmError, VmHost,
  VmHostHooks, VmJobContext,
};
use diagnostics::{Diagnostic, FileId};
use parse_js::ast::class_or_object::{ClassMember, ClassOrObjKey, ClassOrObjVal, ObjMemberType};
use parse_js::ast::expr::lit::{
  LitArrElem, LitArrExpr, LitBigIntExpr, LitBoolExpr, LitNumExpr, LitObjExpr, LitStrExpr,
  LitTemplateExpr, LitTemplatePart,
};
use parse_js::ast::expr::pat::{IdPat, Pat};
use parse_js::ast::expr::{
  ArrowFuncExpr, BinaryExpr, CallExpr, ClassExpr, ComputedMemberExpr, CondExpr, Expr, FuncExpr,
  IdExpr, ImportExpr, MemberExpr, TaggedTemplateExpr, UnaryExpr, UnaryPostfixExpr,
};
use parse_js::ast::func::{Func, FuncBody};
use parse_js::ast::node::{literal_string_code_units, Node, ParenthesizedExpr};
use parse_js::ast::stmt::decl::{ClassDecl, FuncDecl, PatDecl, VarDecl, VarDeclMode};
use parse_js::ast::stmt::{
  BlockStmt, CatchBlock, DoWhileStmt, ExprStmt, ForBody, ForInOfLhs, ForInStmt, ForOfStmt,
  ForTripleStmt, IfStmt, LabelStmt, ReturnStmt, Stmt, SwitchStmt, ThrowStmt, TryStmt, WhileStmt,
  WithStmt,
};
use parse_js::operator::OperatorName;
use parse_js::token::TT;
use parse_js::{Dialect, ParseOptions, SourceType};
use std::collections::{HashSet, VecDeque};
use std::mem;
use std::sync::Arc;

use crate::function::ThisMode;
use crate::vm::EcmaFunctionKind;

/// A `throw` completion value paired with a captured stack trace.
#[derive(Clone, Debug, PartialEq)]
pub struct Thrown {
  pub value: Value,
  pub stack: Vec<StackFrame>,
}

/// An ECMAScript completion record (ECMA-262).
///
/// We model the "empty" completion value explicitly as `None` so statement-list evaluation can
/// implement `UpdateEmpty` correctly (e.g. `1; if (true) {}` should evaluate to `1`).
#[derive(Clone, Debug, PartialEq)]
pub enum Completion {
  Normal(Option<Value>),
  Throw(Thrown),
  Return(Value),
  Break(Option<String>, Option<Value>),
  Continue(Option<String>, Option<Value>),
}

impl Completion {
  pub fn empty() -> Self {
    Completion::Normal(None)
  }

  pub fn normal(value: Value) -> Self {
    Completion::Normal(Some(value))
  }

  pub fn value(&self) -> Option<Value> {
    match self {
      Completion::Normal(v) => *v,
      Completion::Throw(thrown) => Some(thrown.value),
      Completion::Return(v) => Some(*v),
      Completion::Break(_, v) => *v,
      Completion::Continue(_, v) => *v,
    }
  }

  pub fn is_abrupt(&self) -> bool {
    !matches!(self, Completion::Normal(_))
  }

  /// Implements `UpdateEmpty(completion, value)` from ECMA-262.
  pub fn update_empty(self, value: Option<Value>) -> Self {
    match self {
      Completion::Normal(None) => Completion::Normal(value),
      Completion::Break(target, None) => Completion::Break(target, value),
      Completion::Continue(target, None) => Completion::Continue(target, value),
      other => other,
    }
  }
}

fn global_var_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

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

fn detect_use_strict_directive(
  stmts: &[Node<Stmt>],
  mut tick: impl FnMut() -> Result<(), VmError>,
) -> Result<bool, VmError> {
  const TICK_EVERY: usize = 32;
  for (i, stmt) in stmts.iter().enumerate() {
    if i % TICK_EVERY == 0 {
      tick()?;
    }
    let Stmt::Expr(expr_stmt) = &*stmt.stx else {
      break;
    };

    let expr = &expr_stmt.stx.expr;

    // Parenthesized string literals are not directive prologues.
    if expr.assoc.get::<ParenthesizedExpr>().is_some() {
      break;
    }

    let Expr::LitStr(lit) = &*expr.stx else {
      break;
    };

    if lit.stx.value == "use strict" {
      return Ok(true);
    }
  }
  Ok(false)
}

fn throw_type_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let value =
    crate::error_object::new_error(scope, intr.type_error_prototype(), "TypeError", message)?;
  Ok(VmError::Throw(value))
}

fn throw_range_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let value =
    crate::error_object::new_error(scope, intr.range_error_prototype(), "RangeError", message)?;
  Ok(VmError::Throw(value))
}

fn throw_reference_error(
  vm: &Vm,
  scope: &mut Scope<'_>,
  message: &str,
) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let value = crate::error_object::new_error(
    scope,
    intr.reference_error_prototype(),
    "ReferenceError",
    message,
  )?;
  Ok(VmError::Throw(value))
}

fn syntax_error(loc: parse_js::loc::Loc, message: impl Into<String>) -> VmError {
  let span = loc.to_diagnostics_span(FileId(0));
  VmError::Syntax(vec![Diagnostic::error("VMJS0002", message, span)])
}

#[derive(Clone, Copy, Debug)]
enum VarEnv {
  GlobalObject,
  Env(GcEnv),
}

#[derive(Clone, Copy, Debug)]
enum ClassBinding<'a> {
  None,
  Mutable(&'a str),
  Immutable(&'a str),
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeEnv {
  global_object: GcObject,
  lexical_env: GcEnv,
  lexical_root: EnvRootId,
  var_env: VarEnv,
  source: Arc<SourceText>,
  base_offset: u32,
  prefix_len: u32,
}

impl RuntimeEnv {
  fn new(heap: &mut Heap, global_object: GcObject) -> Result<Self, VmError> {
    // Root the global object across env allocation in case it triggers GC.
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global_object))?;

    let lexical_env = scope.env_create(None)?;
    let lexical_root = scope.heap_mut().add_env_root(lexical_env)?;

    Ok(Self {
      global_object,
      lexical_env,
      lexical_root,
      var_env: VarEnv::GlobalObject,
      source: Arc::new(SourceText::new("<init>", "")),
      base_offset: 0,
      prefix_len: 0,
    })
  }

  pub(crate) fn new_with_var_env(
    heap: &mut Heap,
    global_object: GcObject,
    lexical_env: GcEnv,
    var_env: GcEnv,
  ) -> Result<Self, VmError> {
    // Root the global object across root registration in case it triggers GC.
    let mut scope = heap.scope();
    scope.push_root(Value::Object(global_object))?;
    scope.push_env_root(lexical_env)?;
    scope.push_env_root(var_env)?;

    let lexical_root = scope.heap_mut().add_env_root(lexical_env)?;

    Ok(Self {
      global_object,
      lexical_env,
      lexical_root,
      var_env: VarEnv::Env(var_env),
      source: Arc::new(SourceText::new("<init>", "")),
      base_offset: 0,
      prefix_len: 0,
    })
  }

  pub(crate) fn teardown(&mut self, heap: &mut Heap) {
    heap.remove_env_root(self.lexical_root);
  }

  fn set_lexical_env(&mut self, heap: &mut Heap, env: GcEnv) {
    self.lexical_env = env;
    heap.set_env_root(self.lexical_root, env);
  }

  pub(crate) fn set_source_info(
    &mut self,
    source: Arc<SourceText>,
    base_offset: u32,
    prefix_len: u32,
  ) {
    self.source = source;
    self.base_offset = base_offset;
    self.prefix_len = prefix_len;
  }

  pub(crate) fn source(&self) -> Arc<SourceText> {
    self.source.clone()
  }

  pub(crate) fn base_offset(&self) -> u32 {
    self.base_offset
  }

  pub(crate) fn prefix_len(&self) -> u32 {
    self.prefix_len
  }

  pub(crate) fn global_object(&self) -> GcObject {
    self.global_object
  }

  pub(crate) fn lexical_env(&self) -> GcEnv {
    self.lexical_env
  }

  fn resolve_lexical_binding(
    &self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    scope: &mut Scope<'_>,
    name: &str,
  ) -> Result<Option<GcEnv>, VmError> {
    enum EnvKind {
      Declarative,
      Object {
        binding_object: GcObject,
        with_environment: bool,
      },
    }

    let mut current = Some(self.lexical_env);
    while let Some(env) = current {
      let kind = match scope.heap().get_env_record(env)? {
        crate::env::EnvRecord::Declarative(_) => EnvKind::Declarative,
        crate::env::EnvRecord::Object(obj) => EnvKind::Object {
          binding_object: obj.binding_object,
          with_environment: obj.with_environment,
        },
      };

      match kind {
        EnvKind::Declarative => {
          if scope.heap().env_has_binding(env, name)? {
            return Ok(Some(env));
          }
        }
        EnvKind::Object {
          binding_object,
          with_environment,
        } => {
          if self.object_env_has_binding(
            vm,
            host,
            hooks,
            scope,
            binding_object,
            with_environment,
            name,
          )? {
            return Ok(Some(env));
          }
        }
      }

      current = scope.heap().env_outer(env)?;
    }

    Ok(None)
  }

  fn object_env_has_binding(
    &self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    scope: &mut Scope<'_>,
    binding_object: GcObject,
    with_environment: bool,
    name: &str,
  ) -> Result<bool, VmError> {
    // ObjectEnvironmentRecord.HasBinding (ECMA-262).
    //
    // This is only used for `with` environments today, but we keep the generic shape since the heap
    // supports ObjectEnvRecord independently.
    let mut check_scope = scope.reborrow();
    check_scope.push_root(Value::Object(binding_object))?;
    let name_s = check_scope.alloc_string(name)?;
    check_scope.push_root(Value::String(name_s))?;
    let name_key = PropertyKey::from_string(name_s);

    // HasProperty(O, N)
    if !check_scope.ordinary_has_property_with_tick(binding_object, name_key, || vm.tick())? {
      return Ok(false);
    }

    if !with_environment {
      return Ok(true);
    }

    // If `@@unscopables` blocks this name, treat it as not present so resolution falls back to the
    // outer environment.
    let intr = vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    let unscopables_key = PropertyKey::from_symbol(intr.well_known_symbols().unscopables);
    let receiver = Value::Object(binding_object);

    let unscopables = check_scope.ordinary_get_with_host_and_hooks(
      vm,
      host,
      hooks,
      binding_object,
      unscopables_key,
      receiver,
    )?;
    check_scope.push_root(unscopables)?;
    let Value::Object(unscopables_obj) = unscopables else {
      return Ok(true);
    };

    check_scope.push_root(Value::Object(unscopables_obj))?;
    let blocked = check_scope.ordinary_get_with_host_and_hooks(
      vm,
      host,
      hooks,
      unscopables_obj,
      name_key,
      Value::Object(unscopables_obj),
    )?;
    Ok(!check_scope.heap().to_boolean(blocked)?)
  }

  fn declare_var(&mut self, vm: &mut Vm, scope: &mut Scope<'_>, name: &str) -> Result<(), VmError> {
    match self.var_env {
      VarEnv::GlobalObject => {
        let global_object = self.global_object;

        // Root the global object across property-key allocation in case it triggers GC.
        let mut key_scope = scope.reborrow();
        key_scope.push_root(Value::Object(global_object))?;

        let key = PropertyKey::from_string(key_scope.alloc_string(name)?);
        if key_scope
          .heap()
          .object_get_own_property_with_tick(global_object, &key, || vm.tick())?
          .is_some()
        {
          return Ok(());
        }

        key_scope.define_property(
          global_object,
          key,
          global_var_binding_desc(Value::Undefined),
        )?;
        Ok(())
      }
      VarEnv::Env(env) => {
        if scope.heap().env_has_binding(env, name)? {
          return Ok(());
        }
        scope.env_create_mutable_binding(env, name)?;
        scope
          .heap_mut()
          .env_initialize_binding(env, name, Value::Undefined)?;
        Ok(())
      }
    }
  }

  fn get(
    &self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    scope: &mut Scope<'_>,
    name: &str,
  ) -> Result<Option<Value>, VmError> {
    if let Some(env) = self.resolve_lexical_binding(vm, host, hooks, scope, name)? {
      let binding_object = match scope.heap().get_env_record(env)? {
        crate::env::EnvRecord::Declarative(_) => None,
        crate::env::EnvRecord::Object(obj) => Some(obj.binding_object),
      };

      if let Some(binding_object) = binding_object {
        // Object environment record (e.g. `with (obj) { ... }`): resolve the binding via `Get(O, N)`.
        let mut key_scope = scope.reborrow();
        key_scope.push_root(Value::Object(binding_object))?;
        let key_s = key_scope.alloc_string(name)?;
        key_scope.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);
        let receiver = Value::Object(binding_object);
        let value = key_scope.ordinary_get_with_host_and_hooks(
          vm,
          host,
          hooks,
          binding_object,
          key,
          receiver,
        )?;
        return Ok(Some(value));
      }

      match scope.heap().env_get_binding_value(env, name, false) {
        Ok(v) => return Ok(Some(v)),
        // TDZ sentinel from `Heap::{env_get_binding_value, env_set_mutable_binding}`.
        Err(VmError::Throw(Value::Null)) => {
          let msg = format!("Cannot access '{}' before initialization", name);
          return Err(throw_reference_error(vm, scope, &msg)?);
        }
        Err(err) => return Err(err),
      }
    }

    // Fall back to global object property lookup.
    let global_object = self.global_object;
    let mut key_scope = scope.reborrow();
    key_scope.push_root(Value::Object(global_object))?;
    let key_s = key_scope.alloc_string(name)?;
    key_scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);

    // Distinguish between a missing property (unbound identifier) and a present property whose
    // value is actually `undefined`.
    if !key_scope.ordinary_has_property_with_tick(global_object, key, || vm.tick())? {
      return Ok(None);
    }

    let receiver = Value::Object(global_object);
    Ok(Some(key_scope.ordinary_get_with_host_and_hooks(
      vm,
      host,
      hooks,
      global_object,
      key,
      receiver,
    )?))
  }

  pub(crate) fn set(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    scope: &mut Scope<'_>,
    name: &str,
    value: Value,
    strict: bool,
  ) -> Result<(), VmError> {
    if let Some(env) = self.resolve_lexical_binding(vm, host, hooks, scope, name)? {
      let binding_object = match scope.heap().get_env_record(env)? {
        crate::env::EnvRecord::Declarative(_) => None,
        crate::env::EnvRecord::Object(obj) => Some(obj.binding_object),
      };

      if let Some(binding_object) = binding_object {
        let receiver = Value::Object(binding_object);
        let mut key_scope = scope.reborrow();
        key_scope.push_root(receiver)?;
        // Root `value` across key allocation and property assignment in case it triggers a GC.
        key_scope.push_root(value)?;
        let key_s = key_scope.alloc_string(name)?;
        key_scope.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);
        let ok = key_scope.ordinary_set_with_host_and_hooks(
          vm,
          host,
          hooks,
          binding_object,
          key,
          value,
          receiver,
        )?;
        if ok {
          return Ok(());
        }
        if strict {
          return Err(throw_type_error(vm, &mut key_scope, "Cannot assign to read-only property")?);
        }
        return Ok(());
      }

      match scope.heap_mut().env_set_mutable_binding(env, name, value, strict) {
        Ok(()) => return Ok(()),
        // TDZ sentinel from `Heap::{env_get_binding_value, env_set_mutable_binding}`.
        Err(VmError::Throw(Value::Null)) => {
          let msg = format!("Cannot access '{}' before initialization", name);
          return Err(throw_reference_error(vm, scope, &msg)?);
        }
        // `const` assignment sentinel from `Heap::env_set_mutable_binding`.
        Err(VmError::Throw(Value::Undefined)) => {
          return Err(throw_type_error(vm, scope, "Assignment to constant variable.")?);
        }
        Err(err) => return Err(err),
      }
    }

    // Assignment to global (var) bindings is backed by the global object.
    let global_object = self.global_object;
    let mut key_scope = scope.reborrow();
    key_scope.push_root(Value::Object(global_object))?;
    // Root `value` across key allocation and property definition in case they trigger GC.
    key_scope.push_root(value)?;
    let key = PropertyKey::from_string(key_scope.alloc_string(name)?);

    let has_binding =
      key_scope.ordinary_has_property_with_tick(global_object, key, || vm.tick())?;
    if !has_binding {
      if strict {
        let msg = format!("{name} is not defined");
        return Err(throw_reference_error(vm, &mut key_scope, &msg)?);
      }

      // Sloppy-mode: create a new global `var` property.
      key_scope.define_property(global_object, key, global_var_desc(value))?;
      return Ok(());
    }

    if let Some(desc) =
      key_scope
        .heap()
        .object_get_own_property_with_tick(global_object, &key, || vm.tick())?
    {
      match desc.kind {
        PropertyKind::Data { writable: true, .. } => {
          key_scope
            .heap_mut()
            .object_set_existing_data_property_value(global_object, &key, value)?;
          return Ok(());
        }
        PropertyKind::Data {
          writable: false, ..
        } => {
          if strict {
            let msg = format!("Cannot assign to read only property '{name}'");
            return Err(throw_type_error(vm, &mut key_scope, &msg)?);
          }
          return Ok(());
        }
        PropertyKind::Accessor { .. } => {
          let receiver = Value::Object(global_object);
          let ok = key_scope.ordinary_set_with_host_and_hooks(
            vm,
            host,
            hooks,
            global_object,
            key,
            value,
            receiver,
          )?;
          if ok {
            return Ok(());
          }
          if strict {
            let msg = format!("Cannot assign to read only property '{name}'");
            return Err(throw_type_error(vm, &mut key_scope, &msg)?);
          }
          return Ok(());
        }
      }
    }

    // Property is inherited through the prototype chain: define an own data property.
    key_scope.define_property(global_object, key, global_var_desc(value))?;
    Ok(())
  }

  pub(crate) fn set_var(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    scope: &mut Scope<'_>,
    name: &str,
    value: Value,
  ) -> Result<(), VmError> {
    // `var` declarations always assign to the global/function var environment, even when a lexical
    // binding shadows the identifier (e.g. a `catch(e)` parameter).
    // Root the initializer value across var-env binding creation/assignment in case it triggers GC.
    let mut outer_scope = scope.reborrow();
    outer_scope.push_root(value)?;
    self.declare_var(vm, &mut outer_scope, name)?;

    match self.var_env {
      VarEnv::GlobalObject => {
        let global_object = self.global_object;
        let mut key_scope = outer_scope.reborrow();
        key_scope.push_root(Value::Object(global_object))?;
        key_scope.push_root(value)?;
        let key = PropertyKey::from_string(key_scope.alloc_string(name)?);

        if let Some(desc) =
          key_scope
            .heap()
            .object_get_own_property_with_tick(global_object, &key, || vm.tick())?
        {
          match desc.kind {
            PropertyKind::Data { writable: true, .. } => {
              key_scope
                .heap_mut()
                .object_set_existing_data_property_value(global_object, &key, value)?;
              return Ok(());
            }
            PropertyKind::Data {
              writable: false, ..
            } => {
              return Err(VmError::Unimplemented(
                "assignment to non-writable global property",
              ));
            }
            PropertyKind::Accessor { .. } => {
              let receiver = Value::Object(global_object);
              let ok = key_scope.ordinary_set_with_host_and_hooks(
                vm,
                host,
                hooks,
                global_object,
                key,
                value,
                receiver,
              )?;
              if ok {
                return Ok(());
              }
              return Err(VmError::Unimplemented(
                "assignment to non-writable global property",
              ));
            }
          }
        }

        // If the binding was inherited through the prototype chain, define an own data property.
        key_scope.define_property(global_object, key, global_var_binding_desc(value))?;
        Ok(())
      }
      VarEnv::Env(env) => outer_scope
        .heap_mut()
        .env_set_mutable_binding(env, name, value, false),
    }
  }
}

/// An (early, incomplete) AST-interpreting execution engine for `parse-js` syntax trees.
pub struct JsRuntime {
  pub vm: Vm,
  pub heap: Heap,
  realm: Realm,
  env: RuntimeEnv,
  modules: Box<ModuleGraph>,
}

impl JsRuntime {
  pub fn new(vm: Vm, heap: Heap) -> Result<Self, VmError> {
    let mut vm = vm;
    let mut heap = heap;
    let realm = Realm::new(&mut vm, &mut heap)?;
    let env = RuntimeEnv::new(&mut heap, realm.global_object())?;
    let mut modules = Box::new(ModuleGraph::new());
    // Make the runtime-owned module graph available to nested ECMAScript function calls (and other
    // VM entry points that do not naturally thread an explicit `&mut ModuleGraph` parameter).
    vm.set_module_graph(modules.as_mut());

    // Intrinsic Function constructor semantics rely on `[[Realm]]` and (for dynamic functions)
    // `[[Environment]]` being populated:
    // - `builtins::function_constructor_construct` creates functions in the realm of the Function
    //   constructor object.
    // - Those created functions should capture the global lexical environment (not the caller's
    //   lexical environment), so we store the global lexical env on the Function constructor's
    //   closure env slot.
    //
    // Realm initialization happens before `RuntimeEnv::new` creates the global lexical environment,
    // so patch it up here once both exist.
    {
      let function_ctor = realm.intrinsics().function_constructor();
      heap.set_function_realm(function_ctor, realm.global_object())?;
      heap.set_function_job_realm(function_ctor, realm.id())?;
      heap.set_function_closure_env(function_ctor, Some(env.lexical_env()))?;
    }
    Ok(Self {
      vm,
      heap,
      realm,
      env,
      modules,
    })
  }

  pub fn realm(&self) -> &Realm {
    &self.realm
  }

  /// Borrow-split the runtime into its core components: the VM, the current realm, and the heap.
  ///
  /// Embeddings often need `&mut Vm` + `&mut Heap` to execute code, allocate, and/or run GC, while
  /// also needing immutable access to realm metadata (global object, intrinsics, realm id). Doing
  /// this via `JsRuntime::{vm, heap}` + `JsRuntime::realm()` requires an embedder-side raw-pointer
  /// workaround to satisfy the borrow checker.
  ///
  /// This accessor is safe because `vm`, `realm`, and `heap` are stored as disjoint fields inside
  /// [`JsRuntime`].
  pub fn vm_realm_and_heap_mut(&mut self) -> (&mut Vm, &Realm, &mut Heap) {
    let vm = &mut self.vm;
    let realm = &self.realm;
    let heap = &mut self.heap;
    (vm, realm, heap)
  }

  /// Borrow-split the runtime into its core components: the VM, the module graph, and the heap.
  pub fn vm_modules_and_heap_mut(&mut self) -> (&mut Vm, &mut ModuleGraph, &mut Heap) {
    let vm = &mut self.vm;
    let modules = &mut *self.modules;
    let heap = &mut self.heap;
    (vm, modules, heap)
  }

  pub fn heap(&self) -> &Heap {
    &self.heap
  }

  pub fn heap_mut(&mut self) -> &mut Heap {
    &mut self.heap
  }

  /// Returns this runtime's module graph.
  pub fn modules(&self) -> &ModuleGraph {
    &self.modules
  }

  /// Borrows this runtime's module graph mutably.
  pub fn modules_mut(&mut self) -> &mut ModuleGraph {
    &mut self.modules
  }

  /// Registers a native call handler and exposes it as a global binding.
  ///
  /// This is a convenience API for embeddings (e.g. FastRender) that need to expose host functions
  /// (`setTimeout`, DOM bindings, etc.) to interpreted scripts.
  pub fn register_global_native_function(
    &mut self,
    name: &str,
    call: NativeCall,
    length: u32,
  ) -> Result<GcObject, VmError> {
    let call_id = self.vm.register_native_call(call)?;

    let mut scope = self.heap.scope();
    let func_name = scope.alloc_string(name)?;
    let func = scope.alloc_native_function(call_id, None, func_name, length)?;

    // Root the function object across allocations while defining it on the global object.
    scope.push_root(Value::Object(func))?;

    let key = PropertyKey::from_string(scope.alloc_string(name)?);
    scope.define_property(
      self.env.global_object(),
      key,
      global_var_desc(Value::Object(func)),
    )?;

    Ok(func)
  }

  /// Parse and execute a classic script (ECMAScript dialect, `SourceType::Script`).
  ///
  /// ## Host context
  ///
  /// This convenience wrapper passes a **dummy host context** (`()`) to native call/construct
  /// handlers.
  ///
  /// Embeddings that need native handlers to observe real host state should use
  /// [`JsRuntime::exec_script_with_host`] (explicit host; VM-owned microtask queue) or
  /// [`JsRuntime::exec_script_with_host_and_hooks`] (explicit host + custom hooks).
  pub fn exec_script(&mut self, source: &str) -> Result<Value, VmError> {
    let mut host = ();
    self.exec_script_with_host(&mut host, source)
  }

  /// Parse and execute a classic script (ECMAScript dialect, `SourceType::Script`) using an explicit
  /// embedder host context and host hook implementation.
  ///
  /// This is the string-source convenience wrapper for
  /// [`JsRuntime::exec_script_source_with_host_and_hooks`].
  pub fn exec_script_with_host_and_hooks(
    &mut self,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    source: &str,
  ) -> Result<Value, VmError> {
    self.exec_script_source_with_host_and_hooks(
      host,
      hooks,
      Arc::new(SourceText::new("<inline>", source)),
    )
  }

  /// Parse and execute a classic script, using a custom host hook implementation.
  ///
  /// This is intended for embeddings that need Promise jobs enqueued by the script to be routed via
  /// `VmHostHooks::host_enqueue_promise_job` (for example, an HTML microtask queue) instead of the
  /// VM-owned microtask queue used by [`JsRuntime::exec_script`].
  ///
  /// ## Host context
  ///
  /// This hook-only API passes a **dummy host context** (`()`) to native call/construct handlers.
  /// It is intended for unit tests and lightweight embeddings.
  ///
  /// Embeddings that need native handlers to downcast and mutate real host state should use
  /// [`JsRuntime::exec_script_with_host_and_hooks`] /
  /// [`JsRuntime::exec_script_source_with_host_and_hooks`].
  pub fn exec_script_with_hooks(
    &mut self,
    hooks: &mut dyn VmHostHooks,
    source: &str,
  ) -> Result<Value, VmError> {
    self.exec_script_source_with_hooks(hooks, Arc::new(SourceText::new("<inline>", source)))
  }

  /// Parse and execute a classic script (ECMAScript dialect, `SourceType::Script`).
  ///
  /// ## Host context
  ///
  /// This convenience wrapper passes a **dummy host context** (`()`) to native call/construct
  /// handlers.
  ///
  /// Embeddings that need native handlers to observe real host state should use
  /// [`JsRuntime::exec_script_source_with_host`] (explicit host; VM-owned microtask queue) or
  /// [`JsRuntime::exec_script_source_with_host_and_hooks`] (explicit host + custom hooks).
  pub fn exec_script_source(&mut self, source: Arc<SourceText>) -> Result<Value, VmError> {
    let mut host = ();
    self.exec_script_source_with_host(&mut host, source)
  }

  /// Parse and execute a classic script (ECMAScript dialect, `SourceType::Script`) with an explicit
  /// embedder host context.
  pub fn exec_script_with_host(
    &mut self,
    host: &mut dyn VmHost,
    source: &str,
  ) -> Result<Value, VmError> {
    self.exec_script_source_with_host(host, Arc::new(SourceText::new("<inline>", source)))
  }

  /// Parse and execute a classic script (ECMAScript dialect, `SourceType::Script`) with an explicit
  /// embedder host context.
  pub fn exec_script_source_with_host(
    &mut self,
    host: &mut dyn VmHost,
    source: Arc<SourceText>,
  ) -> Result<Value, VmError> {
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    };
    let top = self.vm.parse_top_level_with_budget(&source.text, opts)?;

    let global_object = self.realm.global_object();
    self.env.set_source_info(source.clone(), 0, 0);

    let (line, col) = source.line_col(0);
    let frame = StackFrame {
      function: None,
      source: source.name.clone(),
      line,
      col,
    };

    let exec_ctx = crate::ExecutionContext {
      realm: self.realm.id(),
      script_or_module: None,
    };
    let mut vm_ctx = self.vm.execution_context_guard(exec_ctx);

    // Script evaluation needs a host hook implementation for Promise jobs. `Vm` stores a default
    // microtask queue inside itself, but we need to hold `&mut Vm` and `&mut dyn VmHostHooks`
    // simultaneously. Temporarily move the queue out so it can be passed as `hooks`.
    let mut hooks = mem::take(vm_ctx.microtask_queue_mut());
    let prev_hooks = vm_ctx.push_active_host_hooks(&mut hooks);

    let result: Result<Value, VmError> = (|| {
      let mut vm_frame = vm_ctx.enter_frame(frame)?;

      // Charge at least one tick at script entry so even an empty script respects fuel/deadline /
      // interrupt budgets.
      vm_frame.tick()?;

      let strict = detect_use_strict_directive(&top.stx.body, || vm_frame.tick())?;

      let mut scope = self.heap.scope();
      // In classic scripts, top-level `this` is the global object (even in strict mode).
      let global_this = Value::Object(global_object);
      let mut evaluator = Evaluator {
        vm: &mut *vm_frame,
        host,
        hooks: &mut hooks,
        env: &mut self.env,
        strict,
        this: global_this,
        new_target: Value::Undefined,
      };

      evaluator.instantiate_script(&mut scope, &top.stx.body)?;

      let completion = evaluator.eval_stmt_list(&mut scope, &top.stx.body)?;
      match completion {
        Completion::Normal(v) => Ok(v.unwrap_or(Value::Undefined)),
        Completion::Throw(thrown) => Err(VmError::ThrowWithStack {
          value: thrown.value,
          stack: thrown.stack,
        }),
        Completion::Return(_) => Err(VmError::Unimplemented("return outside of function")),
        Completion::Break(..) => Err(VmError::Unimplemented("break outside of loop")),
        Completion::Continue(..) => Err(VmError::Unimplemented("continue outside of loop")),
      }
    })();

    // Pop any host hooks override before restoring `hooks` back into the VM to avoid leaving the VM
    // with a dangling pointer (the override stores a raw pointer to `hooks`).
    vm_ctx.pop_active_host_hooks(prev_hooks);

    // As a safety net, drain any Promise jobs that were enqueued onto the VM-owned microtask queue
    // (for example by native handlers calling `vm.microtask_queue_mut()` while the queue was moved
    // out) into `hooks` before restoring it.
    while let Some((realm, job)) = vm_ctx.microtask_queue_mut().pop_front() {
      hooks.enqueue_promise_job(job, realm);
    }
    // Restore the VM's microtask queue so the embedding can run a microtask checkpoint later.
    *vm_ctx.microtask_queue_mut() = hooks;

    result
  }

  /// Parse and execute a classic script (ECMAScript dialect, `SourceType::Script`) using an explicit
  /// embedder host context and host hook implementation.
  pub fn exec_script_source_with_host_and_hooks(
    &mut self,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    source: Arc<SourceText>,
  ) -> Result<Value, VmError> {
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    };
    let top = self.vm.parse_top_level_with_budget(&source.text, opts)?;

    let global_object = self.realm.global_object();
    self.env.set_source_info(source.clone(), 0, 0);

    let (line, col) = source.line_col(0);
    let frame = StackFrame {
      function: None,
      source: source.name.clone(),
      line,
      col,
    };

    let exec_ctx = crate::ExecutionContext {
      realm: self.realm.id(),
      script_or_module: None,
    };
    let mut vm_ctx = self.vm.execution_context_guard(exec_ctx);
    let prev_host = vm_ctx.push_active_host_hooks(hooks);

    let result = (|| {
      let mut vm_frame = vm_ctx.enter_frame(frame)?;

      // Charge at least one tick at script entry so even an empty script respects fuel/deadline /
      // interrupt budgets.
      vm_frame.tick()?;

      let strict = detect_use_strict_directive(&top.stx.body, || vm_frame.tick())?;

      let mut scope = self.heap.scope();
      // In classic scripts, top-level `this` is the global object (even in strict mode).
      let global_this = Value::Object(global_object);
      let mut evaluator = Evaluator {
        vm: &mut *vm_frame,
        host,
        hooks,
        env: &mut self.env,
        strict,
        this: global_this,
        new_target: Value::Undefined,
      };

      evaluator.instantiate_script(&mut scope, &top.stx.body)?;

      let completion = evaluator.eval_stmt_list(&mut scope, &top.stx.body)?;
      match completion {
        Completion::Normal(v) => Ok(v.unwrap_or(Value::Undefined)),
        Completion::Throw(thrown) => Err(VmError::ThrowWithStack {
          value: thrown.value,
          stack: thrown.stack,
        }),
        Completion::Return(_) => Err(VmError::Unimplemented("return outside of function")),
        Completion::Break(..) => Err(VmError::Unimplemented("break outside of loop")),
        Completion::Continue(..) => Err(VmError::Unimplemented("continue outside of loop")),
      }
    })();

    vm_ctx.pop_active_host_hooks(prev_host);
    drop(vm_ctx);

    // As a safety net, drain any Promise jobs that were enqueued onto the VM-owned microtask queue
    // into the embedding's host hook implementation.
    while let Some((realm, job)) = self.vm.microtask_queue_mut().pop_front() {
      hooks.host_enqueue_promise_job(job, realm);
    }

    result
  }

  /// Parse and execute a classic script (ECMAScript dialect, `SourceType::Script`) using a custom
  /// host hook implementation.
  ///
  /// ## Host context
  ///
  /// This hook-only API passes a **dummy host context** (`()`) to native call/construct handlers.
  /// It is intended for unit tests and lightweight embeddings.
  ///
  /// Embeddings that need native handlers to downcast and mutate real host state should use
  /// [`JsRuntime::exec_script_source_with_host_and_hooks`].
  pub fn exec_script_source_with_hooks(
    &mut self,
    hooks: &mut dyn VmHostHooks,
    source: Arc<SourceText>,
  ) -> Result<Value, VmError> {
    let mut dummy_host = ();
    self.exec_script_source_with_host_and_hooks(&mut dummy_host, hooks, source)
  }
}

impl Drop for JsRuntime {
  fn drop(&mut self) {
    // Unregister persistent roots created by global lexical bindings and the realm. This keeps heap
    // reuse in tests/embeddings from accumulating roots and satisfies `Realm`'s debug assertion.
    self.env.teardown(&mut self.heap);
    self.realm.teardown(&mut self.heap);
  }
}

impl VmJobContext for JsRuntime {
  fn call(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    // Borrow-split `vm` and `heap` so we can hold a `Scope` while calling into the VM.
    let vm = &mut self.vm;
    let heap = &mut self.heap;
    let mut scope = heap.scope();
    vm.call_with_host(&mut scope, host, callee, this, args)
  }

  fn construct(
    &mut self,
    host: &mut dyn VmHostHooks,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    let vm = &mut self.vm;
    let heap = &mut self.heap;
    let mut scope = heap.scope();
    vm.construct_with_host(&mut scope, host, callee, args, new_target)
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id)
  }
}

struct Evaluator<'a> {
  vm: &'a mut Vm,
  host: &'a mut dyn VmHost,
  hooks: &'a mut dyn VmHostHooks,
  env: &'a mut RuntimeEnv,
  strict: bool,
  this: Value,
  new_target: Value,
}

#[derive(Clone, Copy, Debug)]
enum Reference<'a> {
  Binding(&'a str),
  /// A property reference.
  ///
  /// Note: per ECMA-262, the reference stores the *base value* (which may be a primitive). Property
  /// access/assignment performs `ToObject(base)` for the actual `[[Get]]`/`[[Set]]` operation but
  /// uses the original base value as the `receiver` / call `this` binding.
  Property {
    base: Value,
    key: PropertyKey,
  },
}

#[derive(Clone, Copy, Debug)]
enum ToPrimitiveHint {
  Default,
  String,
  Number,
}

impl ToPrimitiveHint {
  fn as_str(self) -> &'static str {
    match self {
      ToPrimitiveHint::Default => "default",
      ToPrimitiveHint::String => "string",
      ToPrimitiveHint::Number => "number",
    }
  }
}

#[derive(Clone, Copy, Debug)]
enum NumericValue {
  Number(f64),
  BigInt(JsBigInt),
}

impl<'a> Evaluator<'a> {
  /// Runs one VM "tick".
  ///
  /// ## Tick policy (AST evaluator)
  ///
  /// This interpreter charges **one tick** at the start of every statement evaluation
  /// ([`Evaluator::eval_stmt`]) and every expression evaluation ([`Evaluator::eval_expr`]).
  ///
  /// Some constructs (e.g. `for(;;){}` with an empty body and no condition/update expressions) may
  /// otherwise loop without evaluating any statements/expressions per iteration; those paths tick
  /// explicitly as well.
  ///
  /// Additional ticks are also charged inside some literal-construction loops (for example: holey
  /// array literals, object literals with shorthand/method members, tagged template objects) to
  /// ensure that large `O(N)` literal processing cannot run for an unbounded amount of time within
  /// a single expression tick.
  #[inline]
  fn tick(&mut self) -> Result<(), VmError> {
    self.vm.tick()
  }

  #[inline]
  fn call(
    &mut self,
    scope: &mut Scope<'_>,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    self
      .vm
      .call_with_host_and_hooks(&mut *self.host, scope, &mut *self.hooks, callee, this, args)
  }

  #[inline]
  fn construct(
    &mut self,
    scope: &mut Scope<'_>,
    callee: Value,
    args: &[Value],
    new_target: Value,
  ) -> Result<Value, VmError> {
    self.vm.construct_with_host_and_hooks(
      &mut *self.host,
      scope,
      &mut *self.hooks,
      callee,
      args,
      new_target,
    )
  }

  fn function_length(&mut self, func: &parse_js::ast::func::Func) -> Result<u32, VmError> {
    // ECMA-262 `length` is the number of parameters before the first one with a default/rest.
    //
    // This scan can be `O(N)` in the number of parameters, and function expressions/declarations
    // can have very large parameter lists (bounded by source size). Budget it explicitly so a
    // single function literal can't do unbounded work within a single statement/expression tick.
    const TICK_EVERY: usize = 32;
    let mut len: u32 = 0;
    for (i, param) in func.parameters.iter().enumerate() {
      if i % TICK_EVERY == 0 {
        self.tick()?;
      }
      if param.stx.rest || param.stx.default_value.is_some() {
        break;
      }
      len = len.saturating_add(1);
    }
    Ok(len)
  }

  fn instantiate_script(
    &mut self,
    scope: &mut Scope<'_>,
    stmts: &[Node<Stmt>],
  ) -> Result<(), VmError> {
    self.instantiate_stmt_list(scope, stmts)
  }

  fn instantiate_function(
    &mut self,
    scope: &mut Scope<'_>,
    func: &Node<Func>,
    args: &[Value],
  ) -> Result<(), VmError> {
    const PROLOGUE_TICK_EVERY: usize = 32;

    // Pre-create all parameter bindings before evaluating default initializers so identifier
    // references during parameter evaluation observe TDZ semantics.
    let env_rec = self.env.lexical_env;
    for param in &func.stx.parameters {
      self.tick()?;
      self.instantiate_lexical_names_from_pat(
        scope,
        env_rec,
        &param.stx.pattern.stx.pat.stx,
        param.loc,
        true,
      )?;
    }

    // Create a minimal `arguments` object for non-arrow functions.
    //
    // test262's harness expects `arguments` to exist and be array-like (`length`, indexed elements).
    // We do not implement mapped arguments objects yet.
    //
    // `arguments` must be created before default parameter initializers run so defaults can read it.
    if !func.stx.arrow
      && !scope
        .heap()
        .env_has_binding(self.env.lexical_env, "arguments")?
    {
      let intr = self
        .vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

      let args_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(args_obj))?;
      scope
        .heap_mut()
        .object_set_prototype(args_obj, Some(intr.object_prototype()))?;

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
          self.tick()?;
        }
        let mut idx_scope = scope.reborrow();
        idx_scope.push_root(Value::Object(args_obj))?;
        idx_scope.push_root(v)?;
        let key = PropertyKey::from_string(idx_scope.alloc_string(&i.to_string())?);
        idx_scope.define_property(args_obj, key, global_var_desc(v))?;
      }

      scope.env_create_mutable_binding(self.env.lexical_env, "arguments")?;
      scope.heap_mut().env_initialize_binding(
        self.env.lexical_env,
        "arguments",
        Value::Object(args_obj),
      )?;
    }

    // Bind parameters in order, evaluating default initializers as needed.
    for (idx, param) in func.stx.parameters.iter().enumerate() {
      self.tick()?;
      if param.stx.rest {
        // Rest parameter: collect remaining args into an Array.
        let rest_slice = args.get(idx..).unwrap_or(&[]);
        let intr = self
          .vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

        let mut rest_scope = scope.reborrow();
        let arr = rest_scope.alloc_array(0)?;
        rest_scope.push_root(Value::Object(arr))?;
        rest_scope
          .heap_mut()
          .object_set_prototype(arr, Some(intr.array_prototype()))?;

        for (i, v) in rest_slice.iter().copied().enumerate() {
          if i % PROLOGUE_TICK_EVERY == 0 {
            self.tick()?;
          }
          let mut elem_scope = rest_scope.reborrow();
          elem_scope.push_root(Value::Object(arr))?;
          elem_scope.push_root(v)?;
          let key_s = elem_scope.alloc_string(&i.to_string())?;
          elem_scope.push_root(Value::String(key_s))?;
          let key = PropertyKey::from_string(key_s);
          let ok = elem_scope.create_data_property(arr, key, v)?;
          if !ok {
            return Err(VmError::Unimplemented("CreateDataProperty returned false"));
          }
        }

        bind_pattern(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut rest_scope,
          self.env,
          &param.stx.pattern.stx.pat.stx,
          Value::Object(arr),
          BindingKind::Let,
          self.strict,
          self.this,
        )?;

        if idx + 1 != func.stx.parameters.len() {
          return Err(VmError::Unimplemented("rest parameter must be last"));
        }
        break;
      }

      let mut value = args.get(idx).copied().unwrap_or(Value::Undefined);
      if matches!(value, Value::Undefined) {
        if let Some(default_expr) = &param.stx.default_value {
          value = self.eval_expr(scope, default_expr)?;
        }
      }

      bind_pattern(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        scope,
        self.env,
        &param.stx.pattern.stx.pat.stx,
        value,
        BindingKind::Let,
        self.strict,
        self.this,
      )?;
    }

    if let Some(FuncBody::Block(stmts)) = &func.stx.body {
      self.instantiate_stmt_list(scope, stmts)?;
    }
    Ok(())
  }

  fn instantiate_stmt_list(
    &mut self,
    scope: &mut Scope<'_>,
    stmts: &[Node<Stmt>],
  ) -> Result<(), VmError> {
    if self.strict {
      for stmt in stmts {
        self.strict_mode_with_early_error(stmt)?;
      }
    }

    // Minimal early error checks:
    // - Duplicate lexical declarations (let/const) in the same statement list.
    // - Lexical declarations may not collide with var-scoped names (var + function declarations).
    let mut var_names = HashSet::<String>::new();
    for stmt in stmts {
      self.collect_var_names(&stmt.stx, &mut var_names)?;
    }

    if self.strict {
      // Strict mode: only top-level function declarations are var-scoped; block function
      // declarations are instantiated at block entry.
      for stmt in stmts {
        self.tick()?;
        let Stmt::FunctionDecl(decl) = &*stmt.stx else {
          continue;
        };
        let Some(name) = &decl.stx.name else {
          return Err(VmError::Unimplemented("anonymous function declaration"));
        };
        var_names.insert(name.stx.name.clone());
      }
    } else {
      // Non-strict mode: treat block function declarations as var-scoped (Annex B-ish).
      for stmt in stmts {
        self.collect_sloppy_function_decl_names(&stmt.stx, &mut var_names)?;
      }
    }

    let mut lexical_seen = HashSet::<String>::new();
    let mut lexical_bindings: Vec<(String, parse_js::loc::Loc)> = Vec::new();
    for stmt in stmts {
      self.tick()?;
      match &*stmt.stx {
        Stmt::VarDecl(var) => {
          if var.stx.mode != VarDeclMode::Let && var.stx.mode != VarDeclMode::Const {
            continue;
          }
          for declarator in &var.stx.declarators {
            self.tick()?;
            if var.stx.mode == VarDeclMode::Const && declarator.initializer.is_none() {
              return Err(syntax_error(
                declarator.pattern.loc,
                "Missing initializer in const declaration",
              ));
            }
            self.collect_lexical_decl_names_from_pat(
              &declarator.pattern.stx.pat.stx,
              stmt.loc,
              &mut lexical_seen,
              &mut lexical_bindings,
            )?;
          }
        }
        Stmt::ClassDecl(class) => {
          let Some(name) = class.stx.name.as_ref() else {
            continue;
          };
          if !lexical_seen.insert(name.stx.name.clone()) {
            return Err(syntax_error(
              stmt.loc,
              format!("Identifier '{}' has already been declared", name.stx.name),
            ));
          }
          lexical_bindings.push((name.stx.name.clone(), stmt.loc));
        }
        _ => {}
      }
    }

    for (name, loc) in &lexical_bindings {
      // Budget the lexical-vs-var collision check: a statement list can contain very large numbers
      // of lexical bindings (e.g. `let a0,a1,...`) and we must still observe budgets/interrupts
      // while checking each name against the var-scoped set.
      self.tick()?;
      if var_names.contains(name) {
        return Err(syntax_error(
          *loc,
          format!("Identifier '{name}' has already been declared"),
        ));
      }
    }

    self.instantiate_var_decls(scope, stmts)?;
    let lex = self.env.lexical_env;
    self.instantiate_lexical_decls_in_stmt_list(scope, lex, stmts)?;
    self.instantiate_var_scoped_function_decls_in_stmt_list(scope, stmts)?;
    Ok(())
  }

  fn strict_mode_with_early_error(&mut self, stmt: &Node<Stmt>) -> Result<(), VmError> {
    // `with` is a strict mode early error (ECMA-262 14.11.1 Static Semantics: Early Errors).
    //
    // We run this check during instantiation so strict-mode failures do not partially hoist `var`
    // bindings before throwing.
    self.tick()?;

    match &*stmt.stx {
      Stmt::With(_) => Err(syntax_error(
        stmt.loc,
        "with statements are not allowed in strict mode",
      )),
      Stmt::Block(block) => {
        for s in &block.stx.body {
          self.strict_mode_with_early_error(s)?;
        }
        Ok(())
      }
      Stmt::If(stmt) => {
        self.strict_mode_with_early_error(&stmt.stx.consequent)?;
        if let Some(alt) = &stmt.stx.alternate {
          self.strict_mode_with_early_error(alt)?;
        }
        Ok(())
      }
      Stmt::Try(stmt) => {
        for s in &stmt.stx.wrapped.stx.body {
          self.strict_mode_with_early_error(s)?;
        }
        if let Some(catch) = &stmt.stx.catch {
          for s in &catch.stx.body {
            self.strict_mode_with_early_error(s)?;
          }
        }
        if let Some(finally) = &stmt.stx.finally {
          for s in &finally.stx.body {
            self.strict_mode_with_early_error(s)?;
          }
        }
        Ok(())
      }
      Stmt::While(stmt) => self.strict_mode_with_early_error(&stmt.stx.body),
      Stmt::DoWhile(stmt) => self.strict_mode_with_early_error(&stmt.stx.body),
      Stmt::ForTriple(stmt) => {
        for s in &stmt.stx.body.stx.body {
          self.strict_mode_with_early_error(s)?;
        }
        Ok(())
      }
      Stmt::ForIn(stmt) => {
        for s in &stmt.stx.body.stx.body {
          self.strict_mode_with_early_error(s)?;
        }
        Ok(())
      }
      Stmt::ForOf(stmt) => {
        for s in &stmt.stx.body.stx.body {
          self.strict_mode_with_early_error(s)?;
        }
        Ok(())
      }
      Stmt::Label(stmt) => self.strict_mode_with_early_error(&stmt.stx.statement),
      Stmt::Switch(stmt) => {
        const BRANCH_TICK_EVERY: usize = 32;
        for (i, branch) in stmt.stx.branches.iter().enumerate() {
          if i % BRANCH_TICK_EVERY == 0 {
            self.tick()?;
          }
          for s in &branch.stx.body {
            self.strict_mode_with_early_error(s)?;
          }
        }
        Ok(())
      }
      _ => Ok(()),
    }
  }

  fn instantiate_var_decls(
    &mut self,
    scope: &mut Scope<'_>,
    stmts: &[Node<Stmt>],
  ) -> Result<(), VmError> {
    let mut names = HashSet::<String>::new();
    for stmt in stmts {
      self.collect_var_names(&stmt.stx, &mut names)?;
    }
    for name in names {
      self.tick()?;
      self.env.declare_var(self.vm, scope, &name)?;
    }
    Ok(())
  }

  fn instantiate_var_scoped_function_decls_in_stmt_list(
    &mut self,
    scope: &mut Scope<'_>,
    stmts: &[Node<Stmt>],
  ) -> Result<(), VmError> {
    if self.strict {
      for stmt in stmts {
        self.tick()?;
        let Stmt::FunctionDecl(decl) = &*stmt.stx else {
          continue;
        };
        self.instantiate_function_decl(scope, decl)?;
      }
      return Ok(());
    }

    for stmt in stmts {
      self.instantiate_var_scoped_function_decls_in_stmt(scope, &stmt.stx)?;
    }
    Ok(())
  }

  fn instantiate_var_scoped_function_decls_in_stmt(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &Stmt,
  ) -> Result<(), VmError> {
    self.tick()?;
    match stmt {
      Stmt::FunctionDecl(decl) => self.instantiate_function_decl(scope, decl),
      Stmt::Block(block) => {
        self.instantiate_var_scoped_function_decls_in_stmt_list(scope, &block.stx.body)
      }
      Stmt::If(stmt) => {
        self.instantiate_var_scoped_function_decls_in_stmt(scope, &stmt.stx.consequent.stx)?;
        if let Some(alt) = &stmt.stx.alternate {
          self.instantiate_var_scoped_function_decls_in_stmt(scope, &alt.stx)?;
        }
        Ok(())
      }
      Stmt::Try(stmt) => {
        self
          .instantiate_var_scoped_function_decls_in_stmt_list(scope, &stmt.stx.wrapped.stx.body)?;
        if let Some(catch) = &stmt.stx.catch {
          self.instantiate_var_scoped_function_decls_in_stmt_list(scope, &catch.stx.body)?;
        }
        if let Some(finally) = &stmt.stx.finally {
          self.instantiate_var_scoped_function_decls_in_stmt_list(scope, &finally.stx.body)?;
        }
        Ok(())
      }
      Stmt::With(stmt) => {
        self.instantiate_var_scoped_function_decls_in_stmt(scope, &stmt.stx.body.stx)
      }
      Stmt::While(stmt) => {
        self.instantiate_var_scoped_function_decls_in_stmt(scope, &stmt.stx.body.stx)
      }
      Stmt::DoWhile(stmt) => {
        self.instantiate_var_scoped_function_decls_in_stmt(scope, &stmt.stx.body.stx)
      }
      Stmt::ForTriple(stmt) => {
        self.instantiate_var_scoped_function_decls_in_stmt_list(scope, &stmt.stx.body.stx.body)
      }
      Stmt::ForIn(stmt) => {
        self.instantiate_var_scoped_function_decls_in_stmt_list(scope, &stmt.stx.body.stx.body)
      }
      Stmt::ForOf(stmt) => {
        self.instantiate_var_scoped_function_decls_in_stmt_list(scope, &stmt.stx.body.stx.body)
      }
      Stmt::Label(stmt) => {
        self.instantiate_var_scoped_function_decls_in_stmt(scope, &stmt.stx.statement.stx)
      }
      Stmt::Switch(stmt) => {
        const BRANCH_TICK_EVERY: usize = 32;
        for (i, branch) in stmt.stx.branches.iter().enumerate() {
          if i % BRANCH_TICK_EVERY == 0 {
            self.tick()?;
          }
          self.instantiate_var_scoped_function_decls_in_stmt_list(scope, &branch.stx.body)?;
        }
        Ok(())
      }
      _ => Ok(()),
    }
  }

  fn collect_sloppy_function_decl_names(
    &mut self,
    stmt: &Stmt,
    out: &mut HashSet<String>,
  ) -> Result<(), VmError> {
    self.tick()?;
    match stmt {
      Stmt::FunctionDecl(decl) => {
        let Some(name) = &decl.stx.name else {
          return Err(VmError::Unimplemented("anonymous function declaration"));
        };
        out.insert(name.stx.name.clone());
        Ok(())
      }
      Stmt::Block(block) => {
        for stmt in &block.stx.body {
          self.collect_sloppy_function_decl_names(&stmt.stx, out)?;
        }
        Ok(())
      }
      Stmt::If(stmt) => {
        self.collect_sloppy_function_decl_names(&stmt.stx.consequent.stx, out)?;
        if let Some(alt) = &stmt.stx.alternate {
          self.collect_sloppy_function_decl_names(&alt.stx, out)?;
        }
        Ok(())
      }
      Stmt::Try(stmt) => {
        for s in &stmt.stx.wrapped.stx.body {
          self.collect_sloppy_function_decl_names(&s.stx, out)?;
        }
        if let Some(catch) = &stmt.stx.catch {
          for s in &catch.stx.body {
            self.collect_sloppy_function_decl_names(&s.stx, out)?;
          }
        }
        if let Some(finally) = &stmt.stx.finally {
          for s in &finally.stx.body {
            self.collect_sloppy_function_decl_names(&s.stx, out)?;
          }
        }
        Ok(())
      }
      Stmt::With(stmt) => self.collect_sloppy_function_decl_names(&stmt.stx.body.stx, out),
      Stmt::While(stmt) => self.collect_sloppy_function_decl_names(&stmt.stx.body.stx, out),
      Stmt::DoWhile(stmt) => self.collect_sloppy_function_decl_names(&stmt.stx.body.stx, out),
      Stmt::ForTriple(stmt) => {
        for s in &stmt.stx.body.stx.body {
          self.collect_sloppy_function_decl_names(&s.stx, out)?;
        }
        Ok(())
      }
      Stmt::ForIn(stmt) => {
        for s in &stmt.stx.body.stx.body {
          self.collect_sloppy_function_decl_names(&s.stx, out)?;
        }
        Ok(())
      }
      Stmt::ForOf(stmt) => {
        for s in &stmt.stx.body.stx.body {
          self.collect_sloppy_function_decl_names(&s.stx, out)?;
        }
        Ok(())
      }
      Stmt::Label(stmt) => self.collect_sloppy_function_decl_names(&stmt.stx.statement.stx, out),
      Stmt::Switch(stmt) => {
        const BRANCH_TICK_EVERY: usize = 32;
        for (i, branch) in stmt.stx.branches.iter().enumerate() {
          if i % BRANCH_TICK_EVERY == 0 {
            self.tick()?;
          }
          for s in &branch.stx.body {
            self.collect_sloppy_function_decl_names(&s.stx, out)?;
          }
        }
        Ok(())
      }
      _ => Ok(()),
    }
  }

  fn collect_lexical_decl_names_from_pat(
    &mut self,
    pat: &Pat,
    loc: parse_js::loc::Loc,
    seen: &mut HashSet<String>,
    out: &mut Vec<(String, parse_js::loc::Loc)>,
  ) -> Result<(), VmError> {
    match pat {
      Pat::Id(id) => {
        if !seen.insert(id.stx.name.clone()) {
          return Err(syntax_error(
            loc,
            format!("Identifier '{}' has already been declared", id.stx.name),
          ));
        }
        out.push((id.stx.name.clone(), loc));
        Ok(())
      }
      Pat::Obj(obj) => {
        for prop in &obj.stx.properties {
          self.tick()?;
          self.collect_lexical_decl_names_from_pat(&prop.stx.target.stx, loc, seen, out)?;
        }
        if let Some(rest) = &obj.stx.rest {
          self.tick()?;
          self.collect_lexical_decl_names_from_pat(&rest.stx, loc, seen, out)?;
        }
        Ok(())
      }
      Pat::Arr(arr) => {
        for elem in &arr.stx.elements {
          self.tick()?;
          if let Some(elem) = elem {
            self.collect_lexical_decl_names_from_pat(&elem.target.stx, loc, seen, out)?;
          }
        }
        if let Some(rest) = &arr.stx.rest {
          self.tick()?;
          self.collect_lexical_decl_names_from_pat(&rest.stx, loc, seen, out)?;
        }
        Ok(())
      }
      Pat::AssignTarget(_) => Err(VmError::Unimplemented(
        "lexical declaration assignment targets",
      )),
    }
  }

  fn instantiate_function_decl(
    &mut self,
    scope: &mut Scope<'_>,
    decl: &Node<FuncDecl>,
  ) -> Result<(), VmError> {
    let Some(name) = &decl.stx.name else {
      return Err(VmError::Unimplemented("anonymous function declaration"));
    };

    let func_obj = self.create_function_object_for_decl(scope, decl, &name.stx.name)?;

    let mut assign_scope = scope.reborrow();
    assign_scope.push_root(Value::Object(func_obj))?;
    let (vm, env) = (&mut self.vm, &mut self.env);
    env.set_var(
      vm,
      &mut *self.host,
      &mut *self.hooks,
      &mut assign_scope,
      &name.stx.name,
      Value::Object(func_obj),
    )?;
    Ok(())
  }

  fn create_function_object_for_decl(
    &mut self,
    scope: &mut Scope<'_>,
    decl: &Node<FuncDecl>,
    name: &str,
  ) -> Result<GcObject, VmError> {
    use crate::function::ThisMode;
    use crate::vm::EcmaFunctionKind;

    let func = &decl.stx.function.stx;
    if func.generator {
      return Err(VmError::Unimplemented(if func.async_ {
        "async generator functions"
      } else {
        "generator functions"
      }));
    }
    let is_strict = self.strict
      || match &func.body {
        Some(FuncBody::Block(stmts)) => detect_use_strict_directive(stmts, || self.tick())?,
        Some(FuncBody::Expression(_)) => false,
        None => return Err(VmError::Unimplemented("function without body")),
      };

    let this_mode = if func.arrow {
      ThisMode::Lexical
    } else if is_strict {
      ThisMode::Strict
    } else {
      ThisMode::Global
    };

    let name_s = scope.alloc_string(name)?;
    let length = self.function_length(func)?;

    let rel_start = decl.loc.start_u32().saturating_sub(self.env.prefix_len());
    let rel_end = decl.loc.end_u32().saturating_sub(self.env.prefix_len());
    let span_start = self.env.base_offset().saturating_add(rel_start);
    let span_end = self.env.base_offset().saturating_add(rel_end);

    let code_id = self.vm.register_ecma_function(
      self.env.source(),
      span_start,
      span_end,
      EcmaFunctionKind::Decl,
    )?;
    let func_obj = scope.alloc_ecma_function(
      code_id,
      /* is_constructable */ !func.async_,
      name_s,
      length,
      this_mode,
      is_strict,
      Some(self.env.lexical_env),
    )?;
    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    scope
      .heap_mut()
      .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
    scope
      .heap_mut()
      .set_function_realm(func_obj, self.env.global_object())?;
    if let Some(realm) = self.vm.current_realm() {
      scope.heap_mut().set_function_job_realm(func_obj, realm)?;
    }
    Ok(func_obj)
  }

  fn create_class_constructor_object(
    &mut self,
    scope: &mut Scope<'_>,
    name: &str,
    length: u32,
    constructor_body: Option<GcObject>,
  ) -> Result<GcObject, VmError> {
    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

    // Root the optional constructor body function before allocating any strings/function objects in
    // case those allocations trigger GC.
    let mut init_scope = scope.reborrow();
    if let Some(body) = constructor_body {
      init_scope.push_root(Value::Object(body))?;
    }

    let name_s = init_scope.alloc_string(name)?;
    let slots_buf;
    let slots = if let Some(body) = constructor_body {
      slots_buf = [Value::Object(body)];
      &slots_buf[..]
    } else {
      &[][..]
    };

    let func_obj = init_scope.alloc_native_function_with_slots(
      intr.class_constructor_call(),
      Some(intr.class_constructor_construct()),
      name_s,
      length,
      slots,
    )?;
    init_scope.push_root(Value::Object(func_obj))?;

    init_scope
      .heap_mut()
      .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
    init_scope
      .heap_mut()
      .set_function_realm(func_obj, self.env.global_object())?;
    if let Some(realm) = self.vm.current_realm() {
      init_scope
        .heap_mut()
        .set_function_job_realm(func_obj, realm)?;
    }
    Ok(func_obj)
  }

  fn instantiate_block_decls_in_stmt_list(
    &mut self,
    scope: &mut Scope<'_>,
    env: GcEnv,
    stmts: &[Node<Stmt>],
  ) -> Result<(), VmError> {
    // Duplicate block-scoped declarations are early errors.
    //
    // Note: switch/catch share this helper even though their scope isn't a syntactic `{}` block.
    let mut seen = HashSet::<String>::new();
    for stmt in stmts {
      // Budget statement-list traversal even when it contains only function declarations (which are
      // instantiated in a later pass). Without this, a block containing many declarations could do
      // large `O(N)` early-error work without any budget checks.
      self.tick()?;
      match &*stmt.stx {
        Stmt::VarDecl(var)
          if var.stx.mode == VarDeclMode::Let || var.stx.mode == VarDeclMode::Const =>
        {
          for declarator in &var.stx.declarators {
            self.tick()?;
            if var.stx.mode == VarDeclMode::Const && declarator.initializer.is_none() {
              return Err(syntax_error(
                declarator.pattern.loc,
                "Missing initializer in const declaration",
              ));
            }
            // Reuse lexical declaration collection logic to detect duplicates across complex patterns.
            let mut tmp = Vec::new();
            self.collect_lexical_decl_names_from_pat(
              &declarator.pattern.stx.pat.stx,
              stmt.loc,
              &mut seen,
              &mut tmp,
            )?;
          }
        }
        Stmt::ClassDecl(decl) => {
          let Some(name) = decl.stx.name.as_ref() else {
            continue;
          };
          if !seen.insert(name.stx.name.clone()) {
            return Err(syntax_error(
              stmt.loc,
              format!("Identifier '{}' has already been declared", name.stx.name),
            ));
          }
        }
        Stmt::FunctionDecl(decl) if self.strict => {
          let Some(name) = &decl.stx.name else {
            return Err(VmError::Unimplemented("anonymous function declaration"));
          };
          if !seen.insert(name.stx.name.clone()) {
            return Err(syntax_error(
              stmt.loc,
              format!("Identifier '{}' has already been declared", name.stx.name),
            ));
          }
        }
        _ => {}
      }
    }

    self.instantiate_lexical_decls_in_stmt_list(scope, env, stmts)?;
    if self.strict {
      self.instantiate_block_scoped_function_decls_in_stmt_list(scope, env, stmts)?;
    }
    Ok(())
  }

  fn instantiate_block_scoped_function_decls_in_stmt_list(
    &mut self,
    scope: &mut Scope<'_>,
    env: GcEnv,
    stmts: &[Node<Stmt>],
  ) -> Result<(), VmError> {
    for stmt in stmts {
      // Tick per statement list entry so large blocks of function declarations cannot be
      // instantiated without consuming fuel (and so we still check termination conditions even if
      // a prior pass used up all remaining fuel).
      self.tick()?;
      let Stmt::FunctionDecl(decl) = &*stmt.stx else {
        continue;
      };
      self.instantiate_block_scoped_function_decl(scope, env, decl)?;
    }
    Ok(())
  }

  fn instantiate_block_scoped_function_decl(
    &mut self,
    scope: &mut Scope<'_>,
    env: GcEnv,
    decl: &Node<FuncDecl>,
  ) -> Result<(), VmError> {
    let Some(name) = &decl.stx.name else {
      return Err(VmError::Unimplemented("anonymous function declaration"));
    };

    // Block-scoped functions are lexically scoped in strict mode.
    if scope.heap().env_has_binding(env, &name.stx.name)? {
      return Err(syntax_error(
        decl.loc,
        format!("Identifier '{}' has already been declared", name.stx.name),
      ));
    }

    scope.env_create_mutable_binding(env, &name.stx.name)?;

    let func_obj = self.create_function_object_for_decl(scope, decl, &name.stx.name)?;

    let mut init_scope = scope.reborrow();
    init_scope.push_root(Value::Object(func_obj))?;
    init_scope
      .heap_mut()
      .env_initialize_binding(env, &name.stx.name, Value::Object(func_obj))?;
    Ok(())
  }

  fn instantiate_lexical_decls_in_stmt_list(
    &mut self,
    scope: &mut Scope<'_>,
    env: GcEnv,
    stmts: &[Node<Stmt>],
  ) -> Result<(), VmError> {
    for stmt in stmts {
      self.tick()?;
      match &*stmt.stx {
        Stmt::VarDecl(var) => match var.stx.mode {
          VarDeclMode::Let => {
            for declarator in &var.stx.declarators {
              self.tick()?;
              self.instantiate_lexical_names_from_pat(
                scope,
                env,
                &declarator.pattern.stx.pat.stx,
                stmt.loc,
                true,
              )?;
            }
          }
          VarDeclMode::Const => {
            for declarator in &var.stx.declarators {
              self.tick()?;
              self.instantiate_lexical_names_from_pat(
                scope,
                env,
                &declarator.pattern.stx.pat.stx,
                stmt.loc,
                false,
              )?;
            }
          }
          _ => {}
        },
        Stmt::ClassDecl(class) => {
          let Some(name) = class.stx.name.as_ref() else {
            continue;
          };

          if scope.heap().env_has_binding(env, &name.stx.name)? {
            return Err(syntax_error(
              stmt.loc,
              format!("Identifier '{}' has already been declared", name.stx.name),
            ));
          }
          scope.env_create_mutable_binding(env, &name.stx.name)?;
        }
        _ => {}
      }
    }
    Ok(())
  }

  fn instantiate_lexical_names_from_pat(
    &mut self,
    scope: &mut Scope<'_>,
    env: GcEnv,
    pat: &Pat,
    loc: parse_js::loc::Loc,
    mutable: bool,
  ) -> Result<(), VmError> {
    match pat {
      Pat::Id(id) => {
        if scope.heap().env_has_binding(env, &id.stx.name)? {
          return Err(syntax_error(
            loc,
            format!("Identifier '{}' has already been declared", id.stx.name),
          ));
        }
        if mutable {
          scope.env_create_mutable_binding(env, &id.stx.name)?;
        } else {
          scope.env_create_immutable_binding(env, &id.stx.name)?;
        }
        Ok(())
      }
      Pat::Obj(obj) => {
        for prop in &obj.stx.properties {
          self.tick()?;
          self.instantiate_lexical_names_from_pat(
            scope,
            env,
            &prop.stx.target.stx,
            loc,
            mutable,
          )?;
        }
        if let Some(rest) = &obj.stx.rest {
          self.tick()?;
          self.instantiate_lexical_names_from_pat(scope, env, &rest.stx, loc, mutable)?;
        }
        Ok(())
      }
      Pat::Arr(arr) => {
        for elem in &arr.stx.elements {
          self.tick()?;
          if let Some(elem) = elem {
            self.instantiate_lexical_names_from_pat(scope, env, &elem.target.stx, loc, mutable)?;
          }
        }
        if let Some(rest) = &arr.stx.rest {
          self.tick()?;
          self.instantiate_lexical_names_from_pat(scope, env, &rest.stx, loc, mutable)?;
        }
        Ok(())
      }
      Pat::AssignTarget(_) => Err(VmError::Unimplemented(
        "lexical declaration assignment targets",
      )),
    }
  }

  fn collect_var_names(&mut self, stmt: &Stmt, out: &mut HashSet<String>) -> Result<(), VmError> {
    // `VarDeclaredNames` can traverse large statement trees (e.g. nested blocks/ifs with no `var`
    // declarations). Budget it so strict-mode scripts can't bypass fuel/interrupt checks during
    // hoisting by forcing an `O(N)` scan that performs no statement/expression evaluation.
    self.tick()?;
    match stmt {
      Stmt::VarDecl(var) => {
        if var.stx.mode != VarDeclMode::Var {
          return Ok(());
        }
        for decl in &var.stx.declarators {
          self.tick()?;
          self.collect_var_names_from_pat_decl(&decl.pattern.stx, out)?;
        }
      }
      Stmt::Block(block) => {
        for stmt in &block.stx.body {
          self.collect_var_names(&stmt.stx, out)?;
        }
      }
      Stmt::If(stmt) => {
        self.collect_var_names(&stmt.stx.consequent.stx, out)?;
        if let Some(alt) = &stmt.stx.alternate {
          self.collect_var_names(&alt.stx, out)?;
        }
      }
      Stmt::Try(stmt) => {
        for s in &stmt.stx.wrapped.stx.body {
          self.collect_var_names(&s.stx, out)?;
        }
        if let Some(catch) = &stmt.stx.catch {
          for s in &catch.stx.body {
            self.collect_var_names(&s.stx, out)?;
          }
        }
        if let Some(finally) = &stmt.stx.finally {
          for s in &finally.stx.body {
            self.collect_var_names(&s.stx, out)?;
          }
        }
      }
      Stmt::With(stmt) => {
        self.collect_var_names(&stmt.stx.body.stx, out)?;
      }
      Stmt::While(stmt) => {
        self.collect_var_names(&stmt.stx.body.stx, out)?;
      }
      Stmt::DoWhile(stmt) => {
        self.collect_var_names(&stmt.stx.body.stx, out)?;
      }
      Stmt::ForTriple(stmt) => {
        if let parse_js::ast::stmt::ForTripleStmtInit::Decl(decl) = &stmt.stx.init {
          if decl.stx.mode == VarDeclMode::Var {
            for d in &decl.stx.declarators {
              self.tick()?;
              self.collect_var_names_from_pat_decl(&d.pattern.stx, out)?;
            }
          }
        }
        for s in &stmt.stx.body.stx.body {
          self.collect_var_names(&s.stx, out)?;
        }
      }
      Stmt::ForIn(stmt) => {
        if let ForInOfLhs::Decl((mode, pat_decl)) = &stmt.stx.lhs {
          if *mode == VarDeclMode::Var {
            self.collect_var_names_from_pat_decl(&pat_decl.stx, out)?;
          }
        }
        for s in &stmt.stx.body.stx.body {
          self.collect_var_names(&s.stx, out)?;
        }
      }
      Stmt::ForOf(stmt) => {
        if let ForInOfLhs::Decl((mode, pat_decl)) = &stmt.stx.lhs {
          if *mode == VarDeclMode::Var {
            self.collect_var_names_from_pat_decl(&pat_decl.stx, out)?;
          }
        }
        for s in &stmt.stx.body.stx.body {
          self.collect_var_names(&s.stx, out)?;
        }
      }
      Stmt::Label(stmt) => {
        self.collect_var_names(&stmt.stx.statement.stx, out)?;
      }
      Stmt::Switch(stmt) => {
        const BRANCH_TICK_EVERY: usize = 32;
        for (i, branch) in stmt.stx.branches.iter().enumerate() {
          if i % BRANCH_TICK_EVERY == 0 {
            self.tick()?;
          }
          for s in &branch.stx.body {
            self.collect_var_names(&s.stx, out)?;
          }
        }
      }
      _ => {}
    }
    Ok(())
  }

  fn collect_var_names_from_pat_decl(
    &mut self,
    pat_decl: &PatDecl,
    out: &mut HashSet<String>,
  ) -> Result<(), VmError> {
    self.collect_var_names_from_pat(&pat_decl.pat.stx, out)
  }

  fn collect_var_names_from_pat(
    &mut self,
    pat: &Pat,
    out: &mut HashSet<String>,
  ) -> Result<(), VmError> {
    match pat {
      Pat::Id(id) => {
        out.insert(id.stx.name.clone());
        Ok(())
      }
      Pat::Obj(obj) => {
        for prop in &obj.stx.properties {
          self.tick()?;
          self.collect_var_names_from_pat(&prop.stx.target.stx, out)?;
        }
        if let Some(rest) = &obj.stx.rest {
          self.tick()?;
          self.collect_var_names_from_pat(&rest.stx, out)?;
        }
        Ok(())
      }
      Pat::Arr(arr) => {
        for elem in &arr.stx.elements {
          self.tick()?;
          if let Some(elem) = elem {
            self.collect_var_names_from_pat(&elem.target.stx, out)?;
          }
        }
        if let Some(rest) = &arr.stx.rest {
          self.tick()?;
          self.collect_var_names_from_pat(&rest.stx, out)?;
        }
        Ok(())
      }
      Pat::AssignTarget(_) => Err(VmError::Unimplemented("var declaration assignment targets")),
    }
  }

  fn eval_stmt_list(
    &mut self,
    scope: &mut Scope<'_>,
    stmts: &[Node<Stmt>],
  ) -> Result<Completion, VmError> {
    // Root the running completion value so it cannot be collected while evaluating subsequent
    // statements (which may allocate and trigger GC).
    let last_root = scope.heap_mut().add_root(Value::Undefined)?;
    let mut last_value: Option<Value> = None;

    for stmt in stmts {
      let completion = self.eval_stmt(scope, stmt)?;
      let completion = completion.update_empty(last_value);

      match completion {
        Completion::Normal(v) => {
          if let Some(v) = v {
            last_value = Some(v);
            scope.heap_mut().set_root(last_root, v);
          }
        }
        abrupt => {
          scope.heap_mut().remove_root(last_root);
          return Ok(abrupt);
        }
      }
    }

    scope.heap_mut().remove_root(last_root);
    Ok(Completion::Normal(last_value))
  }

  fn eval_block_stmt(
    &mut self,
    scope: &mut Scope<'_>,
    block: &BlockStmt,
  ) -> Result<Completion, VmError> {
    if block.body.is_empty() {
      return Ok(Completion::empty());
    }

    // If the block declares no lexical bindings, evaluating the statement list in the existing
    // lexical environment is equivalent to creating a fresh empty environment.
    //
    // This avoids allocating an empty `EnvRecord` for blocks like `{ x++; }` that are executed in
    // tight loops, keeping fuel-based termination responsive for hostile input.
    let needs_lexical_env = block.body.iter().any(|stmt| match &*stmt.stx {
      Stmt::VarDecl(var) if matches!(var.stx.mode, VarDeclMode::Let | VarDeclMode::Const) => true,
      Stmt::ClassDecl(_) => true,
      Stmt::FunctionDecl(_) => self.strict,
      _ => false,
    });
    if !needs_lexical_env {
      return self.eval_stmt_list(scope, &block.body);
    }

    let outer = self.env.lexical_env;
    let block_env = scope.env_create(Some(outer))?;
    self.env.set_lexical_env(scope.heap_mut(), block_env);

    let result = self
      .instantiate_block_decls_in_stmt_list(scope, block_env, &block.body)
      .and_then(|_| self.eval_stmt_list(scope, &block.body));

    self.env.set_lexical_env(scope.heap_mut(), outer);
    result
  }

  fn eval_stmt(&mut self, scope: &mut Scope<'_>, stmt: &Node<Stmt>) -> Result<Completion, VmError> {
    self.eval_stmt_labelled(scope, stmt, &[])
  }

  /// Evaluates a statement with an associated label set.
  ///
  /// This models ECMA-262 `LabelledEvaluation` / `LoopEvaluation` label propagation:
  /// nested label statements extend `label_set`, and iteration statements use it to determine which
  /// labelled `continue` completions are consumed by the loop.
  fn eval_stmt_labelled(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &Node<Stmt>,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    // One tick per statement.
    self.tick()?;

    let res = match &*stmt.stx {
      Stmt::Empty(_) => Ok(Completion::empty()),
      Stmt::Expr(expr_stmt) => self.eval_expr_stmt(scope, &expr_stmt.stx),
      Stmt::VarDecl(var_decl) => self.eval_var_decl(scope, &var_decl.stx),
      Stmt::ClassDecl(class_decl) => self.eval_class_decl(scope, class_decl),
      Stmt::Debugger(_) => Ok(Completion::empty()),
      Stmt::Block(block) => self.eval_block_stmt(scope, &block.stx),
      Stmt::If(stmt) => self.eval_if(scope, &stmt.stx),
      // Import/export declarations are processed during module linking; their runtime evaluation is
      // defined to produce an empty completion.
      Stmt::Import(_) => Ok(Completion::empty()),
      Stmt::ExportList(_) => Ok(Completion::empty()),
      Stmt::ExportDefaultExpr(stmt) => {
        let value = self.eval_expr(scope, &stmt.stx.expression)?;
        let binding_name = "*default*";
        if !scope
          .heap()
          .env_has_binding(self.env.lexical_env, binding_name)?
        {
          return Err(VmError::InvariantViolation(
            "export default expression missing *default* binding",
          ));
        }
        scope
          .heap_mut()
          .env_initialize_binding(self.env.lexical_env, binding_name, value)?;
        Ok(Completion::empty())
      }
      Stmt::Throw(stmt) => self.eval_throw(scope, stmt),
      Stmt::Try(stmt) => self.eval_try(scope, &stmt.stx),
      Stmt::With(stmt) => self.eval_with(scope, &stmt.stx, label_set),
      Stmt::Return(stmt) => self.eval_return(scope, &stmt.stx),
      Stmt::While(stmt) => self.eval_while(scope, &stmt.stx, label_set),
      Stmt::DoWhile(stmt) => self.eval_do_while(scope, &stmt.stx, label_set),
      Stmt::ForTriple(stmt) => self.eval_for_triple(scope, &stmt.stx, label_set),
      Stmt::ForIn(stmt) => self.eval_for_in(scope, &stmt.stx, label_set),
      Stmt::ForOf(stmt) => self.eval_for_of(scope, &stmt.stx, label_set),
      Stmt::Switch(stmt) => {
        let result = self.eval_switch(scope, &stmt.stx)?;
        Ok(Self::normalise_iteration_break(result))
      }
      Stmt::Label(stmt) => self.eval_label(scope, &stmt.stx, label_set),
      // Function declarations are instantiated during hoisting.
      Stmt::FunctionDecl(_) => Ok(Completion::empty()),
      Stmt::Break(stmt) => Ok(Completion::Break(stmt.stx.label.clone(), None)),
      Stmt::Continue(stmt) => Ok(Completion::Continue(stmt.stx.label.clone(), None)),

      _ => Err(VmError::Unimplemented("statement type")),
    };

    // Treat internal `VmError::Throw*` as a JS throw completion so it is catchable by `try/catch`.
    //
    // This is also the central stack capture point for implicit throws (TDZ errors, TypeErrors,
    // etc) that are surfaced as `Err(VmError::Throw(..))` from lower-level helpers.
    //
    // Note: some callers (e.g. `Vm::call` after `coerce_error_to_throw`) can surface a
    // `VmError::ThrowWithStack` before we reach this point; in that case we still want the top
    // frame to point at the statement location, not at the internal/native frame boundary.
    let source = self.env.source();
    let rel_start = stmt.loc.start_u32().saturating_sub(self.env.prefix_len());
    let abs_offset = self.env.base_offset().saturating_add(rel_start);
    let (line, col) = source.line_col(abs_offset);

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

    let thrown_at_stmt = |value: Value| {
      let mut stack = self.vm.capture_stack();
      update_top_frame(&mut stack);
      Thrown { value, stack }
    };

    match res {
      Err(VmError::Throw(value)) => Ok(Completion::Throw(thrown_at_stmt(value))),
      Err(VmError::ThrowWithStack { value, mut stack }) => {
        // If the stack trace was captured while executing a native builtin, the top frame's
        // location will be `<native>:0:0`. Patch that frame to the current statement location so
        // callers see where the exception was triggered in user code.
        //
        // For user-thrown exceptions, the captured stack already contains meaningful source
        // locations and should not be overwritten.
        if stack.first().is_none() || stack.first().is_some_and(|top| top.line == 0) {
          update_top_frame(&mut stack);
        }
        Ok(Completion::Throw(Thrown { value, stack }))
      }
      Err(VmError::TypeError(message)) => {
        let intr = self
          .vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let err = new_error(scope, intr.type_error_prototype(), "TypeError", message)?;
        Ok(Completion::Throw(thrown_at_stmt(err)))
      }
      Err(VmError::PrototypeCycle) => {
        let intr = self
          .vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let err = new_error(
          scope,
          intr.type_error_prototype(),
          "TypeError",
          "prototype cycle",
        )?;
        Ok(Completion::Throw(thrown_at_stmt(err)))
      }
      Err(VmError::PrototypeChainTooDeep) => {
        let intr = self
          .vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let err = new_error(
          scope,
          intr.type_error_prototype(),
          "TypeError",
          "prototype chain too deep",
        )?;
        Ok(Completion::Throw(thrown_at_stmt(err)))
      }
      Err(VmError::InvalidPropertyDescriptorPatch) => {
        let intr = self
          .vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let err = new_error(
          scope,
          intr.type_error_prototype(),
          "TypeError",
          "invalid property descriptor patch: cannot mix data and accessor fields",
        )?;
        Ok(Completion::Throw(thrown_at_stmt(err)))
      }
      Err(VmError::NotCallable) => {
        let intr = self
          .vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let err = new_error(
          scope,
          intr.type_error_prototype(),
          "TypeError",
          "value is not callable",
        )?;
        Ok(Completion::Throw(thrown_at_stmt(err)))
      }
      Err(VmError::NotConstructable) => {
        let intr = self
          .vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let err = new_error(
          scope,
          intr.type_error_prototype(),
          "TypeError",
          "value is not a constructor",
        )?;
        Ok(Completion::Throw(thrown_at_stmt(err)))
      }
      other => other,
    }
  }

  fn eval_expr_stmt(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &ExprStmt,
  ) -> Result<Completion, VmError> {
    let value = self.eval_expr(scope, &stmt.expr)?;
    Ok(Completion::normal(value))
  }

  fn eval_var_decl(
    &mut self,
    scope: &mut Scope<'_>,
    decl: &VarDecl,
  ) -> Result<Completion, VmError> {
    match decl.mode {
      VarDeclMode::Var => {
        // `var` bindings are hoisted to `undefined` at function/script entry.
        for declarator in &decl.declarators {
          let Some(init) = &declarator.initializer else {
            self.tick()?;
            // Destructuring declarations require an initializer (early error in real JS).
            if !matches!(&*declarator.pattern.stx.pat.stx, Pat::Id(_)) {
              return Err(VmError::Unimplemented(
                "destructuring var without initializer",
              ));
            }
            continue;
          };
          let value = self.eval_expr(scope, init)?;
          bind_pattern(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            scope,
            self.env,
            &declarator.pattern.stx.pat.stx,
            value,
            BindingKind::Var,
            self.strict,
            self.this,
          )?;
        }
        Ok(Completion::empty())
      }
      VarDeclMode::Let => {
        for declarator in &decl.declarators {
          let value = match &declarator.initializer {
            Some(init) => self.eval_expr(scope, init)?,
            None => {
              self.tick()?;
              // Destructuring declarations require an initializer (early error in real JS).
              if !matches!(&*declarator.pattern.stx.pat.stx, Pat::Id(_)) {
                return Err(VmError::Unimplemented(
                  "destructuring let without initializer",
                ));
              }
              Value::Undefined
            }
          };

          bind_pattern(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            scope,
            self.env,
            &declarator.pattern.stx.pat.stx,
            value,
            BindingKind::Let,
            self.strict,
            self.this,
          )?;
        }
        Ok(Completion::empty())
      }
      VarDeclMode::Const => {
        for declarator in &decl.declarators {
          let Some(init) = &declarator.initializer else {
            return Err(syntax_error(
              declarator.pattern.loc,
              "Missing initializer in const declaration",
            ));
          };
          let value = self.eval_expr(scope, init)?;

          bind_pattern(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            scope,
            self.env,
            &declarator.pattern.stx.pat.stx,
            value,
            BindingKind::Const,
            self.strict,
            self.this,
          )?;
        }
        Ok(Completion::empty())
      }

      _ => Err(VmError::Unimplemented("var declaration kind")),
    }
  }

  fn eval_class(
    &mut self,
    scope: &mut Scope<'_>,
    binding: ClassBinding<'_>,
    func_name: &str,
    members: &[Node<ClassMember>],
  ) -> Result<GcObject, VmError> {
    let class_env = self.env.lexical_env;

    // Ensure the requested class binding exists before creating any class element closures that may
    // reference it.
    match binding {
      ClassBinding::None => {}
      ClassBinding::Mutable(name) => {
        if !scope.heap().env_has_binding(class_env, name)? {
          scope.env_create_mutable_binding(class_env, name)?;
        }
      }
      ClassBinding::Immutable(name) => {
        if scope.heap().env_has_binding(class_env, name)? {
          return Err(VmError::InvariantViolation(
            "class binding already exists in class environment",
          ));
        }
        scope.env_create_immutable_binding(class_env, name)?;
      }
    }

    // Find an explicit `constructor(...) { ... }` method, if present.
    let mut ctor_method: Option<(&Node<Func>, u32, parse_js::loc::Loc)> = None;
    for member in members {
      self.tick()?;
      if !member.stx.decorators.is_empty() {
        return Err(VmError::Unimplemented("class member decorators"));
      }
      if member.stx.declare || member.stx.abstract_ {
        return Err(VmError::Unimplemented("class member modifiers"));
      }
      if member.stx.readonly
        || member.stx.accessor
        || member.stx.optional
        || member.stx.override_
        || member.stx.definite_assignment
      {
        return Err(VmError::Unimplemented("class member modifiers"));
      }
      if member.stx.accessibility.is_some() || member.stx.type_annotation.is_some() {
        return Err(VmError::Unimplemented("class member type annotations"));
      }

      if member.stx.static_ {
        continue;
      }
      let ClassOrObjKey::Direct(direct) = &member.stx.key else {
        continue;
      };
      if direct.stx.key != "constructor" {
        continue;
      }

      let ClassOrObjVal::Method(method) = &member.stx.val else {
        continue;
      };

      if ctor_method.is_some() {
        return Err(syntax_error(member.loc, "A class may only have one constructor"));
      }
      ctor_method = Some((&method.stx.func, direct.loc.start_u32(), member.loc));
    }

    let mut ctor_length: u32 = 0;
    let ctor_body_func = if let Some((func_node, key_loc_start, loc)) = ctor_method {
      if func_node.stx.generator {
        return Err(syntax_error(loc, "Class constructor may not be a generator"));
      }

      ctor_length = self.function_length(&func_node.stx)?;

      let rel_start = key_loc_start.saturating_sub(self.env.prefix_len());
      let rel_end = func_node
        .loc
        .end_u32()
        .saturating_sub(self.env.prefix_len());
      let span_start = self.env.base_offset().saturating_add(rel_start);
      let span_end = self.env.base_offset().saturating_add(rel_end);

      let code = self.vm.register_ecma_function(
        self.env.source(),
        span_start,
        span_end,
        EcmaFunctionKind::ObjectMember,
      )?;

      // Class constructor bodies are always strict mode.
      let is_strict = true;
      let this_mode = if func_node.stx.arrow {
        ThisMode::Lexical
      } else {
        ThisMode::Strict
      };
      let closure_env = Some(self.env.lexical_env);

      let mut ctor_scope = scope.reborrow();
      let name_string = ctor_scope.alloc_string("constructor")?;
      let func_obj = ctor_scope.alloc_ecma_function(
        code,
        /* is_constructable */ true,
        name_string,
        ctor_length,
        this_mode,
        is_strict,
        closure_env,
      )?;

      let intr = self
        .vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
      ctor_scope
        .heap_mut()
        .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
      ctor_scope
        .heap_mut()
        .set_function_realm(func_obj, self.env.global_object())?;
      if let Some(realm) = self.vm.current_realm() {
        ctor_scope.heap_mut().set_function_job_realm(func_obj, realm)?;
      }
      Some(func_obj)
    } else {
      None
    };

    let func_obj = self.create_class_constructor_object(scope, func_name, ctor_length, ctor_body_func)?;

    // Initialize the requested binding now that the class constructor object exists.
    if let Some(binding_name) = match binding {
      ClassBinding::None => None,
      ClassBinding::Mutable(name) | ClassBinding::Immutable(name) => Some(name),
    } {
      // Root the class constructor object during initialization so if the operation grows the root
      // stack (and triggers GC) we don't collect the class constructor before it becomes reachable
      // from its binding.
      let mut init_scope = scope.reborrow();
      init_scope.push_root(Value::Object(func_obj))?;
      init_scope
        .heap_mut()
        .env_initialize_binding(class_env, binding_name, Value::Object(func_obj))?;
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
    let PropertyKind::Data { value, .. } = prototype_desc.kind else {
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

    // Per ECMAScript, class constructors have a non-writable `prototype` property.
    class_scope.define_property_or_throw(
      func_obj,
      prototype_key,
      PropertyDescriptorPatch {
        writable: Some(false),
        ..Default::default()
      },
    )?;

    // Define prototype and static methods.
    for member in members {
      self.tick()?;

      if !member.stx.decorators.is_empty() {
        return Err(VmError::Unimplemented("class member decorators"));
      }
      if member.stx.declare || member.stx.abstract_ {
        return Err(VmError::Unimplemented("class member modifiers"));
      }
      if member.stx.readonly
        || member.stx.accessor
        || member.stx.optional
        || member.stx.override_
        || member.stx.definite_assignment
      {
        return Err(VmError::Unimplemented("class member modifiers"));
      }
      if member.stx.accessibility.is_some() || member.stx.type_annotation.is_some() {
        return Err(VmError::Unimplemented("class member type annotations"));
      }

      // Skip the actual `constructor(...) { ... }` method: it's represented by the class constructor
      // object itself (and its hidden body function).
      let is_constructor_method = !member.stx.static_
        && matches!(&member.stx.key, ClassOrObjKey::Direct(direct) if direct.stx.key == "constructor")
        && matches!(&member.stx.val, ClassOrObjVal::Method(_));
      if is_constructor_method {
        continue;
      }

      let target_obj = if member.stx.static_ {
        func_obj
      } else {
        prototype_obj
      };

      // Compute property key.
      let key_loc_start = match &member.stx.key {
        ClassOrObjKey::Direct(direct) => direct.loc.start_u32(),
        ClassOrObjKey::Computed(expr) => expr.loc.start_u32(),
      };

      let mut member_scope = class_scope.reborrow();
      member_scope.push_root(Value::Object(target_obj))?;

      let key = match &member.stx.key {
        ClassOrObjKey::Direct(direct) => {
          let key_s = if let Some(units) = literal_string_code_units(&direct.assoc) {
            member_scope.alloc_string_from_code_units(units)?
          } else if direct.stx.tt == TT::LiteralNumber {
            let n = direct
              .stx
              .key
              .parse::<f64>()
              .map_err(|_| VmError::Unimplemented("numeric literal property name parse"))?;
            member_scope.heap_mut().to_string(Value::Number(n))?
          } else {
            member_scope.alloc_string(&direct.stx.key)?
          };
          PropertyKey::from_string(key_s)
        }
        ClassOrObjKey::Computed(expr) => {
          let value = self.eval_expr(&mut member_scope, expr)?;
          member_scope.push_root(value)?;
          self.to_property_key_operator(&mut member_scope, value)?
        }
      };

      match key {
        PropertyKey::String(s) => member_scope.push_root(Value::String(s))?,
        PropertyKey::Symbol(s) => member_scope.push_root(Value::Symbol(s))?,
      };

      match &member.stx.val {
        ClassOrObjVal::Method(method) => {
          let func_node = &method.stx.func;
          let length = self.function_length(&func_node.stx)?;

          let rel_start = key_loc_start.saturating_sub(self.env.prefix_len());
          let rel_end = func_node
            .loc
            .end_u32()
            .saturating_sub(self.env.prefix_len());
          let span_start = self.env.base_offset().saturating_add(rel_start);
          let span_end = self.env.base_offset().saturating_add(rel_end);

          let code = self.vm.register_ecma_function(
            self.env.source(),
            span_start,
            span_end,
            EcmaFunctionKind::ObjectMember,
          )?;

          // Class methods are always strict mode.
          let is_strict = true;
          let this_mode = if func_node.stx.arrow {
            ThisMode::Lexical
          } else {
            ThisMode::Strict
          };
          let closure_env = Some(self.env.lexical_env);

          let name_string = match key {
            PropertyKey::String(s) => s,
            PropertyKey::Symbol(_) => member_scope.alloc_string("")?,
          };

          let func_obj = member_scope.alloc_ecma_function(
            code,
            /* is_constructable */ false,
            name_string,
            length,
            this_mode,
            is_strict,
            closure_env,
          )?;

          let intr = self
            .vm
            .intrinsics()
            .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
          member_scope
            .heap_mut()
            .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
          member_scope
            .heap_mut()
            .set_function_realm(func_obj, self.env.global_object())?;
          if let Some(realm) = self.vm.current_realm() {
            member_scope
              .heap_mut()
              .set_function_job_realm(func_obj, realm)?;
          }
          member_scope.push_root(Value::Object(func_obj))?;

          // Methods use the property key as the function `name` if possible.
          if !matches!(key, PropertyKey::String(_)) {
            crate::function_properties::set_function_name(&mut member_scope, func_obj, key, None)?;
          }

          member_scope.define_property_or_throw(
            target_obj,
            key,
            PropertyDescriptorPatch {
              value: Some(Value::Object(func_obj)),
              writable: Some(true),
              enumerable: Some(false),
              configurable: Some(true),
              ..Default::default()
            },
          )?;
        }
        ClassOrObjVal::Getter(getter) => {
          let func_node = &getter.stx.func;
          let length = self.function_length(&func_node.stx)?;

          let rel_start = key_loc_start.saturating_sub(self.env.prefix_len());
          let rel_end = func_node
            .loc
            .end_u32()
            .saturating_sub(self.env.prefix_len());
          let span_start = self.env.base_offset().saturating_add(rel_start);
          let span_end = self.env.base_offset().saturating_add(rel_end);

          let code = self.vm.register_ecma_function(
            self.env.source(),
            span_start,
            span_end,
            EcmaFunctionKind::ObjectMember,
          )?;

          // Class accessors are always strict mode.
          let is_strict = true;
          let this_mode = if func_node.stx.arrow {
            ThisMode::Lexical
          } else {
            ThisMode::Strict
          };
          let closure_env = Some(self.env.lexical_env);

          let name_string = member_scope.alloc_string("")?;
          let func_obj = member_scope.alloc_ecma_function(
            code,
            /* is_constructable */ false,
            name_string,
            length,
            this_mode,
            is_strict,
            closure_env,
          )?;

          let intr = self
            .vm
            .intrinsics()
            .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
          member_scope
            .heap_mut()
            .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
          member_scope
            .heap_mut()
            .set_function_realm(func_obj, self.env.global_object())?;
          if let Some(realm) = self.vm.current_realm() {
            member_scope
              .heap_mut()
              .set_function_job_realm(func_obj, realm)?;
          }
          member_scope.push_root(Value::Object(func_obj))?;

          crate::function_properties::set_function_name(
            &mut member_scope,
            func_obj,
            key,
            Some("get"),
          )?;

          member_scope.define_property_or_throw(
            target_obj,
            key,
            PropertyDescriptorPatch {
              get: Some(Value::Object(func_obj)),
              enumerable: Some(false),
              configurable: Some(true),
              ..Default::default()
            },
          )?;
        }
        ClassOrObjVal::Setter(setter) => {
          let func_node = &setter.stx.func;
          let length = self.function_length(&func_node.stx)?;

          let rel_start = key_loc_start.saturating_sub(self.env.prefix_len());
          let rel_end = func_node
            .loc
            .end_u32()
            .saturating_sub(self.env.prefix_len());
          let span_start = self.env.base_offset().saturating_add(rel_start);
          let span_end = self.env.base_offset().saturating_add(rel_end);

          let code = self.vm.register_ecma_function(
            self.env.source(),
            span_start,
            span_end,
            EcmaFunctionKind::ObjectMember,
          )?;

          // Class accessors are always strict mode.
          let is_strict = true;
          let this_mode = if func_node.stx.arrow {
            ThisMode::Lexical
          } else {
            ThisMode::Strict
          };
          let closure_env = Some(self.env.lexical_env);

          let name_string = member_scope.alloc_string("")?;
          let func_obj = member_scope.alloc_ecma_function(
            code,
            /* is_constructable */ false,
            name_string,
            length,
            this_mode,
            is_strict,
            closure_env,
          )?;

          let intr = self
            .vm
            .intrinsics()
            .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
          member_scope
            .heap_mut()
            .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
          member_scope
            .heap_mut()
            .set_function_realm(func_obj, self.env.global_object())?;
          if let Some(realm) = self.vm.current_realm() {
            member_scope
              .heap_mut()
              .set_function_job_realm(func_obj, realm)?;
          }
          member_scope.push_root(Value::Object(func_obj))?;

          crate::function_properties::set_function_name(
            &mut member_scope,
            func_obj,
            key,
            Some("set"),
          )?;

          member_scope.define_property_or_throw(
            target_obj,
            key,
            PropertyDescriptorPatch {
              set: Some(Value::Object(func_obj)),
              enumerable: Some(false),
              configurable: Some(true),
              ..Default::default()
            },
          )?;
        }
        ClassOrObjVal::Prop(_) => {
          return Err(VmError::Unimplemented("class fields"));
        }
        ClassOrObjVal::IndexSignature(_) => {
          return Err(VmError::Unimplemented("class index signature"));
        }
        ClassOrObjVal::StaticBlock(_) => {
          return Err(VmError::Unimplemented("class static block"));
        }
      }
    }

    Ok(func_obj)
  }

  fn eval_class_decl(
    &mut self,
    scope: &mut Scope<'_>,
    decl: &Node<ClassDecl>,
  ) -> Result<Completion, VmError> {
    if decl.stx.extends.is_some() {
      return Err(VmError::Unimplemented("class inheritance"));
    }
    if !decl.stx.decorators.is_empty() {
      return Err(VmError::Unimplemented("class decorators"));
    }
    if decl.stx.type_parameters.is_some() {
      return Err(VmError::Unimplemented("class type parameters"));
    }
    if !decl.stx.implements.is_empty() {
      return Err(VmError::Unimplemented("class implements"));
    }
    if decl.stx.declare || decl.stx.abstract_ {
      return Err(VmError::Unimplemented("class modifiers"));
    }

    let binding_name = match decl.stx.name.as_ref() {
      Some(name) => name.stx.name.as_str(),
      None => "*default*",
    };

    let func_name = match decl.stx.name.as_ref() {
      Some(name) => name.stx.name.as_str(),
      None => "default",
    };

    let _ = self.eval_class(
      scope,
      ClassBinding::Mutable(binding_name),
      func_name,
      &decl.stx.members,
    )?;
    Ok(Completion::empty())
  }

  fn eval_class_expr(&mut self, scope: &mut Scope<'_>, expr: &Node<ClassExpr>) -> Result<Value, VmError> {
    if expr.stx.extends.is_some() {
      return Err(VmError::Unimplemented("class inheritance"));
    }
    if !expr.stx.decorators.is_empty() {
      return Err(VmError::Unimplemented("class decorators"));
    }
    if expr.stx.type_parameters.is_some() {
      return Err(VmError::Unimplemented("class type parameters"));
    }
    if !expr.stx.implements.is_empty() {
      return Err(VmError::Unimplemented("class implements"));
    }

    // Named class expressions introduce an inner immutable binding for the class name.
    let outer = self.env.lexical_env;
    let result = (|| {
      let Some(name) = expr.stx.name.as_ref() else {
        let func_obj = self.eval_class(scope, ClassBinding::None, "", &expr.stx.members)?;
        return Ok(Value::Object(func_obj));
      };

      let class_env = scope.env_create(Some(outer))?;
      self.env.set_lexical_env(scope.heap_mut(), class_env);

      let func_obj = self.eval_class(
        scope,
        ClassBinding::Immutable(name.stx.name.as_str()),
        name.stx.name.as_str(),
        &expr.stx.members,
      )?;
      Ok(Value::Object(func_obj))
    })();

    self.env.set_lexical_env(scope.heap_mut(), outer);
    result
  }

  fn eval_if(&mut self, scope: &mut Scope<'_>, stmt: &IfStmt) -> Result<Completion, VmError> {
    let test = self.eval_expr(scope, &stmt.test)?;
    if to_boolean(scope.heap(), test)? {
      self.eval_stmt(scope, &stmt.consequent)
    } else if let Some(alt) = &stmt.alternate {
      self.eval_stmt(scope, alt)
    } else {
      Ok(Completion::empty())
    }
  }

  fn eval_throw(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &Node<ThrowStmt>,
  ) -> Result<Completion, VmError> {
    let value = self.eval_expr(scope, &stmt.stx.value)?;

    // Capture a stack trace at the throw site.
    //
    // We capture the VM's current call stack and then update the top frame's `source/line/col` to
    // point at the throw statement (rather than the function entry). This aligns better with
    // browser stack traces where the top frame refers to the actual throw location.
    let source = self.env.source();
    let rel_start = stmt.loc.start_u32().saturating_sub(self.env.prefix_len());
    let abs_offset = self.env.base_offset().saturating_add(rel_start);
    let (line, col) = source.line_col(abs_offset);

    let mut stack = self.vm.capture_stack();
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

    Ok(Completion::Throw(Thrown { value, stack }))
  }

  fn eval_try(&mut self, scope: &mut Scope<'_>, stmt: &TryStmt) -> Result<Completion, VmError> {
    let mut result = self.eval_block_stmt(scope, &stmt.wrapped.stx)?;

    if matches!(result, Completion::Throw(_)) {
      if let Some(catch) = &stmt.catch {
        let thrown = match result {
          Completion::Throw(thrown) => thrown.value,
          _ => return Err(VmError::Unimplemented("try/catch missing thrown value")),
        };
        result = self.eval_catch(scope, &catch.stx, thrown)?;
      }
    }

    if let Some(finally) = &stmt.finally {
      // Root the pending completion's value (if any) while evaluating `finally`, which may
      // allocate and trigger GC.
      let pending_root = result
        .value()
        .map(|v| scope.heap_mut().add_root(v))
        .transpose()?;
      let finally_result = self.eval_block_stmt(scope, &finally.stx)?;
      if let Some(root) = pending_root {
        scope.heap_mut().remove_root(root);
      }

      if finally_result.is_abrupt() {
        result = finally_result;
      }
    }

    Ok(result.update_empty(Some(Value::Undefined)))
  }

  fn eval_catch(
    &mut self,
    scope: &mut Scope<'_>,
    catch: &CatchBlock,
    thrown: Value,
  ) -> Result<Completion, VmError> {
    let outer = self.env.lexical_env;
    let catch_env = scope.env_create(Some(outer))?;
    self.env.set_lexical_env(scope.heap_mut(), catch_env);

    let result = {
      // Root the thrown value across catch binding instantiation, which may allocate.
      let mut catch_scope = scope.reborrow();
      catch_scope.push_root(thrown)?;

      self
        .instantiate_block_decls_in_stmt_list(&mut catch_scope, catch_env, &catch.body)
        .and_then(|_| {
          if let Some(param) = &catch.parameter {
            self.bind_catch_param(&mut catch_scope, &param.stx, thrown, catch_env)?;
          }
          self.eval_stmt_list(&mut catch_scope, &catch.body)
        })
    };

    self.env.set_lexical_env(scope.heap_mut(), outer);
    result
  }

  fn bind_catch_param(
    &mut self,
    scope: &mut Scope<'_>,
    param: &PatDecl,
    thrown: Value,
    env: GcEnv,
  ) -> Result<(), VmError> {
    // Bind into the provided catch environment (which should also be the current lexical env).
    let _ = env;
    bind_pattern(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      scope,
      self.env,
      &param.pat.stx,
      thrown,
      BindingKind::Let,
      self.strict,
      self.this,
    )
  }

  fn eval_return(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &ReturnStmt,
  ) -> Result<Completion, VmError> {
    let value = match &stmt.value {
      Some(expr) => self.eval_expr(scope, expr)?,
      None => Value::Undefined,
    };
    Ok(Completion::Return(value))
  }

  /// ECMA-262 `LoopContinues(completion, labelSet)`.
  fn loop_continues(completion: &Completion, label_set: &[String]) -> bool {
    match completion {
      Completion::Normal(_) => true,
      Completion::Continue(None, _) => true,
      Completion::Continue(Some(target), _) => label_set.iter().any(|l| l == target),
      _ => false,
    }
  }

  /// Converts an unlabelled `break` completion from a breakable statement into a normal
  /// completion (ECMA-262 `BreakableStatement` / `LabelledEvaluation` semantics).
  fn normalise_iteration_break(completion: Completion) -> Completion {
    match completion {
      Completion::Break(None, value) => Completion::normal(value.unwrap_or(Value::Undefined)),
      other => other,
    }
  }

  fn eval_while(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &WhileStmt,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    let result = self.while_loop_evaluation(scope, stmt, label_set)?;
    Ok(Self::normalise_iteration_break(result))
  }

  /// ECMA-262 `WhileLoopEvaluation`.
  fn while_loop_evaluation(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &WhileStmt,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    // Root `V` across the loop so the value can't be collected between iterations.
    let mut scope = scope.reborrow();
    let v_root_idx = scope.heap().root_stack.len();
    scope.push_root(Value::Undefined)?;
    let mut v = Value::Undefined;

    loop {
      let test = self.eval_expr(&mut scope, &stmt.condition)?;
      if !to_boolean(scope.heap(), test)? {
        return Ok(Completion::normal(v));
      }

      let stmt_result = self.eval_stmt(&mut scope, &stmt.body)?;
      if !Self::loop_continues(&stmt_result, label_set) {
        return Ok(stmt_result.update_empty(Some(v)));
      }

      if let Some(value) = stmt_result.value() {
        v = value;
        scope.heap_mut().root_stack[v_root_idx] = value;
      }
    }
  }

  fn eval_do_while(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &DoWhileStmt,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    let result = self.do_while_loop_evaluation(scope, stmt, label_set)?;
    Ok(Self::normalise_iteration_break(result))
  }

  /// ECMA-262 `DoWhileLoopEvaluation`.
  fn do_while_loop_evaluation(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &DoWhileStmt,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    // Root `V` across the loop so the value can't be collected between iterations.
    let mut scope = scope.reborrow();
    let v_root_idx = scope.heap().root_stack.len();
    scope.push_root(Value::Undefined)?;
    let mut v = Value::Undefined;

    loop {
      let stmt_result = self.eval_stmt(&mut scope, &stmt.body)?;
      if !Self::loop_continues(&stmt_result, label_set) {
        return Ok(stmt_result.update_empty(Some(v)));
      }

      if let Some(value) = stmt_result.value() {
        v = value;
        scope.heap_mut().root_stack[v_root_idx] = value;
      }

      let test = self.eval_expr(&mut scope, &stmt.condition)?;
      if !to_boolean(scope.heap(), test)? {
        return Ok(Completion::normal(v));
      }
    }
  }

  fn eval_with(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &WithStmt,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    // Minimal ECMA-262 `WithStatement` evaluation:
    //
    // - Evaluate the object expression, then `ToObject` it.
    // - Create an ObjectEnvironmentRecord with `with_environment = true`.
    // - Evaluate the body with that env record as the current lexical environment.
    let mut with_scope = scope.reborrow();
    let object_value = self.eval_expr(&mut with_scope, &stmt.object)?;
    with_scope.push_root(object_value)?;
    let binding_object =
      with_scope.to_object(self.vm, &mut *self.host, &mut *self.hooks, object_value)?;
    with_scope.push_root(Value::Object(binding_object))?;

    let outer = self.env.lexical_env;
    let with_env = with_scope.alloc_object_env_record(binding_object, Some(outer), true)?;
    self.env.set_lexical_env(with_scope.heap_mut(), with_env);

    let result = self.eval_stmt_labelled(&mut with_scope, &stmt.body, label_set);

    // Always restore the outer lexical environment so later statements run in the correct scope.
    self.env.set_lexical_env(with_scope.heap_mut(), outer);
    result
  }

  fn eval_for_triple(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &ForTripleStmt,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    // Note: this is intentionally minimal and does not implement per-iteration lexical
    // environments for `let`/`const`.
    let result = self.for_triple_loop_evaluation(scope, stmt, label_set)?;
    Ok(Self::normalise_iteration_break(result))
  }

  /// ECMA-262 `ForLoopEvaluation` for `for (init; cond; post) { ... }`.
  fn for_triple_loop_evaluation(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &ForTripleStmt,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    // Root `V` across the loop so the value can't be collected between iterations.
    let mut scope = scope.reborrow();
    let v_root_idx = scope.heap().root_stack.len();
    scope.push_root(Value::Undefined)?;
    let mut v = Value::Undefined;

    match &stmt.init {
      parse_js::ast::stmt::ForTripleStmtInit::None => {}
      parse_js::ast::stmt::ForTripleStmtInit::Expr(expr) => {
        let _ = self.eval_expr(&mut scope, expr)?;
      }
      parse_js::ast::stmt::ForTripleStmtInit::Decl(decl) => {
        let _ = self.eval_var_decl(&mut scope, &decl.stx)?;
      }
    }

    // Most `for` loop iterations are naturally budgeted by ticks in:
    // - condition/update expression evaluation (if present), and/or
    // - evaluating at least one statement in the loop body.
    //
    // However, `for(;;){}` executes no statements/expressions per iteration. Tick explicitly to
    // ensure budgets/interrupts are still observed.
    let needs_explicit_iter_tick =
      stmt.cond.is_none() && stmt.post.is_none() && stmt.body.stx.body.is_empty();

    loop {
      if needs_explicit_iter_tick {
        self.tick()?;
      }

      if let Some(cond) = &stmt.cond {
        let test = self.eval_expr(&mut scope, cond)?;
        if !to_boolean(scope.heap(), test)? {
          return Ok(Completion::normal(v));
        }
      }

      let stmt_result = self.eval_for_body(&mut scope, &stmt.body.stx)?;
      if !Self::loop_continues(&stmt_result, label_set) {
        return Ok(stmt_result.update_empty(Some(v)));
      }

      if let Some(value) = stmt_result.value() {
        v = value;
        scope.heap_mut().root_stack[v_root_idx] = value;
      }

      if let Some(post) = &stmt.post {
        let _ = self.eval_expr(&mut scope, post)?;
      }
    }
  }

  fn eval_for_in(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &ForInStmt,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    let result = self.for_in_loop_evaluation(scope, stmt, label_set)?;
    Ok(Self::normalise_iteration_break(result))
  }

  fn for_in_loop_evaluation(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &ForInStmt,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    let rhs_value = self.eval_expr(scope, &stmt.rhs)?;
    if is_nullish(rhs_value) {
      // Minimal semantics: legacy-ish behaviour, treat `null`/`undefined` as an empty iteration.
      return Ok(Completion::normal(Value::Undefined));
    }

    // `for..in` uses `ToObject` on the RHS. Until we have full wrapper objects, treat the `Object`
    // constructor as a converter for primitives.
    let object = match rhs_value {
      Value::Object(obj) => obj,
      other => {
        let intr = self
          .vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let object_ctor = Value::Object(intr.object_constructor());

        let mut to_obj_scope = scope.reborrow();
        to_obj_scope.push_root(other)?;
        to_obj_scope.push_root(object_ctor)?;
        let args = [other];
        let value = self.call(&mut to_obj_scope, object_ctor, Value::Undefined, &args)?;
        match value {
          Value::Object(obj) => obj,
          _ => {
            return Err(VmError::InvariantViolation(
              "Object(..) conversion returned non-object",
            ));
          }
        }
      }
    };

    // Root the base object across key collection + loop body evaluation, which may allocate and
    // trigger GC.
    let mut iter_scope = scope.reborrow();
    iter_scope.push_root(Value::Object(object))?;

    // Snapshot enumerable string keys across the prototype chain, skipping duplicates.
    //
    // Note: this is intentionally minimal and does not track mutations during iteration.
    const KEY_COLLECTION_TICK_EVERY: usize = 256;
    let mut keys: Vec<GcString> = Vec::new();
    let mut visited: Vec<PropertyKey> = Vec::new();

    let mut key_count: usize = 0;
    // De-duplication uses a linear scan over the keys collected so far, which can be `O(N^2)` in
    // the worst case. Tick periodically while scanning to ensure this work stays interruptible even
    // for very large objects/prototype chains.
    const VISITED_SCAN_TICK_EVERY: usize = 4096;
    let mut visited_scan_count: usize = 0;
    let mut current: Option<GcObject> = Some(object);
    while let Some(obj) = current {
      // Budget/interrupt check while walking the prototype chain and collecting enumerable keys.
      self.tick()?;

      let own_keys = iter_scope.ordinary_own_property_keys_with_tick(obj, || self.tick())?;
      for key in own_keys {
        key_count = key_count.wrapping_add(1);
        if (key_count & (KEY_COLLECTION_TICK_EVERY - 1)) == 0 {
          self.tick()?;
        }

        let PropertyKey::String(s) = key else {
          continue;
        };

        let Some(desc) = iter_scope.ordinary_get_own_property(obj, key)? else {
          continue;
        };
        if !desc.enumerable {
          continue;
        }

        let mut already_visited = false;
        for seen in &visited {
          visited_scan_count = visited_scan_count.wrapping_add(1);
          if (visited_scan_count & (VISITED_SCAN_TICK_EVERY - 1)) == 0 {
            self.tick()?;
          }
          if iter_scope.heap().property_key_eq(seen, &key) {
            already_visited = true;
            break;
          }
        }
        if already_visited {
          continue;
        }

        visited.push(key);
        keys.push(s);
        // Root the key for the duration of the loop in case the property is deleted during
        // iteration.
        iter_scope.push_root(Value::String(s))?;
      }

      current = iter_scope.object_get_prototype(obj)?;
    }

    // Root `V` across the loop so the value can't be collected between iterations.
    let v_root_idx = iter_scope.heap().root_stack.len();
    iter_scope.push_root(Value::Undefined)?;
    let mut v = Value::Undefined;

    // If the loop uses a lexical declaration (`let`/`const`), we emulate per-iteration lexical
    // environments by creating a fresh env record per iteration.
    let outer_lex = self.env.lexical_env;

    for key_s in keys {
      // Tick once per iteration so `for (k in o) {}` is budgeted even when the body is empty.
      self.tick()?;

      let mut iter_env: Option<GcEnv> = None;
      if let ForInOfLhs::Decl((mode, _)) = &stmt.lhs {
        if *mode == VarDeclMode::Let || *mode == VarDeclMode::Const {
          let env = iter_scope.env_create(Some(outer_lex))?;
          self.env.set_lexical_env(iter_scope.heap_mut(), env);
          iter_env = Some(env);
        }
      }

      let value = Value::String(key_s);

      let bind_res: Result<(), VmError> = match &stmt.lhs {
        ForInOfLhs::Decl((mode, pat_decl)) => {
          let kind = match *mode {
            VarDeclMode::Var => BindingKind::Var,
            VarDeclMode::Let => BindingKind::Let,
            VarDeclMode::Const => BindingKind::Const,
            _ => {
              return Err(VmError::Unimplemented(
                "for-in loop variable declaration kind",
              ));
            }
          };
          bind_pattern(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            &mut iter_scope,
            self.env,
            &pat_decl.stx.pat.stx,
            value,
            kind,
            self.strict,
            self.this,
          )
        }
        ForInOfLhs::Assign(pat) => bind_pattern(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut iter_scope,
          self.env,
          &pat.stx,
          value,
          BindingKind::Assignment,
          self.strict,
          self.this,
        ),
      };

      if let Err(err) = bind_res {
        if iter_env.is_some() {
          self.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
        }
        return Err(err);
      }

      let body_completion = match self.eval_for_body(&mut iter_scope, &stmt.body.stx) {
        Ok(c) => c,
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

      if !Self::loop_continues(&body_completion, label_set) {
        return Ok(body_completion.update_empty(Some(v)));
      }

      if let Some(value) = body_completion.value() {
        v = value;
        iter_scope.heap_mut().root_stack[v_root_idx] = value;
      }
    }

    Ok(Completion::normal(v))
  }

  fn eval_for_of(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &ForOfStmt,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    let result = self.for_of_loop_evaluation(scope, stmt, label_set)?;
    Ok(Self::normalise_iteration_break(result))
  }

  fn for_of_loop_evaluation(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &ForOfStmt,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    if stmt.await_ {
      return Err(VmError::Unimplemented("for await..of"));
    }

    let iterable = self.eval_expr(scope, &stmt.rhs)?;

    // Root the iterable + iterator record while evaluating the loop body, which may allocate and
    // trigger GC.
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

    // Root `V` across the loop so the value can't be collected between iterations.
    let v_root_idx = iter_scope.heap().root_stack.len();
    iter_scope.push_root(Value::Undefined)?;
    let mut v = Value::Undefined;

    // If the loop uses a lexical declaration (`let`/`const`), we emulate per-iteration lexical
    // environments by creating a fresh env record per iteration.
    let outer_lex = self.env.lexical_env;

    loop {
      // Tick once per iteration so `for (x of xs) {}` is budgeted even when the body is empty.
      self.tick()?;

      let next_value = match iterator::iterator_step_value(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        &mut iter_scope,
        &mut iterator_record,
      ) {
        Ok(v) => v,
        Err(err) => {
          let _ = iterator::iterator_close(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            &mut iter_scope,
            &iterator_record,
          );
          return Err(err);
        }
      };

      let Some(value) = next_value else {
        return Ok(Completion::normal(v));
      };

      let mut iter_env: Option<GcEnv> = None;
      if let ForInOfLhs::Decl((mode, _)) = &stmt.lhs {
        if *mode == VarDeclMode::Let || *mode == VarDeclMode::Const {
          let env = iter_scope.env_create(Some(outer_lex))?;
          self.env.set_lexical_env(iter_scope.heap_mut(), env);
          iter_env = Some(env);
        }
      }

      let bind_res: Result<(), VmError> = match &stmt.lhs {
        ForInOfLhs::Decl((mode, pat_decl)) => {
          let kind = match *mode {
            VarDeclMode::Var => BindingKind::Var,
            VarDeclMode::Let => BindingKind::Let,
            VarDeclMode::Const => BindingKind::Const,
            _ => {
              return Err(VmError::Unimplemented(
                "for-of loop variable declaration kind",
              ));
            }
          };
          bind_pattern(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            &mut iter_scope,
            self.env,
            &pat_decl.stx.pat.stx,
            value,
            kind,
            self.strict,
            self.this,
          )
        }
        ForInOfLhs::Assign(pat) => bind_pattern(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut iter_scope,
          self.env,
          &pat.stx,
          value,
          BindingKind::Assignment,
          self.strict,
          self.this,
        ),
      };

      if let Err(err) = bind_res {
        if iter_env.is_some() {
          self.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
        }
        let _ = iterator::iterator_close(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut iter_scope,
          &iterator_record,
        );
        return Err(err);
      }

      let body_completion = match self.eval_for_body(&mut iter_scope, &stmt.body.stx) {
        Ok(c) => c,
        Err(err) => {
          if iter_env.is_some() {
            self.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
          }
          let _ = iterator::iterator_close(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            &mut iter_scope,
            &iterator_record,
          );
          return Err(err);
        }
      };

      if iter_env.is_some() {
        self.env.set_lexical_env(iter_scope.heap_mut(), outer_lex);
      }

      if !Self::loop_continues(&body_completion, label_set) {
        let _ = iterator::iterator_close(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut iter_scope,
          &iterator_record,
        );
        return Ok(body_completion.update_empty(Some(v)));
      }

      if let Some(value) = body_completion.value() {
        v = value;
        iter_scope.heap_mut().root_stack[v_root_idx] = value;
      }
    }
  }

  fn eval_for_body(
    &mut self,
    scope: &mut Scope<'_>,
    body: &ForBody,
  ) -> Result<Completion, VmError> {
    self.eval_stmt_list(scope, &body.body)
  }

  fn eval_label(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &LabelStmt,
    label_set: &[String],
  ) -> Result<Completion, VmError> {
    let mut new_label_set = label_set.to_vec();
    new_label_set.push(stmt.name.clone());

    let result = self.eval_stmt_labelled(scope, &stmt.statement, &new_label_set)?;

    match result {
      Completion::Break(Some(target), value) if target == stmt.name => {
        // ECMA-262 `LabelledEvaluation`: a labelled `break` is consumed by the matching label,
        // preserving the completion value (which may be ~empty~).
        Ok(Completion::Normal(value))
      }
      other => Ok(other),
    }
  }

  fn eval_switch(
    &mut self,
    scope: &mut Scope<'_>,
    stmt: &SwitchStmt,
  ) -> Result<Completion, VmError> {
    // 13.12.3 Runtime Semantics: Evaluation, SwitchStatement
    let discriminant = self.eval_expr(scope, &stmt.test)?;

    // Root the discriminant across selector evaluation and case-body execution, which may allocate
    // and trigger GC.
    let mut switch_scope = scope.reborrow();
    switch_scope.push_root(discriminant)?;

    // `switch` creates a new lexical environment for the entire case block.
    let outer = self.env.lexical_env;
    let switch_env = switch_scope.env_create(Some(outer))?;
    self
      .env
      .set_lexical_env(switch_scope.heap_mut(), switch_env);

    let result = (|| -> Result<Completion, VmError> {
      const BRANCH_TICK_EVERY: usize = 32;

      // `switch` shares one lexical scope across all case clauses.
      let mut default_idx: Option<usize> = None;
      for (i, branch) in stmt.branches.iter().enumerate() {
        // Budget branch-list traversal: a `switch` statement can contain a very large number of
        // case clauses (bounded by source size), and both block-instantiation and case evaluation
        // may need to walk those lists even when the clause bodies are empty.
        if i % BRANCH_TICK_EVERY == 0 {
          self.tick()?;
        }
        if default_idx.is_none() && branch.stx.case.is_none() {
          default_idx = Some(i);
        }
        self.instantiate_block_decls_in_stmt_list(
          &mut switch_scope,
          switch_env,
          &branch.stx.body,
        )?;
      }

      // ECMA-262 `CaseBlockEvaluation`: `V` starts as `undefined` and is never ~empty~ for normal
      // completion.
      let v_root_idx = switch_scope.heap().root_stack.len();
      switch_scope.push_root(Value::Undefined)?;
      let mut v = Value::Undefined;

      match default_idx {
        None => {
          let mut found = false;
          for (i, branch) in stmt.branches.iter().enumerate() {
            if i % BRANCH_TICK_EVERY == 0 {
              self.tick()?;
            }
            let Some(case_expr) = &branch.stx.case else {
              continue;
            };

            if !found {
              let case_value = self.eval_expr(&mut switch_scope, case_expr)?;
              found = strict_equal(switch_scope.heap(), discriminant, case_value)?;
            }

            if found {
              let r = self.eval_stmt_list(&mut switch_scope, &branch.stx.body)?;
              if let Some(value) = r.value() {
                v = value;
                switch_scope.heap_mut().root_stack[v_root_idx] = value;
              }
              if r.is_abrupt() {
                return Ok(r.update_empty(Some(v)));
              }
            }
          }
          Ok(Completion::normal(v))
        }
        Some(default_idx) => {
          let (a, rest) = stmt.branches.split_at(default_idx);
          let default_branch = &rest[0];
          let b = &rest[1..];

          let mut found = false;
          for (i, branch) in a.iter().enumerate() {
            if i % BRANCH_TICK_EVERY == 0 {
              self.tick()?;
            }
            let Some(case_expr) = &branch.stx.case else {
              continue;
            };

            if !found {
              let case_value = self.eval_expr(&mut switch_scope, case_expr)?;
              found = strict_equal(switch_scope.heap(), discriminant, case_value)?;
            }

            if found {
              let r = self.eval_stmt_list(&mut switch_scope, &branch.stx.body)?;
              if let Some(value) = r.value() {
                v = value;
                switch_scope.heap_mut().root_stack[v_root_idx] = value;
              }
              if r.is_abrupt() {
                return Ok(r.update_empty(Some(v)));
              }
            }
          }

          let mut found_in_b = false;
          if !found {
            for (i, branch) in b.iter().enumerate() {
              if i % BRANCH_TICK_EVERY == 0 {
                self.tick()?;
              }
              let Some(case_expr) = &branch.stx.case else {
                continue;
              };

              if !found_in_b {
                let case_value = self.eval_expr(&mut switch_scope, case_expr)?;
                found_in_b = strict_equal(switch_scope.heap(), discriminant, case_value)?;
              }

              if found_in_b {
                let r = self.eval_stmt_list(&mut switch_scope, &branch.stx.body)?;
                if let Some(value) = r.value() {
                  v = value;
                  switch_scope.heap_mut().root_stack[v_root_idx] = value;
                }
                if r.is_abrupt() {
                  return Ok(r.update_empty(Some(v)));
                }
              }
            }
          }

          if found_in_b {
            return Ok(Completion::normal(v));
          }

          let default_r = self.eval_stmt_list(&mut switch_scope, &default_branch.stx.body)?;
          if let Some(value) = default_r.value() {
            v = value;
            switch_scope.heap_mut().root_stack[v_root_idx] = value;
          }
          if default_r.is_abrupt() {
            return Ok(default_r.update_empty(Some(v)));
          }

          // NOTE: The following is another complete iteration of the after-default clauses.
          for (i, branch) in b.iter().enumerate() {
            if i % BRANCH_TICK_EVERY == 0 {
              self.tick()?;
            }
            let r = self.eval_stmt_list(&mut switch_scope, &branch.stx.body)?;
            if let Some(value) = r.value() {
              v = value;
              switch_scope.heap_mut().root_stack[v_root_idx] = value;
            }
            if r.is_abrupt() {
              return Ok(r.update_empty(Some(v)));
            }
          }

          Ok(Completion::normal(v))
        }
      }
    })();

    // Restore the outer lexical environment no matter how control leaves the switch.
    self.env.set_lexical_env(switch_scope.heap_mut(), outer);
    result
  }

  fn eval_expr(&mut self, scope: &mut Scope<'_>, expr: &Node<Expr>) -> Result<Value, VmError> {
    // One tick per expression.
    self.tick()?;

    match &*expr.stx {
      Expr::LitStr(node) => self.eval_lit_str(scope, node),
      Expr::LitNum(node) => self.eval_lit_num(&node.stx),
      Expr::LitBigInt(node) => self.eval_lit_bigint(&node.stx),
      Expr::LitBool(node) => self.eval_lit_bool(&node.stx),
      Expr::LitNull(_) => Ok(Value::Null),
      Expr::LitArr(node) => self.eval_lit_arr(scope, &node.stx),
      Expr::LitObj(node) => self.eval_lit_obj(scope, &node.stx),
      Expr::LitTemplate(node) => self.eval_lit_template(scope, &node.stx),
      Expr::TaggedTemplate(node) => self.eval_tagged_template(scope, &node.stx),
      Expr::This(_) => Ok(self.this),
      Expr::NewTarget(_) => Ok(self.new_target),
      Expr::Id(node) => self.eval_id(scope, &node.stx),
      Expr::ImportMeta(_) => self.eval_import_meta(scope),
      Expr::Call(node) => self.eval_call(scope, &node.stx),
      Expr::Import(node) => self.eval_import(scope, &node.stx),
      Expr::Func(node) => self.eval_func_expr(scope, node),
      Expr::ArrowFunc(node) => self.eval_arrow_func_expr(scope, node),
      Expr::Class(node) => self.eval_class_expr(scope, node),
      Expr::Member(node) => self.eval_member(scope, &node.stx),
      Expr::ComputedMember(node) => self.eval_computed_member(scope, &node.stx),
      Expr::Unary(node) => self.eval_unary(scope, &node.stx),
      Expr::UnaryPostfix(node) => self.eval_unary_postfix(scope, &node.stx),
      Expr::Binary(node) => self.eval_binary(scope, &node.stx),
      Expr::Cond(node) => self.eval_cond(scope, &node.stx),

      // Patterns sometimes show up in expression position (e.g. assignment targets). We only
      // support simple identifier patterns for now.
      Expr::IdPat(node) => self.eval_id_pat(scope, &node.stx),

      _ => Err(VmError::Unimplemented("expression type")),
    }
  }

  fn eval_import(&mut self, scope: &mut Scope<'_>, expr: &ImportExpr) -> Result<Value, VmError> {
    // Dynamic `import()` expression.
    //
    // This delegates to the spec-shaped implementation in `module_loading::start_dynamic_import`.
    // Evaluate the specifier expression, then the optional `options` argument.
    //
    // Root the intermediate values while evaluating the second argument and while entering the
    // module loading algorithm, which may allocate and trigger GC.
    let mut import_scope = scope.reborrow();
    let specifier = self.eval_expr(&mut import_scope, &expr.module)?;
    import_scope.push_root(specifier)?;

    let options = match expr.attributes.as_ref() {
      Some(options_expr) => {
        let v = self.eval_expr(&mut import_scope, options_expr)?;
        v
      }
      None => Value::Undefined,
    };
    import_scope.push_root(options)?;

    let modules_ptr = self.vm.module_graph_ptr().ok_or(VmError::Unimplemented(
      "dynamic import requires a module graph",
    ))?;
    // Safety: `Vm::module_graph_ptr` is only set by embeddings that ensure the graph outlives the
    // VM (see `Vm::set_module_graph` docs). `JsRuntime` stores the graph in a `Box`, so the pointer
    // remains stable even if the runtime is moved.
    let modules = unsafe { &mut *modules_ptr };

    crate::start_dynamic_import_with_host_and_hooks(
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

  fn eval_member(&mut self, scope: &mut Scope<'_>, expr: &MemberExpr) -> Result<Value, VmError> {
    let base = self.eval_expr(scope, &expr.left)?;
    if expr.optional_chaining && is_nullish(base) {
      return Ok(Value::Undefined);
    }

    // `MemberExpression` creates a property reference for any non-nullish base value. The actual
    // `ToObject` coercion happens when the reference is dereferenced (`GetValue` / `PutValue`) so
    // the original base value can be preserved as the call/receiver `this`.
    let mut key_scope = scope.reborrow();
    key_scope.push_root(base)?;
    let key_s = key_scope.alloc_string(&expr.right)?;
    let reference = Reference::Property {
      base,
      key: PropertyKey::from_string(key_s),
    };
    self.get_value_from_reference(&mut key_scope, &reference)
  }

  fn eval_computed_member(
    &mut self,
    scope: &mut Scope<'_>,
    expr: &ComputedMemberExpr,
  ) -> Result<Value, VmError> {
    let base = self.eval_expr(scope, &expr.object)?;
    if expr.optional_chaining && is_nullish(base) {
      return Ok(Value::Undefined);
    }

    // `ComputedMemberExpression` performs `ToPropertyKey` on the member expression (which may
    // allocate and invoke user code) before the base is coerced via `ToObject` when the reference
    // is dereferenced.
    let mut key_scope = scope.reborrow();
    key_scope.push_root(base)?;
    let member_value = self.eval_expr(&mut key_scope, &expr.member)?;
    key_scope.push_root(member_value)?;
    let key = self.to_property_key_operator(&mut key_scope, member_value)?;
    let reference = Reference::Property { base, key };
    self.get_value_from_reference(&mut key_scope, &reference)
  }

  fn eval_reference<'b>(
    &mut self,
    scope: &mut Scope<'_>,
    expr: &'b Node<Expr>,
  ) -> Result<Reference<'b>, VmError> {
    match &*expr.stx {
      Expr::Id(id) => Ok(Reference::Binding(&id.stx.name)),
      Expr::IdPat(id) => Ok(Reference::Binding(&id.stx.name)),
      Expr::Member(member) => {
        if member.stx.optional_chaining {
          return Err(VmError::Unimplemented("optional chaining member access"));
        }
        let base = self.eval_expr(scope, &member.stx.left)?;
        if is_nullish(base) {
          return Err(throw_type_error(
            self.vm,
            scope,
            "Cannot convert undefined or null to object",
          )?);
        }
        let mut key_scope = scope.reborrow();
        key_scope.push_root(base)?;
        let key_s = key_scope.alloc_string(&member.stx.right)?;
        Ok(Reference::Property {
          base,
          key: PropertyKey::from_string(key_s),
        })
      }
      Expr::ComputedMember(member) => {
        if member.stx.optional_chaining {
          return Err(VmError::Unimplemented(
            "optional chaining computed member access",
          ));
        }
        let base = self.eval_expr(scope, &member.stx.object)?;
        let mut key_scope = scope.reborrow();
        key_scope.push_root(base)?;
        let member_value = self.eval_expr(&mut key_scope, &member.stx.member)?;
        key_scope.push_root(member_value)?;
        let key = self.to_property_key_operator(&mut key_scope, member_value)?;
        if is_nullish(base) {
          return Err(throw_type_error(
            self.vm,
            &mut key_scope,
            "Cannot convert undefined or null to object",
          )?);
        }
        Ok(Reference::Property { base, key })
      }
      _ => Err(VmError::Unimplemented("expression is not a reference")),
    }
  }

  fn root_reference(
    &self,
    scope: &mut Scope<'_>,
    reference: &Reference<'_>,
  ) -> Result<(), VmError> {
    let Reference::Property { base, key } = *reference else {
      return Ok(());
    };
    // Root both base and key together so a GC triggered by root-stack growth cannot collect the
    // not-yet-pushed entry.
    let roots = [
      base,
      match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(s) => Value::Symbol(s),
      },
    ];
    scope.push_roots(&roots)?;
    Ok(())
  }

  fn get_value_from_reference(
    &mut self,
    scope: &mut Scope<'_>,
    reference: &Reference<'_>,
  ) -> Result<Value, VmError> {
    match *reference {
      Reference::Binding(name) => {
        match self.env.get(self.vm, self.host, self.hooks, scope, name)? {
          Some(v) => Ok(v),
          None => {
            let msg = format!("{name} is not defined");
            Err(throw_reference_error(self.vm, scope, &msg)?)
          }
        }
      }
      Reference::Property { base, key } => {
        let mut get_scope = scope.reborrow();
        self.root_reference(&mut get_scope, reference)?;
        let object = self.to_object_operator(&mut get_scope, base)?;
        // Root the boxed object so host hooks/accessors can allocate freely.
        get_scope.push_root(Value::Object(object))?;
        get_scope
          .ordinary_get_with_host_and_hooks(self.vm, self.host, self.hooks, object, key, base)
      }
    }
  }

  fn put_value_to_reference(
    &mut self,
    scope: &mut Scope<'_>,
    reference: &Reference<'_>,
    value: Value,
  ) -> Result<(), VmError> {
    match *reference {
      Reference::Binding(name) => self.env.set(
        self.vm,
        self.host,
        self.hooks,
        scope,
        name,
        value,
        self.strict,
      ),
      Reference::Property { base, key } => {
        let mut set_scope = scope.reborrow();
        self.root_reference(&mut set_scope, reference)?;
        // Root `value` across `ToObject(base)` in case boxing triggers a GC.
        set_scope.push_root(value)?;
        let object = self.to_object_operator(&mut set_scope, base)?;
        set_scope.push_root(Value::Object(object))?;
        let ok = set_scope.ordinary_set_with_host_and_hooks(
          self.vm, self.host, self.hooks, object, key, value, base,
        )?;
        if ok {
          Ok(())
        } else if self.strict {
          Err(throw_type_error(
            self.vm,
            &mut set_scope,
            "Cannot assign to read-only property",
          )?)
        } else {
          // Sloppy-mode assignment to a non-writable/non-extensible target fails silently.
          Ok(())
        }
      }
    }
  }

  fn maybe_set_anonymous_function_name_for_assignment(
    &mut self,
    scope: &mut Scope<'_>,
    reference: &Reference<'_>,
    value: Value,
  ) -> Result<(), VmError> {
    if !scope.heap().is_callable(value)? {
      return Ok(());
    }
    let Value::Object(func_obj) = value else {
      return Ok(());
    };

    let current_name = scope.heap().get_function_name(func_obj)?;
    if !scope
      .heap()
      .get_string(current_name)?
      .as_code_units()
      .is_empty()
    {
      return Ok(());
    }

    let key = match *reference {
      Reference::Binding(name) => {
        // Root the allocated key string: `set_function_name` may allocate and trigger GC while
        // pushing its own roots.
        let name_s = scope.alloc_string(name)?;
        scope.push_root(Value::String(name_s))?;
        PropertyKey::String(name_s)
      }
      Reference::Property { key, .. } => key,
    };

    crate::function_properties::set_function_name(scope, func_obj, key, None)?;
    Ok(())
  }

  fn eval_func_expr(
    &mut self,
    scope: &mut Scope<'_>,
    expr: &Node<FuncExpr>,
  ) -> Result<Value, VmError> {
    let name = expr.stx.name.as_ref().map(|n| n.stx.name.as_str());
    self.instantiate_function_expr(
      scope,
      expr.loc.start_u32(),
      expr.loc.end_u32(),
      name,
      &expr.stx.func.stx,
    )
  }

  fn eval_arrow_func_expr(
    &mut self,
    scope: &mut Scope<'_>,
    expr: &Node<ArrowFuncExpr>,
  ) -> Result<Value, VmError> {
    self.instantiate_arrow_function_expr(
      scope,
      expr.loc.start_u32(),
      expr.loc.end_u32(),
      &expr.stx.func.stx,
    )
  }

  fn instantiate_function_expr(
    &mut self,
    scope: &mut Scope<'_>,
    loc_start: u32,
    loc_end: u32,
    name: Option<&str>,
    func: &parse_js::ast::func::Func,
  ) -> Result<Value, VmError> {
    use crate::function::ThisMode;
    use crate::vm::EcmaFunctionKind;

    if func.generator {
      return Err(VmError::Unimplemented(if func.async_ {
        "async generator functions"
      } else {
        "generator functions"
      }));
    }
    let is_strict = self.strict
      || match &func.body {
        Some(FuncBody::Block(stmts)) => detect_use_strict_directive(stmts, || self.tick())?,
        Some(FuncBody::Expression(_)) => false,
        None => return Err(VmError::Unimplemented("function without body")),
      };
    let this_mode = if func.arrow {
      ThisMode::Lexical
    } else if is_strict {
      ThisMode::Strict
    } else {
      ThisMode::Global
    };

    let name_s = match name {
      Some(name) => scope.alloc_string(name)?,
      None => scope.alloc_string("")?,
    };
    let length = self.function_length(func)?;

    let rel_start = loc_start.saturating_sub(self.env.prefix_len());
    let rel_end = loc_end.saturating_sub(self.env.prefix_len());
    let span_start = self.env.base_offset().saturating_add(rel_start);
    let span_end = self.env.base_offset().saturating_add(rel_end);

    let code_id = self.vm.register_ecma_function(
      self.env.source(),
      span_start,
      span_end,
      EcmaFunctionKind::Expr,
    )?;
    let func_obj = scope.alloc_ecma_function(
      code_id,
      /* is_constructable */ !func.async_,
      name_s,
      length,
      this_mode,
      is_strict,
      Some(self.env.lexical_env),
    )?;
    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    scope
      .heap_mut()
      .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
    scope
      .heap_mut()
      .set_function_realm(func_obj, self.env.global_object())?;
    if let Some(realm) = self.vm.current_realm() {
      scope.heap_mut().set_function_job_realm(func_obj, realm)?;
    }
    if func.arrow {
      scope
        .heap_mut()
        .set_function_bound_this(func_obj, self.this)?;
      scope
        .heap_mut()
        .set_function_bound_new_target(func_obj, self.new_target)?;
    }
    Ok(Value::Object(func_obj))
  }

  fn instantiate_arrow_function_expr(
    &mut self,
    scope: &mut Scope<'_>,
    loc_start: u32,
    loc_end: u32,
    func: &parse_js::ast::func::Func,
  ) -> Result<Value, VmError> {
    use crate::function::ThisMode;
    use crate::vm::EcmaFunctionKind;

    if func.generator {
      return Err(VmError::Unimplemented(if func.async_ {
        "async generator functions"
      } else {
        "generator functions"
      }));
    }
    let is_strict = self.strict
      || match &func.body {
        Some(FuncBody::Block(stmts)) => detect_use_strict_directive(stmts, || self.tick())?,
        Some(FuncBody::Expression(_)) => false,
        None => return Err(VmError::Unimplemented("function without body")),
      };

    let length = self.function_length(func)?;

    let rel_start = loc_start.saturating_sub(self.env.prefix_len());
    let rel_end = loc_end.saturating_sub(self.env.prefix_len());
    let span_start = self.env.base_offset().saturating_add(rel_start);
    let span_end = self.env.base_offset().saturating_add(rel_end);

    let code_id = self.vm.register_ecma_function(
      self.env.source(),
      span_start,
      span_end,
      EcmaFunctionKind::Expr,
    )?;
    let mut alloc_scope = scope.reborrow();
    // Root captured lexical bindings across allocation in case it triggers GC.
    let roots = [self.this, self.new_target];
    alloc_scope.push_roots(&roots)?;
    let name_s = alloc_scope.alloc_string("")?;
    alloc_scope.push_root(Value::String(name_s))?;

    let func_obj = alloc_scope.alloc_ecma_function(
      code_id,
      false,
      name_s,
      length,
      ThisMode::Lexical,
      is_strict,
      Some(self.env.lexical_env),
    )?;
    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    alloc_scope
      .heap_mut()
      .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
    alloc_scope
      .heap_mut()
      .set_function_realm(func_obj, self.env.global_object())?;
    if let Some(realm) = self.vm.current_realm() {
      alloc_scope
        .heap_mut()
        .set_function_job_realm(func_obj, realm)?;
    }
    alloc_scope
      .heap_mut()
      .set_function_bound_this(func_obj, self.this)?;
    alloc_scope
      .heap_mut()
      .set_function_bound_new_target(func_obj, self.new_target)?;
    Ok(Value::Object(func_obj))
  }

  fn eval_lit_str(
    &mut self,
    scope: &mut Scope<'_>,
    node: &Node<LitStrExpr>,
  ) -> Result<Value, VmError> {
    let s = alloc_string_from_lit_str(scope, node)?;
    Ok(Value::String(s))
  }

  fn eval_lit_num(&self, expr: &LitNumExpr) -> Result<Value, VmError> {
    Ok(Value::Number(expr.value.0))
  }

  fn eval_lit_bigint(&self, expr: &LitBigIntExpr) -> Result<Value, VmError> {
    let Some(b) = JsBigInt::from_decimal_str(&expr.value) else {
      return Err(VmError::Unimplemented("BigInt literal out of range"));
    };
    Ok(Value::BigInt(b))
  }

  fn eval_lit_bool(&self, expr: &LitBoolExpr) -> Result<Value, VmError> {
    Ok(Value::Bool(expr.value))
  }

  fn eval_lit_template(
    &mut self,
    scope: &mut Scope<'_>,
    expr: &LitTemplateExpr,
  ) -> Result<Value, VmError> {
    // Untagged template literals evaluate by concatenating their parts after `ToString`
    // conversion of substitutions.
    let mut units: Vec<u16> = Vec::new();
    for part in &expr.parts {
      match part {
        LitTemplatePart::String(s) => {
          let len = s.encode_utf16().count();
          units.try_reserve(len).map_err(|_| VmError::OutOfMemory)?;
          units.extend(s.encode_utf16());
        }
        LitTemplatePart::Substitution(expr) => {
          let value = self.eval_expr(scope, expr)?;
          scope.push_root(value)?;
          let s = self.to_string_operator(scope, value)?;
          scope.push_root(Value::String(s))?;
          let js = scope.heap().get_string(s)?;
          units
            .try_reserve(js.len_code_units())
            .map_err(|_| VmError::OutOfMemory)?;
          units.extend_from_slice(js.as_code_units());
        }
      }
    }

    let s = scope.alloc_string_from_u16_vec(units)?;
    Ok(Value::String(s))
  }

  fn eval_tagged_template(
    &mut self,
    scope: &mut Scope<'_>,
    expr: &TaggedTemplateExpr,
  ) -> Result<Value, VmError> {
    // Compute `callee` and `this` similarly to `CallExpression` evaluation.
    let (callee_value, this_value) = match &*expr.function.stx {
      Expr::Member(member) if member.stx.optional_chaining => {
        let base = self.eval_expr(scope, &member.stx.left)?;
        if is_nullish(base) {
          // Optional chaining short-circuit on the base value.
          return Ok(Value::Undefined);
        }

        // Optional chaining member access: evaluate the property reference, preserving the base
        // value for the call `this`.
        let mut key_scope = scope.reborrow();
        key_scope.push_root(base)?;
        let key_s = key_scope.alloc_string(&member.stx.right)?;
        let reference = Reference::Property {
          base,
          key: PropertyKey::from_string(key_s),
        };
        let callee_value = self.get_value_from_reference(&mut key_scope, &reference)?;
        (callee_value, base)
      }
      Expr::ComputedMember(member) if member.stx.optional_chaining => {
        let base = self.eval_expr(scope, &member.stx.object)?;
        if is_nullish(base) {
          return Ok(Value::Undefined);
        }

        // Optional chaining computed member access: `ToPropertyKey` on the member value may
        // allocate and invoke user code. Only if the base is non-nullish do we proceed to
        // dereference the property reference.
        let mut key_scope = scope.reborrow();
        key_scope.push_root(base)?;
        let member_value = self.eval_expr(&mut key_scope, &member.stx.member)?;
        key_scope.push_root(member_value)?;
        let key = self.to_property_key_operator(&mut key_scope, member_value)?;
        let reference = Reference::Property { base, key };
        let callee_value = self.get_value_from_reference(&mut key_scope, &reference)?;
        (callee_value, base)
      }
      Expr::Member(_) | Expr::ComputedMember(_) | Expr::Id(_) | Expr::IdPat(_) => {
        let reference = self.eval_reference(scope, &expr.function)?;
        let this_value = match reference {
          Reference::Property { base, .. } => base,
          _ => Value::Undefined,
        };

        let mut callee_scope = scope.reborrow();
        self.root_reference(&mut callee_scope, &reference)?;
        let callee_value = self.get_value_from_reference(&mut callee_scope, &reference)?;
        (callee_value, this_value)
      }
      _ => {
        let callee_value = self.eval_expr(scope, &expr.function)?;
        (callee_value, Value::Undefined)
      }
    };

    // Root callee/this/args for the duration of the call.
    let mut call_scope = scope.reborrow();
    call_scope.push_roots(&[callee_value, this_value])?;

    let template_obj = self.create_template_object(&mut call_scope, &expr.parts)?;
    call_scope.push_root(Value::Object(template_obj))?;

    let mut args: Vec<Value> = Vec::new();
    // Pre-reserve based on the total part count (an upper bound on the number of substitutions) so
    // we don't need an extra unbudgeted scan over the parts list.
    let subst_count = expr.parts.len();
    args
      .try_reserve_exact(subst_count.saturating_add(1))
      .map_err(|_| VmError::OutOfMemory)?;
    args.push(Value::Object(template_obj));

    // Evaluate substitutions left-to-right.
    for part in &expr.parts {
      let LitTemplatePart::Substitution(sub_expr) = part else {
        continue;
      };
      let value = self.eval_expr(&mut call_scope, sub_expr)?;
      call_scope.push_root(value)?;
      args.push(value);
    }

    self.call(&mut call_scope, callee_value, this_value, &args)
  }

  fn create_template_object(
    &mut self,
    scope: &mut Scope<'_>,
    parts: &[LitTemplatePart],
  ) -> Result<GcObject, VmError> {
    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

    // Allocate with `length = 0` and let `CreateDataProperty` grow the array length as we define
    // indexed elements. This avoids an extra `O(N)` segment-count scan before we start ticking in
    // the main segment loop.
    let cooked = scope.alloc_array(0)?;
    scope.push_root(Value::Object(cooked))?;
    scope
      .heap_mut()
      .object_set_prototype(cooked, Some(intr.array_prototype()))?;

    let raw = scope.alloc_array(0)?;
    scope.push_root(Value::Object(raw))?;
    scope
      .heap_mut()
      .object_set_prototype(raw, Some(intr.array_prototype()))?;

    let mut idx: u32 = 0;
    for part in parts {
      let LitTemplatePart::String(s) = part else {
        continue;
      };

      // Per-segment tick: tagged templates can contain large numbers of segments, and creating the
      // template object involves allocation + property definition even when no nested expressions
      // are evaluated.
      self.tick()?;

      let mut elem_scope = scope.reborrow();
      let cooked_s = elem_scope.alloc_string(s)?;
      elem_scope.push_root(Value::String(cooked_s))?;
      let key_s = elem_scope.alloc_string(&idx.to_string())?;
      elem_scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);

      let ok = elem_scope.create_data_property(cooked, key, Value::String(cooked_s))?;
      if !ok {
        return Err(VmError::Unimplemented("CreateDataProperty returned false"));
      }
      let ok = elem_scope.create_data_property(raw, key, Value::String(cooked_s))?;
      if !ok {
        return Err(VmError::Unimplemented("CreateDataProperty returned false"));
      }

      idx = idx.saturating_add(1);
    }

    let raw_key_s = scope.alloc_string("raw")?;
    scope.push_root(Value::String(raw_key_s))?;
    scope.define_property(
      cooked,
      PropertyKey::from_string(raw_key_s),
      PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Object(raw),
          writable: false,
        },
      },
    )?;

    Ok(cooked)
  }

  fn eval_lit_arr(&mut self, scope: &mut Scope<'_>, expr: &LitArrExpr) -> Result<Value, VmError> {
    let mut arr_scope = scope.reborrow();
    let arr = arr_scope.alloc_array(0)?;
    arr_scope.push_root(Value::Object(arr))?;
    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    arr_scope
      .heap_mut()
      .object_set_prototype(arr, Some(intr.array_prototype()))?;

    let mut next_index: u32 = 0;
    for elem in &expr.elements {
      match elem {
        LitArrElem::Empty => {
          // Per-hole tick: `[,,,,]` can have arbitrarily many elements without any nested
          // expression evaluations.
          self.tick()?;
          next_index = next_index.saturating_add(1);
        }
        LitArrElem::Rest(rest_expr) => {
          let mut spread_scope = arr_scope.reborrow();
          let spread_value = self.eval_expr(&mut spread_scope, rest_expr)?;
          spread_scope.push_root(spread_value)?;

          let mut iter = iterator::get_iterator(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            &mut spread_scope,
            spread_value,
          )?;
          spread_scope.push_roots(&[iter.iterator, iter.next_method])?;

          while let Some(value) = iterator::iterator_step_value(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            &mut spread_scope,
            &mut iter,
          )? {
            // Per-spread-element tick: spreading large iterators should be budgeted even when the
            // iterator's `next()` is native/cheap.
            self.tick()?;

            let idx = next_index;
            next_index = next_index.saturating_add(1);

            let mut elem_scope = spread_scope.reborrow();
            elem_scope.push_root(value)?;
            let key_s = elem_scope.alloc_string(&idx.to_string())?;
            elem_scope.push_root(Value::String(key_s))?;
            let key = PropertyKey::from_string(key_s);
            let ok = elem_scope.create_data_property(arr, key, value)?;
            if !ok {
              return Err(VmError::Unimplemented("CreateDataProperty returned false"));
            }
          }
        }
        LitArrElem::Single(elem_expr) => {
          let idx = next_index;
          next_index = next_index.saturating_add(1);

          let mut elem_scope = arr_scope.reborrow();
          let value = self.eval_expr(&mut elem_scope, elem_expr)?;
          elem_scope.push_root(value)?;
          let key_s = elem_scope.alloc_string(&idx.to_string())?;
          elem_scope.push_root(Value::String(key_s))?;
          let key = PropertyKey::from_string(key_s);
          let ok = elem_scope.create_data_property(arr, key, value)?;
          if !ok {
            return Err(VmError::Unimplemented("CreateDataProperty returned false"));
          }
        }
      }
    }

    let length_key_s = arr_scope.alloc_string("length")?;
    let length_desc = PropertyDescriptor {
      enumerable: false,
      configurable: false,
      kind: PropertyKind::Data {
        value: Value::Number(next_index as f64),
        writable: true,
      },
    };
    arr_scope.define_property(arr, PropertyKey::from_string(length_key_s), length_desc)?;

    Ok(Value::Object(arr))
  }

  fn eval_lit_obj(&mut self, scope: &mut Scope<'_>, expr: &LitObjExpr) -> Result<Value, VmError> {
    let mut obj_scope = scope.reborrow();
    let obj = obj_scope.alloc_object()?;
    obj_scope.push_root(Value::Object(obj))?;
    let intr = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
    obj_scope
      .heap_mut()
      .object_set_prototype(obj, Some(intr.object_prototype()))?;

    for member in &expr.members {
      // Per-member tick: object literals can do significant work per member even when no nested
      // expressions are evaluated (e.g. shorthand props and methods/getters/setters with direct
      // keys).
      self.tick()?;

      let mut member_scope = obj_scope.reborrow();
      let member = &member.stx.typ;

      match member {
        ObjMemberType::Valued { key, val } => {
          let key_loc_start = match key {
            ClassOrObjKey::Direct(direct) => direct.loc.start_u32(),
            ClassOrObjKey::Computed(expr) => expr.loc.start_u32(),
          };
          let key = match key {
            ClassOrObjKey::Direct(direct) => {
              let key_s = if let Some(units) = literal_string_code_units(&direct.assoc) {
                member_scope.alloc_string_from_code_units(units)?
              } else if direct.stx.tt == TT::LiteralNumber {
                let n = direct
                  .stx
                  .key
                  .parse::<f64>()
                  .map_err(|_| VmError::Unimplemented("numeric literal property name parse"))?;
                member_scope.heap_mut().to_string(Value::Number(n))?
              } else {
                member_scope.alloc_string(&direct.stx.key)?
              };
              PropertyKey::from_string(key_s)
            }
            ClassOrObjKey::Computed(expr) => {
              let value = self.eval_expr(&mut member_scope, expr)?;
              member_scope.push_root(value)?;
              self.to_property_key_operator(&mut member_scope, value)?
            }
          };

          match key {
            PropertyKey::String(s) => member_scope.push_root(Value::String(s))?,
            PropertyKey::Symbol(s) => member_scope.push_root(Value::Symbol(s))?,
          };

          match val {
            ClassOrObjVal::Prop(Some(value_expr)) => {
              let value = self.eval_expr(&mut member_scope, value_expr)?;
              member_scope.push_root(value)?;
              let ok = member_scope.create_data_property(obj, key, value)?;
              if !ok {
                return Err(VmError::Unimplemented("CreateDataProperty returned false"));
              }
            }
            ClassOrObjVal::Prop(None) => {
              return Err(VmError::Unimplemented(
                "object literal property without initializer",
              ));
            }
            ClassOrObjVal::Method(method) => {
              let func_node = &method.stx.func;
              let length = self.function_length(&func_node.stx)?;

              let rel_start = key_loc_start.saturating_sub(self.env.prefix_len());
              let rel_end = func_node
                .loc
                .end_u32()
                .saturating_sub(self.env.prefix_len());
              let span_start = self.env.base_offset().saturating_add(rel_start);
              let span_end = self.env.base_offset().saturating_add(rel_end);
              let code = self.vm.register_ecma_function(
                self.env.source(),
                span_start,
                span_end,
                EcmaFunctionKind::ObjectMember,
              )?;

              let is_strict = self.strict
                || match &func_node.stx.body {
                  Some(FuncBody::Block(stmts)) => {
                    detect_use_strict_directive(stmts, || self.tick())?
                  }
                  Some(FuncBody::Expression(_)) => false,
                  None => return Err(VmError::Unimplemented("method without body")),
                };

              let this_mode = if func_node.stx.arrow {
                ThisMode::Lexical
              } else if is_strict {
                ThisMode::Strict
              } else {
                ThisMode::Global
              };

              let closure_env = Some(self.env.lexical_env);

              let name_string = match key {
                PropertyKey::String(s) => s,
                PropertyKey::Symbol(_) => member_scope.alloc_string("")?,
              };

              let func_obj = member_scope.alloc_ecma_function(
                code,
                /* is_constructable */ false,
                name_string,
                length,
                this_mode,
                is_strict,
                closure_env,
              )?;
              let intr = self
                .vm
                .intrinsics()
                .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
              member_scope
                .heap_mut()
                .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
              member_scope
                .heap_mut()
                .set_function_realm(func_obj, self.env.global_object())?;
              if let Some(realm) = self.vm.current_realm() {
                member_scope
                  .heap_mut()
                  .set_function_job_realm(func_obj, realm)?;
              }
              if func_node.stx.arrow {
                member_scope
                  .heap_mut()
                  .set_function_bound_this(func_obj, self.this)?;
                member_scope
                  .heap_mut()
                  .set_function_bound_new_target(func_obj, self.new_target)?;
              }
              member_scope.push_root(Value::Object(func_obj))?;

              // Methods use the property key as the function `name` if possible.
              if !matches!(key, PropertyKey::String(_)) {
                crate::function_properties::set_function_name(
                  &mut member_scope,
                  func_obj,
                  key,
                  None,
                )?;
              }

              let ok = member_scope.create_data_property(obj, key, Value::Object(func_obj))?;
              if !ok {
                return Err(VmError::Unimplemented("CreateDataProperty returned false"));
              }
            }
            ClassOrObjVal::Getter(getter) => {
              let func_node = &getter.stx.func;
              let length = self.function_length(&func_node.stx)?;

              let rel_start = key_loc_start.saturating_sub(self.env.prefix_len());
              let rel_end = func_node
                .loc
                .end_u32()
                .saturating_sub(self.env.prefix_len());
              let span_start = self.env.base_offset().saturating_add(rel_start);
              let span_end = self.env.base_offset().saturating_add(rel_end);
              let code = self.vm.register_ecma_function(
                self.env.source(),
                span_start,
                span_end,
                EcmaFunctionKind::ObjectMember,
              )?;

              let is_strict = self.strict
                || match &func_node.stx.body {
                  Some(FuncBody::Block(stmts)) => {
                    detect_use_strict_directive(stmts, || self.tick())?
                  }
                  Some(FuncBody::Expression(_)) => false,
                  None => return Err(VmError::Unimplemented("getter without body")),
                };

              let this_mode = if func_node.stx.arrow {
                ThisMode::Lexical
              } else if is_strict {
                ThisMode::Strict
              } else {
                ThisMode::Global
              };

              let closure_env = Some(self.env.lexical_env);

              let name_string = member_scope.alloc_string("")?;
              let func_obj = member_scope.alloc_ecma_function(
                code,
                /* is_constructable */ false,
                name_string,
                length,
                this_mode,
                is_strict,
                closure_env,
              )?;
              let intr = self
                .vm
                .intrinsics()
                .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
              member_scope
                .heap_mut()
                .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
              member_scope
                .heap_mut()
                .set_function_realm(func_obj, self.env.global_object())?;
              if let Some(realm) = self.vm.current_realm() {
                member_scope
                  .heap_mut()
                  .set_function_job_realm(func_obj, realm)?;
              }
              if func_node.stx.arrow {
                member_scope
                  .heap_mut()
                  .set_function_bound_this(func_obj, self.this)?;
                member_scope
                  .heap_mut()
                  .set_function_bound_new_target(func_obj, self.new_target)?;
              }
              member_scope.push_root(Value::Object(func_obj))?;
              crate::function_properties::set_function_name(
                &mut member_scope,
                func_obj,
                key,
                Some("get"),
              )?;

              let ok = member_scope.define_own_property(
                obj,
                key,
                PropertyDescriptorPatch {
                  get: Some(Value::Object(func_obj)),
                  enumerable: Some(true),
                  configurable: Some(true),
                  ..Default::default()
                },
              )?;
              if !ok {
                return Err(VmError::Unimplemented("DefineOwnProperty returned false"));
              }
            }
            ClassOrObjVal::Setter(setter) => {
              let func_node = &setter.stx.func;
              let length = self.function_length(&func_node.stx)?;

              let rel_start = key_loc_start.saturating_sub(self.env.prefix_len());
              let rel_end = func_node
                .loc
                .end_u32()
                .saturating_sub(self.env.prefix_len());
              let span_start = self.env.base_offset().saturating_add(rel_start);
              let span_end = self.env.base_offset().saturating_add(rel_end);
              let code = self.vm.register_ecma_function(
                self.env.source(),
                span_start,
                span_end,
                EcmaFunctionKind::ObjectMember,
              )?;

              let is_strict = self.strict
                || match &func_node.stx.body {
                  Some(FuncBody::Block(stmts)) => {
                    detect_use_strict_directive(stmts, || self.tick())?
                  }
                  Some(FuncBody::Expression(_)) => false,
                  None => return Err(VmError::Unimplemented("setter without body")),
                };

              let this_mode = if func_node.stx.arrow {
                ThisMode::Lexical
              } else if is_strict {
                ThisMode::Strict
              } else {
                ThisMode::Global
              };

              let closure_env = Some(self.env.lexical_env);

              let name_string = member_scope.alloc_string("")?;
              let func_obj = member_scope.alloc_ecma_function(
                code,
                /* is_constructable */ false,
                name_string,
                length,
                this_mode,
                is_strict,
                closure_env,
              )?;
              let intr = self
                .vm
                .intrinsics()
                .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
              member_scope
                .heap_mut()
                .object_set_prototype(func_obj, Some(intr.function_prototype()))?;
              member_scope
                .heap_mut()
                .set_function_realm(func_obj, self.env.global_object())?;
              if let Some(realm) = self.vm.current_realm() {
                member_scope
                  .heap_mut()
                  .set_function_job_realm(func_obj, realm)?;
              }
              if func_node.stx.arrow {
                member_scope
                  .heap_mut()
                  .set_function_bound_this(func_obj, self.this)?;
                member_scope
                  .heap_mut()
                  .set_function_bound_new_target(func_obj, self.new_target)?;
              }
              member_scope.push_root(Value::Object(func_obj))?;
              crate::function_properties::set_function_name(
                &mut member_scope,
                func_obj,
                key,
                Some("set"),
              )?;

              let ok = member_scope.define_own_property(
                obj,
                key,
                PropertyDescriptorPatch {
                  set: Some(Value::Object(func_obj)),
                  enumerable: Some(true),
                  configurable: Some(true),
                  ..Default::default()
                },
              )?;
              if !ok {
                return Err(VmError::Unimplemented("DefineOwnProperty returned false"));
              }
            }
            ClassOrObjVal::IndexSignature(_) => {
              return Err(VmError::Unimplemented("object literal index signature"));
            }
            ClassOrObjVal::StaticBlock(_) => {
              return Err(VmError::Unimplemented("object literal static block"));
            }
          }
        }
        ObjMemberType::Shorthand { id } => {
          let key_s = member_scope.alloc_string(&id.stx.name)?;
          member_scope.push_root(Value::String(key_s))?;
          let key = PropertyKey::from_string(key_s);
          let value = self.eval_id(&mut member_scope, &id.stx)?;
          member_scope.push_root(value)?;
          let ok = member_scope.create_data_property(obj, key, value)?;
          if !ok {
            return Err(VmError::Unimplemented("CreateDataProperty returned false"));
          }
        }
        ObjMemberType::Rest { val } => {
          let src_value = self.eval_expr(&mut member_scope, val)?;
          member_scope.push_root(src_value)?;

          let src_obj = match src_value {
            Value::Undefined | Value::Null => continue,
            Value::Object(o) => o,
            _ => return Err(VmError::Unimplemented("object spread source type")),
          };

          let keys = member_scope.ordinary_own_property_keys_with_tick(src_obj, || self.tick())?;
          for key in keys {
            // Per-copied-property tick: spreading a large object can be `O(N)` without evaluating
            // nested expressions.
            self.tick()?;

            let mut key_scope = member_scope.reborrow();
            key_scope.push_root(Value::Object(src_obj))?;
            match key {
              PropertyKey::String(s) => key_scope.push_root(Value::String(s))?,
              PropertyKey::Symbol(s) => key_scope.push_root(Value::Symbol(s))?,
            };

            let Some(desc) = key_scope.ordinary_get_own_property(src_obj, key)? else {
              continue;
            };
            if !desc.enumerable {
              continue;
            }

            let value = key_scope.ordinary_get_with_host_and_hooks(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              src_obj,
              key,
              Value::Object(src_obj),
            )?;
            key_scope.push_root(value)?;
            let ok = key_scope.create_data_property(obj, key, value)?;
            if !ok {
              return Err(VmError::Unimplemented("CreateDataProperty returned false"));
            }
          }
        }
      }
    }

    Ok(Value::Object(obj))
  }

  fn eval_import_meta(&mut self, scope: &mut Scope<'_>) -> Result<Value, VmError> {
    let Some(ScriptOrModule::Module(module)) = self.vm.get_active_script_or_module() else {
      return Err(VmError::Unimplemented("import.meta outside of modules"));
    };
    let obj = self
      .vm
      .get_or_create_import_meta_object(scope, self.hooks, module)?;
    Ok(Value::Object(obj))
  }

  fn eval_id(&mut self, scope: &mut Scope<'_>, expr: &IdExpr) -> Result<Value, VmError> {
    match self.env.get(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      scope,
      &expr.name,
    )? {
      Some(v) => Ok(v),
      None => {
        let msg = format!("{name} is not defined", name = expr.name);
        Err(throw_reference_error(self.vm, scope, &msg)?)
      }
    }
  }

  fn eval_id_pat(&mut self, scope: &mut Scope<'_>, expr: &IdPat) -> Result<Value, VmError> {
    match self.env.get(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      scope,
      &expr.name,
    )? {
      Some(v) => Ok(v),
      None => {
        let msg = format!("{name} is not defined", name = expr.name);
        Err(throw_reference_error(self.vm, scope, &msg)?)
      }
    }
  }

  fn eval_unary(&mut self, scope: &mut Scope<'_>, expr: &UnaryExpr) -> Result<Value, VmError> {
    match expr.operator {
      OperatorName::PrefixIncrement => self.eval_update_expression(scope, &expr.argument, 1, true),
      OperatorName::PrefixDecrement => self.eval_update_expression(scope, &expr.argument, -1, true),
      OperatorName::PostfixIncrement => {
        self.eval_update_expression(scope, &expr.argument, 1, false)
      }
      OperatorName::PostfixDecrement => {
        self.eval_update_expression(scope, &expr.argument, -1, false)
      }
      OperatorName::Delete => match &*expr.argument.stx {
        Expr::Id(id) => {
          if self.strict {
            return Err(syntax_error(
              expr.argument.loc,
              "Delete of an unqualified identifier in strict mode.",
            ));
          }

          // Sloppy-mode: deleting an unqualified identifier returns `true` if the reference is
          // unresolvable, otherwise it deletes the binding. Declarative env-record bindings are
          // not deletable, but `with`-introduced ObjectEnvironmentRecord bindings are deletable (as
          // they are backed by object properties).
          if let Some(env) = self.env.resolve_lexical_binding(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            scope,
            &id.stx.name,
          )? {
            match scope.heap().get_env_record(env)? {
              crate::env::EnvRecord::Declarative(_) => return Ok(Value::Bool(false)),
              crate::env::EnvRecord::Object(obj) => {
                let binding_object = obj.binding_object;
                let mut key_scope = scope.reborrow();
                key_scope.push_root(Value::Object(binding_object))?;
                let key_s = key_scope.alloc_string(&id.stx.name)?;
                key_scope.push_root(Value::String(key_s))?;
                let key = PropertyKey::from_string(key_s);
                return Ok(Value::Bool(key_scope.ordinary_delete_with_host_and_hooks(
                  self.vm,
                  &mut *self.host,
                  &mut *self.hooks,
                  binding_object,
                  key,
                )?));
              }
            }
          }

          let global_object = self.env.global_object;
          let mut key_scope = scope.reborrow();
          key_scope.push_root(Value::Object(global_object))?;
          let key_s = key_scope.alloc_string(&id.stx.name)?;
          key_scope.push_root(Value::String(key_s))?;
          let key = PropertyKey::from_string(key_s);

          if !key_scope.ordinary_has_property_with_tick(global_object, key, || self.tick())? {
            return Ok(Value::Bool(true));
          }

          Ok(Value::Bool(key_scope.ordinary_delete_with_host_and_hooks(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            global_object,
            key,
          )?))
        }
        Expr::IdPat(id) => {
          if self.strict {
            return Err(syntax_error(
              expr.argument.loc,
              "Delete of an unqualified identifier in strict mode.",
            ));
          }

          if let Some(env) = self.env.resolve_lexical_binding(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            scope,
            &id.stx.name,
          )? {
            match scope.heap().get_env_record(env)? {
              crate::env::EnvRecord::Declarative(_) => return Ok(Value::Bool(false)),
              crate::env::EnvRecord::Object(obj) => {
                let binding_object = obj.binding_object;
                let mut key_scope = scope.reborrow();
                key_scope.push_root(Value::Object(binding_object))?;
                let key_s = key_scope.alloc_string(&id.stx.name)?;
                key_scope.push_root(Value::String(key_s))?;
                let key = PropertyKey::from_string(key_s);
                return Ok(Value::Bool(key_scope.ordinary_delete_with_host_and_hooks(
                  self.vm,
                  &mut *self.host,
                  &mut *self.hooks,
                  binding_object,
                  key,
                )?));
              }
            }
          }

          let global_object = self.env.global_object;
          let mut key_scope = scope.reborrow();
          key_scope.push_root(Value::Object(global_object))?;
          let key_s = key_scope.alloc_string(&id.stx.name)?;
          key_scope.push_root(Value::String(key_s))?;
          let key = PropertyKey::from_string(key_s);

          if !key_scope.ordinary_has_property_with_tick(global_object, key, || self.tick())? {
            return Ok(Value::Bool(true));
          }

          Ok(Value::Bool(key_scope.ordinary_delete_with_host_and_hooks(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            global_object,
            key,
          )?))
        }
        Expr::Member(member) if member.stx.optional_chaining => {
          // `delete obj?.prop` short-circuits to `true` when the base is nullish.
          let base = self.eval_expr(scope, &member.stx.left)?;
          if is_nullish(base) {
            return Ok(Value::Bool(true));
          }

          let mut del_scope = scope.reborrow();
          del_scope.push_root(base)?;
          let key_s = del_scope.alloc_string(&member.stx.right)?;
          del_scope.push_root(Value::String(key_s))?;
          let key = PropertyKey::from_string(key_s);

          let object = self.to_object_operator(&mut del_scope, base)?;
          del_scope.push_root(Value::Object(object))?;
          Ok(Value::Bool(del_scope.ordinary_delete_with_host_and_hooks(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            object,
            key,
          )?))
        }
        Expr::ComputedMember(member) if member.stx.optional_chaining => {
          // `delete obj?.[expr]` short-circuits to `true` when the base is nullish and does not
          // evaluate the member expression.
          let base = self.eval_expr(scope, &member.stx.object)?;
          if is_nullish(base) {
            return Ok(Value::Bool(true));
          }

          let mut del_scope = scope.reborrow();
          del_scope.push_root(base)?;
          let member_value = self.eval_expr(&mut del_scope, &member.stx.member)?;
          del_scope.push_root(member_value)?;
          let key = self.to_property_key_operator(&mut del_scope, member_value)?;
          let key_root = match key {
            PropertyKey::String(s) => Value::String(s),
            PropertyKey::Symbol(s) => Value::Symbol(s),
          };
          del_scope.push_root(key_root)?;

          let object = self.to_object_operator(&mut del_scope, base)?;
          del_scope.push_root(Value::Object(object))?;
          Ok(Value::Bool(del_scope.ordinary_delete_with_host_and_hooks(
            self.vm,
            &mut *self.host,
            &mut *self.hooks,
            object,
            key,
          )?))
        }
        Expr::Member(_) | Expr::ComputedMember(_) => {
          let reference = self.eval_reference(scope, &expr.argument)?;
          match reference {
            Reference::Property { base, key } => {
              let mut del_scope = scope.reborrow();
              self.root_reference(&mut del_scope, &reference)?;
              let object = self.to_object_operator(&mut del_scope, base)?;
              del_scope.push_root(Value::Object(object))?;
              Ok(Value::Bool(del_scope.ordinary_delete_with_host_and_hooks(
                self.vm,
                &mut *self.host,
                &mut *self.hooks,
                object,
                key,
              )?))
            }
            // Deleting bindings (`delete x`) is handled above.
            Reference::Binding(_) => Ok(Value::Bool(false)),
          }
        }
        // `delete` of non-reference expressions always returns true (after evaluating the operand).
        _ => {
          let _ = self.eval_expr(scope, &expr.argument)?;
          Ok(Value::Bool(true))
        }
      },
      OperatorName::LogicalNot => {
        let argument = self.eval_expr(scope, &expr.argument)?;
        Ok(Value::Bool(!to_boolean(scope.heap(), argument)?))
      }
      OperatorName::UnaryPlus => {
        let argument = self.eval_expr(scope, &expr.argument)?;
        let n = self.to_number_operator(scope, argument)?;
        Ok(Value::Number(n))
      }
      OperatorName::UnaryNegation => {
        let argument = self.eval_expr(scope, &expr.argument)?;
        let num = self.to_numeric(scope, argument)?;
        Ok(match num {
          NumericValue::Number(n) => Value::Number(-n),
          NumericValue::BigInt(b) => Value::BigInt(b.negate()),
        })
      }
      OperatorName::BitwiseNot => {
        let argument = self.eval_expr(scope, &expr.argument)?;
        let num = self.to_numeric(scope, argument)?;
        Ok(match num {
          NumericValue::Number(n) => Value::Number((!to_int32(n)) as f64),
          NumericValue::BigInt(b) => {
            let Some(out) = b.checked_bitwise_not() else {
              return Err(VmError::Unimplemented("BigInt bitwise not out of range"));
            };
            Value::BigInt(out)
          }
        })
      }
      OperatorName::Typeof => {
        let argument = match &*expr.argument.stx {
          Expr::Id(id) => {
            // Preserve the `typeof` special-case: unresolvable identifiers yield `"undefined"`
            // instead of throwing a ReferenceError.
            self.tick()?;
            self
              .env
              .get(
                self.vm,
                &mut *self.host,
                &mut *self.hooks,
                scope,
                &id.stx.name,
              )?
              .unwrap_or(Value::Undefined)
          }
          Expr::IdPat(id) => {
            self.tick()?;
            self
              .env
              .get(
                self.vm,
                &mut *self.host,
                &mut *self.hooks,
                scope,
                &id.stx.name,
              )?
              .unwrap_or(Value::Undefined)
          }
          _ => self.eval_expr(scope, &expr.argument)?,
        };
        let t = typeof_name(scope.heap(), argument)?;
        let s = scope.alloc_string(t)?;
        Ok(Value::String(s))
      }
      OperatorName::Void => {
        let _ = self.eval_expr(scope, &expr.argument)?;
        Ok(Value::Undefined)
      }
      OperatorName::New => {
        // `parse-js` represents `new f()` as a `UnaryExpr` whose argument is a `CallExpr`.
        let (callee_expr, call_args) = match &*expr.argument.stx {
          Expr::Call(call) => (&call.stx.callee, Some(&call.stx.arguments)),
          _ => (&expr.argument, None),
        };

        let mut new_scope = scope.reborrow();
        let callee = self.eval_expr(&mut new_scope, callee_expr)?;
        new_scope.push_root(callee)?;

        let mut args: Vec<Value> = Vec::new();
        if let Some(call_args) = call_args {
          for arg in call_args {
            if arg.stx.spread {
              let spread_value = self.eval_expr(&mut new_scope, &arg.stx.value)?;
              new_scope.push_root(spread_value)?;

              let mut iter = iterator::get_iterator(
                self.vm,
                &mut *self.host,
                &mut *self.hooks,
                &mut new_scope,
                spread_value,
              )?;
              new_scope.push_root(iter.iterator)?;
              new_scope.push_root(iter.next_method)?;

              while let Some(value) = iterator::iterator_step_value(
                self.vm,
                &mut *self.host,
                &mut *self.hooks,
                &mut new_scope,
                &mut iter,
              )? {
                self.tick()?;
                new_scope.push_root(value)?;
                args.push(value);
              }
            } else {
              let value = self.eval_expr(&mut new_scope, &arg.stx.value)?;
              new_scope.push_root(value)?;
              args.push(value);
            }
          }
        }

        // For `new`, the `newTarget` is the same as the constructor.
        match self.construct(&mut new_scope, callee, &args, callee) {
          Ok(v) => Ok(v),
          Err(VmError::NotConstructable) => Err(throw_type_error(
            self.vm,
            &mut new_scope,
            "Value is not a constructor",
          )?),
          Err(err) => Err(err),
        }
      }
      _ => Err(VmError::Unimplemented("unary operator")),
    }
  }

  fn eval_unary_postfix(
    &mut self,
    scope: &mut Scope<'_>,
    expr: &UnaryPostfixExpr,
  ) -> Result<Value, VmError> {
    match expr.operator {
      OperatorName::PostfixIncrement => {
        self.eval_update_expression(scope, &expr.argument, 1, false)
      }
      OperatorName::PostfixDecrement => {
        self.eval_update_expression(scope, &expr.argument, -1, false)
      }
      _ => Err(VmError::Unimplemented("postfix unary operator")),
    }
  }

  fn eval_update_expression(
    &mut self,
    scope: &mut Scope<'_>,
    argument: &Node<Expr>,
    delta: i8,
    prefix: bool,
  ) -> Result<Value, VmError> {
    let reference = self.eval_reference(scope, argument)?;
    let mut update_scope = scope.reborrow();
    self.root_reference(&mut update_scope, &reference)?;

    let old_value = self.get_value_from_reference(&mut update_scope, &reference)?;
    update_scope.push_root(old_value)?;

    let old_numeric = self.to_numeric(&mut update_scope, old_value)?;
    let delta_bigint = if delta >= 0 {
      JsBigInt::from_u128(delta as u128)
    } else {
      JsBigInt::from_u128((-delta) as u128).negate()
    };

    let (old_out, new_value) = match old_numeric {
      NumericValue::Number(n) => {
        let new_n = n + f64::from(delta);
        (Value::Number(n), Value::Number(new_n))
      }
      NumericValue::BigInt(b) => {
        let Some(out) = b.checked_add(delta_bigint) else {
          return Err(VmError::Unimplemented(
            "BigInt increment/decrement overflow",
          ));
        };
        (Value::BigInt(b), Value::BigInt(out))
      }
    };

    update_scope.push_root(new_value)?;
    self.put_value_to_reference(&mut update_scope, &reference, new_value)?;
    if prefix {
      Ok(new_value)
    } else {
      Ok(old_out)
    }
  }

  fn eval_call(&mut self, scope: &mut Scope<'_>, expr: &CallExpr) -> Result<Value, VmError> {
    // Special-case direct `eval(...)` calls.
    //
    // `vm-js` does not yet expose a global `eval` builtin, but unit tests (and real-world scripts)
    // expect `eval("...")` to work. Treat an identifier call as direct eval and interpret the
    // argument string in the current environment.
    //
    // This is intentionally minimal but spec-shaped:
    // - Arguments are evaluated left-to-right (including spreads).
    // - Non-String inputs are returned unchanged.
    if !expr.optional_chaining {
      match &*expr.callee.stx {
        Expr::Id(id) if id.stx.name == "eval" => return self.eval_direct_eval(scope, expr),
        Expr::IdPat(id) if id.stx.name == "eval" => return self.eval_direct_eval(scope, expr),
        _ => {}
      }
    }

    // Evaluate the callee and compute the `this` value for the call.
    let (callee_value, this_value) = match &*expr.callee.stx {
      Expr::Member(member) if member.stx.optional_chaining => {
        let base = self.eval_expr(scope, &member.stx.left)?;
        if is_nullish(base) {
          // Optional chaining short-circuit on the base value.
          return Ok(Value::Undefined);
        }

        // Optional chaining member call: preserve the base value for the call `this` binding.
        let mut key_scope = scope.reborrow();
        key_scope.push_root(base)?;
        let key_s = key_scope.alloc_string(&member.stx.right)?;
        let reference = Reference::Property {
          base,
          key: PropertyKey::from_string(key_s),
        };
        let callee_value = self.get_value_from_reference(&mut key_scope, &reference)?;
        (callee_value, base)
      }
      Expr::ComputedMember(member) if member.stx.optional_chaining => {
        let base = self.eval_expr(scope, &member.stx.object)?;
        if is_nullish(base) {
          return Ok(Value::Undefined);
        }

        // Optional chaining computed-member call: `ToPropertyKey` may allocate and invoke user
        // code. Only if the base is non-nullish do we dereference the property reference.
        let mut key_scope = scope.reborrow();
        key_scope.push_root(base)?;
        let member_value = self.eval_expr(&mut key_scope, &member.stx.member)?;
        key_scope.push_root(member_value)?;
        let key = self.to_property_key_operator(&mut key_scope, member_value)?;
        let reference = Reference::Property { base, key };
        let callee_value = self.get_value_from_reference(&mut key_scope, &reference)?;
        (callee_value, base)
      }
      Expr::Member(_) | Expr::ComputedMember(_) | Expr::Id(_) | Expr::IdPat(_) => {
        let reference = self.eval_reference(scope, &expr.callee)?;
        let this_value = match reference {
          Reference::Property { base, .. } => base,
          _ => Value::Undefined,
        };

        let mut callee_scope = scope.reborrow();
        self.root_reference(&mut callee_scope, &reference)?;
        let callee_value = self.get_value_from_reference(&mut callee_scope, &reference)?;
        (callee_value, this_value)
      }
      _ => {
        let callee_value = self.eval_expr(scope, &expr.callee)?;
        (callee_value, Value::Undefined)
      }
    };

    // Optional call: if the callee is nullish, return `undefined` without evaluating args.
    if expr.optional_chaining && is_nullish(callee_value) {
      return Ok(Value::Undefined);
    }

    // Root callee/this/args for the duration of the call.
    let mut call_scope = scope.reborrow();
    call_scope.push_roots(&[callee_value, this_value])?;

    let mut args: Vec<Value> = Vec::new();
    args
      .try_reserve_exact(expr.arguments.len())
      .map_err(|_| VmError::OutOfMemory)?;
    for arg in &expr.arguments {
      if arg.stx.spread {
        let spread_value = self.eval_expr(&mut call_scope, &arg.stx.value)?;
        call_scope.push_root(spread_value)?;

        let mut iter = iterator::get_iterator(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut call_scope,
          spread_value,
        )?;
        call_scope.push_roots(&[iter.iterator, iter.next_method])?;

        while let Some(value) = iterator::iterator_step_value(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut call_scope,
          &mut iter,
        )? {
          self.tick()?;
          call_scope.push_root(value)?;
          args.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
          args.push(value);
        }
      } else {
        let value = self.eval_expr(&mut call_scope, &arg.stx.value)?;
        call_scope.push_root(value)?;
        args.push(value);
      }
    }
    self.vm.call_with_host_and_hooks(
      &mut *self.host,
      &mut call_scope,
      &mut *self.hooks,
      callee_value,
      this_value,
      &args,
    )
  }

  fn eval_direct_eval(&mut self, scope: &mut Scope<'_>, expr: &CallExpr) -> Result<Value, VmError> {
    let mut call_scope = scope.reborrow();
    let mut args: Vec<Value> = Vec::new();
    args
      .try_reserve_exact(expr.arguments.len())
      .map_err(|_| VmError::OutOfMemory)?;

    for arg in &expr.arguments {
      if arg.stx.spread {
        let spread_value = self.eval_expr(&mut call_scope, &arg.stx.value)?;
        call_scope.push_root(spread_value)?;

        let mut iter = iterator::get_iterator(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut call_scope,
          spread_value,
        )?;
        call_scope.push_roots(&[iter.iterator, iter.next_method])?;

        while let Some(value) = iterator::iterator_step_value(
          self.vm,
          &mut *self.host,
          &mut *self.hooks,
          &mut call_scope,
          &mut iter,
        )? {
          self.tick()?;
          call_scope.push_root(value)?;
          args.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
          args.push(value);
        }
      } else {
        let value = self.eval_expr(&mut call_scope, &arg.stx.value)?;
        call_scope.push_root(value)?;
        args.push(value);
      }
    }

    let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);
    match arg0 {
      Value::String(s) => self.eval_direct_eval_string(&mut call_scope, s),
      other => Ok(other),
    }
  }

  fn eval_direct_eval_string(
    &mut self,
    scope: &mut Scope<'_>,
    source_string: GcString,
  ) -> Result<Value, VmError> {
    let source = scope.heap().get_string(source_string)?.to_utf8_lossy();

    let source = Arc::new(SourceText::new("<eval>", source));
    let opts = ParseOptions {
      dialect: Dialect::Ecma,
      source_type: SourceType::Script,
    };
    let top = self.vm.parse_top_level_with_budget(&source.text, opts)?;
    let strict = self.strict || detect_use_strict_directive(&top.stx.body, || self.tick())?;

    // Save and restore the runtime's source and lexical environment while running eval code. This
    // keeps nested function source spans aligned with the eval input.
    let prev_source = self.env.source();
    let prev_base_offset = self.env.base_offset();
    let prev_prefix_len = self.env.prefix_len();
    let prev_lexical_env = self.env.lexical_env;
    let prev_strict = self.strict;

    self.env.set_source_info(source.clone(), 0, 0);
    let eval_lex = scope.env_create(Some(prev_lexical_env))?;
    self.env.set_lexical_env(scope.heap_mut(), eval_lex);
    self.strict = strict;

    let result = (|| {
      self.instantiate_script(scope, &top.stx.body)?;

      let completion = self.eval_stmt_list(scope, &top.stx.body)?;
      match completion {
        Completion::Normal(v) => Ok(v.unwrap_or(Value::Undefined)),
        Completion::Throw(thrown) => Err(VmError::ThrowWithStack {
          value: thrown.value,
          stack: thrown.stack,
        }),
        Completion::Return(_) => Err(VmError::Unimplemented("return in eval")),
        Completion::Break(..) => Err(VmError::Unimplemented("break in eval")),
        Completion::Continue(..) => Err(VmError::Unimplemented("continue in eval")),
      }
    })();

    self.strict = prev_strict;
    self.env.set_lexical_env(scope.heap_mut(), prev_lexical_env);
    self
      .env
      .set_source_info(prev_source, prev_base_offset, prev_prefix_len);

    result
  }

  fn eval_cond(&mut self, scope: &mut Scope<'_>, expr: &CondExpr) -> Result<Value, VmError> {
    let test = self.eval_expr(scope, &expr.test)?;
    if to_boolean(scope.heap(), test)? {
      self.eval_expr(scope, &expr.consequent)
    } else {
      self.eval_expr(scope, &expr.alternate)
    }
  }

  fn eval_binary(&mut self, scope: &mut Scope<'_>, expr: &BinaryExpr) -> Result<Value, VmError> {
    match expr.operator {
      OperatorName::Assignment => {
        // Destructuring assignment patterns appear in expression position as `Expr::ObjPat` /
        // `Expr::ArrPat` nodes. These are not valid "references" and must be handled by pattern
        // binding.
        match &*expr.left.stx {
          Expr::ObjPat(_) | Expr::ArrPat(_) => {
            let value = self.eval_expr(scope, &expr.right)?;
            bind_assignment_target(
              self.vm,
              &mut *self.host,
              &mut *self.hooks,
              scope,
              self.env,
              &expr.left,
              value,
              self.strict,
              self.this,
            )?;
            Ok(value)
          }
          _ => {
            let reference = self.eval_reference(scope, &expr.left)?;
            let mut rhs_scope = scope.reborrow();
            self.root_reference(&mut rhs_scope, &reference)?;
            let value = self.eval_expr(&mut rhs_scope, &expr.right)?;
            rhs_scope.push_root(value)?;
            self.maybe_set_anonymous_function_name_for_assignment(&mut rhs_scope, &reference, value)?;
            self.put_value_to_reference(&mut rhs_scope, &reference, value)?;
            Ok(value)
          }
        }
      }
      OperatorName::AssignmentAddition => match &*expr.left.stx {
        Expr::ObjPat(_) | Expr::ArrPat(_) => Err(VmError::Unimplemented(
          "assignment addition to destructuring patterns",
        )),
        _ => {
          let reference = self.eval_reference(scope, &expr.left)?;
          let mut op_scope = scope.reborrow();
          self.root_reference(&mut op_scope, &reference)?;

          let left = self.get_value_from_reference(&mut op_scope, &reference)?;
          // Root `left` across evaluation of the RHS in case it allocates and triggers GC.
          op_scope.push_root(left)?;
          let right = self.eval_expr(&mut op_scope, &expr.right)?;

          let value = self.addition_operator(&mut op_scope, left, right)?;
          op_scope.push_root(value)?;
          self.put_value_to_reference(&mut op_scope, &reference, value)?;
          Ok(value)
        }
      },
      OperatorName::LogicalAnd => {
        let left = self.eval_expr(scope, &expr.left)?;
        if !to_boolean(scope.heap(), left)? {
          return Ok(left);
        }
        self.eval_expr(scope, &expr.right)
      }
      OperatorName::LogicalOr => {
        let left = self.eval_expr(scope, &expr.left)?;
        if to_boolean(scope.heap(), left)? {
          return Ok(left);
        }
        self.eval_expr(scope, &expr.right)
      }
      OperatorName::NullishCoalescing => {
        let left = self.eval_expr(scope, &expr.left)?;
        if is_nullish(left) {
          self.eval_expr(scope, &expr.right)
        } else {
          Ok(left)
        }
      }
      OperatorName::StrictEquality => {
        let left = self.eval_expr(scope, &expr.left)?;
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(left)?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        Ok(Value::Bool(strict_equal(rhs_scope.heap(), left, right)?))
      }
      OperatorName::StrictInequality => {
        let left = self.eval_expr(scope, &expr.left)?;
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(left)?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        Ok(Value::Bool(!strict_equal(rhs_scope.heap(), left, right)?))
      }
      OperatorName::Equality => {
        let left = self.eval_expr(scope, &expr.left)?;
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(left)?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        Ok(Value::Bool(abstract_equality(
          rhs_scope.heap_mut(),
          left,
          right,
        )?))
      }
      OperatorName::Inequality => {
        let left = self.eval_expr(scope, &expr.left)?;
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(left)?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        Ok(Value::Bool(!abstract_equality(
          rhs_scope.heap_mut(),
          left,
          right,
        )?))
      }
      OperatorName::In => {
        let left = self.eval_expr(scope, &expr.left)?;
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(left)?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        let Value::Object(obj) = right else {
          return Err(throw_type_error(
            self.vm,
            &mut rhs_scope,
            "Right-hand side of 'in' should be an object",
          )?);
        };

        // Root the RHS object across `ToPropertyKey`, which may allocate and trigger GC.
        rhs_scope.push_root(Value::Object(obj))?;
        let key = self.to_property_key_operator(&mut rhs_scope, left)?;
        Ok(Value::Bool(rhs_scope.ordinary_has_property_with_tick(
          obj,
          key,
          || self.tick(),
        )?))
      }
      OperatorName::Instanceof => {
        let left = self.eval_expr(scope, &expr.left)?;
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(left)?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        rhs_scope.push_root(right)?;
        Ok(Value::Bool(self.instanceof_operator(
          &mut rhs_scope,
          left,
          right,
        )?))
      }
      OperatorName::Addition => {
        let left = self.eval_expr(scope, &expr.left)?;
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(left)?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        self.addition_operator(&mut rhs_scope, left, right)
      }
      OperatorName::Multiplication => {
        let left = self.eval_expr(scope, &expr.left)?;
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(left)?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        rhs_scope.push_root(right)?;

        let left_num = self.to_numeric(&mut rhs_scope, left)?;
        let right_num = self.to_numeric(&mut rhs_scope, right)?;
        match (left_num, right_num) {
          (NumericValue::Number(a), NumericValue::Number(b)) => Ok(Value::Number(a * b)),
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let Some(out) = a.checked_mul(b) else {
              return Err(VmError::Unimplemented("BigInt multiplication overflow"));
            };
            Ok(Value::BigInt(out))
          }
          _ => Err(throw_type_error(
            self.vm,
            &mut rhs_scope,
            "Cannot mix BigInt and other types",
          )?),
        }
      }
      OperatorName::BitwiseAnd | OperatorName::BitwiseOr | OperatorName::BitwiseXor => {
        let left = self.eval_expr(scope, &expr.left)?;
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(left)?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        rhs_scope.push_root(right)?;

        let left_num = self.to_numeric(&mut rhs_scope, left)?;
        let right_num = self.to_numeric(&mut rhs_scope, right)?;
        match (left_num, right_num) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            let a = to_int32(a);
            let b = to_int32(b);
            let out = match expr.operator {
              OperatorName::BitwiseAnd => a & b,
              OperatorName::BitwiseOr => a | b,
              OperatorName::BitwiseXor => a ^ b,
              _ => unreachable!(),
            };
            Ok(Value::Number(out as f64))
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            let out = match expr.operator {
              OperatorName::BitwiseAnd => a.checked_bitwise_and(b),
              OperatorName::BitwiseOr => a.checked_bitwise_or(b),
              OperatorName::BitwiseXor => a.checked_bitwise_xor(b),
              _ => unreachable!(),
            };
            let Some(out) = out else {
              return Err(VmError::Unimplemented("BigInt bitwise out of range"));
            };
            Ok(Value::BigInt(out))
          }
          _ => Err(throw_type_error(
            self.vm,
            &mut rhs_scope,
            "Cannot mix BigInt and other types",
          )?),
        }
      }
      OperatorName::BitwiseLeftShift
      | OperatorName::BitwiseRightShift
      | OperatorName::BitwiseUnsignedRightShift => {
        let left = self.eval_expr(scope, &expr.left)?;
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(left)?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        rhs_scope.push_root(right)?;

        let left_num = self.to_numeric(&mut rhs_scope, left)?;
        let right_num = self.to_numeric(&mut rhs_scope, right)?;
        match (left_num, right_num) {
          (NumericValue::Number(a), NumericValue::Number(b)) => {
            let shift = (to_uint32(b) & 0x1f) as u32;
            match expr.operator {
              OperatorName::BitwiseLeftShift => {
                let a = to_int32(a);
                Ok(Value::Number(a.wrapping_shl(shift) as f64))
              }
              OperatorName::BitwiseRightShift => {
                let a = to_int32(a);
                Ok(Value::Number(a.wrapping_shr(shift) as f64))
              }
              OperatorName::BitwiseUnsignedRightShift => {
                let a = to_uint32(a);
                Ok(Value::Number((a >> shift) as f64))
              }
              _ => unreachable!(),
            }
          }
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            if matches!(expr.operator, OperatorName::BitwiseUnsignedRightShift) {
              return Err(throw_type_error(
                self.vm,
                &mut rhs_scope,
                "BigInt does not support unsigned right shift",
              )?);
            }

            let (shift_negative, shift) = match b.try_to_i128() {
              Some(shift_i) => {
                let shift_mag: u128 = if shift_i == i128::MIN {
                  1u128 << 127
                } else if shift_i < 0 {
                  (-shift_i) as u128
                } else {
                  shift_i as u128
                };
                let shift = u32::try_from(shift_mag).unwrap_or(u32::MAX).min(256);
                (shift_i < 0, shift)
              }
              None => (b.is_negative(), 256),
            };

            match expr.operator {
              OperatorName::BitwiseLeftShift => {
                if shift_negative {
                  Ok(Value::BigInt(a.shr(shift)))
                } else {
                  let Some(out) = a.checked_shl(shift) else {
                    return Err(VmError::Unimplemented("BigInt left shift overflow"));
                  };
                  Ok(Value::BigInt(out))
                }
              }
              OperatorName::BitwiseRightShift => {
                if shift_negative {
                  let Some(out) = a.checked_shl(shift) else {
                    return Err(VmError::Unimplemented("BigInt left shift overflow"));
                  };
                  Ok(Value::BigInt(out))
                } else {
                  Ok(Value::BigInt(a.shr(shift)))
                }
              }
              OperatorName::BitwiseUnsignedRightShift => unreachable!(),
              _ => unreachable!(),
            }
          }
          _ => Err(throw_type_error(
            self.vm,
            &mut rhs_scope,
            "Cannot mix BigInt and other types",
          )?),
        }
      }
      OperatorName::Subtraction | OperatorName::Division | OperatorName::Remainder => {
        let left = self.eval_expr(scope, &expr.left)?;
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(left)?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        rhs_scope.push_root(right)?;

        let left_num = self.to_numeric(&mut rhs_scope, left)?;
        let right_num = self.to_numeric(&mut rhs_scope, right)?;

        match (left_num, right_num) {
          (NumericValue::Number(a), NumericValue::Number(b)) => Ok(match expr.operator {
            OperatorName::Subtraction => Value::Number(a - b),
            OperatorName::Division => Value::Number(a / b),
            OperatorName::Remainder => Value::Number(a % b),
            _ => unreachable!(),
          }),
          (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
            if b.is_zero()
              && matches!(
                expr.operator,
                OperatorName::Division | OperatorName::Remainder
              )
            {
              return Err(throw_range_error(
                self.vm,
                &mut rhs_scope,
                "Division by zero",
              )?);
            }

            let out = match expr.operator {
              OperatorName::Subtraction => a
                .checked_sub(b)
                .ok_or(VmError::Unimplemented("BigInt subtraction overflow"))?,
              OperatorName::Division => a
                .checked_div(b)
                .ok_or(VmError::InvariantViolation("BigInt division returned None"))?,
              OperatorName::Remainder => a.checked_rem(b).ok_or(VmError::InvariantViolation(
                "BigInt remainder returned None",
              ))?,
              _ => unreachable!(),
            };
            Ok(Value::BigInt(out))
          }
          _ => Err(throw_type_error(
            self.vm,
            &mut rhs_scope,
            "Cannot mix BigInt and other types",
          )?),
        }
      }
      OperatorName::LessThan
      | OperatorName::LessThanOrEqual
      | OperatorName::GreaterThan
      | OperatorName::GreaterThanOrEqual => {
        let left = self.eval_expr(scope, &expr.left)?;
        // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
        let mut rhs_scope = scope.reborrow();
        rhs_scope.push_root(left)?;
        let right = self.eval_expr(&mut rhs_scope, &expr.right)?;
        // Root `right` for the duration of numeric conversion: `ToNumber` may allocate when called
        // on objects (via `ToPrimitive`).
        rhs_scope.push_root(right)?;
        let left_n = self.to_number_operator(&mut rhs_scope, left)?;
        let right_n = self.to_number_operator(&mut rhs_scope, right)?;

        Ok(match expr.operator {
          OperatorName::LessThan => Value::Bool(left_n < right_n),
          OperatorName::LessThanOrEqual => Value::Bool(left_n <= right_n),
          OperatorName::GreaterThan => Value::Bool(left_n > right_n),
          OperatorName::GreaterThanOrEqual => Value::Bool(left_n >= right_n),
          _ => {
            debug_assert!(false, "unexpected operator in numeric binary op fast path");
            return Err(VmError::InvariantViolation(
              "internal error: unexpected operator in numeric binary op",
            ));
          }
        })
      }
      OperatorName::Comma => {
        let _ = self.eval_expr(scope, &expr.left)?;
        self.eval_expr(scope, &expr.right)
      }
      _ => Err(VmError::Unimplemented("binary operator")),
    }
  }

  fn instanceof_operator(
    &mut self,
    scope: &mut Scope<'_>,
    object: Value,
    constructor: Value,
  ) -> Result<bool, VmError> {
    // Root inputs for the duration of the operation: `instanceof` may allocate when performing
    // `GetMethod`/`Get`/`Call`.
    let mut scope = scope.reborrow();
    scope.push_root(object)?;
    scope.push_root(constructor)?;

    // 1. `C` must be callable.
    if !scope.heap().is_callable(constructor)? {
      return Err(throw_type_error(
        self.vm,
        &mut scope,
        "Right-hand side of 'instanceof' is not callable",
      )?);
    }
    let Value::Object(constructor_obj) = constructor else {
      // `Heap::is_callable` returning true for a non-object would be an internal bug.
      return Err(VmError::InvariantViolation(
        "instanceof: is_callable returned true for non-object",
      ));
    };

    // 2. GetMethod(C, @@hasInstance).
    let has_instance_sym = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?
      .well_known_symbols()
      .has_instance;
    let has_instance_key = PropertyKey::from_symbol(has_instance_sym);
    let method = {
      let value = scope.ordinary_get_with_host_and_hooks(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        constructor_obj,
        has_instance_key,
        Value::Object(constructor_obj),
      )?;
      if matches!(value, Value::Undefined | Value::Null) {
        None
      } else if !scope.heap().is_callable(value)? {
        return Err(throw_type_error(
          self.vm,
          &mut scope,
          "@@hasInstance is not callable",
        )?);
      } else {
        Some(value)
      }
    };

    if let Some(method) = method {
      let result = self.call(
        &mut scope,
        method,
        Value::Object(constructor_obj),
        &[object],
      )?;
      return Ok(to_boolean(scope.heap(), result)?);
    }

    // 3. If `C` is not constructable, `instanceof` is `false`.
    if !scope.heap().is_constructor(constructor)? {
      return Ok(false);
    }

    self.ordinary_has_instance(&mut scope, object, constructor_obj)
  }

  fn ordinary_has_instance(
    &mut self,
    scope: &mut Scope<'_>,
    object: Value,
    constructor: GcObject,
  ) -> Result<bool, VmError> {
    // Bound functions delegate `instanceof` checks to their target.
    if let Ok(func) = scope.heap().get_function(constructor) {
      if let Some(bound_target) = func.bound_target {
        return self.instanceof_operator(scope, object, Value::Object(bound_target));
      }
    }

    // If the LHS is not an object, `instanceof` is `false` without further observable actions.
    let Value::Object(object) = object else {
      return Ok(false);
    };

    // P = Get(C, "prototype").
    let prototype_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(prototype_s))?;
    let prototype = scope.ordinary_get_with_host_and_hooks(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      constructor,
      PropertyKey::from_string(prototype_s),
      Value::Object(constructor),
    )?;

    let Value::Object(prototype) = prototype else {
      return Err(throw_type_error(
        self.vm,
        scope,
        "Function has non-object prototype in instanceof check",
      )?);
    };

    // Walk `object`'s prototype chain until we find `prototype` or reach the end.
    let mut current = scope.heap().object_prototype(object)?;
    let mut steps = 0usize;
    let mut visited: HashSet<GcObject> = HashSet::new();
    while let Some(obj) = current {
      if steps >= crate::MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      if !visited.insert(obj) {
        return Err(VmError::PrototypeCycle);
      }

      if obj == prototype {
        return Ok(true);
      }
      current = scope.heap().object_prototype(obj)?;
    }

    Ok(false)
  }

  fn is_primitive_value(&self, value: Value) -> bool {
    !matches!(value, Value::Object(_))
  }

  fn to_primitive(
    &mut self,
    scope: &mut Scope<'_>,
    value: Value,
    hint: ToPrimitiveHint,
  ) -> Result<Value, VmError> {
    let Value::Object(obj) = value else {
      return Ok(value);
    };

    // Root `obj` across property lookups / calls (which may allocate and trigger GC).
    let mut prim_scope = scope.reborrow();
    prim_scope.push_root(Value::Object(obj))?;

    // 1. GetMethod(input, @@toPrimitive).
    let to_prim_sym = self
      .vm
      .intrinsics()
      .ok_or(VmError::Unimplemented("intrinsics not initialized"))?
      .well_known_symbols()
      .to_primitive;
    let to_prim_key = PropertyKey::from_symbol(to_prim_sym);
    let exotic = prim_scope.ordinary_get_with_host_and_hooks(
      self.vm,
      &mut *self.host,
      &mut *self.hooks,
      obj,
      to_prim_key,
      Value::Object(obj),
    )?;

    if !matches!(exotic, Value::Undefined | Value::Null) {
      if !prim_scope.heap().is_callable(exotic)? {
        return Err(throw_type_error(
          self.vm,
          &mut prim_scope,
          "@@toPrimitive is not callable",
        )?);
      }

      let hint_s = prim_scope.alloc_string(hint.as_str())?;
      prim_scope.push_root(Value::String(hint_s))?;
      let out = self.call(
        &mut prim_scope,
        exotic,
        Value::Object(obj),
        &[Value::String(hint_s)],
      )?;
      if self.is_primitive_value(out) {
        return Ok(out);
      }
      return Err(throw_type_error(
        self.vm,
        &mut prim_scope,
        "Cannot convert object to primitive value",
      )?);
    }

    // 2. OrdinaryToPrimitive.
    self.ordinary_to_primitive(&mut prim_scope, obj, hint)
  }

  fn ordinary_to_primitive(
    &mut self,
    scope: &mut Scope<'_>,
    obj: GcObject,
    hint: ToPrimitiveHint,
  ) -> Result<Value, VmError> {
    let hint = match hint {
      ToPrimitiveHint::Default => ToPrimitiveHint::Number,
      other => other,
    };
    let methods = match hint {
      ToPrimitiveHint::String => ["toString", "valueOf"],
      ToPrimitiveHint::Number | ToPrimitiveHint::Default => ["valueOf", "toString"],
    };

    for name in methods {
      let key_s = scope.alloc_string(name)?;
      scope.push_root(Value::String(key_s))?;
      let key = PropertyKey::from_string(key_s);
      let method = scope.ordinary_get_with_host_and_hooks(
        self.vm,
        &mut *self.host,
        &mut *self.hooks,
        obj,
        key,
        Value::Object(obj),
      )?;

      if matches!(method, Value::Undefined | Value::Null) {
        continue;
      }
      if !scope.heap().is_callable(method)? {
        continue;
      }

      let result = self.call(scope, method, Value::Object(obj), &[])?;
      if self.is_primitive_value(result) {
        return Ok(result);
      }
    }

    Err(throw_type_error(
      self.vm,
      scope,
      "Cannot convert object to primitive value",
    )?)
  }

  fn to_number_operator(&mut self, scope: &mut Scope<'_>, value: Value) -> Result<f64, VmError> {
    // `ToNumber` includes `ToPrimitive` with a Number hint.
    let mut num_scope = scope.reborrow();
    num_scope.push_root(value)?;
    let prim = self.to_primitive(&mut num_scope, value, ToPrimitiveHint::Number)?;
    num_scope.push_root(prim)?;

    match to_number(num_scope.heap_mut(), prim) {
      Ok(n) => Ok(n),
      Err(VmError::TypeError(msg)) => Err(throw_type_error(self.vm, &mut num_scope, msg)?),
      Err(err) => Err(err),
    }
  }

  fn to_string_operator(
    &mut self,
    scope: &mut Scope<'_>,
    value: Value,
  ) -> Result<GcString, VmError> {
    // `ToString` includes `ToPrimitive` with a String hint.
    let mut string_scope = scope.reborrow();
    string_scope.push_root(value)?;
    let prim = self.to_primitive(&mut string_scope, value, ToPrimitiveHint::String)?;
    string_scope.push_root(prim)?;
    debug_assert!(
      self.is_primitive_value(prim),
      "to_primitive returned object"
    );

    match string_scope.heap_mut().to_string(prim) {
      Ok(s) => Ok(s),
      Err(VmError::TypeError(msg)) => Err(throw_type_error(self.vm, &mut string_scope, msg)?),
      Err(err) => Err(err),
    }
  }

  fn to_object_operator(
    &mut self,
    scope: &mut Scope<'_>,
    value: Value,
  ) -> Result<GcObject, VmError> {
    match value {
      Value::Object(obj) => Ok(obj),
      Value::Undefined | Value::Null => Err(throw_type_error(
        self.vm,
        scope,
        "Cannot convert undefined or null to object",
      )?),
      other => {
        // Use the intrinsic `Object` constructor to box primitives. This matches `ToObject` for
        // non-nullish primitives and shares wrapper marker semantics with our built-in prototype
        // methods.
        let intr = self
          .vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let object_ctor = Value::Object(intr.object_constructor());

        let mut to_obj_scope = scope.reborrow();
        to_obj_scope.push_root(other)?;
        to_obj_scope.push_root(object_ctor)?;
        let args = [other];
        let value = self.call(&mut to_obj_scope, object_ctor, Value::Undefined, &args)?;
        match value {
          Value::Object(obj) => Ok(obj),
          _ => Err(VmError::InvariantViolation(
            "Object(..) conversion returned non-object",
          )),
        }
      }
    }
  }

  fn to_property_key_operator(
    &mut self,
    scope: &mut Scope<'_>,
    value: Value,
  ) -> Result<PropertyKey, VmError> {
    // `ToPropertyKey` includes `ToPrimitive` with a String hint, and may allocate.
    let mut key_scope = scope.reborrow();
    key_scope.push_root(value)?;
    let prim = self.to_primitive(&mut key_scope, value, ToPrimitiveHint::String)?;
    key_scope.push_root(prim)?;
    debug_assert!(
      self.is_primitive_value(prim),
      "to_primitive returned object"
    );

    match prim {
      Value::Symbol(sym) => Ok(PropertyKey::Symbol(sym)),
      other => {
        let s = self.to_string_operator(&mut key_scope, other)?;
        Ok(PropertyKey::String(s))
      }
    }
  }

  fn addition_operator(
    &mut self,
    scope: &mut Scope<'_>,
    left: Value,
    right: Value,
  ) -> Result<Value, VmError> {
    // Root inputs and intermediates for the duration of the operation: `+` may allocate
    // (string concatenation, ToString) and thus trigger GC.
    let mut add_scope = scope.reborrow();
    add_scope.push_root(left)?;
    add_scope.push_root(right)?;

    // ECMA-262 AdditionOperator (+): ToPrimitive (default), then string concat if either side is a
    // string; otherwise numeric addition.
    let left_prim = self.to_primitive(&mut add_scope, left, ToPrimitiveHint::Default)?;
    add_scope.push_root(left_prim)?;
    let right_prim = self.to_primitive(&mut add_scope, right, ToPrimitiveHint::Default)?;
    add_scope.push_root(right_prim)?;

    if matches!(left_prim, Value::String(_)) || matches!(right_prim, Value::String(_)) {
      let left_s = self.to_string_operator(&mut add_scope, left_prim)?;
      add_scope.push_root(Value::String(left_s))?;
      let right_s = self.to_string_operator(&mut add_scope, right_prim)?;
      add_scope.push_root(Value::String(right_s))?;

      let left_units = add_scope.heap().get_string(left_s)?.as_code_units();
      let right_units = add_scope.heap().get_string(right_s)?.as_code_units();

      let total_len = left_units
        .len()
        .checked_add(right_units.len())
        .ok_or(VmError::OutOfMemory)?;
      let mut units: Vec<u16> = Vec::new();
      units
        .try_reserve_exact(total_len)
        .map_err(|_| VmError::OutOfMemory)?;
      units.extend_from_slice(left_units);
      units.extend_from_slice(right_units);

      let s = add_scope.alloc_string_from_u16_vec(units)?;
      Ok(Value::String(s))
    } else {
      let left_num = self.to_numeric(&mut add_scope, left_prim)?;
      let right_num = self.to_numeric(&mut add_scope, right_prim)?;
      Ok(match (left_num, right_num) {
        (NumericValue::Number(a), NumericValue::Number(b)) => Value::Number(a + b),
        (NumericValue::BigInt(a), NumericValue::BigInt(b)) => {
          let Some(out) = a.checked_add(b) else {
            return Err(VmError::Unimplemented("BigInt addition overflow"));
          };
          Value::BigInt(out)
        }
        _ => {
          return Err(throw_type_error(
            self.vm,
            &mut add_scope,
            "Cannot mix BigInt and other types",
          )?)
        }
      })
    }
  }

  fn to_numeric(&mut self, scope: &mut Scope<'_>, value: Value) -> Result<NumericValue, VmError> {
    // ECMA-262 `ToNumeric`: ToPrimitive (hint Number), then return BigInt directly or convert to
    // Number.
    let prim = self.to_primitive(scope, value, ToPrimitiveHint::Number)?;
    match prim {
      Value::BigInt(b) => Ok(NumericValue::BigInt(b)),
      other => match to_number(scope.heap_mut(), other) {
        Ok(n) => Ok(NumericValue::Number(n)),
        Err(VmError::TypeError(msg)) => Err(throw_type_error(self.vm, scope, msg)?),
        Err(err) => Err(err),
      },
    }
  }
}

fn alloc_string_from_lit_str(
  scope: &mut Scope<'_>,
  node: &Node<LitStrExpr>,
) -> Result<GcString, VmError> {
  if let Some(units) = literal_string_code_units(&node.assoc) {
    scope.alloc_string_from_code_units(units)
  } else {
    scope.alloc_string_from_utf8(&node.stx.value)
  }
}

/// Persistent continuation frames for async function execution.
///
/// This is an explicit reification of the evaluator call stack needed to resume execution after an
/// `await` suspension point.
#[derive(Debug)]
enum AsyncFrame {
  /// Root frame for block-bodied async functions (`async function f(){...}`).
  RootBlockBody,
  /// Root frame for expression-bodied async arrow functions (`async () => expr`).
  RootExprBody,

  /// Resume statement-list evaluation after a suspended statement completes.
  StmtList {
    stmts: *const Vec<Node<Stmt>>,
    next_index: usize,
    last_value_root: RootId,
    last_value_is_set: bool,
  },

  /// Restore the outer lexical environment after finishing a block/catch/finally body.
  RestoreLexEnv {
    outer: GcEnv,
  },

  /// Finish an expression statement after its expression is evaluated.
  ExprStmt,
  /// Finish a return statement after its value expression is evaluated.
  Return,

  /// Continue a `var`/`let`/`const` declaration after a declarator initializer completes.
  VarDecl {
    decl: *const VarDecl,
    next_declarator_index: usize,
  },

  /// Continue an `if` statement after evaluating the test expression.
  IfAfterTest {
    consequent: *const Node<Stmt>,
    alternate: Option<*const Node<Stmt>>,
  },

  /// Continue a `while` loop after evaluating the test expression.
  WhileAfterTest {
    stmt: *const WhileStmt,
    label_set: Vec<String>,
    v_root: RootId,
  },
  /// Continue a `while` loop after evaluating the body statement.
  WhileAfterBody {
    stmt: *const WhileStmt,
    label_set: Vec<String>,
    v_root: RootId,
  },

  /// Continue a `do..while` loop after evaluating the body statement.
  DoWhileAfterBody {
    stmt: *const DoWhileStmt,
    label_set: Vec<String>,
    v_root: RootId,
  },
  /// Continue a `do..while` loop after evaluating the test expression.
  DoWhileAfterTest {
    stmt: *const DoWhileStmt,
    label_set: Vec<String>,
    v_root: RootId,
  },

  /// Continue a `for (init; cond; post) { ... }` loop after evaluating the initializer.
  ForTripleAfterInit {
    stmt: *const ForTripleStmt,
    label_set: Vec<String>,
    v_root: RootId,
    needs_explicit_iter_tick: bool,
  },
  /// Continue a `for (init; cond; post) { ... }` loop after evaluating the test expression.
  ForTripleAfterTest {
    stmt: *const ForTripleStmt,
    label_set: Vec<String>,
    v_root: RootId,
    needs_explicit_iter_tick: bool,
  },
  /// Continue a `for (init; cond; post) { ... }` loop after evaluating the body.
  ForTripleAfterBody {
    stmt: *const ForTripleStmt,
    label_set: Vec<String>,
    v_root: RootId,
    needs_explicit_iter_tick: bool,
  },
  /// Continue a `for (init; cond; post) { ... }` loop after evaluating the post expression.
  ForTripleAfterPost {
    stmt: *const ForTripleStmt,
    label_set: Vec<String>,
    v_root: RootId,
    needs_explicit_iter_tick: bool,
  },

  /// Continue a `for..in` loop after evaluating the RHS expression.
  ForInAfterRhs {
    stmt: *const ForInStmt,
    label_set: Vec<String>,
  },
  /// Continue a `for..in` loop after evaluating the body statement list for one iteration.
  ForInAfterBody {
    stmt: *const ForInStmt,
    label_set: Vec<String>,
    object_root: RootId,
    key_roots: Vec<RootId>,
    next_key_index: usize,
    v_root: RootId,
    outer_lex: GcEnv,
  },

  /// Continue a `for..of` loop after evaluating the RHS iterable expression.
  ForOfAfterRhs {
    stmt: *const ForOfStmt,
    label_set: Vec<String>,
  },
  /// Continue a `for..of` loop after evaluating the body statement list for one iteration.
  ForOfAfterBody {
    stmt: *const ForOfStmt,
    label_set: Vec<String>,
    iterator_record: iterator::IteratorRecord,
    iterator_root: RootId,
    next_method_root: RootId,
    v_root: RootId,
    outer_lex: GcEnv,
  },

  /// Continue a `switch` statement after evaluating the discriminant expression.
  SwitchAfterDiscriminant {
    stmt: *const SwitchStmt,
  },
  /// Continue a `switch` statement after evaluating a case selector expression.
  SwitchAfterCaseExpr {
    stmt: *const SwitchStmt,
    discriminant_root: RootId,
    v_root: RootId,
    default_index: Option<usize>,
    branch_index: usize,
  },
  /// Continue a `switch` statement after evaluating a case body statement list.
  SwitchAfterBody {
    stmt: *const SwitchStmt,
    discriminant_root: RootId,
    v_root: RootId,
    next_branch_index: usize,
  },

  /// Continue a `try` statement after evaluating the wrapped block.
  TryAfterWrapped {
    stmt: *const TryStmt,
  },
  /// Continue a `try` statement after evaluating the catch block.
  TryAfterCatch {
    stmt: *const TryStmt,
  },
  /// Continue a `try` statement after evaluating the finally block.
  TryAfterFinally {
    pending: RootedCompletion,
  },

  /// Continue evaluating a unary `await` after its operand expression completes (nested await).
  AwaitAfterOperand,
  /// Continue evaluating a non-`await` unary expression after its operand completes.
  UnaryAfterArgument {
    expr: *const UnaryExpr,
  },

  /// Continue a member access after evaluating the base value.
  MemberAfterBase {
    expr: *const MemberExpr,
  },
  /// Continue a computed member access after evaluating the base value.
  ComputedMemberAfterBase {
    expr: *const ComputedMemberExpr,
  },
  /// Continue a computed member access after evaluating the member expression.
  ComputedMemberAfterMember {
    expr: *const ComputedMemberExpr,
    base_root: RootId,
  },

  /// Continue a conditional expression after evaluating the test.
  CondAfterTest {
    expr: *const CondExpr,
  },

  /// Continue a binary expression after evaluating the left operand.
  BinaryAfterLeft {
    expr: *const BinaryExpr,
  },
  /// Continue a binary expression after evaluating the right operand.
  BinaryAfterRight {
    expr: *const BinaryExpr,
    left_root: RootId,
  },

  /// Continue an assignment expression after evaluating the RHS (binding target).
  AssignAfterRhsBinding {
    name: *const String,
  },
  /// Continue an assignment expression after evaluating the RHS (property target).
  AssignAfterRhsProperty {
    base_root: RootId,
    key_root: RootId,
  },
  /// Continue a destructuring assignment after evaluating the RHS.
  AssignAfterRhsPattern {
    expr: *const BinaryExpr,
  },

  /// Continue a call expression while evaluating arguments.
  CallAfterCallee {
    expr: *const CallExpr,
  },
  CallMemberAfterBase {
    expr: *const CallExpr,
    member: *const MemberExpr,
  },
  CallComputedMemberAfterBase {
    expr: *const CallExpr,
    member: *const ComputedMemberExpr,
  },
  CallComputedMemberAfterMember {
    expr: *const CallExpr,
    member: *const ComputedMemberExpr,
    base_root: RootId,
  },
  CallArgs {
    expr: *const CallExpr,
    callee_root: RootId,
    this_root: RootId,
    arg_roots: Vec<RootId>,
    arg_index: usize,
  },
}

#[derive(Debug)]
struct AsyncSuspend {
  await_value: Value,
  frames: VecDeque<AsyncFrame>,
}

#[derive(Debug)]
enum AsyncEval<T> {
  Complete(T),
  Suspend(AsyncSuspend),
}

#[derive(Debug)]
struct RootedCompletion {
  kind: RootedCompletionKind,
}

#[derive(Debug)]
enum RootedCompletionKind {
  Normal(Option<RootId>),
  Throw {
    value_root: RootId,
    stack: Vec<StackFrame>,
  },
  Return(RootId),
  Break(Option<String>, Option<RootId>),
  Continue(Option<String>, Option<RootId>),
}

impl RootedCompletion {
  fn new(scope: &mut Scope<'_>, completion: Completion) -> Result<Self, VmError> {
    let mut root_value = |v: Value| -> Result<RootId, VmError> {
      let mut root_scope = scope.reborrow();
      root_scope.push_root(v)?;
      root_scope.heap_mut().add_root(v)
    };

    let kind = match completion {
      Completion::Normal(v) => RootedCompletionKind::Normal(v.map(root_value).transpose()?),
      Completion::Return(v) => RootedCompletionKind::Return(root_value(v)?),
      Completion::Throw(thrown) => RootedCompletionKind::Throw {
        value_root: root_value(thrown.value)?,
        stack: thrown.stack,
      },
      Completion::Break(target, v) => RootedCompletionKind::Break(target, v.map(root_value).transpose()?),
      Completion::Continue(target, v) => {
        RootedCompletionKind::Continue(target, v.map(root_value).transpose()?)
      }
    };
    Ok(Self { kind })
  }

  fn to_completion(&self, heap: &Heap) -> Result<Completion, VmError> {
    let get = |root: RootId| {
      heap
        .get_root(root)
        .ok_or(VmError::InvariantViolation("missing rooted completion value"))
    };
    Ok(match &self.kind {
      RootedCompletionKind::Normal(v) => Completion::Normal(v.map(|id| get(id)).transpose()?),
      RootedCompletionKind::Return(id) => Completion::Return(get(*id)?),
      RootedCompletionKind::Throw { value_root, stack } => Completion::Throw(Thrown {
        value: get(*value_root)?,
        stack: stack.clone(),
      }),
      RootedCompletionKind::Break(target, v) => Completion::Break(
        target.clone(),
        v.map(|id| get(id)).transpose()?,
      ),
      RootedCompletionKind::Continue(target, v) => Completion::Continue(
        target.clone(),
        v.map(|id| get(id)).transpose()?,
      ),
    })
  }

  fn teardown(&mut self, heap: &mut Heap) {
    let mut remove_opt = |id: &mut Option<RootId>| {
      if let Some(id) = id.take() {
        heap.remove_root(id);
      }
    };

    match &mut self.kind {
      RootedCompletionKind::Normal(v) => remove_opt(v),
      RootedCompletionKind::Return(id) => heap.remove_root(*id),
      RootedCompletionKind::Throw { value_root, .. } => heap.remove_root(*value_root),
      RootedCompletionKind::Break(_, v) => remove_opt(v),
      RootedCompletionKind::Continue(_, v) => remove_opt(v),
    }
  }
}

fn async_teardown_frame(heap: &mut Heap, frame: &mut AsyncFrame) {
  match frame {
    AsyncFrame::StmtList {
      last_value_root, ..
    } => heap.remove_root(*last_value_root),
    AsyncFrame::WhileAfterTest { v_root, .. }
    | AsyncFrame::WhileAfterBody { v_root, .. }
    | AsyncFrame::DoWhileAfterBody { v_root, .. }
    | AsyncFrame::DoWhileAfterTest { v_root, .. }
    | AsyncFrame::ForTripleAfterInit { v_root, .. }
    | AsyncFrame::ForTripleAfterTest { v_root, .. }
    | AsyncFrame::ForTripleAfterBody { v_root, .. }
    | AsyncFrame::ForTripleAfterPost { v_root, .. } => heap.remove_root(*v_root),
    AsyncFrame::ForInAfterBody {
      object_root,
      key_roots,
      v_root,
      ..
    } => {
      heap.remove_root(*object_root);
      heap.remove_root(*v_root);
      for id in key_roots.drain(..) {
        heap.remove_root(id);
      }
    }
    AsyncFrame::ForOfAfterBody {
      iterator_root,
      next_method_root,
      v_root,
      ..
    } => {
      heap.remove_root(*iterator_root);
      heap.remove_root(*next_method_root);
      heap.remove_root(*v_root);
    }
    AsyncFrame::SwitchAfterCaseExpr {
      discriminant_root,
      v_root,
      ..
    }
    | AsyncFrame::SwitchAfterBody {
      discriminant_root,
      v_root,
      ..
    } => {
      heap.remove_root(*discriminant_root);
      heap.remove_root(*v_root);
    }
    AsyncFrame::TryAfterFinally { pending } => pending.teardown(heap),
    AsyncFrame::ComputedMemberAfterMember { base_root, .. } => heap.remove_root(*base_root),
    AsyncFrame::CallComputedMemberAfterMember { base_root, .. } => heap.remove_root(*base_root),
    AsyncFrame::BinaryAfterRight { left_root, .. } => heap.remove_root(*left_root),
    AsyncFrame::AssignAfterRhsProperty { base_root, key_root } => {
      heap.remove_root(*base_root);
      heap.remove_root(*key_root);
    }
    AsyncFrame::CallArgs {
      callee_root,
      this_root,
      arg_roots,
      ..
    } => {
      heap.remove_root(*callee_root);
      heap.remove_root(*this_root);
      for id in arg_roots.drain(..) {
        heap.remove_root(id);
      }
    }
    _ => {}
  }
}

#[derive(Debug)]
pub(crate) struct AsyncContinuation {
  env: RuntimeEnv,
  strict: bool,
  this_root: RootId,
  new_target_root: RootId,
  promise_root: RootId,
  resolve_root: RootId,
  reject_root: RootId,
  awaited_promise_root: Option<RootId>,
  frames: VecDeque<AsyncFrame>,
}

fn async_teardown_continuation(scope: &mut Scope<'_>, mut cont: AsyncContinuation) {
  cont.env.teardown(scope.heap_mut());
  scope.heap_mut().remove_root(cont.this_root);
  scope.heap_mut().remove_root(cont.new_target_root);
  scope.heap_mut().remove_root(cont.promise_root);
  scope.heap_mut().remove_root(cont.resolve_root);
  scope.heap_mut().remove_root(cont.reject_root);
  if let Some(root) = cont.awaited_promise_root.take() {
    scope.heap_mut().remove_root(root);
  }
  for mut frame in cont.frames {
    async_teardown_frame(scope.heap_mut(), &mut frame);
  }
}

fn async_handle_body_result(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  id: u32,
  mut cont: AsyncContinuation,
  resolve: Value,
  reject: Value,
  result: Result<AsyncBodyResult, VmError>,
) -> Result<Value, VmError> {
  let global_object = cont.env.global_object();

  match result {
    Ok(AsyncBodyResult::CompleteOk(v)) => {
      let mut call_scope = scope.reborrow();
      if let Err(err) = call_scope.push_roots(&[resolve, v]) {
        async_teardown_continuation(&mut call_scope, cont);
        return Err(err);
      }
      let res = vm.call_with_host_and_hooks(
        host,
        &mut call_scope,
        hooks,
        resolve,
        Value::Undefined,
        &[v],
      );
      async_teardown_continuation(&mut call_scope, cont);
      res.map(|_| Value::Undefined)
    }
    Ok(AsyncBodyResult::CompleteThrow(reason)) => {
      let mut call_scope = scope.reborrow();
      if let Err(err) = call_scope.push_roots(&[reject, reason]) {
        async_teardown_continuation(&mut call_scope, cont);
        return Err(err);
      }
      let res = vm.call_with_host_and_hooks(
        host,
        &mut call_scope,
        hooks,
        reject,
        Value::Undefined,
        &[reason],
      );
      async_teardown_continuation(&mut call_scope, cont);
      res.map(|_| Value::Undefined)
    }
    Ok(AsyncBodyResult::Await { await_value, frames }) => {
      // Suspend again: PromiseResolve + PerformPromiseThen(p, onFulfilled, onRejected).
      cont.frames = frames;

      let mut await_scope = scope.reborrow();
      if let Err(err) = await_scope.push_roots(&[await_value]) {
        async_teardown_continuation(&mut await_scope, cont);
        return Err(err);
      }
      let awaited_promise = match crate::promise_ops::promise_resolve_with_host_and_hooks(
        vm,
        &mut await_scope,
        host,
        hooks,
        await_value,
      ) {
        Ok(p) => p,
        Err(err) => {
          async_teardown_continuation(&mut await_scope, cont);
          return Err(err);
        }
      };
      if let Err(err) = await_scope.push_root(awaited_promise) {
        async_teardown_continuation(&mut await_scope, cont);
        return Err(err);
      }

      let awaited_root = match await_scope.heap_mut().add_root(awaited_promise) {
        Ok(root) => root,
        Err(err) => {
          async_teardown_continuation(&mut await_scope, cont);
          return Err(err);
        }
      };
      cont.awaited_promise_root = Some(awaited_root);

      // Reinsert continuation before scheduling any resumption callbacks.
      if let Err(err) = vm.reserve_async_continuations(1) {
        async_teardown_continuation(&mut await_scope, cont);
        return Err(err);
      }
      vm.replace_async_continuation(id, cont)?;

      let then_res = (|| -> Result<(), VmError> {
        let call_id = vm.async_resume_call_id()?;
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let job_realm = vm.current_realm();

        let name = await_scope.alloc_string("")?;
        let slots_fulfill = [Value::Number(id as f64), Value::Bool(false)];
        let on_fulfilled =
          await_scope.alloc_native_function_with_slots(call_id, None, name, 1, &slots_fulfill)?;
        await_scope.push_root(Value::Object(on_fulfilled))?;

        let name = await_scope.alloc_string("")?;
        let slots_reject = [Value::Number(id as f64), Value::Bool(true)];
        let on_rejected =
          await_scope.alloc_native_function_with_slots(call_id, None, name, 1, &slots_reject)?;
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
        }

        let _ = crate::promise_ops::perform_promise_then_with_host_and_hooks(
          vm,
          &mut await_scope,
          host,
          hooks,
          awaited_promise,
          Some(Value::Object(on_fulfilled)),
          Some(Value::Object(on_rejected)),
        )?;
        Ok(())
      })();

      if let Err(err) = then_res {
        if let Some(cont) = vm.take_async_continuation(id) {
          async_teardown_continuation(&mut await_scope, cont);
        }
        return Err(err);
      }

      Ok(Value::Undefined)
    }
    Err(err) => {
      // Fatal error during resumption: clean up roots/env to avoid leaks.
      async_teardown_continuation(scope, cont);
      Err(err)
    }
  }
}

pub(crate) fn async_resume_call(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let id = match slots.get(0).copied().unwrap_or(Value::Undefined) {
    Value::Number(n) => n as u32,
    _ => {
      return Err(VmError::InvariantViolation(
        "async resume callback missing continuation id",
      ))
    }
  };
  let is_reject = match slots.get(1).copied().unwrap_or(Value::Undefined) {
    Value::Bool(b) => b,
    _ => {
      return Err(VmError::InvariantViolation(
        "async resume callback missing reject flag",
      ))
    }
  };

  let Some(mut cont) = vm.take_async_continuation(id) else {
    return Err(VmError::InvariantViolation("async continuation not found"));
  };

  // The awaited promise has settled; it no longer needs to be rooted by the continuation.
  if let Some(root) = cont.awaited_promise_root.take() {
    scope.heap_mut().remove_root(root);
  }

  let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);

  let resolve = scope
    .heap()
    .get_root(cont.resolve_root)
    .ok_or(VmError::InvariantViolation(
      "async continuation missing resolve root",
    ))?;
  let reject = scope
    .heap()
    .get_root(cont.reject_root)
    .ok_or(VmError::InvariantViolation(
      "async continuation missing reject root",
    ))?;

  let this = scope
    .heap()
    .get_root(cont.this_root)
    .ok_or(VmError::InvariantViolation(
      "async continuation missing this root",
    ))?;
  let new_target =
    scope
      .heap()
      .get_root(cont.new_target_root)
      .ok_or(VmError::InvariantViolation(
        "async continuation missing new.target root",
      ))?;

  let mut evaluator = Evaluator {
    vm,
    host,
    hooks,
    env: &mut cont.env,
    strict: cont.strict,
    this,
    new_target,
  };

  let frames = mem::take(&mut cont.frames);
  let resume_value = if is_reject { Err(arg0) } else { Ok(arg0) };
  let result = async_resume_from_frames(&mut evaluator, scope, frames, resume_value);

  async_handle_body_result(vm, scope, host, hooks, id, cont, resolve, reject, result)
}

enum AsyncBodyResult {
  CompleteOk(Value),
  CompleteThrow(Value),
  Await { await_value: Value, frames: VecDeque<AsyncFrame> },
}

fn coerce_error_to_throw_for_async(vm: &Vm, scope: &mut Scope<'_>, err: VmError) -> VmError {
  match err {
    VmError::Throw(_) | VmError::ThrowWithStack { .. } => err,
    VmError::TypeError(message) => throw_type_error(vm, scope, message).unwrap_or_else(|e| e),
    VmError::NotCallable => {
      throw_type_error(vm, scope, "value is not callable").unwrap_or_else(|e| e)
    }
    VmError::NotConstructable => {
      throw_type_error(vm, scope, "value is not a constructor").unwrap_or_else(|e| e)
    }
    VmError::PrototypeCycle => throw_type_error(vm, scope, "prototype cycle").unwrap_or_else(|e| e),
    VmError::PrototypeChainTooDeep => {
      throw_type_error(vm, scope, "prototype chain too deep").unwrap_or_else(|e| e)
    }
    VmError::InvalidPropertyDescriptorPatch => throw_type_error(
      vm,
      scope,
      "invalid property descriptor patch: cannot mix data and accessor fields",
    )
    .unwrap_or_else(|e| e),
    other => other,
  }
}

fn expr_contains_await(expr: &Node<Expr>) -> bool {
  match &*expr.stx {
    Expr::Unary(unary) => {
      unary.stx.operator == OperatorName::Await || expr_contains_await(&unary.stx.argument)
    }
    Expr::UnaryPostfix(unary) => expr_contains_await(&unary.stx.argument),
    Expr::Binary(binary) => {
      expr_contains_await(&binary.stx.left) || expr_contains_await(&binary.stx.right)
    }
    Expr::Cond(cond) => {
      expr_contains_await(&cond.stx.test)
        || expr_contains_await(&cond.stx.consequent)
        || expr_contains_await(&cond.stx.alternate)
    }
    Expr::Member(member) => expr_contains_await(&member.stx.left),
    Expr::ComputedMember(member) => {
      expr_contains_await(&member.stx.object) || expr_contains_await(&member.stx.member)
    }
    Expr::Call(call) => {
      expr_contains_await(&call.stx.callee)
        || call
          .stx
          .arguments
          .iter()
          .any(|arg| expr_contains_await(&arg.stx.value))
    }
    Expr::Import(import) => {
      expr_contains_await(&import.stx.module)
        || import
          .stx
          .attributes
          .as_ref()
          .is_some_and(|attrs| expr_contains_await(attrs))
    }
    Expr::TaggedTemplate(tag) => {
      expr_contains_await(&tag.stx.function)
        || tag.stx.parts.iter().any(|part| match part {
          LitTemplatePart::Substitution(expr) => expr_contains_await(expr),
          LitTemplatePart::String(_) => false,
        })
    }
    Expr::LitArr(arr) => arr.stx.elements.iter().any(|elem| match elem {
      LitArrElem::Single(expr) | LitArrElem::Rest(expr) => expr_contains_await(expr),
      LitArrElem::Empty => false,
    }),
    Expr::LitObj(obj) => obj.stx.members.iter().any(|member| match &member.stx.typ {
      ObjMemberType::Valued { key, val } => {
        let key_has_await = match key {
          ClassOrObjKey::Direct(_) => false,
          ClassOrObjKey::Computed(expr) => expr_contains_await(expr),
        };

        let val_has_await = match val {
          ClassOrObjVal::Prop(Some(expr)) => expr_contains_await(expr),
          ClassOrObjVal::Prop(None) => false,
          // Function-valued members: the function body is not evaluated at object creation time.
          ClassOrObjVal::Getter(_)
          | ClassOrObjVal::Setter(_)
          | ClassOrObjVal::Method(_)
          | ClassOrObjVal::IndexSignature(_)
          | ClassOrObjVal::StaticBlock(_) => false,
        };

        key_has_await || val_has_await
      }
      ObjMemberType::Shorthand { .. } => false,
      ObjMemberType::Rest { val } => expr_contains_await(val),
    }),
    Expr::LitTemplate(tpl) => tpl.stx.parts.iter().any(|part| match part {
      LitTemplatePart::Substitution(expr) => expr_contains_await(expr),
      LitTemplatePart::String(_) => false,
    }),

    // Nested functions are not evaluated when the function value is created.
    Expr::Func(_) | Expr::ArrowFunc(_) => false,

    // TypeScript-only nodes: only the wrapped expression is evaluated.
    Expr::Instantiation(inst) => expr_contains_await(&inst.stx.expression),
    Expr::TypeAssertion(expr) => expr_contains_await(&expr.stx.expression),
    Expr::NonNullAssertion(expr) => expr_contains_await(&expr.stx.expression),
    Expr::SatisfiesExpr(expr) => expr_contains_await(&expr.stx.expression),

    _ => false,
  }
}

fn stmt_contains_await(stmt: &Node<Stmt>) -> bool {
  match &*stmt.stx {
    Stmt::Empty(_)
    | Stmt::Debugger(_)
    | Stmt::Import(_)
    | Stmt::ExportList(_)
    | Stmt::FunctionDecl(_)
    | Stmt::ClassDecl(_)
    | Stmt::Break(_)
    | Stmt::Continue(_) => false,
    Stmt::Expr(expr_stmt) => expr_contains_await(&expr_stmt.stx.expr),
    Stmt::Return(ret) => ret.stx.value.as_ref().is_some_and(expr_contains_await),
    Stmt::Throw(throw_stmt) => expr_contains_await(&throw_stmt.stx.value),
    Stmt::VarDecl(decl) => decl
      .stx
      .declarators
      .iter()
      .any(|d| d.initializer.as_ref().is_some_and(expr_contains_await)),
    Stmt::Block(block) => block.stx.body.iter().any(stmt_contains_await),
    Stmt::If(if_stmt) => {
      expr_contains_await(&if_stmt.stx.test)
        || stmt_contains_await(&if_stmt.stx.consequent)
        || if_stmt.stx.alternate.as_ref().is_some_and(stmt_contains_await)
    }
    Stmt::Try(try_stmt) => {
      try_stmt.stx.wrapped.stx.body.iter().any(stmt_contains_await)
        || try_stmt
          .stx
          .catch
          .as_ref()
          .is_some_and(|c| c.stx.body.iter().any(stmt_contains_await))
        || try_stmt
          .stx
          .finally
          .as_ref()
          .is_some_and(|f| f.stx.body.iter().any(stmt_contains_await))
    }
    Stmt::While(while_stmt) => {
      expr_contains_await(&while_stmt.stx.condition) || stmt_contains_await(&while_stmt.stx.body)
    }
    Stmt::DoWhile(do_while) => {
      expr_contains_await(&do_while.stx.condition) || stmt_contains_await(&do_while.stx.body)
    }
    Stmt::ForTriple(for_stmt) => {
      let init_has_await = match &for_stmt.stx.init {
        parse_js::ast::stmt::ForTripleStmtInit::None => false,
        parse_js::ast::stmt::ForTripleStmtInit::Expr(expr) => expr_contains_await(expr),
        parse_js::ast::stmt::ForTripleStmtInit::Decl(decl) => decl
          .stx
          .declarators
          .iter()
          .any(|d| d.initializer.as_ref().is_some_and(expr_contains_await)),
      };
 
      init_has_await
        || for_stmt.stx.cond.as_ref().is_some_and(expr_contains_await)
        || for_stmt.stx.post.as_ref().is_some_and(expr_contains_await)
        || for_stmt
          .stx
          .body
          .stx
          .body
          .iter()
          .any(stmt_contains_await)
    }
    Stmt::ForIn(for_in) => {
      expr_contains_await(&for_in.stx.rhs)
        || for_in.stx.body.stx.body.iter().any(stmt_contains_await)
    }
    Stmt::ForOf(for_of) => {
      expr_contains_await(&for_of.stx.rhs)
        || for_of.stx.body.stx.body.iter().any(stmt_contains_await)
    }
    Stmt::Switch(switch_stmt) => {
      expr_contains_await(&switch_stmt.stx.test)
        || switch_stmt.stx.branches.iter().any(|branch| {
          branch
            .stx
            .case
            .as_ref()
            .is_some_and(expr_contains_await)
            || branch.stx.body.iter().any(stmt_contains_await)
        })
    }
    // Conservatively assume unsupported statement kinds do not contain await so we preserve the
    // existing synchronous evaluator behaviour for them.
    _ => false,
  }
}

fn async_frames_push(frames: &mut VecDeque<AsyncFrame>, frame: AsyncFrame) -> Result<(), VmError> {
  frames.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
  frames.push_back(frame);
  Ok(())
}

fn completion_from_expr_result(expr: Result<Value, VmError>) -> Result<Completion, VmError> {
  match expr {
    Ok(v) => Ok(Completion::normal(v)),
    Err(VmError::Throw(value)) => Ok(Completion::Throw(Thrown {
      value,
      stack: Vec::new(),
    })),
    Err(VmError::ThrowWithStack { value, stack }) => Ok(Completion::Throw(Thrown { value, stack })),
    Err(other) => Err(other),
  }
}

fn completion_from_expr_result_for_return(expr: Result<Value, VmError>) -> Result<Completion, VmError> {
  match expr {
    Ok(v) => Ok(Completion::Return(v)),
    Err(VmError::Throw(value)) => Ok(Completion::Throw(Thrown {
      value,
      stack: Vec::new(),
    })),
    Err(VmError::ThrowWithStack { value, stack }) => Ok(Completion::Throw(Thrown { value, stack })),
    Err(other) => Err(other),
  }
}

fn async_eval_stmt_list(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmts: &Vec<Node<Stmt>>,
) -> Result<AsyncEval<Completion>, VmError> {
  // Mirror the synchronous evaluator's approach: use a persistent root for the running completion
  // value so it remains GC-safe across statement evaluation and across `await` suspensions.
  let last_root = scope.heap_mut().add_root(Value::Undefined)?;
  async_eval_stmt_list_from(evaluator, scope, stmts, 0, last_root, false)
}

fn async_eval_stmt_list_from(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmts: &Vec<Node<Stmt>>,
  start_index: usize,
  last_value_root: RootId,
  mut last_value_is_set: bool,
) -> Result<AsyncEval<Completion>, VmError> {
  let mut last_value: Option<Value> = if last_value_is_set {
    Some(
      scope
        .heap()
        .get_root(last_value_root)
        .ok_or(VmError::InvariantViolation("missing stmt-list last value root"))?,
    )
  } else {
    None
  };

  for (idx, stmt) in stmts.iter().enumerate().skip(start_index) {
    let completion_eval = async_eval_stmt_labelled(evaluator, scope, stmt, &[]);
    let completion = match completion_eval? {
      AsyncEval::Complete(c) => c,
      AsyncEval::Suspend(mut suspend) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::StmtList {
            stmts: stmts as *const Vec<Node<Stmt>>,
            next_index: idx.saturating_add(1),
            last_value_root,
            last_value_is_set,
          },
        )?;
        return Ok(AsyncEval::Suspend(suspend));
      }
    };

    let completion = completion.update_empty(last_value);
    match completion {
      Completion::Normal(v) => {
        if let Some(v) = v {
          last_value = Some(v);
          last_value_is_set = true;
          scope.heap_mut().set_root(last_value_root, v);
        }
      }
      abrupt => {
        scope.heap_mut().remove_root(last_value_root);
        return Ok(AsyncEval::Complete(abrupt));
      }
    }
  }

  scope.heap_mut().remove_root(last_value_root);
  Ok(AsyncEval::Complete(Completion::Normal(last_value)))
}

fn async_eval_block_stmt(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  block: &BlockStmt,
) -> Result<AsyncEval<Completion>, VmError> {
  if block.body.is_empty() {
    return Ok(AsyncEval::Complete(Completion::empty()));
  }

  let needs_lexical_env = block.body.iter().any(|stmt| match &*stmt.stx {
    Stmt::VarDecl(var) if matches!(var.stx.mode, VarDeclMode::Let | VarDeclMode::Const) => true,
    Stmt::ClassDecl(_) => true,
    Stmt::FunctionDecl(_) => evaluator.strict,
    _ => false,
  });
  if !needs_lexical_env {
    return async_eval_stmt_list(evaluator, scope, &block.body);
  }

  let outer = evaluator.env.lexical_env;
  let block_env = scope.env_create(Some(outer))?;
  evaluator.env.set_lexical_env(scope.heap_mut(), block_env);

  if let Err(err) = evaluator.instantiate_block_decls_in_stmt_list(scope, block_env, &block.body) {
    evaluator.env.set_lexical_env(scope.heap_mut(), outer);
    return Err(err);
  }

  match async_eval_stmt_list(evaluator, scope, &block.body)? {
    AsyncEval::Complete(c) => {
      evaluator.env.set_lexical_env(scope.heap_mut(), outer);
      Ok(AsyncEval::Complete(c))
    }
    AsyncEval::Suspend(mut suspend) => {
      async_frames_push(&mut suspend.frames, AsyncFrame::RestoreLexEnv { outer })?;
      Ok(AsyncEval::Suspend(suspend))
    }
  }
}

fn async_eval_catch(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  catch: &CatchBlock,
  thrown: Value,
) -> Result<AsyncEval<Completion>, VmError> {
  let outer = evaluator.env.lexical_env;
  let catch_env = scope.env_create(Some(outer))?;
  evaluator.env.set_lexical_env(scope.heap_mut(), catch_env);

  {
    // Root thrown across catch binding instantiation which may allocate.
    let mut catch_scope = scope.reborrow();
    catch_scope.push_root(thrown)?;
    evaluator.instantiate_block_decls_in_stmt_list(&mut catch_scope, catch_env, &catch.body)?;
    if let Some(param) = &catch.parameter {
      evaluator.bind_catch_param(&mut catch_scope, &param.stx, thrown, catch_env)?;
    }
  }

  match async_eval_stmt_list(evaluator, scope, &catch.body)? {
    AsyncEval::Complete(c) => {
      evaluator.env.set_lexical_env(scope.heap_mut(), outer);
      Ok(AsyncEval::Complete(c))
    }
    AsyncEval::Suspend(mut suspend) => {
      async_frames_push(&mut suspend.frames, AsyncFrame::RestoreLexEnv { outer })?;
      Ok(AsyncEval::Suspend(suspend))
    }
  }
}

fn async_eval_try(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &TryStmt,
) -> Result<AsyncEval<Completion>, VmError> {
  match async_eval_block_stmt(evaluator, scope, &stmt.wrapped.stx)? {
    AsyncEval::Complete(c) => async_try_after_wrapped(evaluator, scope, stmt, c),
    AsyncEval::Suspend(mut suspend) => {
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::TryAfterWrapped {
          stmt: stmt as *const TryStmt,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
  }
}

fn async_try_after_wrapped(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &TryStmt,
  mut result: Completion,
) -> Result<AsyncEval<Completion>, VmError> {
  if matches!(result, Completion::Throw(_)) {
    if let Some(catch) = &stmt.catch {
      let thrown = match result {
        Completion::Throw(thrown) => thrown.value,
        _ => unreachable!(),
      };
      match async_eval_catch(evaluator, scope, &catch.stx, thrown)? {
        AsyncEval::Complete(c) => result = c,
        AsyncEval::Suspend(mut suspend) => {
          async_frames_push(
            &mut suspend.frames,
            AsyncFrame::TryAfterCatch {
              stmt: stmt as *const TryStmt,
            },
          )?;
          return Ok(AsyncEval::Suspend(suspend));
        }
      }
    }
  }

  async_try_after_catch(evaluator, scope, stmt, result)
}

fn async_try_after_catch(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &TryStmt,
  result: Completion,
) -> Result<AsyncEval<Completion>, VmError> {
  let Some(finally) = &stmt.finally else {
    return Ok(AsyncEval::Complete(result.update_empty(Some(Value::Undefined))));
  };

  let pending = RootedCompletion::new(scope, result)?;
  match async_eval_block_stmt(evaluator, scope, &finally.stx)? {
    AsyncEval::Complete(finally_result) => {
      let pending_completion = pending.to_completion(scope.heap())?;
      let mut pending = pending;
      pending.teardown(scope.heap_mut());

      let result = if finally_result.is_abrupt() {
        finally_result
      } else {
        pending_completion
      };
      Ok(AsyncEval::Complete(result.update_empty(Some(Value::Undefined))))
    }
    AsyncEval::Suspend(mut suspend) => {
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::TryAfterFinally { pending },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
  }
}

fn async_eval_stmt_labelled(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &Node<Stmt>,
  label_set: &[String],
) -> Result<AsyncEval<Completion>, VmError> {
  // One tick per statement.
  evaluator.tick()?;

  if !stmt_contains_await(stmt) {
    return Ok(AsyncEval::Complete(evaluator.eval_stmt_labelled(scope, stmt, label_set)?));
  }

  let res = match &*stmt.stx {
    Stmt::Empty(_) => Ok(AsyncEval::Complete(Completion::empty())),
    Stmt::Expr(expr_stmt) => match async_eval_expr(evaluator, scope, &expr_stmt.stx.expr) {
      Ok(AsyncEval::Complete(v)) => Ok(AsyncEval::Complete(Completion::normal(v))),
      Ok(AsyncEval::Suspend(mut suspend)) => {
        async_frames_push(&mut suspend.frames, AsyncFrame::ExprStmt)?;
        Ok(AsyncEval::Suspend(suspend))
      }
      Err(err) => Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?)),
    },
    Stmt::Return(ret) => match &ret.stx.value {
      Some(value_expr) => match async_eval_expr(evaluator, scope, value_expr) {
        Ok(AsyncEval::Complete(v)) => Ok(AsyncEval::Complete(Completion::Return(v))),
        Ok(AsyncEval::Suspend(mut suspend)) => {
          async_frames_push(&mut suspend.frames, AsyncFrame::Return)?;
          Ok(AsyncEval::Suspend(suspend))
        }
        Err(err) => Ok(AsyncEval::Complete(completion_from_expr_result_for_return(Err(err))?)),
      },
      None => Ok(AsyncEval::Complete(Completion::Return(Value::Undefined))),
    },
    Stmt::VarDecl(decl) => async_eval_var_decl(evaluator, scope, &decl.stx, 0),
    Stmt::Debugger(_) => Ok(AsyncEval::Complete(Completion::empty())),
    Stmt::Block(block) => async_eval_block_stmt(evaluator, scope, &block.stx),
    Stmt::If(if_stmt) => {
      match async_eval_expr(evaluator, scope, &if_stmt.stx.test) {
        Ok(AsyncEval::Complete(v)) => {
          if to_boolean(scope.heap(), v)? {
            async_eval_stmt_labelled(evaluator, scope, &if_stmt.stx.consequent, label_set)
          } else if let Some(alt) = &if_stmt.stx.alternate {
            async_eval_stmt_labelled(evaluator, scope, alt, label_set)
          } else {
            Ok(AsyncEval::Complete(Completion::empty()))
          }
        }
        Ok(AsyncEval::Suspend(mut suspend)) => {
          async_frames_push(
            &mut suspend.frames,
            AsyncFrame::IfAfterTest {
              consequent: &if_stmt.stx.consequent as *const Node<Stmt>,
              alternate: if_stmt
                .stx
                .alternate
                .as_ref()
                .map(|alt| alt as *const Node<Stmt>),
            },
          )?;
          Ok(AsyncEval::Suspend(suspend))
        }
        Err(err) => Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?)),
      }
    }
    Stmt::Try(try_stmt) => async_eval_try(evaluator, scope, &try_stmt.stx),
    Stmt::While(while_stmt) => async_eval_while(evaluator, scope, &while_stmt.stx, label_set),
    Stmt::DoWhile(do_while) => async_eval_do_while(evaluator, scope, &do_while.stx, label_set),
    Stmt::ForTriple(for_stmt) => async_eval_for_triple(evaluator, scope, &for_stmt.stx, label_set),
    Stmt::ForIn(for_stmt) => async_eval_for_in(evaluator, scope, &for_stmt.stx, label_set),
    Stmt::ForOf(for_stmt) => async_eval_for_of(evaluator, scope, &for_stmt.stx, label_set),
    Stmt::Switch(switch_stmt) => async_eval_switch(evaluator, scope, &switch_stmt.stx),
    Stmt::Break(break_stmt) => Ok(AsyncEval::Complete(Completion::Break(break_stmt.stx.label.clone(), None))),
    Stmt::Continue(cont_stmt) => Ok(AsyncEval::Complete(Completion::Continue(cont_stmt.stx.label.clone(), None))),
    _ => Err(VmError::Unimplemented("await in statement type")),
  };

  // Async statement evaluation does not go through `Evaluator::eval_stmt_labelled`, so we must
  // ensure `VmError::Throw(..)` propagates as a JS throw completion (catchable by `try/catch`)
  // rather than bubbling out as a fatal internal error.
  match res {
    Ok(v) => Ok(v),
    Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
      Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?))
    }
    Err(err) => Err(err),
  }
}

fn async_eval_var_decl(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  decl: &VarDecl,
  start_declarator_index: usize,
) -> Result<AsyncEval<Completion>, VmError> {
  for (idx, declarator) in decl
    .declarators
    .iter()
    .enumerate()
    .skip(start_declarator_index)
  {
    let Some(init) = &declarator.initializer else {
      match decl.mode {
        VarDeclMode::Var => {
          evaluator.tick()?;
          if !matches!(&*declarator.pattern.stx.pat.stx, Pat::Id(_)) {
            return Err(VmError::Unimplemented(
              "destructuring var without initializer",
            ));
          }
          continue;
        }
        VarDeclMode::Let => {
          let Pat::Id(id) = &*declarator.pattern.stx.pat.stx else {
            return Err(VmError::Unimplemented(
              "destructuring let without initializer",
            ));
          };
          evaluator.tick()?;
          let name = id.stx.name.as_str();
          if !scope.heap().env_has_binding(evaluator.env.lexical_env, name)? {
            scope.env_create_mutable_binding(evaluator.env.lexical_env, name)?;
          }
          scope
            .heap_mut()
            .env_initialize_binding(evaluator.env.lexical_env, name, Value::Undefined)?;
          continue;
        }
        VarDeclMode::Const => {
          return Err(syntax_error(
            declarator.pattern.loc,
            "Missing initializer in const declaration",
          ));
        }
        _ => return Err(VmError::Unimplemented("var declaration kind")),
      }
    };

    match async_eval_expr(evaluator, scope, init) {
      Ok(AsyncEval::Complete(v)) => {
        async_bind_var_declarator_value(evaluator, scope, decl, idx, v)?;
      }
      Ok(AsyncEval::Suspend(mut suspend)) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::VarDecl {
            decl: decl as *const VarDecl,
            next_declarator_index: idx,
          },
        )?;
        return Ok(AsyncEval::Suspend(suspend));
      }
      Err(err) => {
        return Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?));
      }
    }
  }

  Ok(AsyncEval::Complete(Completion::empty()))
}

fn async_bind_var_declarator_value(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  decl: &VarDecl,
  declarator_index: usize,
  value: Value,
) -> Result<(), VmError> {
  let declarator = decl
    .declarators
    .get(declarator_index)
    .ok_or(VmError::InvariantViolation(
      "async var decl continuation out of bounds",
    ))?;

  match decl.mode {
    VarDeclMode::Var => bind_pattern(
      evaluator.vm,
      &mut *evaluator.host,
      &mut *evaluator.hooks,
      scope,
      evaluator.env,
      &declarator.pattern.stx.pat.stx,
      value,
      BindingKind::Var,
      evaluator.strict,
      evaluator.this,
    )?,
    VarDeclMode::Let => {
      let Pat::Id(id) = &*declarator.pattern.stx.pat.stx else {
        bind_pattern(
          evaluator.vm,
          &mut *evaluator.host,
          &mut *evaluator.hooks,
          scope,
          evaluator.env,
          &declarator.pattern.stx.pat.stx,
          value,
          BindingKind::Let,
          evaluator.strict,
          evaluator.this,
        )?;
        return Ok(());
      };

      let name = id.stx.name.as_str();
      if !scope.heap().env_has_binding(evaluator.env.lexical_env, name)? {
        scope.env_create_mutable_binding(evaluator.env.lexical_env, name)?;
      }
      scope
        .heap_mut()
        .env_initialize_binding(evaluator.env.lexical_env, name, value)?;
    }
    VarDeclMode::Const => {
      if !matches!(&*declarator.pattern.stx.pat.stx, Pat::Id(_)) {
        bind_pattern(
          evaluator.vm,
          &mut *evaluator.host,
          &mut *evaluator.hooks,
          scope,
          evaluator.env,
          &declarator.pattern.stx.pat.stx,
          value,
          BindingKind::Const,
          evaluator.strict,
          evaluator.this,
        )?;
        return Ok(());
      }

      let Pat::Id(id) = &*declarator.pattern.stx.pat.stx else {
        return Err(VmError::InvariantViolation(
          "internal error: const declaration pattern mismatch",
        ));
      };
      let name = id.stx.name.as_str();
      if !scope.heap().env_has_binding(evaluator.env.lexical_env, name)? {
        scope.env_create_immutable_binding(evaluator.env.lexical_env, name)?;
      }
      scope
        .heap_mut()
        .env_initialize_binding(evaluator.env.lexical_env, name, value)?;
    }
    _ => return Err(VmError::Unimplemented("var declaration kind")),
  }

  Ok(())
}

fn async_eval_while(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &WhileStmt,
  label_set: &[String],
) -> Result<AsyncEval<Completion>, VmError> {
  let v_root = scope.heap_mut().add_root(Value::Undefined)?;
  let mut label_vec: Vec<String> = Vec::new();
  label_vec
    .try_reserve_exact(label_set.len())
    .map_err(|_| VmError::OutOfMemory)?;
  label_vec.extend_from_slice(label_set);

  match async_eval_expr(evaluator, scope, &stmt.condition) {
    Ok(AsyncEval::Complete(test)) => async_while_after_test(evaluator, scope, stmt, label_vec, v_root, test),
    Ok(AsyncEval::Suspend(mut suspend)) => {
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::WhileAfterTest {
          stmt: stmt as *const WhileStmt,
          label_set: label_vec,
          v_root,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
    Err(err) => {
      scope.heap_mut().remove_root(v_root);
      Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?))
    }
  }
}

fn async_while_after_test(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &WhileStmt,
  label_set: Vec<String>,
  v_root: RootId,
  test_value: Value,
) -> Result<AsyncEval<Completion>, VmError> {
  let v = scope
    .heap()
    .get_root(v_root)
    .ok_or(VmError::InvariantViolation("missing while loop value root"))?;

  if !to_boolean(scope.heap(), test_value)? {
    scope.heap_mut().remove_root(v_root);
    let result = Evaluator::normalise_iteration_break(Completion::normal(v));
    return Ok(AsyncEval::Complete(result));
  }

  match async_eval_stmt_labelled(evaluator, scope, &stmt.body, &[])? {
    AsyncEval::Complete(c) => async_while_after_body(evaluator, scope, stmt, label_set, v_root, c),
    AsyncEval::Suspend(mut suspend) => {
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::WhileAfterBody {
          stmt: stmt as *const WhileStmt,
          label_set,
          v_root,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
  }
}

fn async_while_after_body(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &WhileStmt,
  label_set: Vec<String>,
  v_root: RootId,
  stmt_result: Completion,
) -> Result<AsyncEval<Completion>, VmError> {
  let mut v = scope
    .heap()
    .get_root(v_root)
    .ok_or(VmError::InvariantViolation("missing while loop value root"))?;

  if !Evaluator::loop_continues(&stmt_result, &label_set) {
    scope.heap_mut().remove_root(v_root);
    let result = Evaluator::normalise_iteration_break(stmt_result.update_empty(Some(v)));
    return Ok(AsyncEval::Complete(result));
  }

  if let Some(value) = stmt_result.value() {
    v = value;
    scope.heap_mut().set_root(v_root, v);
  }

  match async_eval_expr(evaluator, scope, &stmt.condition) {
    Ok(AsyncEval::Complete(test)) => async_while_after_test(evaluator, scope, stmt, label_set, v_root, test),
    Ok(AsyncEval::Suspend(mut suspend)) => {
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::WhileAfterTest {
          stmt: stmt as *const WhileStmt,
          label_set,
          v_root,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
    Err(err) => {
      scope.heap_mut().remove_root(v_root);
      Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?))
    }
  }
}

fn async_eval_do_while(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &DoWhileStmt,
  label_set: &[String],
) -> Result<AsyncEval<Completion>, VmError> {
  let v_root = scope.heap_mut().add_root(Value::Undefined)?;
  let mut label_vec: Vec<String> = Vec::new();
  label_vec
    .try_reserve_exact(label_set.len())
    .map_err(|_| VmError::OutOfMemory)?;
  label_vec.extend_from_slice(label_set);

  match async_eval_stmt_labelled(evaluator, scope, &stmt.body, &[]) {
    Ok(AsyncEval::Complete(c)) => async_do_while_after_body(evaluator, scope, stmt, label_vec, v_root, c),
    Ok(AsyncEval::Suspend(mut suspend)) => {
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::DoWhileAfterBody {
          stmt: stmt as *const DoWhileStmt,
          label_set: label_vec,
          v_root,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
    Err(err) => {
      scope.heap_mut().remove_root(v_root);
      Err(err)
    }
  }
}

fn async_do_while_after_body(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &DoWhileStmt,
  label_set: Vec<String>,
  v_root: RootId,
  stmt_result: Completion,
) -> Result<AsyncEval<Completion>, VmError> {
  let mut v = scope
    .heap()
    .get_root(v_root)
    .ok_or(VmError::InvariantViolation("missing do-while loop value root"))?;

  if !Evaluator::loop_continues(&stmt_result, &label_set) {
    scope.heap_mut().remove_root(v_root);
    let result = Evaluator::normalise_iteration_break(stmt_result.update_empty(Some(v)));
    return Ok(AsyncEval::Complete(result));
  }

  if let Some(value) = stmt_result.value() {
    v = value;
    scope.heap_mut().set_root(v_root, v);
  }

  match async_eval_expr(evaluator, scope, &stmt.condition) {
    Ok(AsyncEval::Complete(test)) => async_do_while_after_test(evaluator, scope, stmt, label_set, v_root, test),
    Ok(AsyncEval::Suspend(mut suspend)) => {
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::DoWhileAfterTest {
          stmt: stmt as *const DoWhileStmt,
          label_set,
          v_root,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
    Err(err) => {
      scope.heap_mut().remove_root(v_root);
      Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?))
    }
  }
}

fn async_do_while_after_test(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &DoWhileStmt,
  label_set: Vec<String>,
  v_root: RootId,
  test_value: Value,
) -> Result<AsyncEval<Completion>, VmError> {
  let v = scope
    .heap()
    .get_root(v_root)
    .ok_or(VmError::InvariantViolation("missing do-while loop value root"))?;

  if !to_boolean(scope.heap(), test_value)? {
    scope.heap_mut().remove_root(v_root);
    let result = Evaluator::normalise_iteration_break(Completion::normal(v));
    return Ok(AsyncEval::Complete(result));
  }

  match async_eval_stmt_labelled(evaluator, scope, &stmt.body, &[]) {
    Ok(AsyncEval::Complete(c)) => async_do_while_after_body(evaluator, scope, stmt, label_set, v_root, c),
    Ok(AsyncEval::Suspend(mut suspend)) => {
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::DoWhileAfterBody {
          stmt: stmt as *const DoWhileStmt,
          label_set,
          v_root,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
    Err(err) => {
      scope.heap_mut().remove_root(v_root);
      Err(err)
    }
  }
}

fn async_eval_for_body(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  body: &ForBody,
) -> Result<AsyncEval<Completion>, VmError> {
  async_eval_stmt_list(evaluator, scope, &body.body)
}

fn async_eval_for_triple(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &ForTripleStmt,
  label_set: &[String],
) -> Result<AsyncEval<Completion>, VmError> {
  let v_root = scope.heap_mut().add_root(Value::Undefined)?;
  let mut label_vec: Vec<String> = Vec::new();
  label_vec
    .try_reserve_exact(label_set.len())
    .map_err(|_| VmError::OutOfMemory)?;
  label_vec.extend_from_slice(label_set);

  let needs_explicit_iter_tick =
    stmt.cond.is_none() && stmt.post.is_none() && stmt.body.stx.body.is_empty();

  match &stmt.init {
    parse_js::ast::stmt::ForTripleStmtInit::None => {
      async_for_triple_begin_iteration(evaluator, scope, stmt, label_vec, v_root, needs_explicit_iter_tick)
    }
    parse_js::ast::stmt::ForTripleStmtInit::Expr(expr) => match async_eval_expr(evaluator, scope, expr) {
      Ok(AsyncEval::Complete(_)) => {
        async_for_triple_begin_iteration(evaluator, scope, stmt, label_vec, v_root, needs_explicit_iter_tick)
      }
      Ok(AsyncEval::Suspend(mut suspend)) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::ForTripleAfterInit {
            stmt: stmt as *const ForTripleStmt,
            label_set: label_vec,
            v_root,
            needs_explicit_iter_tick,
          },
        )?;
        Ok(AsyncEval::Suspend(suspend))
      }
      Err(err) => {
        scope.heap_mut().remove_root(v_root);
        Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?))
      }
    },
    parse_js::ast::stmt::ForTripleStmtInit::Decl(decl) => match async_eval_var_decl(evaluator, scope, &decl.stx, 0) {
      Ok(AsyncEval::Complete(c)) => {
        if c.is_abrupt() {
          scope.heap_mut().remove_root(v_root);
          return Ok(AsyncEval::Complete(c));
        }
        async_for_triple_begin_iteration(evaluator, scope, stmt, label_vec, v_root, needs_explicit_iter_tick)
      }
      Ok(AsyncEval::Suspend(mut suspend)) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::ForTripleAfterInit {
            stmt: stmt as *const ForTripleStmt,
            label_set: label_vec,
            v_root,
            needs_explicit_iter_tick,
          },
        )?;
        Ok(AsyncEval::Suspend(suspend))
      }
      Err(err) => {
        scope.heap_mut().remove_root(v_root);
        Err(err)
      }
    },
  }
}

fn async_for_triple_begin_iteration(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &ForTripleStmt,
  label_set: Vec<String>,
  v_root: RootId,
  needs_explicit_iter_tick: bool,
) -> Result<AsyncEval<Completion>, VmError> {
  if needs_explicit_iter_tick {
    if let Err(err) = evaluator.tick() {
      scope.heap_mut().remove_root(v_root);
      return Err(err);
    }
  }

  match &stmt.cond {
    Some(cond) => match async_eval_expr(evaluator, scope, cond) {
      Ok(AsyncEval::Complete(test)) => async_for_triple_after_test(
        evaluator,
        scope,
        stmt,
        label_set,
        v_root,
        needs_explicit_iter_tick,
        test,
      ),
      Ok(AsyncEval::Suspend(mut suspend)) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::ForTripleAfterTest {
            stmt: stmt as *const ForTripleStmt,
            label_set,
            v_root,
            needs_explicit_iter_tick,
          },
        )?;
        Ok(AsyncEval::Suspend(suspend))
      }
      Err(err) => {
        scope.heap_mut().remove_root(v_root);
        Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?))
      }
    },
    None => {
      async_for_triple_after_test(
        evaluator,
        scope,
        stmt,
        label_set,
        v_root,
        needs_explicit_iter_tick,
        Value::Bool(true),
      )
    }
  }
}

fn async_for_triple_after_test(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &ForTripleStmt,
  label_set: Vec<String>,
  v_root: RootId,
  needs_explicit_iter_tick: bool,
  test_value: Value,
) -> Result<AsyncEval<Completion>, VmError> {
  let v = scope
    .heap()
    .get_root(v_root)
    .ok_or(VmError::InvariantViolation("missing for loop value root"))?;

  if !to_boolean(scope.heap(), test_value)? {
    scope.heap_mut().remove_root(v_root);
    return Ok(AsyncEval::Complete(Completion::normal(v)));
  }

  match async_eval_for_body(evaluator, scope, &stmt.body.stx) {
    Ok(AsyncEval::Complete(c)) => async_for_triple_after_body(
      evaluator,
      scope,
      stmt,
      label_set,
      v_root,
      needs_explicit_iter_tick,
      c,
    ),
    Ok(AsyncEval::Suspend(mut suspend)) => {
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::ForTripleAfterBody {
          stmt: stmt as *const ForTripleStmt,
          label_set,
          v_root,
          needs_explicit_iter_tick,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
    Err(err) => {
      scope.heap_mut().remove_root(v_root);
      Err(err)
    }
  }
}

fn async_for_triple_after_body(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &ForTripleStmt,
  label_set: Vec<String>,
  v_root: RootId,
  needs_explicit_iter_tick: bool,
  body_completion: Completion,
) -> Result<AsyncEval<Completion>, VmError> {
  let mut v = scope
    .heap()
    .get_root(v_root)
    .ok_or(VmError::InvariantViolation("missing for loop value root"))?;

  if !Evaluator::loop_continues(&body_completion, &label_set) {
    scope.heap_mut().remove_root(v_root);
    let result = Evaluator::normalise_iteration_break(body_completion.update_empty(Some(v)));
    return Ok(AsyncEval::Complete(result));
  }

  if let Some(value) = body_completion.value() {
    v = value;
    scope.heap_mut().set_root(v_root, v);
  }

  match &stmt.post {
    Some(post) => match async_eval_expr(evaluator, scope, post) {
      Ok(AsyncEval::Complete(_)) => {
        async_for_triple_begin_iteration(evaluator, scope, stmt, label_set, v_root, needs_explicit_iter_tick)
      }
      Ok(AsyncEval::Suspend(mut suspend)) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::ForTripleAfterPost {
            stmt: stmt as *const ForTripleStmt,
            label_set,
            v_root,
            needs_explicit_iter_tick,
          },
        )?;
        Ok(AsyncEval::Suspend(suspend))
      }
      Err(err) => {
        scope.heap_mut().remove_root(v_root);
        Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?))
      }
    },
    None => {
      async_for_triple_begin_iteration(evaluator, scope, stmt, label_set, v_root, needs_explicit_iter_tick)
    }
  }
}

fn async_eval_for_in(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &ForInStmt,
  label_set: &[String],
) -> Result<AsyncEval<Completion>, VmError> {
  let mut label_vec: Vec<String> = Vec::new();
  label_vec
    .try_reserve_exact(label_set.len())
    .map_err(|_| VmError::OutOfMemory)?;
  label_vec.extend_from_slice(label_set);

  match async_eval_expr(evaluator, scope, &stmt.rhs) {
    Ok(AsyncEval::Complete(rhs_value)) => async_for_in_after_rhs(evaluator, scope, stmt, label_vec, rhs_value),
    Ok(AsyncEval::Suspend(mut suspend)) => {
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::ForInAfterRhs {
          stmt: stmt as *const ForInStmt,
          label_set: label_vec,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
    Err(err) => Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?)),
  }
}

fn async_for_in_cleanup(scope: &mut Scope<'_>, object_root: RootId, key_roots: &mut Vec<RootId>, v_root: RootId) {
  if scope.heap().get_root(object_root).is_some() {
    scope.heap_mut().remove_root(object_root);
  }
  for id in key_roots.drain(..) {
    if scope.heap().get_root(id).is_some() {
      scope.heap_mut().remove_root(id);
    }
  }
  if scope.heap().get_root(v_root).is_some() {
    scope.heap_mut().remove_root(v_root);
  }
}

fn async_for_in_after_rhs(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &ForInStmt,
  label_set: Vec<String>,
  rhs_value: Value,
) -> Result<AsyncEval<Completion>, VmError> {
  if is_nullish(rhs_value) {
    return Ok(AsyncEval::Complete(Completion::normal(Value::Undefined)));
  }

  // `for..in` uses `ToObject` on the RHS. Until we have full wrapper objects, treat the `Object`
  // constructor as a converter for primitives.
  let object = match rhs_value {
    Value::Object(obj) => obj,
    other => {
      let intr = evaluator
        .vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
      let object_ctor = Value::Object(intr.object_constructor());

      let mut to_obj_scope = scope.reborrow();
      to_obj_scope.push_roots(&[other, object_ctor])?;
      let args = [other];
      let value = evaluator
        .call(&mut to_obj_scope, object_ctor, Value::Undefined, &args)
        .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut to_obj_scope, err))?;
      match value {
        Value::Object(obj) => obj,
        _ => {
          return Err(VmError::InvariantViolation(
            "Object(..) conversion returned non-object",
          ));
        }
      }
    }
  };

  // Root the base object across key collection + loop body evaluation, which may allocate and
  // trigger GC.
  let object_root = {
    let mut root_scope = scope.reborrow();
    root_scope.push_root(Value::Object(object))?;
    root_scope.heap_mut().add_root(Value::Object(object))?
  };

  // Snapshot enumerable string keys across the prototype chain, skipping duplicates.
  //
  // Note: this is intentionally minimal and does not track mutations during iteration.
  const KEY_COLLECTION_TICK_EVERY: usize = 256;
  let mut keys: Vec<RootId> = Vec::new();
  let mut visited: Vec<PropertyKey> = Vec::new();

  let mut key_count: usize = 0;
  // De-duplication uses a linear scan over the keys collected so far, which can be `O(N^2)` in
  // the worst case. Tick periodically while scanning to ensure this work stays interruptible even
  // for very large objects/prototype chains.
  const VISITED_SCAN_TICK_EVERY: usize = 4096;
  let mut visited_scan_count: usize = 0;

  let collect_res: Result<(), VmError> = (|| {
    let mut current: Option<GcObject> = Some(object);
    while let Some(obj) = current {
      evaluator.tick()?;

      let own_keys = scope
        .ordinary_own_property_keys_with_tick(obj, || evaluator.tick())
        .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, scope, err))?;
      for key in own_keys {
        key_count = key_count.wrapping_add(1);
        if (key_count & (KEY_COLLECTION_TICK_EVERY - 1)) == 0 {
          evaluator.tick()?;
        }

        let PropertyKey::String(s) = key else {
          continue;
        };

        let Some(desc) = scope
          .ordinary_get_own_property(obj, key)
          .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, scope, err))?
        else {
          continue;
        };
        if !desc.enumerable {
          continue;
        }

        let mut already_visited = false;
        for seen in &visited {
          visited_scan_count = visited_scan_count.wrapping_add(1);
          if (visited_scan_count & (VISITED_SCAN_TICK_EVERY - 1)) == 0 {
            evaluator.tick()?;
          }
          if scope.heap().property_key_eq(seen, &key) {
            already_visited = true;
            break;
          }
        }
        if already_visited {
          continue;
        }

        visited.push(key);

        // Ensure we have space in `keys` before allocating a persistent root so we don't leak the
        // root on an OOM during `Vec` growth.
        keys.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;

        let id = {
          let mut root_scope = scope.reborrow();
          root_scope.push_root(Value::String(s))?;
          root_scope.heap_mut().add_root(Value::String(s))?
        };
        keys.push(id);
      }

      current = scope
        .object_get_prototype(obj)
        .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, scope, err))?;
    }
    Ok(())
  })();

  if let Err(err) = collect_res {
    scope.heap_mut().remove_root(object_root);
    for id in keys.drain(..) {
      scope.heap_mut().remove_root(id);
    }
    return Err(err);
  }

  let v_root = match scope.heap_mut().add_root(Value::Undefined) {
    Ok(id) => id,
    Err(err) => {
      scope.heap_mut().remove_root(object_root);
      for id in keys.drain(..) {
        scope.heap_mut().remove_root(id);
      }
      return Err(err);
    }
  };

  let outer_lex = evaluator.env.lexical_env;
  async_for_in_loop_from(
    evaluator,
    scope,
    stmt,
    label_set,
    object_root,
    keys,
    0,
    v_root,
    outer_lex,
  )
}

fn async_for_in_loop_from(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &ForInStmt,
  label_set: Vec<String>,
  object_root: RootId,
  mut key_roots: Vec<RootId>,
  start_index: usize,
  v_root: RootId,
  outer_lex: GcEnv,
) -> Result<AsyncEval<Completion>, VmError> {
  for (i, key_root) in key_roots.iter().copied().enumerate().skip(start_index) {
    // Tick once per iteration so `for (k in o) {}` is budgeted even when the body is empty.
    if let Err(err) = evaluator.tick() {
      async_for_in_cleanup(scope, object_root, &mut key_roots, v_root);
      return Err(err);
    }

    let mut iter_env_created = false;
    if let ForInOfLhs::Decl((mode, _)) = &stmt.lhs {
      if *mode == VarDeclMode::Let || *mode == VarDeclMode::Const {
        let env = match scope.env_create(Some(outer_lex)) {
          Ok(env) => env,
          Err(err) => {
            async_for_in_cleanup(scope, object_root, &mut key_roots, v_root);
            return Err(err);
          }
        };
        evaluator.env.set_lexical_env(scope.heap_mut(), env);
        iter_env_created = true;
      }
    }

    let value = match scope.heap().get_root(key_root) {
      Some(v) => v,
      None => {
        scope.heap_mut().remove_root(object_root);
        for id in key_roots.drain(..) {
          if scope.heap().get_root(id).is_some() {
            scope.heap_mut().remove_root(id);
          }
        }
        if scope.heap().get_root(v_root).is_some() {
          scope.heap_mut().remove_root(v_root);
        }
        return Err(VmError::InvariantViolation("missing for-in key root"));
      }
    };

    let bind_res: Result<(), VmError> = match &stmt.lhs {
      ForInOfLhs::Decl((mode, pat_decl)) => {
        let kind = match *mode {
          VarDeclMode::Var => BindingKind::Var,
          VarDeclMode::Let => BindingKind::Let,
          VarDeclMode::Const => BindingKind::Const,
          _ => return Err(VmError::Unimplemented("for-in loop variable declaration kind")),
        };
        bind_pattern(
          evaluator.vm,
          &mut *evaluator.host,
          &mut *evaluator.hooks,
          scope,
          evaluator.env,
          &pat_decl.stx.pat.stx,
          value,
          kind,
          evaluator.strict,
          evaluator.this,
        )
      }
      ForInOfLhs::Assign(pat) => bind_pattern(
        evaluator.vm,
        &mut *evaluator.host,
        &mut *evaluator.hooks,
        scope,
        evaluator.env,
        &pat.stx,
        value,
        BindingKind::Assignment,
        evaluator.strict,
        evaluator.this,
      ),
    };

    if let Err(err) = bind_res {
      if iter_env_created {
        evaluator.env.set_lexical_env(scope.heap_mut(), outer_lex);
      }
      async_for_in_cleanup(scope, object_root, &mut key_roots, v_root);
      return Err(err);
    }

    let body_eval = match async_eval_for_body(evaluator, scope, &stmt.body.stx) {
      Ok(v) => v,
      Err(err) => {
        if iter_env_created {
          evaluator.env.set_lexical_env(scope.heap_mut(), outer_lex);
        }
        async_for_in_cleanup(scope, object_root, &mut key_roots, v_root);
        return Err(err);
      }
    };

    match body_eval {
      AsyncEval::Complete(body_completion) => {
        if iter_env_created {
          evaluator.env.set_lexical_env(scope.heap_mut(), outer_lex);
        }

        let mut v = match scope.heap().get_root(v_root) {
          Some(v) => v,
          None => {
            scope.heap_mut().remove_root(object_root);
            for id in key_roots.drain(..) {
              scope.heap_mut().remove_root(id);
            }
            return Err(VmError::InvariantViolation("missing for-in loop value root"));
          }
        };

        if !Evaluator::loop_continues(&body_completion, &label_set) {
          async_for_in_cleanup(scope, object_root, &mut key_roots, v_root);
          let result = Evaluator::normalise_iteration_break(body_completion.update_empty(Some(v)));
          return Ok(AsyncEval::Complete(result));
        }

        if let Some(value) = body_completion.value() {
          v = value;
          scope.heap_mut().set_root(v_root, v);
        }
      }
      AsyncEval::Suspend(mut suspend) => {
        if iter_env_created {
          async_frames_push(&mut suspend.frames, AsyncFrame::RestoreLexEnv { outer: outer_lex })?;
        }

        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::ForInAfterBody {
            stmt: stmt as *const ForInStmt,
            label_set,
            object_root,
            key_roots,
            next_key_index: i.saturating_add(1),
            v_root,
            outer_lex,
          },
        )?;
        return Ok(AsyncEval::Suspend(suspend));
      }
    }
  }

  let v = match scope.heap().get_root(v_root) {
    Some(v) => v,
    None => {
      scope.heap_mut().remove_root(object_root);
      for id in key_roots.drain(..) {
        scope.heap_mut().remove_root(id);
      }
      return Err(VmError::InvariantViolation("missing for-in loop value root"));
    }
  };
  async_for_in_cleanup(scope, object_root, &mut key_roots, v_root);
  Ok(AsyncEval::Complete(Completion::normal(v)))
}

fn async_eval_for_of(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &ForOfStmt,
  label_set: &[String],
) -> Result<AsyncEval<Completion>, VmError> {
  let mut label_vec: Vec<String> = Vec::new();
  label_vec
    .try_reserve_exact(label_set.len())
    .map_err(|_| VmError::OutOfMemory)?;
  label_vec.extend_from_slice(label_set);

  if stmt.await_ {
    return Err(VmError::Unimplemented("for await..of"));
  }

  match async_eval_expr(evaluator, scope, &stmt.rhs) {
    Ok(AsyncEval::Complete(iterable)) => async_for_of_after_rhs(evaluator, scope, stmt, label_vec, iterable),
    Ok(AsyncEval::Suspend(mut suspend)) => {
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::ForOfAfterRhs {
          stmt: stmt as *const ForOfStmt,
          label_set: label_vec,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
    Err(err) => Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?)),
  }
}

fn async_for_of_cleanup(
  scope: &mut Scope<'_>,
  iterator_root: RootId,
  next_method_root: RootId,
  v_root: RootId,
) {
  if scope.heap().get_root(iterator_root).is_some() {
    scope.heap_mut().remove_root(iterator_root);
  }
  if scope.heap().get_root(next_method_root).is_some() {
    scope.heap_mut().remove_root(next_method_root);
  }
  if scope.heap().get_root(v_root).is_some() {
    scope.heap_mut().remove_root(v_root);
  }
}

fn async_for_of_after_rhs(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &ForOfStmt,
  label_set: Vec<String>,
  iterable: Value,
) -> Result<AsyncEval<Completion>, VmError> {
  let mut iter_scope = scope.reborrow();
  iter_scope.push_root(iterable)?;

  let iterator_record = iterator::get_iterator(
    evaluator.vm,
    &mut *evaluator.host,
    &mut *evaluator.hooks,
    &mut iter_scope,
    iterable,
  )
  .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut iter_scope, err))?;

  let (iterator_root, next_method_root) = {
    let mut root_scope = iter_scope.reborrow();
    root_scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;
    let iterator_root = root_scope.heap_mut().add_root(iterator_record.iterator)?;
    let next_method_root = root_scope.heap_mut().add_root(iterator_record.next_method)?;
    (iterator_root, next_method_root)
  };

  let v_root = match iter_scope.heap_mut().add_root(Value::Undefined) {
    Ok(id) => id,
    Err(err) => {
      iter_scope.heap_mut().remove_root(iterator_root);
      iter_scope.heap_mut().remove_root(next_method_root);
      return Err(err);
    }
  };

  let outer_lex = evaluator.env.lexical_env;
  async_for_of_loop(
    evaluator,
    &mut iter_scope,
    stmt,
    label_set,
    iterator_record,
    iterator_root,
    next_method_root,
    v_root,
    outer_lex,
  )
}

fn async_for_of_loop(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &ForOfStmt,
  label_set: Vec<String>,
  mut iterator_record: iterator::IteratorRecord,
  iterator_root: RootId,
  next_method_root: RootId,
  v_root: RootId,
  outer_lex: GcEnv,
) -> Result<AsyncEval<Completion>, VmError> {
  loop {
    // Tick once per iteration so `for (x of xs) {}` is budgeted even when the body is empty.
    if let Err(err) = evaluator.tick() {
      let _ = iterator::iterator_close(
        evaluator.vm,
        &mut *evaluator.host,
        &mut *evaluator.hooks,
        scope,
        &iterator_record,
      );
      async_for_of_cleanup(scope, iterator_root, next_method_root, v_root);
      return Err(err);
    }

    let next_value = match iterator::iterator_step_value(
      evaluator.vm,
      &mut *evaluator.host,
      &mut *evaluator.hooks,
      scope,
      &mut iterator_record,
    ) {
      Ok(v) => v,
      Err(err) => {
        let _ = iterator::iterator_close(
          evaluator.vm,
          &mut *evaluator.host,
          &mut *evaluator.hooks,
          scope,
          &iterator_record,
        );
        async_for_of_cleanup(scope, iterator_root, next_method_root, v_root);
        return Err(coerce_error_to_throw_for_async(evaluator.vm, scope, err));
      }
    };

    let Some(value) = next_value else {
      let v = match scope.heap().get_root(v_root) {
        Some(v) => v,
        None => {
          scope.heap_mut().remove_root(iterator_root);
          scope.heap_mut().remove_root(next_method_root);
          return Err(VmError::InvariantViolation("missing for-of loop value root"));
        }
      };
      async_for_of_cleanup(scope, iterator_root, next_method_root, v_root);
      return Ok(AsyncEval::Complete(Completion::normal(v)));
    };

    let mut iter_env_created = false;
    if let ForInOfLhs::Decl((mode, _)) = &stmt.lhs {
      if *mode == VarDeclMode::Let || *mode == VarDeclMode::Const {
        let env = match scope.env_create(Some(outer_lex)) {
          Ok(env) => env,
          Err(err) => {
            let _ = iterator::iterator_close(
              evaluator.vm,
              &mut *evaluator.host,
              &mut *evaluator.hooks,
              scope,
              &iterator_record,
            );
            async_for_of_cleanup(scope, iterator_root, next_method_root, v_root);
            return Err(err);
          }
        };
        evaluator.env.set_lexical_env(scope.heap_mut(), env);
        iter_env_created = true;
      }
    }

    let bind_res: Result<(), VmError> = match &stmt.lhs {
      ForInOfLhs::Decl((mode, pat_decl)) => {
        let kind = match *mode {
          VarDeclMode::Var => BindingKind::Var,
          VarDeclMode::Let => BindingKind::Let,
          VarDeclMode::Const => BindingKind::Const,
          _ => return Err(VmError::Unimplemented("for-of loop variable declaration kind")),
        };
        bind_pattern(
          evaluator.vm,
          &mut *evaluator.host,
          &mut *evaluator.hooks,
          scope,
          evaluator.env,
          &pat_decl.stx.pat.stx,
          value,
          kind,
          evaluator.strict,
          evaluator.this,
        )
      }
      ForInOfLhs::Assign(pat) => bind_pattern(
        evaluator.vm,
        &mut *evaluator.host,
        &mut *evaluator.hooks,
        scope,
        evaluator.env,
        &pat.stx,
        value,
        BindingKind::Assignment,
        evaluator.strict,
        evaluator.this,
      ),
    };

    if let Err(err) = bind_res {
      if iter_env_created {
        evaluator.env.set_lexical_env(scope.heap_mut(), outer_lex);
      }
      let _ = iterator::iterator_close(
        evaluator.vm,
        &mut *evaluator.host,
        &mut *evaluator.hooks,
        scope,
        &iterator_record,
      );
      async_for_of_cleanup(scope, iterator_root, next_method_root, v_root);
      return Err(err);
    }

    let body_eval = match async_eval_for_body(evaluator, scope, &stmt.body.stx) {
      Ok(v) => v,
      Err(err) => {
        if iter_env_created {
          evaluator.env.set_lexical_env(scope.heap_mut(), outer_lex);
        }
        let _ = iterator::iterator_close(
          evaluator.vm,
          &mut *evaluator.host,
          &mut *evaluator.hooks,
          scope,
          &iterator_record,
        );
        async_for_of_cleanup(scope, iterator_root, next_method_root, v_root);
        return Err(err);
      }
    };

    match body_eval {
      AsyncEval::Complete(body_completion) => {
        if iter_env_created {
          evaluator.env.set_lexical_env(scope.heap_mut(), outer_lex);
        }

        let mut v = match scope.heap().get_root(v_root) {
          Some(v) => v,
          None => {
            let _ = iterator::iterator_close(
              evaluator.vm,
              &mut *evaluator.host,
              &mut *evaluator.hooks,
              scope,
              &iterator_record,
            );
            scope.heap_mut().remove_root(iterator_root);
            scope.heap_mut().remove_root(next_method_root);
            return Err(VmError::InvariantViolation("missing for-of loop value root"));
          }
        };

        if !Evaluator::loop_continues(&body_completion, &label_set) {
          let _ = iterator::iterator_close(
            evaluator.vm,
            &mut *evaluator.host,
            &mut *evaluator.hooks,
            scope,
            &iterator_record,
          );
          async_for_of_cleanup(scope, iterator_root, next_method_root, v_root);
          let result = Evaluator::normalise_iteration_break(body_completion.update_empty(Some(v)));
          return Ok(AsyncEval::Complete(result));
        }

        if let Some(value) = body_completion.value() {
          v = value;
          scope.heap_mut().set_root(v_root, v);
        }
      }
      AsyncEval::Suspend(mut suspend) => {
        if iter_env_created {
          async_frames_push(&mut suspend.frames, AsyncFrame::RestoreLexEnv { outer: outer_lex })?;
        }

        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::ForOfAfterBody {
            stmt: stmt as *const ForOfStmt,
            label_set,
            iterator_record,
            iterator_root,
            next_method_root,
            v_root,
            outer_lex,
          },
        )?;
        return Ok(AsyncEval::Suspend(suspend));
      }
    }
  }
}

fn async_eval_switch(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &SwitchStmt,
) -> Result<AsyncEval<Completion>, VmError> {
  match async_eval_expr(evaluator, scope, &stmt.test) {
    Ok(AsyncEval::Complete(discriminant)) => async_switch_after_discriminant(evaluator, scope, stmt, discriminant),
    Ok(AsyncEval::Suspend(mut suspend)) => {
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::SwitchAfterDiscriminant {
          stmt: stmt as *const SwitchStmt,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
    Err(err) => Ok(AsyncEval::Complete(completion_from_expr_result(Err(err))?)),
  }
}

fn async_switch_cleanup(scope: &mut Scope<'_>, discriminant_root: RootId, v_root: RootId) {
  if scope.heap().get_root(discriminant_root).is_some() {
    scope.heap_mut().remove_root(discriminant_root);
  }
  if scope.heap().get_root(v_root).is_some() {
    scope.heap_mut().remove_root(v_root);
  }
}

fn async_switch_finish(
  scope: &mut Scope<'_>,
  discriminant_root: RootId,
  v_root: RootId,
  completion: Completion,
) -> Result<Completion, VmError> {
  async_switch_cleanup(scope, discriminant_root, v_root);
  Ok(Evaluator::normalise_iteration_break(completion))
}

fn async_switch_after_discriminant(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &SwitchStmt,
  discriminant: Value,
) -> Result<AsyncEval<Completion>, VmError> {
  // Root the discriminant across selector evaluation and case-body execution, which may allocate
  // and trigger GC.
  let discriminant_root = {
    let mut root_scope = scope.reborrow();
    root_scope.push_root(discriminant)?;
    root_scope.heap_mut().add_root(discriminant)?
  };

  // `switch` creates a new lexical environment for the entire case block.
  let outer = evaluator.env.lexical_env;
  let switch_env = match scope.env_create(Some(outer)) {
    Ok(env) => env,
    Err(err) => {
      scope.heap_mut().remove_root(discriminant_root);
      return Err(err);
    }
  };
  evaluator
    .env
    .set_lexical_env(scope.heap_mut(), switch_env);

  // Instantiate lexical declarations for the shared switch scope.
  const BRANCH_TICK_EVERY: usize = 32;
  for (i, branch) in stmt.branches.iter().enumerate() {
    if i % BRANCH_TICK_EVERY == 0 {
      if let Err(err) = evaluator.tick() {
        evaluator.env.set_lexical_env(scope.heap_mut(), outer);
        scope.heap_mut().remove_root(discriminant_root);
        return Err(err);
      }
    }
    if let Err(err) =
      evaluator.instantiate_block_decls_in_stmt_list(scope, switch_env, &branch.stx.body)
    {
      evaluator.env.set_lexical_env(scope.heap_mut(), outer);
      scope.heap_mut().remove_root(discriminant_root);
      return Err(err);
    }
  }

  // ECMA-262 `CaseBlockEvaluation`: `V` starts as `undefined` and is never ~empty~ for normal
  // completion.
  let v_root = match scope.heap_mut().add_root(Value::Undefined) {
    Ok(id) => id,
    Err(err) => {
      evaluator.env.set_lexical_env(scope.heap_mut(), outer);
      scope.heap_mut().remove_root(discriminant_root);
      return Err(err);
    }
  };

  match async_switch_scan_and_exec_from(evaluator, scope, stmt, discriminant_root, v_root, 0, None) {
    Ok(AsyncEval::Complete(c)) => {
      evaluator.env.set_lexical_env(scope.heap_mut(), outer);
      Ok(AsyncEval::Complete(c))
    }
    Ok(AsyncEval::Suspend(mut suspend)) => {
      async_frames_push(&mut suspend.frames, AsyncFrame::RestoreLexEnv { outer })?;
      Ok(AsyncEval::Suspend(suspend))
    }
    Err(err) => {
      evaluator.env.set_lexical_env(scope.heap_mut(), outer);
      Err(err)
    }
  }
}

fn async_switch_scan_and_exec_from(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &SwitchStmt,
  discriminant_root: RootId,
  v_root: RootId,
  start_index: usize,
  mut default_index: Option<usize>,
) -> Result<AsyncEval<Completion>, VmError> {
  let discriminant = match scope.heap().get_root(discriminant_root) {
    Some(v) => v,
    None => {
      async_switch_cleanup(scope, discriminant_root, v_root);
      return Err(VmError::InvariantViolation("missing switch discriminant root"));
    }
  };

  const BRANCH_TICK_EVERY: usize = 32;
  for (i, branch) in stmt.branches.iter().enumerate().skip(start_index) {
    if i % BRANCH_TICK_EVERY == 0 {
      if let Err(err) = evaluator.tick() {
        async_switch_cleanup(scope, discriminant_root, v_root);
        return Err(err);
      }
    }

    let Some(case_expr) = &branch.stx.case else {
      if default_index.is_none() {
        default_index = Some(i);
      }
      continue;
    };

    match async_eval_expr(evaluator, scope, case_expr) {
      Ok(AsyncEval::Complete(case_value)) => {
        let matches = match strict_equal(scope.heap(), discriminant, case_value) {
          Ok(m) => m,
          Err(err) => {
            async_switch_cleanup(scope, discriminant_root, v_root);
            return Err(err);
          }
        };
        if matches {
          return async_switch_exec_from(evaluator, scope, stmt, discriminant_root, v_root, i);
        }
      }
      Ok(AsyncEval::Suspend(mut suspend)) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::SwitchAfterCaseExpr {
            stmt: stmt as *const SwitchStmt,
            discriminant_root,
            v_root,
            default_index,
            branch_index: i,
          },
        )?;
        return Ok(AsyncEval::Suspend(suspend));
      }
      Err(err) => {
        let completion = completion_from_expr_result(Err(err))?;
        let completion = async_switch_finish(scope, discriminant_root, v_root, completion)?;
        return Ok(AsyncEval::Complete(completion));
      }
    }
  }

  let Some(default_idx) = default_index else {
    let v = match scope.heap().get_root(v_root) {
      Some(v) => v,
      None => {
        async_switch_cleanup(scope, discriminant_root, v_root);
        return Err(VmError::InvariantViolation("missing switch V root"));
      }
    };
    let completion = async_switch_finish(scope, discriminant_root, v_root, Completion::normal(v))?;
    return Ok(AsyncEval::Complete(completion));
  };

  async_switch_exec_from(
    evaluator,
    scope,
    stmt,
    discriminant_root,
    v_root,
    default_idx,
  )
}

fn async_switch_exec_from(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &SwitchStmt,
  discriminant_root: RootId,
  v_root: RootId,
  start_index: usize,
) -> Result<AsyncEval<Completion>, VmError> {
  const BRANCH_TICK_EVERY: usize = 32;
  for (i, branch) in stmt.branches.iter().enumerate().skip(start_index) {
    if i % BRANCH_TICK_EVERY == 0 {
      if let Err(err) = evaluator.tick() {
        async_switch_cleanup(scope, discriminant_root, v_root);
        return Err(err);
      }
    }

    match async_eval_stmt_list(evaluator, scope, &branch.stx.body) {
      Ok(AsyncEval::Complete(body_completion)) => {
        match async_switch_after_body_completion(
          evaluator,
          scope,
          stmt,
          discriminant_root,
          v_root,
          i.saturating_add(1),
          body_completion,
        ) {
          Ok(AsyncEval::Complete(done)) => return Ok(AsyncEval::Complete(done)),
          Ok(AsyncEval::Suspend(suspend)) => return Ok(AsyncEval::Suspend(suspend)),
          Err(err) => {
            async_switch_cleanup(scope, discriminant_root, v_root);
            return Err(err);
          }
        }
      }
      Ok(AsyncEval::Suspend(mut suspend)) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::SwitchAfterBody {
            stmt: stmt as *const SwitchStmt,
            discriminant_root,
            v_root,
            next_branch_index: i.saturating_add(1),
          },
        )?;
        return Ok(AsyncEval::Suspend(suspend));
      }
      Err(err) => {
        async_switch_cleanup(scope, discriminant_root, v_root);
        return Err(err);
      }
    }
  }

  let v = match scope.heap().get_root(v_root) {
    Some(v) => v,
    None => {
      async_switch_cleanup(scope, discriminant_root, v_root);
      return Err(VmError::InvariantViolation("missing switch V root"));
    }
  };
  let completion = async_switch_finish(scope, discriminant_root, v_root, Completion::normal(v))?;
  Ok(AsyncEval::Complete(completion))
}

fn async_switch_after_body_completion(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  stmt: &SwitchStmt,
  discriminant_root: RootId,
  v_root: RootId,
  next_branch_index: usize,
  body_completion: Completion,
) -> Result<AsyncEval<Completion>, VmError> {
  let mut v = match scope.heap().get_root(v_root) {
    Some(v) => v,
    None => {
      async_switch_cleanup(scope, discriminant_root, v_root);
      return Err(VmError::InvariantViolation("missing switch V root"));
    }
  };

  if let Some(value) = body_completion.value() {
    v = value;
    scope.heap_mut().set_root(v_root, v);
  }

  if body_completion.is_abrupt() {
    let completion = body_completion.update_empty(Some(v));
    let completion = async_switch_finish(scope, discriminant_root, v_root, completion)?;
    return Ok(AsyncEval::Complete(completion));
  }

  async_switch_exec_from(
    evaluator,
    scope,
    stmt,
    discriminant_root,
    v_root,
    next_branch_index,
  )
}

fn async_eval_expr(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  expr: &Node<Expr>,
) -> Result<AsyncEval<Value>, VmError> {
  evaluator.tick()?;

  if !expr_contains_await(expr) {
    return match evaluator.eval_expr(scope, expr) {
      Ok(v) => Ok(AsyncEval::Complete(v)),
      Err(err) => Err(coerce_error_to_throw_for_async(evaluator.vm, scope, err)),
    };
  }

  match &*expr.stx {
    Expr::Unary(unary) if unary.stx.operator == OperatorName::Await => {
      match async_eval_expr(evaluator, scope, &unary.stx.argument)? {
        AsyncEval::Complete(v) => Ok(AsyncEval::Suspend(AsyncSuspend {
          await_value: v,
          frames: VecDeque::new(),
        })),
        AsyncEval::Suspend(mut suspend) => {
          async_frames_push(&mut suspend.frames, AsyncFrame::AwaitAfterOperand)?;
          Ok(AsyncEval::Suspend(suspend))
        }
      }
    }
    Expr::Unary(unary) => {
      // Only reached when the operand contains an await (otherwise the early return above covers
      // the whole expression).
      match async_eval_expr(evaluator, scope, &unary.stx.argument)? {
        AsyncEval::Complete(v) => {
          let out = async_apply_unary_operator(evaluator, scope, &unary.stx, v)?;
          Ok(AsyncEval::Complete(out))
        }
        AsyncEval::Suspend(mut suspend) => {
          async_frames_push(
            &mut suspend.frames,
            AsyncFrame::UnaryAfterArgument {
              expr: &*unary.stx as *const UnaryExpr,
            },
          )?;
          Ok(AsyncEval::Suspend(suspend))
        }
      }
    }
    Expr::Member(member) => match async_eval_expr(evaluator, scope, &member.stx.left)? {
      AsyncEval::Complete(base) => Ok(AsyncEval::Complete(async_member_after_base(
        evaluator,
        scope,
        &member.stx,
        base,
      )?)),
      AsyncEval::Suspend(mut suspend) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::MemberAfterBase {
            expr: &*member.stx as *const MemberExpr,
          },
        )?;
        Ok(AsyncEval::Suspend(suspend))
      }
    },
    Expr::ComputedMember(member) => match async_eval_expr(evaluator, scope, &member.stx.object)? {
      AsyncEval::Complete(base) => async_computed_member_after_base(evaluator, scope, &member.stx, base),
      AsyncEval::Suspend(mut suspend) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::ComputedMemberAfterBase {
            expr: &*member.stx as *const ComputedMemberExpr,
          },
        )?;
        Ok(AsyncEval::Suspend(suspend))
      }
    },
    Expr::Call(call) => async_eval_call(evaluator, scope, &call.stx),
    Expr::Cond(cond) => match async_eval_expr(evaluator, scope, &cond.stx.test)? {
      AsyncEval::Complete(test) => {
        if to_boolean(scope.heap(), test)? {
          async_eval_expr(evaluator, scope, &cond.stx.consequent)
        } else {
          async_eval_expr(evaluator, scope, &cond.stx.alternate)
        }
      }
      AsyncEval::Suspend(mut suspend) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::CondAfterTest {
            expr: &*cond.stx as *const CondExpr,
          },
        )?;
        Ok(AsyncEval::Suspend(suspend))
      }
    },
    Expr::Binary(binary) if binary.stx.operator == OperatorName::Assignment => {
      async_eval_assignment_expr(evaluator, scope, &binary.stx)
    }
    Expr::Binary(binary) => match async_eval_expr(evaluator, scope, &binary.stx.left)? {
      AsyncEval::Complete(left) => async_binary_after_left(evaluator, scope, &binary.stx, left),
      AsyncEval::Suspend(mut suspend) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::BinaryAfterLeft {
            expr: &*binary.stx as *const BinaryExpr,
          },
        )?;
        Ok(AsyncEval::Suspend(suspend))
      }
    },
    _ => Err(VmError::Unimplemented("await in expression type")),
  }
}

fn async_eval_assignment_expr(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  expr: &BinaryExpr,
) -> Result<AsyncEval<Value>, VmError> {
  debug_assert_eq!(expr.operator, OperatorName::Assignment);

  // Destructuring assignment patterns appear in expression position as `Expr::ObjPat` /
  // `Expr::ArrPat` nodes. These are not valid "references" and must be handled by pattern binding.
  if matches!(&*expr.left.stx, Expr::ObjPat(_) | Expr::ArrPat(_)) {
    return match async_eval_expr(evaluator, scope, &expr.right) {
      Ok(AsyncEval::Complete(v)) => {
        let mut bind_scope = scope.reborrow();
        bind_scope.push_root(v)?;
        bind_assignment_target(
          evaluator.vm,
          &mut *evaluator.host,
          &mut *evaluator.hooks,
          &mut bind_scope,
          evaluator.env,
          &expr.left,
          v,
          evaluator.strict,
          evaluator.this,
        )
        .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut bind_scope, err))?;
        Ok(AsyncEval::Complete(v))
      }
      Ok(AsyncEval::Suspend(mut suspend)) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::AssignAfterRhsPattern {
            expr: expr as *const BinaryExpr,
          },
        )?;
        Ok(AsyncEval::Suspend(suspend))
      }
      Err(err) => Err(err),
    };
  }

  // Evaluate the assignment target reference before the RHS (ECMA-262 `AssignmentExpression`).
  let reference =
    evaluator
      .eval_reference(scope, &expr.left)
      .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, scope, err))?;

  match reference {
    Reference::Binding(_) => {
      let name_ptr = match &*expr.left.stx {
        Expr::Id(id) => &id.stx.name as *const String,
        Expr::IdPat(id) => &id.stx.name as *const String,
        _ => {
          return Err(VmError::InvariantViolation(
            "binding reference without identifier assignment target",
          ))
        }
      };

      match async_eval_expr(evaluator, scope, &expr.right) {
        Ok(AsyncEval::Complete(v)) => {
          evaluator
            .env
            .set(
              evaluator.vm,
              &mut *evaluator.host,
              &mut *evaluator.hooks,
              scope,
              unsafe { &*name_ptr },
              v,
              evaluator.strict,
            )
            .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, scope, err))?;
          Ok(AsyncEval::Complete(v))
        }
        Ok(AsyncEval::Suspend(mut suspend)) => {
          async_frames_push(&mut suspend.frames, AsyncFrame::AssignAfterRhsBinding { name: name_ptr })?;
          Ok(AsyncEval::Suspend(suspend))
        }
        Err(err) => Err(err),
      }
    }
    Reference::Property { base, key } => {
      let key_value = match key {
        PropertyKey::String(s) => Value::String(s),
        PropertyKey::Symbol(sym) => Value::Symbol(sym),
      };

      let (base_root, key_root) = {
        let mut root_scope = scope.reborrow();
        root_scope.push_roots(&[base, key_value])?;
        let base_root = root_scope.heap_mut().add_root(base)?;
        let key_root = root_scope.heap_mut().add_root(key_value)?;
        (base_root, key_root)
      };

      match async_eval_expr(evaluator, scope, &expr.right) {
        Ok(AsyncEval::Complete(v)) => {
          let res = async_assign_to_rooted_property_reference(evaluator, scope, base_root, key_root, v);
          // `async_assign_to_rooted_property_reference` always removes `base_root/key_root`.
          res.map(AsyncEval::Complete)
        }
        Ok(AsyncEval::Suspend(mut suspend)) => {
          async_frames_push(
            &mut suspend.frames,
            AsyncFrame::AssignAfterRhsProperty { base_root, key_root },
          )?;
          Ok(AsyncEval::Suspend(suspend))
        }
        Err(err) => {
          scope.heap_mut().remove_root(base_root);
          scope.heap_mut().remove_root(key_root);
          Err(err)
        }
      }
    }
  }
}

fn async_assign_to_rooted_property_reference(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  base_root: RootId,
  key_root: RootId,
  value: Value,
) -> Result<Value, VmError> {
  let base = scope
    .heap()
    .get_root(base_root)
    .ok_or(VmError::InvariantViolation(
      "missing assignment target base root",
    ))?;
  let key_value = scope
    .heap()
    .get_root(key_root)
    .ok_or(VmError::InvariantViolation("missing assignment target key root"))?;
  let key = match key_value {
    Value::String(s) => PropertyKey::from_string(s),
    Value::Symbol(sym) => PropertyKey::from_symbol(sym),
    _ => {
      scope.heap_mut().remove_root(base_root);
      scope.heap_mut().remove_root(key_root);
      return Err(VmError::InvariantViolation(
        "assignment target key root is not a string or symbol",
      ));
    }
  };

  let mut assign_scope = scope.reborrow();
  assign_scope.push_roots(&[base, key_value, value])?;
  let reference = Reference::Property { base, key };
  let res = evaluator
    .put_value_to_reference(&mut assign_scope, &reference, value)
    .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut assign_scope, err));

  // Always remove roots; they are no longer needed after assignment completes.
  assign_scope.heap_mut().remove_root(base_root);
  assign_scope.heap_mut().remove_root(key_root);

  res?;
  Ok(value)
}

fn async_apply_unary_operator(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  expr: &UnaryExpr,
  argument: Value,
) -> Result<Value, VmError> {
  match expr.operator {
    OperatorName::LogicalNot => Ok(Value::Bool(!to_boolean(scope.heap(), argument)?)),
    OperatorName::UnaryPlus => Ok(Value::Number(evaluator.to_number_operator(scope, argument)?)),
    OperatorName::UnaryNegation => {
      let num = evaluator.to_numeric(scope, argument)?;
      Ok(match num {
        NumericValue::Number(n) => Value::Number(-n),
        NumericValue::BigInt(b) => Value::BigInt(b.negate()),
      })
    }
    OperatorName::BitwiseNot => {
      let num = evaluator.to_numeric(scope, argument)?;
      Ok(match num {
        NumericValue::Number(n) => Value::Number((!to_int32(n)) as f64),
        NumericValue::BigInt(b) => {
          let Some(out) = b.checked_bitwise_not() else {
            return Err(VmError::Unimplemented("BigInt bitwise not out of range"));
          };
          Value::BigInt(out)
        }
      })
    }
    OperatorName::Typeof => {
      let t = typeof_name(scope.heap(), argument)?;
      let s = scope.alloc_string(t)?;
      Ok(Value::String(s))
    }
    OperatorName::Void => Ok(Value::Undefined),
    _ => Err(VmError::Unimplemented("await in unary operator")),
  }
}

fn async_member_after_base(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  expr: &MemberExpr,
  base: Value,
) -> Result<Value, VmError> {
  if expr.optional_chaining && is_nullish(base) {
    return Ok(Value::Undefined);
  }

  let mut key_scope = scope.reborrow();
  key_scope.push_root(base)?;
  let key_s = key_scope.alloc_string(&expr.right)?;
  let reference = Reference::Property {
    base,
    key: PropertyKey::from_string(key_s),
  };
  evaluator
    .get_value_from_reference(&mut key_scope, &reference)
    .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut key_scope, err))
}

fn async_computed_member_after_base(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  expr: &ComputedMemberExpr,
  base: Value,
) -> Result<AsyncEval<Value>, VmError> {
  if expr.optional_chaining && is_nullish(base) {
    return Ok(AsyncEval::Complete(Value::Undefined));
  }

  match async_eval_expr(evaluator, scope, &expr.member)? {
    AsyncEval::Complete(member_value) => {
      let value = async_computed_member_after_member(evaluator, scope, expr, base, member_value)?;
      Ok(AsyncEval::Complete(value))
    }
    AsyncEval::Suspend(mut suspend) => {
      let mut root_scope = scope.reborrow();
      root_scope.push_root(base)?;
      let base_root = root_scope.heap_mut().add_root(base)?;
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::ComputedMemberAfterMember {
          expr: expr as *const ComputedMemberExpr,
          base_root,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
  }
}

fn async_computed_member_after_member(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  _expr: &ComputedMemberExpr,
  base: Value,
  member_value: Value,
) -> Result<Value, VmError> {
  let mut key_scope = scope.reborrow();
  key_scope.push_root(base)?;
  key_scope.push_root(member_value)?;
  let key = evaluator
    .to_property_key_operator(&mut key_scope, member_value)
    .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut key_scope, err))?;
  let reference = Reference::Property { base, key };
  evaluator
    .get_value_from_reference(&mut key_scope, &reference)
    .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut key_scope, err))
}

fn async_binary_after_left(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  expr: &BinaryExpr,
  left: Value,
) -> Result<AsyncEval<Value>, VmError> {
  match expr.operator {
    OperatorName::LogicalAnd => {
      if !to_boolean(scope.heap(), left)? {
        return Ok(AsyncEval::Complete(left));
      }
      async_eval_expr(evaluator, scope, &expr.right)
    }
    OperatorName::LogicalOr => {
      if to_boolean(scope.heap(), left)? {
        return Ok(AsyncEval::Complete(left));
      }
      async_eval_expr(evaluator, scope, &expr.right)
    }
    OperatorName::NullishCoalescing => {
      if !is_nullish(left) {
        return Ok(AsyncEval::Complete(left));
      }
      async_eval_expr(evaluator, scope, &expr.right)
    }
    OperatorName::Addition
    | OperatorName::StrictEquality
    | OperatorName::StrictInequality
    | OperatorName::Equality
    | OperatorName::Inequality
    | OperatorName::LessThan
    | OperatorName::LessThanOrEqual
    | OperatorName::GreaterThan
    | OperatorName::GreaterThanOrEqual => {
      // Root `left` across evaluation of `right` in case the RHS allocates and triggers GC.
      let mut rhs_scope = scope.reborrow();
      rhs_scope.push_root(left)?;

      match async_eval_expr(evaluator, &mut rhs_scope, &expr.right)? {
        AsyncEval::Complete(right) => {
          let out = async_apply_binary_operator(evaluator, &mut rhs_scope, expr.operator, left, right)?;
          Ok(AsyncEval::Complete(out))
        }
        AsyncEval::Suspend(mut suspend) => {
          let left_root = rhs_scope.heap_mut().add_root(left)?;
          async_frames_push(
            &mut suspend.frames,
            AsyncFrame::BinaryAfterRight {
              expr: expr as *const BinaryExpr,
              left_root,
            },
          )?;
          Ok(AsyncEval::Suspend(suspend))
        }
      }
    }
    _ => Err(VmError::Unimplemented("await in binary operator")),
  }
}

fn async_apply_binary_operator(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  operator: OperatorName,
  left: Value,
  right: Value,
) -> Result<Value, VmError> {
  match operator {
    OperatorName::Addition => {
      let mut op_scope = scope.reborrow();
      op_scope.push_roots(&[left, right])?;
      evaluator
        .addition_operator(&mut op_scope, left, right)
        .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut op_scope, err))
    }
    OperatorName::StrictEquality => Ok(Value::Bool(strict_equal(scope.heap(), left, right)?)),
    OperatorName::StrictInequality => Ok(Value::Bool(!strict_equal(scope.heap(), left, right)?)),
    OperatorName::Equality => {
      let mut op_scope = scope.reborrow();
      op_scope.push_roots(&[left, right])?;
      let ok = abstract_equality(op_scope.heap_mut(), left, right)
        .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut op_scope, err))?;
      Ok(Value::Bool(ok))
    }
    OperatorName::Inequality => {
      let mut op_scope = scope.reborrow();
      op_scope.push_roots(&[left, right])?;
      let ok = abstract_equality(op_scope.heap_mut(), left, right)
        .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut op_scope, err))?;
      Ok(Value::Bool(!ok))
    }
    OperatorName::LessThan
    | OperatorName::LessThanOrEqual
    | OperatorName::GreaterThan
    | OperatorName::GreaterThanOrEqual => {
      let mut op_scope = scope.reborrow();
      op_scope.push_roots(&[left, right])?;
      let left_n = evaluator
        .to_number_operator(&mut op_scope, left)
        .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut op_scope, err))?;
      let right_n = evaluator
        .to_number_operator(&mut op_scope, right)
        .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut op_scope, err))?;

      Ok(match operator {
        OperatorName::LessThan => Value::Bool(left_n < right_n),
        OperatorName::LessThanOrEqual => Value::Bool(left_n <= right_n),
        OperatorName::GreaterThan => Value::Bool(left_n > right_n),
        OperatorName::GreaterThanOrEqual => Value::Bool(left_n >= right_n),
        _ => unreachable!(),
      })
    }
    _ => Err(VmError::Unimplemented("binary operator")),
  }
}

fn async_eval_call(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  expr: &CallExpr,
) -> Result<AsyncEval<Value>, VmError> {
  match &*expr.callee.stx {
    Expr::Member(member) => match async_eval_expr(evaluator, scope, &member.stx.left)? {
      AsyncEval::Complete(base) => {
        async_call_member_after_base(evaluator, scope, expr, &member.stx, base)
      }
      AsyncEval::Suspend(mut suspend) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::CallMemberAfterBase {
            expr: expr as *const CallExpr,
            member: &*member.stx as *const MemberExpr,
          },
        )?;
        Ok(AsyncEval::Suspend(suspend))
      }
    },
    Expr::ComputedMember(member) => match async_eval_expr(evaluator, scope, &member.stx.object)? {
      AsyncEval::Complete(base) => {
        async_call_computed_member_after_base(evaluator, scope, expr, &member.stx, base)
      }
      AsyncEval::Suspend(mut suspend) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::CallComputedMemberAfterBase {
            expr: expr as *const CallExpr,
            member: &*member.stx as *const ComputedMemberExpr,
          },
        )?;
        Ok(AsyncEval::Suspend(suspend))
      }
    },
    _ => match async_eval_expr(evaluator, scope, &expr.callee)? {
      AsyncEval::Complete(callee_value) => {
        async_call_begin(evaluator, scope, expr, callee_value, Value::Undefined)
      }
      AsyncEval::Suspend(mut suspend) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::CallAfterCallee {
            expr: expr as *const CallExpr,
          },
        )?;
        Ok(AsyncEval::Suspend(suspend))
      }
    },
  }
}

fn async_call_member_after_base(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  call: &CallExpr,
  member: &MemberExpr,
  base: Value,
) -> Result<AsyncEval<Value>, VmError> {
  if member.optional_chaining && is_nullish(base) {
    return Ok(AsyncEval::Complete(Value::Undefined));
  }
  if is_nullish(base) {
    return Err(throw_type_error(
      evaluator.vm,
      scope,
      "Cannot convert undefined or null to object",
    )?);
  }

  let callee_value = {
    let mut key_scope = scope.reborrow();
    key_scope.push_root(base)?;
    let key_s = key_scope.alloc_string(&member.right)?;
    let reference = Reference::Property {
      base,
      key: PropertyKey::from_string(key_s),
    };
    evaluator
      .get_value_from_reference(&mut key_scope, &reference)
      .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut key_scope, err))?
  };

  async_call_begin(evaluator, scope, call, callee_value, base)
}

fn async_call_computed_member_after_base(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  call: &CallExpr,
  member: &ComputedMemberExpr,
  base: Value,
) -> Result<AsyncEval<Value>, VmError> {
  if member.optional_chaining && is_nullish(base) {
    return Ok(AsyncEval::Complete(Value::Undefined));
  }

  match async_eval_expr(evaluator, scope, &member.member)? {
    AsyncEval::Complete(member_value) => {
      async_call_computed_member_after_member(evaluator, scope, call, member, base, member_value)
    }
    AsyncEval::Suspend(mut suspend) => {
      let mut root_scope = scope.reborrow();
      root_scope.push_root(base)?;
      let base_root = root_scope.heap_mut().add_root(base)?;
      async_frames_push(
        &mut suspend.frames,
        AsyncFrame::CallComputedMemberAfterMember {
          expr: call as *const CallExpr,
          member: member as *const ComputedMemberExpr,
          base_root,
        },
      )?;
      Ok(AsyncEval::Suspend(suspend))
    }
  }
}

fn async_call_computed_member_after_member(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  call: &CallExpr,
  _member: &ComputedMemberExpr,
  base: Value,
  member_value: Value,
) -> Result<AsyncEval<Value>, VmError> {
  let mut key_scope = scope.reborrow();
  key_scope.push_root(base)?;
  key_scope.push_root(member_value)?;
  let key = evaluator
    .to_property_key_operator(&mut key_scope, member_value)
    .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut key_scope, err))?;

  if is_nullish(base) {
    return Err(throw_type_error(
      evaluator.vm,
      &mut key_scope,
      "Cannot convert undefined or null to object",
    )?);
  }

  let reference = Reference::Property { base, key };
  let callee_value = evaluator
    .get_value_from_reference(&mut key_scope, &reference)
    .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut key_scope, err))?;

  // Drop `key_scope` before proceeding to argument evaluation (which reborrows `scope`).
  drop(key_scope);

  async_call_begin(evaluator, scope, call, callee_value, base)
}

fn async_call_begin(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  call: &CallExpr,
  callee_value: Value,
  this_value: Value,
) -> Result<AsyncEval<Value>, VmError> {
  // Optional call: if the callee is nullish, return `undefined` without evaluating args.
  if call.optional_chaining && is_nullish(callee_value) {
    return Ok(AsyncEval::Complete(Value::Undefined));
  }

  // Fast-path: no arguments means no opportunity to suspend during argument evaluation, so we can
  // call directly without allocating a `CallArgs` frame.
  if call.arguments.is_empty() {
    let mut call_scope = scope.reborrow();
    call_scope.push_roots(&[callee_value, this_value])?;
    let res = evaluator
      .call(&mut call_scope, callee_value, this_value, &[])
      .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut call_scope, err));
    return match res {
      Ok(v) => Ok(AsyncEval::Complete(v)),
      Err(err) => Err(err),
    };
  }

  // Create persistent roots for `callee`/`this` so they survive across argument-evaluation `await`
  // points.
  let (callee_root, this_root) = {
    let mut root_scope = scope.reborrow();
    root_scope.push_roots(&[callee_value, this_value])?;
    let callee_root = root_scope.heap_mut().add_root(callee_value)?;
    let this_root = root_scope.heap_mut().add_root(this_value)?;
    (callee_root, this_root)
  };

  async_call_continue_args(
    evaluator,
    scope,
    call,
    callee_root,
    this_root,
    Vec::new(),
    0,
  )
}

fn async_call_cleanup(scope: &mut Scope<'_>, callee_root: RootId, this_root: RootId, arg_roots: &mut Vec<RootId>) {
  scope.heap_mut().remove_root(callee_root);
  scope.heap_mut().remove_root(this_root);
  for id in arg_roots.drain(..) {
    scope.heap_mut().remove_root(id);
  }
}

fn async_call_store_arg_value(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  spread: bool,
  value: Value,
  arg_roots: &mut Vec<RootId>,
) -> Result<(), VmError> {
  if spread {
    let mut iter_scope = scope.reborrow();
    iter_scope.push_root(value)?;
    let mut iter = iterator::get_iterator(
      evaluator.vm,
      &mut *evaluator.host,
      &mut *evaluator.hooks,
      &mut iter_scope,
      value,
    )
    .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut iter_scope, err))?;
    iter_scope.push_roots(&[iter.iterator, iter.next_method])?;
    while let Some(v) = iterator::iterator_step_value(
      evaluator.vm,
      &mut *evaluator.host,
      &mut *evaluator.hooks,
      &mut iter_scope,
      &mut iter,
    )
    .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut iter_scope, err))?
    {
      evaluator.tick()?;
      let mut root_scope = iter_scope.reborrow();
      root_scope.push_root(v)?;
      let id = root_scope.heap_mut().add_root(v)?;
      arg_roots.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      arg_roots.push(id);
    }
  } else {
    let mut root_scope = scope.reborrow();
    root_scope.push_root(value)?;
    let id = root_scope.heap_mut().add_root(value)?;
    arg_roots.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
    arg_roots.push(id);
  }
  Ok(())
}

fn async_call_continue_args(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  call: &CallExpr,
  callee_root: RootId,
  this_root: RootId,
  mut arg_roots: Vec<RootId>,
  start_index: usize,
) -> Result<AsyncEval<Value>, VmError> {
  // Evaluate each remaining argument in order.
  for (idx, arg) in call.arguments.iter().enumerate().skip(start_index) {
    match async_eval_expr(evaluator, scope, &arg.stx.value) {
      Ok(AsyncEval::Complete(v)) => {
        async_call_store_arg_value(evaluator, scope, arg.stx.spread, v, &mut arg_roots)?;
      }
      Ok(AsyncEval::Suspend(mut suspend)) => {
        async_frames_push(
          &mut suspend.frames,
          AsyncFrame::CallArgs {
            expr: call as *const CallExpr,
            callee_root,
            this_root,
            arg_roots,
            arg_index: idx,
          },
        )?;
        return Ok(AsyncEval::Suspend(suspend));
      }
      Err(err) => {
        async_call_cleanup(scope, callee_root, this_root, &mut arg_roots);
        return Err(err);
      }
    }
  }

  // Invoke the call with the collected arguments.
  let callee_value = scope
    .heap()
    .get_root(callee_root)
    .ok_or(VmError::InvariantViolation("missing call callee root"))?;
  let this_value = scope
    .heap()
    .get_root(this_root)
    .ok_or(VmError::InvariantViolation("missing call this root"))?;

  let mut args: Vec<Value> = Vec::new();
  args
    .try_reserve_exact(arg_roots.len())
    .map_err(|_| VmError::OutOfMemory)?;
  for id in arg_roots.iter().copied() {
    let v = scope
      .heap()
      .get_root(id)
      .ok_or(VmError::InvariantViolation("missing call arg root"))?;
    args.push(v);
  }

  let res = evaluator
    .call(scope, callee_value, this_value, &args)
    .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, scope, err));

  // Clean up roots after call completion.
  async_call_cleanup(scope, callee_root, this_root, &mut arg_roots);

  match res {
    Ok(v) => Ok(AsyncEval::Complete(v)),
    Err(err) => Err(err),
  }
}

fn async_start_body(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  func: &Arc<Node<Func>>,
) -> Result<AsyncBodyResult, VmError> {
  let Some(body) = &func.stx.body else {
    return Err(VmError::Unimplemented("function without body"));
  };

  match body {
    FuncBody::Expression(expr) => match async_eval_expr(evaluator, scope, expr) {
      Ok(AsyncEval::Complete(v)) => Ok(AsyncBodyResult::CompleteOk(v)),
      Ok(AsyncEval::Suspend(mut suspend)) => {
        async_frames_push(&mut suspend.frames, AsyncFrame::RootExprBody)?;
        Ok(AsyncBodyResult::Await {
          await_value: suspend.await_value,
          frames: suspend.frames,
        })
      }
      Err(VmError::Throw(value)) | Err(VmError::ThrowWithStack { value, .. }) => {
        Ok(AsyncBodyResult::CompleteThrow(value))
      }
      Err(err) => Err(err),
    },
    FuncBody::Block(stmts) => match async_eval_stmt_list(evaluator, scope, stmts) {
      Ok(AsyncEval::Complete(completion)) => match completion {
        Completion::Normal(_) => Ok(AsyncBodyResult::CompleteOk(Value::Undefined)),
        Completion::Return(v) => Ok(AsyncBodyResult::CompleteOk(v)),
        Completion::Throw(thrown) => Ok(AsyncBodyResult::CompleteThrow(thrown.value)),
        Completion::Break(..) => Err(VmError::Unimplemented("break outside of loop")),
        Completion::Continue(..) => Err(VmError::Unimplemented("continue outside of loop")),
      },
      Ok(AsyncEval::Suspend(mut suspend)) => {
        async_frames_push(&mut suspend.frames, AsyncFrame::RootBlockBody)?;
        Ok(AsyncBodyResult::Await {
          await_value: suspend.await_value,
          frames: suspend.frames,
        })
      }
      Err(err) => Err(err),
    },
  }
}

fn async_resume_from_frames(
  evaluator: &mut Evaluator<'_>,
  scope: &mut Scope<'_>,
  mut frames: VecDeque<AsyncFrame>,
  resume_value: Result<Value, Value>,
) -> Result<AsyncBodyResult, VmError> {
  enum AsyncState {
    Expr(Result<Value, VmError>),
    Completion(Completion),
  }

  // The resumed value is the result of the suspended `await` expression. Awaited promise rejection
  // must re-enter the async function as a thrown completion at the await site.
  let mut state = AsyncState::Expr(match resume_value {
    Ok(v) => Ok(v),
    Err(reason) => Err(VmError::Throw(reason)),
  });

  loop {
    evaluator.tick()?;
    let Some(frame) = frames.pop_front() else {
      return Err(VmError::InvariantViolation(
        "async continuation resumed with empty frame stack",
      ));
    };

    match frame {
      AsyncFrame::RootExprBody => match state {
        AsyncState::Expr(res) => match res {
          Ok(v) => return Ok(AsyncBodyResult::CompleteOk(v)),
          Err(VmError::Throw(value) | VmError::ThrowWithStack { value, .. }) => {
            return Ok(AsyncBodyResult::CompleteThrow(value))
          }
          Err(err) => return Err(err),
        },
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "async expr body resumed with completion state",
          ))
        }
      },

      AsyncFrame::RootBlockBody => match state {
        AsyncState::Completion(completion) => match completion {
          Completion::Normal(_) => return Ok(AsyncBodyResult::CompleteOk(Value::Undefined)),
          Completion::Return(v) => return Ok(AsyncBodyResult::CompleteOk(v)),
          Completion::Throw(thrown) => return Ok(AsyncBodyResult::CompleteThrow(thrown.value)),
          Completion::Break(..) => return Err(VmError::Unimplemented("break outside of loop")),
          Completion::Continue(..) => return Err(VmError::Unimplemented("continue outside of loop")),
        },
        AsyncState::Expr(_) => {
          return Err(VmError::InvariantViolation(
            "async block body resumed with expression state",
          ))
        }
      },

      AsyncFrame::AwaitAfterOperand => match state {
        AsyncState::Expr(Ok(v)) => {
          return Ok(AsyncBodyResult::Await {
            await_value: v,
            frames,
          })
        }
        AsyncState::Expr(Err(err)) => state = AsyncState::Expr(Err(err)),
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "await operand frame received completion state",
          ))
        }
      },

      AsyncFrame::UnaryAfterArgument { expr } => match state {
        AsyncState::Expr(Ok(v)) => {
          let expr = unsafe { &*expr };
          match async_apply_unary_operator(evaluator, scope, expr, v) {
            Ok(out) => state = AsyncState::Expr(Ok(out)),
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              state = AsyncState::Expr(Err(err))
            }
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(Err(err)) => state = AsyncState::Expr(Err(err)),
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "unary operator frame received completion state",
          ))
        }
      },

      AsyncFrame::MemberAfterBase { expr } => match state {
        AsyncState::Expr(Ok(base)) => {
          let expr = unsafe { &*expr };
          match async_member_after_base(evaluator, scope, expr, base) {
            Ok(v) => state = AsyncState::Expr(Ok(v)),
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              state = AsyncState::Expr(Err(err))
            }
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(Err(err)) => state = AsyncState::Expr(Err(err)),
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "member access frame received completion state",
          ))
        }
      },

      AsyncFrame::ComputedMemberAfterBase { expr } => match state {
        AsyncState::Expr(Ok(base)) => {
          let expr = unsafe { &*expr };
          match async_computed_member_after_base(evaluator, scope, expr, base) {
            Ok(AsyncEval::Complete(v)) => state = AsyncState::Expr(Ok(v)),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              state = AsyncState::Expr(Err(err))
            }
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(Err(err)) => state = AsyncState::Expr(Err(err)),
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "computed member frame received completion state",
          ))
        }
      },

      AsyncFrame::ComputedMemberAfterMember { expr, base_root } => match state {
        AsyncState::Expr(member_res) => {
          let expr = unsafe { &*expr };
          let base = scope
            .heap()
            .get_root(base_root)
            .ok_or(VmError::InvariantViolation(
              "missing computed member base root",
            ))?;
          scope.heap_mut().remove_root(base_root);

          match member_res {
            Ok(member_value) => match async_computed_member_after_member(
              evaluator,
              scope,
              expr,
              base,
              member_value,
            ) {
              Ok(v) => state = AsyncState::Expr(Ok(v)),
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                state = AsyncState::Expr(Err(err))
              }
              Err(err) => return Err(err),
            },
            Err(err) => state = AsyncState::Expr(Err(err)),
          }
        }
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "computed member after member frame received completion state",
          ))
        }
      },

      AsyncFrame::CondAfterTest { expr } => match state {
        AsyncState::Expr(Ok(test)) => {
          let expr = unsafe { &*expr };
          let branch = if to_boolean(scope.heap(), test)? {
            &expr.consequent
          } else {
            &expr.alternate
          };

          match async_eval_expr(evaluator, scope, branch) {
            Ok(AsyncEval::Complete(v)) => state = AsyncState::Expr(Ok(v)),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              state = AsyncState::Expr(Err(err))
            }
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(Err(err)) => state = AsyncState::Expr(Err(err)),
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "conditional test frame received completion state",
          ))
        }
      },

      AsyncFrame::BinaryAfterLeft { expr } => match state {
        AsyncState::Expr(Ok(left)) => {
          let expr = unsafe { &*expr };
          match async_binary_after_left(evaluator, scope, expr, left) {
            Ok(AsyncEval::Complete(v)) => state = AsyncState::Expr(Ok(v)),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              state = AsyncState::Expr(Err(err))
            }
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(Err(err)) => state = AsyncState::Expr(Err(err)),
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "binary operator frame received completion state",
          ))
        }
      },

      AsyncFrame::BinaryAfterRight { expr, left_root } => match state {
        AsyncState::Expr(right_res) => {
          let expr = unsafe { &*expr };

          let left = scope
            .heap()
            .get_root(left_root)
            .ok_or(VmError::InvariantViolation("missing binary left root"))?;
          scope.heap_mut().remove_root(left_root);

          match right_res {
            Ok(right) => {
              match async_apply_binary_operator(evaluator, scope, expr.operator, left, right) {
                Ok(v) => state = AsyncState::Expr(Ok(v)),
                Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => state = AsyncState::Expr(Err(err)),
                Err(err) => return Err(err),
              }
            }
            Err(err) => state = AsyncState::Expr(Err(err)),
          }
        }
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "binary right frame received completion state",
          ))
        }
      },

      AsyncFrame::AssignAfterRhsBinding { name } => match state {
        AsyncState::Expr(Ok(v)) => {
          let name = unsafe { &*name };
          match evaluator
            .env
            .set(
              evaluator.vm,
              &mut *evaluator.host,
              &mut *evaluator.hooks,
              scope,
              name,
              v,
              evaluator.strict,
            )
            .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, scope, err))
          {
            Ok(()) => state = AsyncState::Expr(Ok(v)),
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => state = AsyncState::Expr(Err(err)),
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(Err(err)) => state = AsyncState::Expr(Err(err)),
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "assign-after-rhs frame received completion state",
          ))
        }
      },

      AsyncFrame::AssignAfterRhsProperty { base_root, key_root } => match state {
        AsyncState::Expr(rhs_res) => match rhs_res {
          Ok(v) => match async_assign_to_rooted_property_reference(evaluator, scope, base_root, key_root, v) {
            Ok(v) => state = AsyncState::Expr(Ok(v)),
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => state = AsyncState::Expr(Err(err)),
            Err(err) => return Err(err),
          },
          Err(err) => {
            scope.heap_mut().remove_root(base_root);
            scope.heap_mut().remove_root(key_root);
            state = AsyncState::Expr(Err(err));
          }
        },
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "assign-after-rhs-property frame received completion state",
          ))
        }
      },

      AsyncFrame::AssignAfterRhsPattern { expr } => match state {
        AsyncState::Expr(Ok(v)) => {
          let expr = unsafe { &*expr };
          let mut bind_scope = scope.reborrow();
          bind_scope.push_root(v)?;
          match bind_assignment_target(
            evaluator.vm,
            &mut *evaluator.host,
            &mut *evaluator.hooks,
            &mut bind_scope,
            evaluator.env,
            &expr.left,
            v,
            evaluator.strict,
            evaluator.this,
          )
          .map_err(|err| coerce_error_to_throw_for_async(evaluator.vm, &mut bind_scope, err))
          {
            Ok(()) => state = AsyncState::Expr(Ok(v)),
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => state = AsyncState::Expr(Err(err)),
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(Err(err)) => state = AsyncState::Expr(Err(err)),
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "assign-after-rhs-pattern frame received completion state",
          ))
        }
      },

      AsyncFrame::CallAfterCallee { expr } => match state {
        AsyncState::Expr(Ok(callee_value)) => {
          let expr = unsafe { &*expr };
          match async_call_begin(evaluator, scope, expr, callee_value, Value::Undefined) {
            Ok(AsyncEval::Complete(v)) => state = AsyncState::Expr(Ok(v)),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              state = AsyncState::Expr(Err(err))
            }
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(Err(err)) => state = AsyncState::Expr(Err(err)),
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "call frame received completion state",
          ))
        }
      },

      AsyncFrame::CallMemberAfterBase { expr, member } => match state {
        AsyncState::Expr(Ok(base)) => {
          let expr = unsafe { &*expr };
          let member = unsafe { &*member };
          match async_call_member_after_base(evaluator, scope, expr, member, base) {
            Ok(AsyncEval::Complete(v)) => state = AsyncState::Expr(Ok(v)),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              state = AsyncState::Expr(Err(err))
            }
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(Err(err)) => state = AsyncState::Expr(Err(err)),
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "call member frame received completion state",
          ))
        }
      },

      AsyncFrame::CallComputedMemberAfterBase { expr, member } => match state {
        AsyncState::Expr(Ok(base)) => {
          let expr = unsafe { &*expr };
          let member = unsafe { &*member };
          match async_call_computed_member_after_base(evaluator, scope, expr, member, base) {
            Ok(AsyncEval::Complete(v)) => state = AsyncState::Expr(Ok(v)),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              state = AsyncState::Expr(Err(err))
            }
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(Err(err)) => state = AsyncState::Expr(Err(err)),
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "call computed member base frame received completion state",
          ))
        }
      },

      AsyncFrame::CallComputedMemberAfterMember {
        expr,
        member,
        base_root,
      } => match state {
        AsyncState::Expr(member_res) => {
          let expr = unsafe { &*expr };
          let member = unsafe { &*member };
          let base = scope
            .heap()
            .get_root(base_root)
            .ok_or(VmError::InvariantViolation(
              "missing computed call base root",
            ))?;
          scope.heap_mut().remove_root(base_root);

          match member_res {
            Ok(member_value) => match async_call_computed_member_after_member(
              evaluator,
              scope,
              expr,
              member,
              base,
              member_value,
            ) {
              Ok(AsyncEval::Complete(v)) => state = AsyncState::Expr(Ok(v)),
              Ok(AsyncEval::Suspend(mut suspend)) => {
                suspend.frames.append(&mut frames);
                return Ok(AsyncBodyResult::Await {
                  await_value: suspend.await_value,
                  frames: suspend.frames,
                });
              }
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                state = AsyncState::Expr(Err(err))
              }
              Err(err) => return Err(err),
            },
            Err(err) => state = AsyncState::Expr(Err(err)),
          }
        }
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "call computed member after member frame received completion state",
          ))
        }
      },

      AsyncFrame::CallArgs {
        expr,
        callee_root,
        this_root,
        mut arg_roots,
        arg_index,
      } => match state {
        AsyncState::Expr(arg_res) => {
          let expr = unsafe { &*expr };
          let Some(arg) = expr.arguments.get(arg_index) else {
            async_call_cleanup(scope, callee_root, this_root, &mut arg_roots);
            return Err(VmError::InvariantViolation(
              "async call args continuation out of bounds",
            ));
          };

          match arg_res {
            Ok(v) => {
              if let Err(err) =
                async_call_store_arg_value(evaluator, scope, arg.stx.spread, v, &mut arg_roots)
              {
                async_call_cleanup(scope, callee_root, this_root, &mut arg_roots);
                state = AsyncState::Expr(Err(err));
                continue;
              }

              match async_call_continue_args(
                evaluator,
                scope,
                expr,
                callee_root,
                this_root,
                arg_roots,
                arg_index.saturating_add(1),
              ) {
                Ok(AsyncEval::Complete(v)) => state = AsyncState::Expr(Ok(v)),
                Ok(AsyncEval::Suspend(mut suspend)) => {
                  suspend.frames.append(&mut frames);
                  return Ok(AsyncBodyResult::Await {
                    await_value: suspend.await_value,
                    frames: suspend.frames,
                  });
                }
                Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                  // `async_call_continue_args` cleans up roots when returning Err.
                  state = AsyncState::Expr(Err(err))
                }
                Err(err) => return Err(err),
              }
            }
            Err(err) => {
              async_call_cleanup(scope, callee_root, this_root, &mut arg_roots);
              state = AsyncState::Expr(Err(err));
            }
          }
        }
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "call args frame received completion state",
          ))
        }
      },

      AsyncFrame::ExprStmt => match state {
        AsyncState::Expr(expr_res) => {
          let completion = completion_from_expr_result(expr_res)?;
          state = AsyncState::Completion(completion);
        }
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "expr stmt frame received completion state",
          ))
        }
      },

      AsyncFrame::Return => match state {
        AsyncState::Expr(expr_res) => {
          let completion = completion_from_expr_result_for_return(expr_res)?;
          state = AsyncState::Completion(completion);
        }
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "return frame received completion state",
          ))
        }
      },

      AsyncFrame::VarDecl {
        decl,
        next_declarator_index,
      } => match state {
        AsyncState::Expr(expr_res) => match expr_res {
          Ok(v) => {
            let decl = unsafe { &*decl };
            if let Err(err) =
              async_bind_var_declarator_value(evaluator, scope, decl, next_declarator_index, v)
            {
              state = AsyncState::Completion(completion_from_expr_result(Err(err))?);
              continue;
            }

            match async_eval_var_decl(
              evaluator,
              scope,
              decl,
              next_declarator_index.saturating_add(1),
            ) {
              Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
              Ok(AsyncEval::Suspend(mut suspend)) => {
                suspend.frames.append(&mut frames);
                return Ok(AsyncBodyResult::Await {
                  await_value: suspend.await_value,
                  frames: suspend.frames,
                });
              }
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
              }
              Err(err) => return Err(err),
            }
          }
          Err(err) => state = AsyncState::Completion(completion_from_expr_result(Err(err))?),
        },
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "var decl frame received completion state",
          ))
        }
      },

      AsyncFrame::IfAfterTest {
        consequent,
        alternate,
      } => match state {
        AsyncState::Expr(test_res) => match test_res {
          Ok(test_value) => {
            let stmt = if to_boolean(scope.heap(), test_value)? {
              unsafe { &*consequent }
            } else if let Some(alt) = alternate {
              unsafe { &*alt }
            } else {
              state = AsyncState::Completion(Completion::empty());
              continue;
            };

            match async_eval_stmt_labelled(evaluator, scope, stmt, &[])? {
              AsyncEval::Complete(c) => state = AsyncState::Completion(c),
              AsyncEval::Suspend(mut suspend) => {
                suspend.frames.append(&mut frames);
                return Ok(AsyncBodyResult::Await {
                  await_value: suspend.await_value,
                  frames: suspend.frames,
                });
              }
            }
          }
          Err(err) => state = AsyncState::Completion(completion_from_expr_result(Err(err))?),
        },
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "if test frame received completion state",
          ))
        }
      },

      AsyncFrame::StmtList {
        stmts,
        next_index,
        last_value_root,
        mut last_value_is_set,
      } => match state {
        AsyncState::Completion(completion) => {
          let stmts = unsafe { &*stmts };
          let last_value = if last_value_is_set {
            Some(
              scope
                .heap()
                .get_root(last_value_root)
                .ok_or(VmError::InvariantViolation("missing stmt-list last value root"))?,
            )
          } else {
            None
          };

          let completion = completion.update_empty(last_value);
          match completion {
            Completion::Normal(v) => {
              if let Some(v) = v {
                last_value_is_set = true;
                scope.heap_mut().set_root(last_value_root, v);
              }
            }
            abrupt => {
              scope.heap_mut().remove_root(last_value_root);
              state = AsyncState::Completion(abrupt);
              continue;
            }
          }

          match async_eval_stmt_list_from(
            evaluator,
            scope,
            stmts,
            next_index,
            last_value_root,
            last_value_is_set,
          ) {
            Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err) => {
              // `async_eval_stmt_list_from` exits early on fatal errors without removing the
              // persistent `last_value_root`.
              scope.heap_mut().remove_root(last_value_root);
              return Err(err);
            }
          }
        }
        AsyncState::Expr(_) => {
          return Err(VmError::InvariantViolation(
            "stmt list frame received expression state",
          ))
        }
      },

      AsyncFrame::RestoreLexEnv { outer } => match state {
        AsyncState::Completion(c) => {
          evaluator.env.set_lexical_env(scope.heap_mut(), outer);
          state = AsyncState::Completion(c);
        }
        AsyncState::Expr(_) => {
          return Err(VmError::InvariantViolation(
            "restore lexical env frame received expression state",
          ))
        }
      },

      AsyncFrame::WhileAfterTest {
        stmt,
        label_set,
        v_root,
      } => match state {
        AsyncState::Expr(test_res) => match test_res {
          Ok(test_value) => {
            let stmt = unsafe { &*stmt };
            match async_while_after_test(evaluator, scope, stmt, label_set, v_root, test_value) {
              Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
              Ok(AsyncEval::Suspend(mut suspend)) => {
                suspend.frames.append(&mut frames);
                return Ok(AsyncBodyResult::Await {
                  await_value: suspend.await_value,
                  frames: suspend.frames,
                });
              }
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                if scope.heap().get_root(v_root).is_some() {
                  scope.heap_mut().remove_root(v_root);
                }
                state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
              }
              Err(err) => {
                if scope.heap().get_root(v_root).is_some() {
                  scope.heap_mut().remove_root(v_root);
                }
                return Err(err);
              }
            }
          }
          Err(err) => {
            if scope.heap().get_root(v_root).is_some() {
              scope.heap_mut().remove_root(v_root);
            }
            state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
          }
        },
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "while test frame received completion state",
          ))
        }
      },

      AsyncFrame::WhileAfterBody {
        stmt,
        label_set,
        v_root,
      } => match state {
        AsyncState::Completion(body_completion) => {
          let stmt = unsafe { &*stmt };
          match async_while_after_body(evaluator, scope, stmt, label_set, v_root, body_completion) {
            Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              if scope.heap().get_root(v_root).is_some() {
                scope.heap_mut().remove_root(v_root);
              }
              state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
            }
            Err(err) => {
              if scope.heap().get_root(v_root).is_some() {
                scope.heap_mut().remove_root(v_root);
              }
              return Err(err);
            }
          }
        }
        AsyncState::Expr(_) => {
          return Err(VmError::InvariantViolation(
            "while body frame received expression state",
          ))
        }
      },

      AsyncFrame::DoWhileAfterBody {
        stmt,
        label_set,
        v_root,
      } => match state {
        AsyncState::Completion(body_completion) => {
          let stmt = unsafe { &*stmt };
          match async_do_while_after_body(evaluator, scope, stmt, label_set, v_root, body_completion) {
            Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              if scope.heap().get_root(v_root).is_some() {
                scope.heap_mut().remove_root(v_root);
              }
              state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
            }
            Err(err) => {
              if scope.heap().get_root(v_root).is_some() {
                scope.heap_mut().remove_root(v_root);
              }
              return Err(err);
            }
          }
        }
        AsyncState::Expr(_) => {
          return Err(VmError::InvariantViolation(
            "do-while body frame received expression state",
          ))
        }
      },

      AsyncFrame::DoWhileAfterTest {
        stmt,
        label_set,
        v_root,
      } => match state {
        AsyncState::Expr(test_res) => match test_res {
          Ok(test_value) => {
            let stmt = unsafe { &*stmt };
            match async_do_while_after_test(evaluator, scope, stmt, label_set, v_root, test_value) {
              Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
              Ok(AsyncEval::Suspend(mut suspend)) => {
                suspend.frames.append(&mut frames);
                return Ok(AsyncBodyResult::Await {
                  await_value: suspend.await_value,
                  frames: suspend.frames,
                });
              }
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                if scope.heap().get_root(v_root).is_some() {
                  scope.heap_mut().remove_root(v_root);
                }
                state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
              }
              Err(err) => {
                if scope.heap().get_root(v_root).is_some() {
                  scope.heap_mut().remove_root(v_root);
                }
                return Err(err);
              }
            }
          }
          Err(err) => {
            if scope.heap().get_root(v_root).is_some() {
              scope.heap_mut().remove_root(v_root);
            }
            state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
          }
        },
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "do-while test frame received completion state",
          ))
        }
      },

      AsyncFrame::ForTripleAfterInit {
        stmt,
        label_set,
        v_root,
        needs_explicit_iter_tick,
      } => match state {
        AsyncState::Expr(init_res) => match init_res {
          Ok(_) => {
            let stmt = unsafe { &*stmt };
            match async_for_triple_begin_iteration(
              evaluator,
              scope,
              stmt,
              label_set,
              v_root,
              needs_explicit_iter_tick,
            ) {
              Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
              Ok(AsyncEval::Suspend(mut suspend)) => {
                suspend.frames.append(&mut frames);
                return Ok(AsyncBodyResult::Await {
                  await_value: suspend.await_value,
                  frames: suspend.frames,
                });
              }
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                if scope.heap().get_root(v_root).is_some() {
                  scope.heap_mut().remove_root(v_root);
                }
                state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
              }
              Err(err) => {
                if scope.heap().get_root(v_root).is_some() {
                  scope.heap_mut().remove_root(v_root);
                }
                return Err(err);
              }
            }
          }
          Err(err) => {
            if scope.heap().get_root(v_root).is_some() {
              scope.heap_mut().remove_root(v_root);
            }
            state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
          }
        },
        AsyncState::Completion(init_completion) => {
          if init_completion.is_abrupt() {
            if scope.heap().get_root(v_root).is_some() {
              scope.heap_mut().remove_root(v_root);
            }
            state = AsyncState::Completion(init_completion);
            continue;
          }

          let stmt = unsafe { &*stmt };
          match async_for_triple_begin_iteration(
            evaluator,
            scope,
            stmt,
            label_set,
            v_root,
            needs_explicit_iter_tick,
          ) {
            Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              if scope.heap().get_root(v_root).is_some() {
                scope.heap_mut().remove_root(v_root);
              }
              state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
            }
            Err(err) => {
              if scope.heap().get_root(v_root).is_some() {
                scope.heap_mut().remove_root(v_root);
              }
              return Err(err);
            }
          }
        }
      },

      AsyncFrame::ForTripleAfterTest {
        stmt,
        label_set,
        v_root,
        needs_explicit_iter_tick,
      } => match state {
        AsyncState::Expr(test_res) => match test_res {
          Ok(test_value) => {
            let stmt = unsafe { &*stmt };
            match async_for_triple_after_test(
              evaluator,
              scope,
              stmt,
              label_set,
              v_root,
              needs_explicit_iter_tick,
              test_value,
            ) {
              Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
              Ok(AsyncEval::Suspend(mut suspend)) => {
                suspend.frames.append(&mut frames);
                return Ok(AsyncBodyResult::Await {
                  await_value: suspend.await_value,
                  frames: suspend.frames,
                });
              }
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                if scope.heap().get_root(v_root).is_some() {
                  scope.heap_mut().remove_root(v_root);
                }
                state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
              }
              Err(err) => {
                if scope.heap().get_root(v_root).is_some() {
                  scope.heap_mut().remove_root(v_root);
                }
                return Err(err);
              }
            }
          }
          Err(err) => {
            if scope.heap().get_root(v_root).is_some() {
              scope.heap_mut().remove_root(v_root);
            }
            state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
          }
        },
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "for test frame received completion state",
          ))
        }
      },

      AsyncFrame::ForTripleAfterBody {
        stmt,
        label_set,
        v_root,
        needs_explicit_iter_tick,
      } => match state {
        AsyncState::Completion(body_completion) => {
          let stmt = unsafe { &*stmt };
          match async_for_triple_after_body(
            evaluator,
            scope,
            stmt,
            label_set,
            v_root,
            needs_explicit_iter_tick,
            body_completion,
          ) {
            Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              if scope.heap().get_root(v_root).is_some() {
                scope.heap_mut().remove_root(v_root);
              }
              state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
            }
            Err(err) => {
              if scope.heap().get_root(v_root).is_some() {
                scope.heap_mut().remove_root(v_root);
              }
              return Err(err);
            }
          }
        }
        AsyncState::Expr(_) => {
          return Err(VmError::InvariantViolation(
            "for body frame received expression state",
          ))
        }
      },

      AsyncFrame::ForTripleAfterPost {
        stmt,
        label_set,
        v_root,
        needs_explicit_iter_tick,
      } => match state {
        AsyncState::Expr(post_res) => match post_res {
          Ok(_) => {
            let stmt = unsafe { &*stmt };
            match async_for_triple_begin_iteration(
              evaluator,
              scope,
              stmt,
              label_set,
              v_root,
              needs_explicit_iter_tick,
            ) {
              Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
              Ok(AsyncEval::Suspend(mut suspend)) => {
                suspend.frames.append(&mut frames);
                return Ok(AsyncBodyResult::Await {
                  await_value: suspend.await_value,
                  frames: suspend.frames,
                });
              }
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                if scope.heap().get_root(v_root).is_some() {
                  scope.heap_mut().remove_root(v_root);
                }
                state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
              }
              Err(err) => {
                if scope.heap().get_root(v_root).is_some() {
                  scope.heap_mut().remove_root(v_root);
                }
                return Err(err);
              }
            }
          }
          Err(err) => {
            if scope.heap().get_root(v_root).is_some() {
              scope.heap_mut().remove_root(v_root);
            }
            state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
          }
        },
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "for post frame received completion state",
          ))
        }
      },

      AsyncFrame::ForInAfterRhs { stmt, label_set } => match state {
        AsyncState::Expr(rhs_res) => match rhs_res {
          Ok(rhs_value) => {
            let stmt = unsafe { &*stmt };
            match async_for_in_after_rhs(evaluator, scope, stmt, label_set, rhs_value) {
              Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
              Ok(AsyncEval::Suspend(mut suspend)) => {
                suspend.frames.append(&mut frames);
                return Ok(AsyncBodyResult::Await {
                  await_value: suspend.await_value,
                  frames: suspend.frames,
                });
              }
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
              }
              Err(err) => return Err(err),
            }
          }
          Err(err) => state = AsyncState::Completion(completion_from_expr_result(Err(err))?),
        },
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "for-in rhs frame received completion state",
          ))
        }
      },

      AsyncFrame::ForInAfterBody {
        stmt,
        label_set,
        object_root,
        mut key_roots,
        next_key_index,
        v_root,
        outer_lex,
      } => match state {
        AsyncState::Completion(body_completion) => {
          let mut v = match scope.heap().get_root(v_root) {
            Some(v) => v,
            None => {
              async_for_in_cleanup(scope, object_root, &mut key_roots, v_root);
              return Err(VmError::InvariantViolation("missing for-in loop value root"));
            }
          };

          if !Evaluator::loop_continues(&body_completion, &label_set) {
            let result = Evaluator::normalise_iteration_break(body_completion.update_empty(Some(v)));
            async_for_in_cleanup(scope, object_root, &mut key_roots, v_root);
            state = AsyncState::Completion(result);
            continue;
          }

          if let Some(value) = body_completion.value() {
            v = value;
            scope.heap_mut().set_root(v_root, v);
          }

          let stmt = unsafe { &*stmt };
          match async_for_in_loop_from(
            evaluator,
            scope,
            stmt,
            label_set,
            object_root,
            key_roots,
            next_key_index,
            v_root,
            outer_lex,
          ) {
            Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
            }
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(_) => {
          return Err(VmError::InvariantViolation(
            "for-in body frame received expression state",
          ))
        }
      },

      AsyncFrame::ForOfAfterRhs { stmt, label_set } => match state {
        AsyncState::Expr(rhs_res) => match rhs_res {
          Ok(iterable) => {
            let stmt = unsafe { &*stmt };
            match async_for_of_after_rhs(evaluator, scope, stmt, label_set, iterable) {
              Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
              Ok(AsyncEval::Suspend(mut suspend)) => {
                suspend.frames.append(&mut frames);
                return Ok(AsyncBodyResult::Await {
                  await_value: suspend.await_value,
                  frames: suspend.frames,
                });
              }
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
              }
              Err(err) => return Err(err),
            }
          }
          Err(err) => state = AsyncState::Completion(completion_from_expr_result(Err(err))?),
        },
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "for-of rhs frame received completion state",
          ))
        }
      },

      AsyncFrame::ForOfAfterBody {
        stmt,
        label_set,
        iterator_record,
        iterator_root,
        next_method_root,
        v_root,
        outer_lex,
      } => match state {
        AsyncState::Completion(body_completion) => {
          let mut v = match scope.heap().get_root(v_root) {
            Some(v) => v,
            None => {
              let _ = iterator::iterator_close(
                evaluator.vm,
                &mut *evaluator.host,
                &mut *evaluator.hooks,
                scope,
                &iterator_record,
              );
              async_for_of_cleanup(scope, iterator_root, next_method_root, v_root);
              return Err(VmError::InvariantViolation("missing for-of loop value root"));
            }
          };

          if !Evaluator::loop_continues(&body_completion, &label_set) {
            let _ = iterator::iterator_close(
              evaluator.vm,
              &mut *evaluator.host,
              &mut *evaluator.hooks,
              scope,
              &iterator_record,
            );
            let result = Evaluator::normalise_iteration_break(body_completion.update_empty(Some(v)));
            async_for_of_cleanup(scope, iterator_root, next_method_root, v_root);
            state = AsyncState::Completion(result);
            continue;
          }

          if let Some(value) = body_completion.value() {
            v = value;
            scope.heap_mut().set_root(v_root, v);
          }

          let stmt = unsafe { &*stmt };
          match async_for_of_loop(
            evaluator,
            scope,
            stmt,
            label_set,
            iterator_record,
            iterator_root,
            next_method_root,
            v_root,
            outer_lex,
          ) {
            Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
            }
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(_) => {
          return Err(VmError::InvariantViolation(
            "for-of body frame received expression state",
          ))
        }
      },

      AsyncFrame::SwitchAfterDiscriminant { stmt } => match state {
        AsyncState::Expr(discriminant_res) => match discriminant_res {
          Ok(discriminant) => {
            let stmt = unsafe { &*stmt };
            match async_switch_after_discriminant(evaluator, scope, stmt, discriminant) {
              Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
              Ok(AsyncEval::Suspend(mut suspend)) => {
                suspend.frames.append(&mut frames);
                return Ok(AsyncBodyResult::Await {
                  await_value: suspend.await_value,
                  frames: suspend.frames,
                });
              }
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
              }
              Err(err) => return Err(err),
            }
          }
          Err(err) => state = AsyncState::Completion(completion_from_expr_result(Err(err))?),
        },
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "switch discriminant frame received completion state",
          ))
        }
      },

      AsyncFrame::SwitchAfterCaseExpr {
        stmt,
        discriminant_root,
        v_root,
        default_index,
        branch_index,
      } => match state {
        AsyncState::Expr(case_res) => match case_res {
          Ok(case_value) => {
            let discriminant = match scope.heap().get_root(discriminant_root) {
              Some(v) => v,
              None => {
                async_switch_cleanup(scope, discriminant_root, v_root);
                return Err(VmError::InvariantViolation("missing switch discriminant root"));
              }
            };

            let matches = match strict_equal(scope.heap(), discriminant, case_value) {
              Ok(m) => m,
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                let completion = completion_from_expr_result(Err(err))?;
                let completion = async_switch_finish(scope, discriminant_root, v_root, completion)?;
                state = AsyncState::Completion(completion);
                continue;
              }
              Err(err) => {
                async_switch_cleanup(scope, discriminant_root, v_root);
                return Err(err);
              }
            };

            let stmt = unsafe { &*stmt };
            let res = if matches {
              async_switch_exec_from(evaluator, scope, stmt, discriminant_root, v_root, branch_index)
            } else {
              async_switch_scan_and_exec_from(
                evaluator,
                scope,
                stmt,
                discriminant_root,
                v_root,
                branch_index.saturating_add(1),
                default_index,
              )
            };
            match res {
              Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
              Ok(AsyncEval::Suspend(mut suspend)) => {
                suspend.frames.append(&mut frames);
                return Ok(AsyncBodyResult::Await {
                  await_value: suspend.await_value,
                  frames: suspend.frames,
                });
              }
              Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
                let completion = completion_from_expr_result(Err(err))?;
                let completion = async_switch_finish(scope, discriminant_root, v_root, completion)?;
                state = AsyncState::Completion(completion);
              }
              Err(err) => {
                async_switch_cleanup(scope, discriminant_root, v_root);
                return Err(err);
              }
            }
          }
          Err(err) => {
            let completion = completion_from_expr_result(Err(err))?;
            let completion = async_switch_finish(scope, discriminant_root, v_root, completion)?;
            state = AsyncState::Completion(completion);
          }
        },
        AsyncState::Completion(_) => {
          return Err(VmError::InvariantViolation(
            "switch case frame received completion state",
          ))
        }
      },

      AsyncFrame::SwitchAfterBody {
        stmt,
        discriminant_root,
        v_root,
        next_branch_index,
      } => match state {
        AsyncState::Completion(body_completion) => {
          let stmt = unsafe { &*stmt };
          match async_switch_after_body_completion(
            evaluator,
            scope,
            stmt,
            discriminant_root,
            v_root,
            next_branch_index,
            body_completion,
          ) {
            Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              let completion = completion_from_expr_result(Err(err))?;
              let completion = async_switch_finish(scope, discriminant_root, v_root, completion)?;
              state = AsyncState::Completion(completion);
            }
            Err(err) => {
              async_switch_cleanup(scope, discriminant_root, v_root);
              return Err(err);
            }
          }
        }
        AsyncState::Expr(_) => {
          return Err(VmError::InvariantViolation(
            "switch body frame received expression state",
          ))
        }
      },

      AsyncFrame::TryAfterWrapped { stmt } => match state {
        AsyncState::Completion(wrapped_completion) => {
          let stmt = unsafe { &*stmt };
          match async_try_after_wrapped(evaluator, scope, stmt, wrapped_completion) {
            Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
            }
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(_) => {
          return Err(VmError::InvariantViolation(
            "try frame received expression state",
          ))
        }
      },

      AsyncFrame::TryAfterCatch { stmt } => match state {
        AsyncState::Completion(catch_completion) => {
          let stmt = unsafe { &*stmt };
          match async_try_after_catch(evaluator, scope, stmt, catch_completion) {
            Ok(AsyncEval::Complete(c)) => state = AsyncState::Completion(c),
            Ok(AsyncEval::Suspend(mut suspend)) => {
              suspend.frames.append(&mut frames);
              return Ok(AsyncBodyResult::Await {
                await_value: suspend.await_value,
                frames: suspend.frames,
              });
            }
            Err(err @ (VmError::Throw(_) | VmError::ThrowWithStack { .. })) => {
              state = AsyncState::Completion(completion_from_expr_result(Err(err))?)
            }
            Err(err) => return Err(err),
          }
        }
        AsyncState::Expr(_) => {
          return Err(VmError::InvariantViolation(
            "try catch frame received expression state",
          ))
        }
      },

      AsyncFrame::TryAfterFinally { mut pending } => match state {
        AsyncState::Completion(finally_completion) => {
          let pending_completion = pending.to_completion(scope.heap());
          pending.teardown(scope.heap_mut());
          let pending_completion = pending_completion?;

          let result = if finally_completion.is_abrupt() {
            finally_completion
          } else {
            pending_completion
          };

          state = AsyncState::Completion(result.update_empty(Some(Value::Undefined)));
        }
        AsyncState::Expr(_) => {
          return Err(VmError::InvariantViolation(
            "try finally frame received expression state",
          ))
        }
      },
    }
  }
}

pub(crate) fn run_ecma_function(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  env: &mut RuntimeEnv,
  source: Arc<SourceText>,
  base_offset: u32,
  prefix_len: u32,
  strict: bool,
  this: Value,
  new_target: Value,
  func: Arc<Node<Func>>,
  args: &[Value],
) -> Result<Value, VmError> {
  if func.stx.generator {
    return Err(VmError::Unimplemented(if func.stx.async_ {
      "async generator functions"
    } else {
      "generator functions"
    }));
  }
  env.set_source_info(source, base_offset, prefix_len);

  let Some(body) = &func.stx.body else {
    return Err(VmError::Unimplemented("function without body"));
  };

  let mut evaluator = Evaluator {
    vm,
    host,
    hooks,
    env,
    strict,
    this,
    new_target,
  };
  evaluator.instantiate_function(scope, func.as_ref(), args)?;

  if func.stx.async_ {
    // Async function invocation returns a Promise and executes the body until the first `await`.
    let cap = crate::promise_ops::new_promise_capability_with_host_and_hooks(
      evaluator.vm,
      scope,
      &mut *evaluator.host,
      &mut *evaluator.hooks,
    )?;
    let promise = cap.promise;

    let body_result = async_start_body(&mut evaluator, scope, &func);

    return match body_result {
      Ok(AsyncBodyResult::CompleteOk(v)) => {
        let mut call_scope = scope.reborrow();
        if let Err(err) = call_scope.push_roots(&[cap.resolve, v]) {
          evaluator.env.teardown(call_scope.heap_mut());
          return Err(err);
        }
        let res = evaluator.vm.call_with_host_and_hooks(
          &mut *evaluator.host,
          &mut call_scope,
          &mut *evaluator.hooks,
          cap.resolve,
          Value::Undefined,
          &[v],
        );
        evaluator.env.teardown(call_scope.heap_mut());
        res.map(|_| promise)
      }
      Ok(AsyncBodyResult::CompleteThrow(reason)) => {
        let mut call_scope = scope.reborrow();
        if let Err(err) = call_scope.push_roots(&[cap.reject, reason]) {
          evaluator.env.teardown(call_scope.heap_mut());
          return Err(err);
        }
        let res = evaluator.vm.call_with_host_and_hooks(
          &mut *evaluator.host,
          &mut call_scope,
          &mut *evaluator.hooks,
          cap.reject,
          Value::Undefined,
          &[reason],
        );
        evaluator.env.teardown(call_scope.heap_mut());
        res.map(|_| promise)
      }
      Ok(AsyncBodyResult::Await { await_value, frames }) => {
        // Root all GC-managed values while we create persistent roots and schedule the resumption.
        let mut root_scope = scope.reborrow();
        if let Err(err) = root_scope.push_roots(&[
          promise,
          cap.resolve,
          cap.reject,
          this,
          new_target,
          await_value,
        ]) {
          evaluator.env.teardown(root_scope.heap_mut());
          return Err(err);
        }

        let awaited_promise = match crate::promise_ops::promise_resolve_with_host_and_hooks(
          evaluator.vm,
          &mut root_scope,
          &mut *evaluator.host,
          &mut *evaluator.hooks,
          await_value,
        ) {
          Ok(p) => p,
          Err(e) => {
            evaluator.env.teardown(root_scope.heap_mut());
            return Err(e);
          }
        };

        if let Err(err) = root_scope.push_root(awaited_promise) {
          evaluator.env.teardown(root_scope.heap_mut());
          return Err(err);
        }

        // Create persistent roots for the async continuation.
        let values = [
          this,
          new_target,
          promise,
          cap.resolve,
          cap.reject,
          awaited_promise,
        ];
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
              evaluator.env.teardown(root_scope.heap_mut());
              return Err(e);
            }
          }
        }

        let this_root = roots[0];
        let new_target_root = roots[1];
        let promise_root = roots[2];
        let resolve_root = roots[3];
        let reject_root = roots[4];
        let awaited_root = roots[5];

        let cont = AsyncContinuation {
          env: evaluator.env.clone(),
          strict,
          this_root,
          new_target_root,
          promise_root,
          resolve_root,
          reject_root,
          awaited_promise_root: Some(awaited_root),
          frames,
        };

        let id = match evaluator.vm.insert_async_continuation(cont) {
          Ok(id) => id,
          Err(e) => {
            for id in roots.drain(..) {
              root_scope.heap_mut().remove_root(id);
            }
            evaluator.env.teardown(root_scope.heap_mut());
            return Err(e);
          }
        };

        let schedule_res = (|| -> Result<(), VmError> {
          let call_id = evaluator.vm.async_resume_call_id()?;
          let intr = evaluator
            .vm
            .intrinsics()
            .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
          let global_object = evaluator.env.global_object();
          let job_realm = evaluator.vm.current_realm();

          let name = root_scope.alloc_string("")?;
          let slots_fulfill = [Value::Number(id as f64), Value::Bool(false)];
          let on_fulfilled =
            root_scope.alloc_native_function_with_slots(call_id, None, name, 1, &slots_fulfill)?;
          root_scope.push_root(Value::Object(on_fulfilled))?;

          let name = root_scope.alloc_string("")?;
          let slots_reject = [Value::Number(id as f64), Value::Bool(true)];
          let on_rejected =
            root_scope.alloc_native_function_with_slots(call_id, None, name, 1, &slots_reject)?;
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
          }

          let _ = crate::promise_ops::perform_promise_then_with_host_and_hooks(
            evaluator.vm,
            &mut root_scope,
            &mut *evaluator.host,
            &mut *evaluator.hooks,
            awaited_promise,
            Some(Value::Object(on_fulfilled)),
            Some(Value::Object(on_rejected)),
          )?;

          Ok(())
        })();

        if let Err(err) = schedule_res {
          let _ = evaluator.vm.take_async_continuation(id);
          for id in roots.drain(..) {
            root_scope.heap_mut().remove_root(id);
          }
          evaluator.env.teardown(root_scope.heap_mut());
          return Err(err);
        }

        Ok(promise)
      }
      Err(err) => {
        evaluator.env.teardown(scope.heap_mut());
        Err(err)
      }
    };
  }

  match body {
    FuncBody::Expression(expr) => match evaluator.eval_expr(scope, expr) {
      Ok(v) => Ok(v),
      Err(VmError::Throw(value)) => {
        // Capture stack + annotate the top frame with the expression start location. Expression-body
        // arrow functions do not go through `eval_stmt`, so this is the best central capture point.
        let source = evaluator.env.source();
        let rel_start = expr
          .loc
          .start_u32()
          .saturating_sub(evaluator.env.prefix_len());
        let abs_offset = evaluator.env.base_offset().saturating_add(rel_start);
        let (line, col) = source.line_col(abs_offset);

        let mut stack = evaluator.vm.capture_stack();
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

        Err(VmError::ThrowWithStack { value, stack })
      }
      other => other,
    },
    FuncBody::Block(stmts) => {
      let completion = evaluator.eval_stmt_list(scope, stmts)?;
      match completion {
        Completion::Normal(_) => Ok(Value::Undefined),
        Completion::Return(v) => Ok(v),
        Completion::Throw(thrown) => Err(VmError::ThrowWithStack {
          value: thrown.value,
          stack: thrown.stack,
        }),
        Completion::Break(..) => Err(VmError::Unimplemented("break outside of loop")),
        Completion::Continue(..) => Err(VmError::Unimplemented("continue outside of loop")),
      }
    }
  }
}

pub(crate) fn instantiate_module_decls(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  global_object: GcObject,
  module_env: GcEnv,
  source: Arc<SourceText>,
  stmts: &[Node<Stmt>],
) -> Result<(), VmError> {
  let mut env =
    RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, module_env, module_env)?;
  env.set_source_info(source, 0, 0);

  // Module instantiation does not execute code, but reuses the evaluator's hoisting/instantiation
  // logic to create bindings and pre-create function objects.
  let mut dummy_host = ();
  let mut dummy_hooks = crate::MicrotaskQueue::new();
  let mut evaluator = Evaluator {
    vm,
    host: &mut dummy_host,
    hooks: &mut dummy_hooks,
    env: &mut env,
    // Modules are always strict mode.
    strict: true,
    this: Value::Undefined,
    new_target: Value::Undefined,
  };

  evaluator.instantiate_stmt_list(scope, stmts)?;
  env.teardown(scope.heap_mut());
  Ok(())
}

pub(crate) fn run_module(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  global_object: GcObject,
  realm_id: RealmId,
  module_id: ModuleId,
  module_env: GcEnv,
  source: Arc<SourceText>,
  stmts: &[Node<Stmt>],
) -> Result<(), VmError> {
  // Ensure module execution reports an active ScriptOrModule so `import.meta` can consult it.
  let exec_ctx = ExecutionContext {
    realm: realm_id,
    script_or_module: Some(ScriptOrModule::Module(module_id)),
  };

  vm.push_execution_context(exec_ctx);

  let result = (|| -> Result<(), VmError> {
    let mut env =
      RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, module_env, module_env)?;
    env.set_source_info(source.clone(), 0, 0);

    let result = (|| -> Result<(), VmError> {
      let (line, col) = source.line_col(0);
      let frame = StackFrame {
        function: None,
        source: source.name.clone(),
        line,
        col,
      };
      let mut vm_frame = vm.enter_frame(frame)?;

      let mut evaluator = Evaluator {
        vm: &mut *vm_frame,
        host,
        hooks,
        env: &mut env,
        strict: true,
        // Per ECMA-262, module top-level `this` is `undefined`.
        this: Value::Undefined,
        new_target: Value::Undefined,
      };

      let completion = evaluator.eval_stmt_list(scope, stmts)?;

      match completion {
        Completion::Normal(_) => Ok(()),
        Completion::Throw(thrown) => Err(VmError::ThrowWithStack {
          value: thrown.value,
          stack: thrown.stack,
        }),
        Completion::Return(_) => Err(VmError::Unimplemented("return from module")),
        Completion::Break(..) => Err(VmError::Unimplemented("break outside of loop")),
        Completion::Continue(..) => Err(VmError::Unimplemented("continue outside of loop")),
      }
    })();

    env.teardown(scope.heap_mut());
    result
  })();

  let popped = vm.pop_execution_context();
  debug_assert_eq!(popped, Some(exec_ctx));
  debug_assert!(
    popped.is_some(),
    "module execution popped no execution context"
  );
  result
}

/// Result of executing a module statement list until a supported top-level `await` boundary.
///
/// This is used to model a minimal subset of top-level await by executing a module in "chunks"
/// separated by `await <expr>;` expression statements (where the awaited value is resolved via
/// Promise jobs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ModuleTlaStepResult {
  /// The module body ran to completion (no further supported `await` statements).
  Completed,
  /// Execution suspended at `await <expr>;`, returning the awaited Promise and the statement index
  /// at which execution should resume.
  Await { promise: Value, resume_index: usize },
}

/// Executes a module's statement list starting at `start_index`, stopping when it encounters a
/// supported top-level `await <expr>;` expression statement.
///
/// Notes / limitations:
/// - This is **not** a full implementation of `await` as an expression.
/// - Only `await` used as an expression statement is treated as a suspension point.
/// - Other uses of `await` (e.g. in variable initializers) remain unimplemented.
pub(crate) fn run_module_until_await(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  global_object: GcObject,
  realm_id: RealmId,
  module_id: ModuleId,
  module_env: GcEnv,
  source: Arc<SourceText>,
  stmts: &[Node<Stmt>],
  start_index: usize,
) -> Result<ModuleTlaStepResult, VmError> {
  if start_index >= stmts.len() {
    return Ok(ModuleTlaStepResult::Completed);
  }

  // Ensure module execution reports an active ScriptOrModule so `import.meta` can consult it.
  let exec_ctx = ExecutionContext {
    realm: realm_id,
    script_or_module: Some(ScriptOrModule::Module(module_id)),
  };
  vm.push_execution_context(exec_ctx);

  let result = (|| -> Result<ModuleTlaStepResult, VmError> {
    let mut env = RuntimeEnv::new_with_var_env(scope.heap_mut(), global_object, module_env, module_env)?;
    env.set_source_info(source.clone(), 0, 0);

    let result = (|| -> Result<ModuleTlaStepResult, VmError> {
      let (line, col) = source.line_col(0);
      let frame = StackFrame {
        function: None,
        source: source.name.clone(),
        line,
        col,
      };
      let mut vm_frame = vm.enter_frame(frame)?;

      let mut evaluator = Evaluator {
        vm: &mut *vm_frame,
        host,
        hooks,
        env: &mut env,
        strict: true,
        // Per ECMA-262, module top-level `this` is `undefined`.
        this: Value::Undefined,
        new_target: Value::Undefined,
      };

      // Root the running completion value so it cannot be collected while evaluating subsequent
      // statements (which may allocate and trigger GC).
      let last_root = scope.heap_mut().add_root(Value::Undefined)?;
      let mut last_value: Option<Value> = None;

      for (idx, stmt) in stmts.iter().enumerate().skip(start_index) {
        // Minimal top-level await support: suspend on `await <expr>;` expression statements.
        if let Stmt::Expr(expr_stmt) = &*stmt.stx {
          if let Expr::Unary(unary) = &*expr_stmt.stx.expr.stx {
            if unary.stx.operator == OperatorName::Await {
              // Clean up the per-step completion root before suspending.
              scope.heap_mut().remove_root(last_root);

              // Evaluate the awaited expression (argument) and coerce it with `PromiseResolve`.
              let awaited_value = evaluator.eval_expr(scope, &unary.stx.argument)?;
              let mut promise_scope = scope.reborrow();
              promise_scope.push_root(awaited_value)?;
              let promise = crate::promise_ops::promise_resolve_with_host_and_hooks(
                &mut *vm_frame,
                &mut promise_scope,
                host,
                hooks,
                awaited_value,
              )?;
              return Ok(ModuleTlaStepResult::Await {
                promise,
                resume_index: idx.saturating_add(1),
              });
            }
          }
        }

        let completion = evaluator.eval_stmt(scope, stmt)?;
        let completion = completion.update_empty(last_value);
        match completion {
          Completion::Normal(v) => {
            if let Some(v) = v {
              last_value = Some(v);
              scope.heap_mut().set_root(last_root, v);
            }
          }
          abrupt => {
            scope.heap_mut().remove_root(last_root);
            match abrupt {
              Completion::Normal(_) => unreachable!("covered above"),
              Completion::Throw(thrown) => {
                return Err(VmError::ThrowWithStack {
                  value: thrown.value,
                  stack: thrown.stack,
                });
              }
              Completion::Return(_) => return Err(VmError::Unimplemented("return from module")),
              Completion::Break(..) => return Err(VmError::Unimplemented("break outside of loop")),
              Completion::Continue(..) => {
                return Err(VmError::Unimplemented("continue outside of loop"))
              }
            }
          }
        }
      }

      scope.heap_mut().remove_root(last_root);
      Ok(ModuleTlaStepResult::Completed)
    })();

    env.teardown(scope.heap_mut());
    result
  })();

  let popped = vm.pop_execution_context();
  debug_assert_eq!(popped, Some(exec_ctx));
  debug_assert!(popped.is_some(), "module execution popped no execution context");
  result
}

pub(crate) fn eval_expr(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  env: &mut RuntimeEnv,
  strict: bool,
  this: Value,
  scope: &mut Scope<'_>,
  expr: &Node<Expr>,
) -> Result<Value, VmError> {
  let mut evaluator = Evaluator {
    vm,
    host,
    hooks,
    env,
    strict,
    this,
    new_target: Value::Undefined,
  };
  evaluator.eval_expr(scope, expr)
}

fn is_nullish(value: Value) -> bool {
  matches!(value, Value::Undefined | Value::Null)
}

fn to_boolean(heap: &Heap, value: Value) -> Result<bool, VmError> {
  Ok(match value {
    Value::Undefined | Value::Null => false,
    Value::Bool(b) => b,
    Value::Number(n) => n != 0.0 && !n.is_nan(),
    Value::BigInt(n) => !n.is_zero(),
    Value::String(s) => !heap.get_string(s)?.as_code_units().is_empty(),
    Value::Symbol(_) | Value::Object(_) => true,
  })
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

fn typeof_name(heap: &Heap, value: Value) -> Result<&'static str, VmError> {
  Ok(match value {
    Value::Undefined => "undefined",
    Value::Null => "object",
    Value::Bool(_) => "boolean",
    Value::Number(_) => "number",
    Value::BigInt(_) => "bigint",
    Value::String(_) => "string",
    Value::Symbol(_) => "symbol",
    Value::Object(obj) => match heap.get_function_call_handler(obj) {
      Ok(_) => "function",
      Err(VmError::NotCallable) => "object",
      Err(err) => return Err(err),
    },
  })
}

fn strict_equal(heap: &Heap, a: Value, b: Value) -> Result<bool, VmError> {
  Ok(match (a, b) {
    (Value::Undefined, Value::Undefined) => true,
    (Value::Null, Value::Null) => true,
    (Value::Bool(x), Value::Bool(y)) => x == y,
    (Value::Number(x), Value::Number(y)) => x == y,
    (Value::BigInt(x), Value::BigInt(y)) => x == y,
    (Value::String(x), Value::String(y)) => heap.get_string(x)? == heap.get_string(y)?,
    (Value::Symbol(x), Value::Symbol(y)) => x == y,
    (Value::Object(x), Value::Object(y)) => x == y,
    _ => false,
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{HeapLimits, VmOptions};

  #[test]
  fn prototype_cycle_throw_captures_statement_location() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut rt = JsRuntime::new(vm, heap)?;

    let err = rt
      // Ensure the statement isn't on line 1 so the test verifies location capture.
      .exec_script("let a = {};\n\nObject.setPrototypeOf(a, a);")
      .unwrap_err();
    match err {
      VmError::ThrowWithStack { stack, .. } => {
        assert!(
          !stack.is_empty(),
          "ThrowWithStack must include at least one frame"
        );
        assert_eq!(stack[0].line, 3);
        assert_eq!(stack[0].col, 1);
      }
      other => panic!("expected ThrowWithStack, got {other:?}"),
    }

    Ok(())
  }
}
