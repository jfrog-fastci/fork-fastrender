use crate::exec::{eval_expr, eval_expr_named, ResolvedBinding, RuntimeEnv};
use crate::function::CallHandler;
use crate::property::{PropertyKey, PropertyKind};
use crate::{GcObject, Scope, Value, Vm, VmError, VmHost, VmHostHooks};
use parse_js::ast::class_or_object::ClassOrObjKey;
use parse_js::ast::expr::pat::{ArrPat, ObjPat, Pat};
use parse_js::ast::expr::{ComputedMemberExpr, Expr, MemberExpr};
use parse_js::ast::node::{literal_string_code_units, Node};
use parse_js::token::TT;

fn throw_type_error(vm: &Vm, scope: &mut Scope<'_>, message: &str) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let value = crate::error_object::new_error(
    scope,
    intr.type_error_prototype(),
    "TypeError",
    message,
  )?;
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

fn get_super_receiver(
  vm: &Vm,
  scope: &mut Scope<'_>,
  this: Value,
  derived_constructor: bool,
  this_initialized: bool,
) -> Result<Value, VmError> {
  // Derived class constructors have an uninitialized `this` binding until `super()` returns.
  //
  // `vm-js` represents that state either:
  // - via `derived_constructor && !this_initialized` for ordinary evaluation contexts, or
  // - via a heap-owned `DerivedConstructorState` cell captured by arrow functions and direct eval.
  //
  // This helper mirrors `Evaluator::get_this_binding` so `super` property assignment targets in
  // destructuring (`[super.x] = ...`, `{ x: super[y] } = ...`) observe the same initialization
  // ordering as ordinary `super` references.
  if let Value::Object(obj) = this {
    if scope.heap().is_derived_constructor_state(obj) {
      let state = scope.heap().get_derived_constructor_state(obj)?;
      if let Some(this_obj) = state.this_value {
        return Ok(Value::Object(this_obj));
      }
      return Err(throw_reference_error(
        vm,
        scope,
        "Must call super constructor in derived class before accessing 'this'",
      )?);
    }
  }

  if derived_constructor && !this_initialized {
    return Err(throw_reference_error(
      vm,
      scope,
      "Must call super constructor in derived class before accessing 'this'",
    )?);
  }

  Ok(this)
}

fn iterator_close_on_err(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  iterator_record: &crate::iterator::IteratorRecord,
  err: VmError,
) -> Result<(), VmError> {
  if iterator_record.done {
    return Err(err);
  }

  // `IteratorClose` precedence rules (ECMA-262):
  // - Errors produced while getting/calling `iterator.return` override the incoming completion
  //   (even when the incoming completion is a throw completion).
  // - Only the return-value-is-not-object TypeError check is skipped for throw completions.
  // - Never allow close-time JS throw completions to replace fatal VM failures (termination, OOM,
  //   etc).
  let original_is_throw = err.is_throw_completion();

  // Root the pending thrown value across `IteratorClose`, which can allocate and trigger GC.
  if original_is_throw {
    if let Some(thrown) = err.thrown_value() {
      scope.push_root(thrown)?;
    }
  }

  match crate::iterator::iterator_close(
    vm,
    host,
    hooks,
    scope,
    iterator_record,
    crate::iterator::CloseCompletionKind::Throw,
  ) {
    Ok(()) => Err(err),
    Err(close_err) => Err(if original_is_throw { close_err } else { err }),
  }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum BindingKind {
  Var,
  /// Function parameter binding (formal parameters).
  ///
  /// This is like a mutable lexical binding, but it must tolerate duplicate parameter names in
  /// sloppy-mode functions with a simple parameter list (e.g. `function f(a, a) {}`), where later
  /// parameters update the same binding.
  Param,
  Let,
  Const,
  Assignment,
}

pub(crate) fn bind_pattern(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  env: &mut RuntimeEnv,
  pat: &Pat,
  value: Value,
  kind: BindingKind,
  strict: bool,
  this: Value,
  home_object: Option<GcObject>,
  derived_constructor: bool,
  this_initialized: bool,
) -> Result<(), VmError> {
  // Keep temporary roots local to this binding operation.
  let mut scope = scope.reborrow();
  // Root the input value so destructuring can allocate without the RHS being collected.
  let value = scope.push_root(value)?;

  match pat {
    Pat::Id(id) => bind_identifier(
      vm,
      host,
      hooks,
      env,
      &mut scope,
      &id.stx.name,
      value,
      kind,
      strict,
    ),
    Pat::Obj(obj) => bind_object_pattern(
      vm,
      host,
      hooks,
      &mut scope,
      env,
      &obj.stx,
      value,
      kind,
      strict,
      this,
      home_object,
      derived_constructor,
      this_initialized,
    ),
    Pat::Arr(arr) => bind_array_pattern(
      vm,
      host,
      hooks,
      &mut scope,
      env,
      &arr.stx,
      value,
      kind,
      strict,
      this,
      home_object,
      derived_constructor,
      this_initialized,
    ),
    Pat::AssignTarget(expr) => {
      if !matches!(kind, BindingKind::Assignment) {
        return Err(VmError::Unimplemented(
          "assignment target pattern in binding context",
        ));
      }
      bind_assignment_target(
        vm,
        host,
        hooks,
        &mut scope,
        env,
        expr,
        value,
        strict,
        this,
        home_object,
        derived_constructor,
        this_initialized,
      )
    }
  }
}

pub(crate) fn bind_assignment_target(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  env: &mut RuntimeEnv,
  target: &Node<Expr>,
  value: Value,
  strict: bool,
  this: Value,
  home_object: Option<GcObject>,
  derived_constructor: bool,
  this_initialized: bool,
) -> Result<(), VmError> {
  // Keep temporary roots local to this binding operation.
  let mut scope = scope.reborrow();
  let value = scope.push_root(value)?;

  match &*target.stx {
    Expr::Id(id) => bind_identifier(
      vm,
      host,
      hooks,
      env,
      &mut scope,
      &id.stx.name,
      value,
      BindingKind::Assignment,
      strict,
    ),
    Expr::IdPat(id) => bind_identifier(
      vm,
      host,
      hooks,
      env,
      &mut scope,
      &id.stx.name,
      value,
      BindingKind::Assignment,
      strict,
    ),
    Expr::ObjPat(obj) => bind_object_pattern(
      vm,
      host,
      hooks,
      &mut scope,
      env,
      &obj.stx,
      value,
      BindingKind::Assignment,
      strict,
      this,
      home_object,
      derived_constructor,
      this_initialized,
    ),
    Expr::ArrPat(arr) => bind_array_pattern(
      vm,
      host,
      hooks,
      &mut scope,
      env,
      &arr.stx,
      value,
      BindingKind::Assignment,
      strict,
      this,
      home_object,
      derived_constructor,
      this_initialized,
    ),
    Expr::Member(member) => assign_to_member(
      vm,
      host,
      hooks,
      &mut scope,
      env,
      &member.stx,
      value,
      strict,
      this,
      home_object,
      derived_constructor,
      this_initialized,
    ),
    Expr::ComputedMember(member) => assign_to_computed_member(
      vm,
      host,
      hooks,
      &mut scope,
      env,
      &member.stx,
      value,
      strict,
      this,
      home_object,
      derived_constructor,
      this_initialized,
    ),
    _ => Err(VmError::Unimplemented("assignment target")),
  }
}

fn bind_identifier(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  env: &mut RuntimeEnv,
  scope: &mut Scope<'_>,
  name: &str,
  value: Value,
  kind: BindingKind,
  strict: bool,
) -> Result<(), VmError> {
  match kind {
    BindingKind::Var => env.set_var(vm, host, hooks, scope, name, value),
    BindingKind::Param => {
      let env_rec = env.lexical_env();
      if !scope.heap().env_has_binding(env_rec, name)? {
        scope.env_create_mutable_binding(env_rec, name)?;
      }
      // Sloppy-mode functions with a simple parameter list may contain duplicate parameter names.
      // When a duplicate is encountered, the binding has already been initialized by the earlier
      // parameter and should be updated instead.
      match scope.heap().env_get_binding_value(env_rec, name, /* strict */ false) {
        Ok(_) => scope
          .heap_mut()
          .env_set_mutable_binding(env_rec, name, value, /* strict */ false),
        // TDZ sentinel from `Heap::env_get_binding_value`.
        Err(VmError::Throw(Value::Null)) => scope.heap_mut().env_initialize_binding(env_rec, name, value),
        Err(err) => Err(err),
      }
    }
    BindingKind::Let => {
      let env_rec = env.lexical_env();
      if !scope.heap().env_has_binding(env_rec, name)? {
        // Non-block statement contexts may not have performed lexical hoisting yet.
        scope.env_create_mutable_binding(env_rec, name)?;
      }
      scope.heap_mut().env_initialize_binding(env_rec, name, value)
    }
    BindingKind::Const => {
      let env_rec = env.lexical_env();
      if !scope.heap().env_has_binding(env_rec, name)? {
        // Non-block statement contexts may not have performed lexical hoisting yet.
        scope.env_create_immutable_binding(env_rec, name)?;
      }
      scope.heap_mut().env_initialize_binding(env_rec, name, value)
    }
    BindingKind::Assignment => env.set(vm, host, hooks, scope, name, value, strict),
  }
}

pub(crate) fn is_anonymous_function_definition(expr: &Node<Expr>) -> bool {
  match &*expr.stx {
    Expr::Func(func) => func.stx.name.is_none(),
    Expr::Class(class) => class.stx.name.is_none(),
    // Arrow functions do not have an explicit name position.
    Expr::ArrowFunc(_) => true,
    _ => false,
  }
}

pub(crate) fn maybe_set_anonymous_function_name(
  scope: &mut Scope<'_>,
  value: Value,
  name: &str,
) -> Result<(), VmError> {
  let Value::Object(func_obj) = value else {
    return Ok(());
  };

  // `SetFunctionName` only applies to actual Function objects. Callable Proxies are callable, but
  // they are not function objects and should not have their `name` mutated.
  let is_native_non_constructable = match scope.heap().get_function(func_obj) {
    Ok(f) => matches!(f.call, CallHandler::Native(_)) && f.construct.is_none(),
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

  // Root the function object while probing/modifying its `name` property: key allocation and
  // `set_function_name` can trigger GC.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(func_obj))?;

  // In spec terms, this is the `HasOwnProperty(F, "name")` check. `vm-js` eagerly defines
  // `F.name` at function allocation, so we approximate the spec by treating an **empty string**
  // `name` data property as "missing" (and therefore eligible for inference), while avoiding
  // clobbering a `static name() {}` method (where `name` is a function object).
  let name_key_s = scope.common_key_name()?;
  scope.push_root(Value::String(name_key_s))?;
  let existing = scope
    .heap()
    .get_own_property(func_obj, PropertyKey::String(name_key_s))?;
  let should_set = match existing {
    None => true,
    Some(desc) => match desc.kind {
      PropertyKind::Data {
        value: Value::String(s),
        ..
      } => scope.heap().get_string(s)?.as_code_units().is_empty(),
      _ => false,
    },
  };
  if !should_set {
    return Ok(());
  }

  let name_s = scope.alloc_string(name)?;
  crate::function_properties::set_function_name(&mut scope, func_obj, PropertyKey::String(name_s), None)?;
  Ok(())
}

fn maybe_set_anonymous_function_name_for_assignment_key(
  scope: &mut Scope<'_>,
  value: Value,
  key: PropertyKey,
) -> Result<(), VmError> {
  let Value::Object(func_obj) = value else {
    return Ok(());
  };

  // `SetFunctionName` only applies to actual Function objects. Callable Proxies are callable, but
  // they are not function objects and should not have their `name` mutated.
  let (current_name, is_native_non_constructable) = match scope.heap().get_function(func_obj) {
    Ok(f) => (
      f.name,
      matches!(f.call, CallHandler::Native(_)) && f.construct.is_none(),
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

  // Root the function object + key across any allocations while defining `name`.
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(func_obj))?;
  match key {
    PropertyKey::String(s) => {
      scope.push_root(Value::String(s))?;
      crate::function_properties::set_function_name(&mut scope, func_obj, PropertyKey::String(s), None)?;
    }
    PropertyKey::Symbol(sym) => {
      scope.push_root(Value::Symbol(sym))?;
      crate::function_properties::set_function_name(&mut scope, func_obj, PropertyKey::Symbol(sym), None)?;
    }
  }
  Ok(())
}

fn bind_object_pattern(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  env: &mut RuntimeEnv,
  pat: &ObjPat,
  value: Value,
  kind: BindingKind,
  strict: bool,
  this: Value,
  home_object: Option<GcObject>,
  derived_constructor: bool,
  this_initialized: bool,
) -> Result<(), VmError> {
  // Object destructuring follows `GetV` semantics: property lookup uses `ToObject(value)`, but
  // accessors must observe `this = value` (the original RHS value), not the boxed object.
  //
  // Root the original RHS value across boxing: `ToObject` can allocate and therefore trigger GC.
  let src_value = scope.push_root(value)?;
  let obj = match scope.to_object(vm, host, hooks, src_value) {
    Ok(obj) => obj,
    Err(VmError::TypeError(msg)) => return Err(throw_type_error(vm, scope, msg)?),
    Err(err) => return Err(err),
  };
  scope.push_root(Value::Object(obj))?;

  let mut excluded: Vec<PropertyKey> = Vec::new();
  excluded
    .try_reserve_exact(pat.properties.len())
    .map_err(|_| VmError::OutOfMemory)?;

  for prop in &pat.properties {
    // Budget object destructuring by pattern size: large patterns can do significant work even
    // without evaluating nested expressions (direct keys, no defaults).
    vm.tick()?;
    let key = resolve_obj_pat_key(
      vm,
      host,
      hooks,
      scope,
      env,
      &prop.stx.key,
      strict,
      this,
      home_object,
      derived_constructor,
      this_initialized,
    )?;
    root_property_key(scope, key)?;
    excluded.push(key);

    // Keep temporary roots local to each property to avoid unbounded root-stack growth for large
    // patterns.
    let mut prop_scope = scope.reborrow();

    // --- Assignment target evaluation order (ECMA-262 `KeyedDestructuringAssignmentEvaluation`) ---
    //
    // For destructuring *assignment* (not binding), the spec evaluates property-reference targets
    // (base + key expression) before calling `GetV(value, propertyName)`. This ensures that an
    // abrupt completion in the LHS (for example a throwing computed property key) does not access
    // the source property.
    //
    // Additionally, for computed member targets (`obj[expr]`), the `ToPropertyKey` conversion is
    // delayed until `PutValue`, after `GetV` / default evaluation. This matches test262:
    // `language/expressions/assignment/destructuring/keyed-destructuring-property-reference-target-evaluation-order.js`
    // and `...-with-bindings.js`.
    enum PropertyAssignmentTarget<'a> {
      Binding(ResolvedBinding<'a>),
      Member { base: Value, key: &'a str },
      ComputedMember { base: Value, key_value: Value },
      SuperMember {
        super_base: Option<GcObject>,
        key: &'a str,
      },
      SuperComputedMember {
        super_base: Option<GcObject>,
        key_value: Value,
      },
    }

    let mut assignment_target: Option<PropertyAssignmentTarget<'_>> = None;
    if matches!(kind, BindingKind::Assignment) {
      match &*prop.stx.target.stx {
        Pat::Id(id) => {
          let binding =
            env.resolve_binding_reference(vm, host, hooks, &mut prop_scope, &id.stx.name)?;
          assignment_target = Some(PropertyAssignmentTarget::Binding(binding));
        }
        Pat::AssignTarget(target) => {
          let target_ref = (|| -> Result<Option<PropertyAssignmentTarget<'_>>, VmError> {
            match &*target.stx {
              Expr::Id(id) => Ok(Some(PropertyAssignmentTarget::Binding(
                env.resolve_binding_reference(vm, host, hooks, &mut prop_scope, &id.stx.name)?,
              ))),
              Expr::IdPat(id) => Ok(Some(PropertyAssignmentTarget::Binding(
                env.resolve_binding_reference(vm, host, hooks, &mut prop_scope, &id.stx.name)?,
              ))),
              Expr::Member(member) => {
                if member.stx.optional_chaining {
                  return Err(VmError::InvariantViolation(
                    "optional chaining used in assignment target",
                  ));
                }

                if matches!(&*member.stx.left.stx, Expr::Super(_)) {
                  // `GetThisBinding` (and derived-constructor initialization checks) must happen
                  // before evaluating the source property (`GetV`).
                  let _ = get_super_receiver(
                    vm,
                    &mut prop_scope,
                    this,
                    derived_constructor,
                    this_initialized,
                  )?;
                  let Some(home) = home_object else {
                    return Err(VmError::InvariantViolation(
                      "super property assignment missing [[HomeObject]]",
                    ));
                  };
                  let super_base = prop_scope.heap().object_prototype(home)?;
                  if let Some(base_obj) = super_base {
                    prop_scope.push_root(Value::Object(base_obj))?;
                  }
                  return Ok(Some(PropertyAssignmentTarget::SuperMember {
                    super_base,
                    key: &member.stx.right,
                  }));
                }

                let base = eval_expr(
                  vm,
                  host,
                  hooks,
                  env,
                  strict,
                  this,
                  home_object,
                  derived_constructor,
                  this_initialized,
                  &mut prop_scope,
                  &member.stx.left,
                )?;
                let base = prop_scope.push_root(base)?;
                Ok(Some(PropertyAssignmentTarget::Member {
                  base,
                  key: &member.stx.right,
                }))
              }
              Expr::ComputedMember(member) => {
                if member.stx.optional_chaining {
                  return Err(VmError::InvariantViolation(
                    "optional chaining used in assignment target",
                  ));
                }

                if matches!(&*member.stx.object.stx, Expr::Super(_)) {
                  // `GetThisBinding` must happen before evaluating the computed key expression so
                  // derived constructors throw before observing key side effects.
                  let _ = get_super_receiver(
                    vm,
                    &mut prop_scope,
                    this,
                    derived_constructor,
                    this_initialized,
                  )?;
                  let Some(home) = home_object else {
                    return Err(VmError::InvariantViolation(
                      "super property assignment missing [[HomeObject]]",
                    ));
                  };
                  let key_value = eval_expr(
                    vm,
                    host,
                    hooks,
                    env,
                    strict,
                    this,
                    home_object,
                    derived_constructor,
                    this_initialized,
                    &mut prop_scope,
                    &member.stx.member,
                  )?;
                  let key_value = prop_scope.push_root(key_value)?;
                  // ECMA-262 `SuperProperty : super [ Expression ]`:
                  // - the key expression is evaluated to a value before `GetSuperBase`, and
                  // - `GetSuperBase` is observed before the deferred `ToPropertyKey` conversion so
                  //   prototype mutation during key coercion does not affect the resolved super
                  //   base.
                  let super_base = prop_scope.heap().object_prototype(home)?;
                  if let Some(base_obj) = super_base {
                    prop_scope.push_root(Value::Object(base_obj))?;
                  }
                  return Ok(Some(PropertyAssignmentTarget::SuperComputedMember {
                    super_base,
                    key_value,
                  }));
                }

                let base = eval_expr(
                  vm,
                  host,
                  hooks,
                  env,
                  strict,
                  this,
                  home_object,
                  derived_constructor,
                  this_initialized,
                  &mut prop_scope,
                  &member.stx.object,
                )?;
                let base = prop_scope.push_root(base)?;
                let key_value = eval_expr(
                  vm,
                  host,
                  hooks,
                  env,
                  strict,
                  this,
                  home_object,
                  derived_constructor,
                  this_initialized,
                  &mut prop_scope,
                  &member.stx.member,
                )?;
                let key_value = prop_scope.push_root(key_value)?;
                Ok(Some(PropertyAssignmentTarget::ComputedMember { base, key_value }))
              }
              _ => Ok(None),
            }
          })();
          match target_ref {
            Ok(v) => assignment_target = v,
            Err(err) => return Err(err),
          }
        }
        _ => {}
      }
    }

    let mut prop_value =
      prop_scope.get_with_host_and_hooks(vm, host, hooks, obj, key, src_value)?;
    if matches!(prop_value, Value::Undefined) {
      if let Some(default_expr) = &prop.stx.default_value {
        prop_value = if matches!(kind, BindingKind::Param) {
          // Default values in parameter destructuring should only infer function names when the
          // default expression is a syntactic anonymous function/class definition.
          if let Pat::Id(id) = &*prop.stx.target.stx {
            let name_s = prop_scope.alloc_string(&id.stx.name)?;
            let key = PropertyKey::from_string(name_s);
            eval_expr_named(
              vm,
              host,
              hooks,
              env,
              strict,
              this,
              home_object,
              derived_constructor,
              this_initialized,
              &mut prop_scope,
              default_expr,
              key,
            )?
          } else {
            eval_expr(
              vm,
              host,
              hooks,
              env,
              strict,
              this,
              home_object,
              derived_constructor,
              this_initialized,
              &mut prop_scope,
              default_expr,
            )?
          }
        } else {
          eval_expr(
            vm,
            host,
            hooks,
            env,
            strict,
            this,
            home_object,
            derived_constructor,
            this_initialized,
            &mut prop_scope,
            default_expr,
          )?
        };

        // `SingleNameBinding` name inference (`IsAnonymousFunctionDefinition` / `SetFunctionName`).
        //
        // Important: only apply this when the *default initializer* is actually evaluated (i.e.
        // when the property value is `undefined`).
        if is_anonymous_function_definition(default_expr) {
          // Binding patterns (let/const/var/params): infer from the binding identifier.
          if !matches!(kind, BindingKind::Assignment) {
            if let Pat::Id(id) = &*prop.stx.target.stx {
              maybe_set_anonymous_function_name(&mut prop_scope, prop_value, id.stx.name.as_str())?;
            }
          } else if let Some(PropertyAssignmentTarget::Binding(binding)) = assignment_target.as_ref()
          {
            // Destructuring assignment: infer from the identifier reference name.
            maybe_set_anonymous_function_name(&mut prop_scope, prop_value, binding.name())?;
          }
        }
      }
    }

    if let Some(target) = assignment_target {
      // Root the property value across any allocations while constructing the property key and
      // performing the assignment. `GetV` may return a freshly-allocated object that is only
      // reachable from this local binding.
      let prop_value = prop_scope.push_root(prop_value)?;

      let res = match target {
        PropertyAssignmentTarget::Binding(binding) => {
          env.set_resolved_binding(vm, host, hooks, &mut prop_scope, binding, prop_value, strict)
        }
        PropertyAssignmentTarget::Member { base, key } => {
          let key_s = prop_scope.alloc_string(key)?;
          prop_scope.push_root(Value::String(key_s))?;
          let key = PropertyKey::from_string(key_s);
          assign_to_property_key(vm, host, hooks, &mut prop_scope, base, key, prop_value, strict)
        }
        PropertyAssignmentTarget::ComputedMember { base, key_value } => {
          let key = match prop_scope.to_property_key(vm, host, hooks, key_value) {
            Ok(key) => key,
            Err(VmError::TypeError(msg)) => {
              let err = match throw_type_error(vm, &mut prop_scope, msg) {
                Ok(e) => e,
                Err(e) => e,
              };
              return Err(err);
            }
            Err(err) => return Err(err),
          };
          root_property_key(&mut prop_scope, key)?;
          assign_to_property_key(vm, host, hooks, &mut prop_scope, base, key, prop_value, strict)
        }
        PropertyAssignmentTarget::SuperMember { super_base, key } => assign_to_super_member(
          vm,
          host,
          hooks,
          &mut prop_scope,
          super_base,
          key,
          prop_value,
          strict,
          this,
          derived_constructor,
          this_initialized,
        ),
        PropertyAssignmentTarget::SuperComputedMember {
          super_base,
          key_value,
        } => assign_to_super_computed_member(
          vm,
          host,
          hooks,
          &mut prop_scope,
          super_base,
          key_value,
          prop_value,
          strict,
          this,
          derived_constructor,
          this_initialized,
        ),
      };
      res?;
    } else {
      bind_pattern(
        vm,
        host,
        hooks,
        &mut prop_scope,
        env,
        &prop.stx.target.stx,
        prop_value,
        kind,
        strict,
        this,
        home_object,
        derived_constructor,
        this_initialized,
      )?;
    }
  }

  let Some(rest_pat) = &pat.rest else {
    return Ok(());
  };

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  // Rest property assignment (e.g. `{...obj[prop]} = source`) must evaluate the LHS reference
  // before copying properties from the source object. Otherwise, getters (or a large source object)
  // could run before an abrupt LHS evaluation.
  enum RestAssignmentTarget<'a> {
    Binding(ResolvedBinding<'a>),
    Member { base: Value, key: &'a str },
    ComputedMember { base: Value, key_value: Value },
    SuperMember {
      super_base: Option<GcObject>,
      key: &'a str,
    },
    SuperComputedMember {
      super_base: Option<GcObject>,
      key_value: Value,
    },
  }

  let mut rest_assignment_target: Option<RestAssignmentTarget<'_>> = None;
  if matches!(kind, BindingKind::Assignment) {
    match &*rest_pat.stx {
      Pat::Id(id) => {
        let binding = env.resolve_binding_reference(vm, host, hooks, scope, &id.stx.name)?;
        rest_assignment_target = Some(RestAssignmentTarget::Binding(binding));
      }
      Pat::AssignTarget(target) => {
        let target_ref = (|| -> Result<Option<RestAssignmentTarget<'_>>, VmError> {
          match &*target.stx {
            Expr::Id(id) => Ok(Some(RestAssignmentTarget::Binding(
              env.resolve_binding_reference(vm, host, hooks, scope, &id.stx.name)?,
            ))),
            Expr::IdPat(id) => Ok(Some(RestAssignmentTarget::Binding(
              env.resolve_binding_reference(vm, host, hooks, scope, &id.stx.name)?,
            ))),
            Expr::Member(member) => {
              if member.stx.optional_chaining {
                return Err(VmError::Unimplemented("optional chaining assignment target"));
              }

              if matches!(&*member.stx.left.stx, Expr::Super(_)) {
                // `GetThisBinding` (and derived-constructor initialization checks) must happen
                // before copying the rest properties (`CopyDataProperties`).
                let _ = get_super_receiver(vm, scope, this, derived_constructor, this_initialized)?;
                let Some(home) = home_object else {
                  return Err(VmError::InvariantViolation(
                    "super property assignment missing [[HomeObject]]",
                  ));
                };
                let super_base = scope.heap().object_prototype(home)?;
                if let Some(base_obj) = super_base {
                  scope.push_root(Value::Object(base_obj))?;
                }
                return Ok(Some(RestAssignmentTarget::SuperMember {
                  super_base,
                  key: &member.stx.right,
                }));
              }

              let base = eval_expr(
                vm,
                host,
                hooks,
                env,
                strict,
                this,
                home_object,
                derived_constructor,
                this_initialized,
                scope,
                &member.stx.left,
              )?;
              let base = scope.push_root(base)?;
              Ok(Some(RestAssignmentTarget::Member {
                base,
                key: &member.stx.right,
              }))
            }
            Expr::ComputedMember(member) => {
              if member.stx.optional_chaining {
                return Err(VmError::Unimplemented("optional chaining assignment target"));
              }

              if matches!(&*member.stx.object.stx, Expr::Super(_)) {
                // `GetThisBinding` must happen before evaluating the computed key expression so
                // derived constructors throw before observing key side effects.
                let _ = get_super_receiver(vm, scope, this, derived_constructor, this_initialized)?;
                let Some(home) = home_object else {
                  return Err(VmError::InvariantViolation(
                    "super property assignment missing [[HomeObject]]",
                  ));
                };
                let key_value = eval_expr(
                  vm,
                  host,
                  hooks,
                  env,
                  strict,
                  this,
                  home_object,
                  derived_constructor,
                  this_initialized,
                  scope,
                  &member.stx.member,
                )?;
                let key_value = scope.push_root(key_value)?;
                // Spec: key expression value before `GetSuperBase`, and `GetSuperBase` before the
                // deferred `ToPropertyKey` conversion.
                let super_base = scope.heap().object_prototype(home)?;
                if let Some(base_obj) = super_base {
                  scope.push_root(Value::Object(base_obj))?;
                }
                return Ok(Some(RestAssignmentTarget::SuperComputedMember {
                  super_base,
                  key_value,
                }));
              }

              let base = eval_expr(
                vm,
                host,
                hooks,
                env,
                strict,
                this,
                home_object,
                derived_constructor,
                this_initialized,
                scope,
                &member.stx.object,
              )?;
              let base = scope.push_root(base)?;
              let key_value =
                eval_expr(
                  vm,
                  host,
                  hooks,
                  env,
                  strict,
                  this,
                  home_object,
                  derived_constructor,
                  this_initialized,
                  scope,
                  &member.stx.member,
                )?;
              let key_value = scope.push_root(key_value)?;
              Ok(Some(RestAssignmentTarget::ComputedMember { base, key_value }))
            }
            _ => Ok(None),
          }
        })();
        match target_ref {
          Ok(v) => rest_assignment_target = v,
          Err(err) => return Err(err),
        }
      }
      _ => {}
    }
  }
  // `...rest` uses `ObjectCreate(%Object.prototype%)` / `CopyDataProperties`.
  let rest_obj = scope.alloc_object_with_prototype(Some(intr.object_prototype()))?;
  scope.push_root(Value::Object(rest_obj))?;

  crate::spec_ops::copy_data_properties_with_host_and_hooks(
    vm,
    scope,
    host,
    hooks,
    rest_obj,
    Value::Object(obj),
    &excluded,
  )?;

  if let Some(target) = rest_assignment_target {
    match target {
      RestAssignmentTarget::Binding(binding) => {
        env.set_resolved_binding(vm, host, hooks, scope, binding, Value::Object(rest_obj), strict)
      }
      RestAssignmentTarget::Member { base, key } => {
        let key_s = scope.alloc_string(key)?;
        scope.push_root(Value::String(key_s))?;
        let key = PropertyKey::from_string(key_s);
        assign_to_property_key(vm, host, hooks, scope, base, key, Value::Object(rest_obj), strict)
      }
      RestAssignmentTarget::ComputedMember { base, key_value } => {
        let key = match scope.to_property_key(vm, host, hooks, key_value) {
          Ok(key) => key,
          Err(VmError::TypeError(msg)) => {
            let err = match throw_type_error(vm, scope, msg) {
              Ok(e) => e,
              Err(e) => e,
            };
            return Err(err);
          }
          Err(err) => return Err(err),
        };
        root_property_key(scope, key)?;
        assign_to_property_key(vm, host, hooks, scope, base, key, Value::Object(rest_obj), strict)
      }
      RestAssignmentTarget::SuperMember { super_base, key } => assign_to_super_member(
        vm,
        host,
        hooks,
        scope,
        super_base,
        key,
        Value::Object(rest_obj),
        strict,
        this,
        derived_constructor,
        this_initialized,
      ),
      RestAssignmentTarget::SuperComputedMember {
        super_base,
        key_value,
      } => assign_to_super_computed_member(
        vm,
        host,
        hooks,
        scope,
        super_base,
        key_value,
        Value::Object(rest_obj),
        strict,
        this,
        derived_constructor,
        this_initialized,
      ),
    }
  } else {
    bind_pattern(
      vm,
      host,
      hooks,
      scope,
      env,
      &rest_pat.stx,
      Value::Object(rest_obj),
      kind,
      strict,
      this,
      home_object,
      derived_constructor,
      this_initialized,
    )
  }
}

fn bind_array_pattern(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  env: &mut RuntimeEnv,
  pat: &ArrPat,
  value: Value,
  kind: BindingKind,
  strict: bool,
  this: Value,
  home_object: Option<GcObject>,
  derived_constructor: bool,
  this_initialized: bool,
) -> Result<(), VmError> {
  // RequireObjectCoercible (ECMA-262): array destructuring disallows null/undefined but supports
  // primitives like String via iterator protocol.
  if matches!(value, Value::Undefined | Value::Null) {
    return Err(throw_type_error(
      vm,
      scope,
      "array destructuring requires object coercible",
    )?);
  }

  // --- Iterator-based destructuring (no array-like fallback) ---
  let mut iterator_record =
    crate::iterator::get_iterator(vm, host, hooks, scope, value)?;
  // Root the iterator record across evaluation of defaults / nested bindings, which can allocate.
  scope.push_roots(&[iterator_record.iterator, iterator_record.next_method])?;

  for elem in &pat.elements {
    // Budget array destructuring by pattern size: holes and identifiers don't evaluate nested
    // expressions, but still advance the iterator.
    if let Err(err) = vm.tick() {
      return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err);
    }

    // Keep temporary roots local to each element to avoid unbounded root-stack growth for large
    // patterns.
    let mut elem_scope = scope.reborrow();

    let Some(elem) = elem else {
      // Elision: still advance the iterator but do not read `value`.
      //
      // Spec: `IteratorBindingInitialization` uses `IteratorStep` for elisions, *not*
      // `IteratorStepValue`. This avoids observable access to the iterator result's `value`
      // property (e.g. a throwing getter) when the element is skipped.
      if let Err(err) =
        crate::iterator::iterator_step(vm, host, hooks, &mut elem_scope, &mut iterator_record)
      {
        return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err);
      }
      continue;
    };

    // --- Assignment target evaluation order (ECMA-262 `IteratorDestructuringAssignmentEvaluation`) ---
    //
    // For destructuring *assignment* (not binding), the spec evaluates property-reference targets
    // (base + key expression) before calling `IteratorStep`/`IteratorStepValue`. This ensures that
    // an abrupt completion in the LHS (for example a throwing computed property key) does not
    // advance the iterator.
    //
    // Additionally, for computed member targets (`obj[expr]`), the `ToPropertyKey` conversion is
    // delayed until `PutValue`, after the iterator step/default evaluation. This matches test262:
    // `language/expressions/assignment/destructuring/iterator-destructuring-property-reference-target-evaluation-order.js`.
    enum ElementAssignmentTarget<'a> {
      Binding(ResolvedBinding<'a>),
      Member { base: Value, key: &'a str },
      ComputedMember { base: Value, key_value: Value },
      SuperMember {
        super_base: Option<GcObject>,
        key: &'a str,
      },
      SuperComputedMember {
        super_base: Option<GcObject>,
        key_value: Value,
      },
    }

    let mut assignment_target: Option<ElementAssignmentTarget<'_>> = None;
    if matches!(kind, BindingKind::Assignment) {
      match &*elem.target.stx {
        Pat::Id(id) => {
          let binding =
            match env.resolve_binding_reference(vm, host, hooks, &mut elem_scope, &id.stx.name) {
              Ok(b) => b,
              Err(err) => {
                return iterator_close_on_err(
                  vm,
                  host,
                  hooks,
                  &mut elem_scope,
                  &iterator_record,
                  err,
                )
              }
            };
          assignment_target = Some(ElementAssignmentTarget::Binding(binding));
        }
        Pat::AssignTarget(target) => {
          let target_ref = (|| -> Result<Option<ElementAssignmentTarget<'_>>, VmError> {
            match &*target.stx {
              Expr::Id(id) => Ok(Some(ElementAssignmentTarget::Binding(
                env.resolve_binding_reference(vm, host, hooks, &mut elem_scope, &id.stx.name)?,
              ))),
              Expr::IdPat(id) => Ok(Some(ElementAssignmentTarget::Binding(
                env.resolve_binding_reference(vm, host, hooks, &mut elem_scope, &id.stx.name)?,
              ))),
              Expr::Member(member) => {
                if member.stx.optional_chaining {
                  return Err(VmError::InvariantViolation(
                    "optional chaining used in assignment target",
                  ));
                }

                if matches!(&*member.stx.left.stx, Expr::Super(_)) {
                  // `GetThisBinding` (and derived-constructor initialization checks) must happen
                  // before advancing the iterator.
                  let _ = get_super_receiver(
                    vm,
                    &mut elem_scope,
                    this,
                    derived_constructor,
                    this_initialized,
                  )?;
                  let Some(home) = home_object else {
                    return Err(VmError::InvariantViolation(
                      "super property assignment missing [[HomeObject]]",
                    ));
                  };
                  let super_base = elem_scope.heap().object_prototype(home)?;
                  if let Some(base_obj) = super_base {
                    elem_scope.push_root(Value::Object(base_obj))?;
                  }
                  return Ok(Some(ElementAssignmentTarget::SuperMember {
                    super_base,
                    key: &member.stx.right,
                  }));
                }

                let base = eval_expr(
                  vm,
                  host,
                  hooks,
                  env,
                  strict,
                  this,
                  home_object,
                  derived_constructor,
                  this_initialized,
                  &mut elem_scope,
                  &member.stx.left,
                )?;
                let base = elem_scope.push_root(base)?;
                Ok(Some(ElementAssignmentTarget::Member {
                  base,
                  key: &member.stx.right,
                }))
              }
              Expr::ComputedMember(member) => {
                if member.stx.optional_chaining {
                  return Err(VmError::InvariantViolation(
                    "optional chaining used in assignment target",
                  ));
                }

                if matches!(&*member.stx.object.stx, Expr::Super(_)) {
                  // `GetThisBinding` must happen before evaluating the computed key expression so
                  // derived constructors throw before observing key side effects.
                  let _ = get_super_receiver(
                    vm,
                    &mut elem_scope,
                    this,
                    derived_constructor,
                    this_initialized,
                  )?;
                  let Some(home) = home_object else {
                    return Err(VmError::InvariantViolation(
                      "super property assignment missing [[HomeObject]]",
                    ));
                  };
                  let key_value = eval_expr(
                    vm,
                    host,
                    hooks,
                    env,
                    strict,
                    this,
                    home_object,
                    derived_constructor,
                    this_initialized,
                    &mut elem_scope,
                    &member.stx.member,
                  )?;
                  let key_value = elem_scope.push_root(key_value)?;
                  // Spec: key expression value before `GetSuperBase`, and `GetSuperBase` before the
                  // deferred `ToPropertyKey` conversion.
                  let super_base = elem_scope.heap().object_prototype(home)?;
                  if let Some(base_obj) = super_base {
                    elem_scope.push_root(Value::Object(base_obj))?;
                  }
                  return Ok(Some(ElementAssignmentTarget::SuperComputedMember {
                    super_base,
                    key_value,
                  }));
                }

                let base = eval_expr(
                  vm,
                  host,
                  hooks,
                  env,
                  strict,
                  this,
                  home_object,
                  derived_constructor,
                  this_initialized,
                  &mut elem_scope,
                  &member.stx.object,
                )?;
                let base = elem_scope.push_root(base)?;
                let key_value = eval_expr(
                  vm,
                  host,
                  hooks,
                  env,
                  strict,
                  this,
                  home_object,
                  derived_constructor,
                  this_initialized,
                  &mut elem_scope,
                  &member.stx.member,
                )?;
                let key_value = elem_scope.push_root(key_value)?;
                Ok(Some(ElementAssignmentTarget::ComputedMember { base, key_value }))
              }
              _ => Ok(None),
            }
          })();
          match target_ref {
            Ok(v) => assignment_target = v,
            Err(err) => {
              return iterator_close_on_err(
                vm,
                host,
                hooks,
                &mut elem_scope,
                &iterator_record,
                err,
              )
            }
          }
        }
        _ => {}
      }
    }

    let mut item = match crate::iterator::iterator_step_value(
      vm,
      host,
      hooks,
      &mut elem_scope,
      &mut iterator_record,
    ) {
      Ok(Some(v)) => v,
      Ok(None) => Value::Undefined,
      Err(err) => {
        return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err)
      }
    };

    if matches!(item, Value::Undefined) {
      if let Some(default_expr) = &elem.default_value {
        item = if matches!(kind, BindingKind::Param) {
          // Default values in parameter destructuring should only infer function names when the
          // default expression is a syntactic anonymous function/class definition.
          if let Pat::Id(id) = &*elem.target.stx {
            let name_s = match elem_scope.alloc_string(&id.stx.name) {
              Ok(s) => s,
              Err(err) => {
                return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err)
              }
            };
            let key = PropertyKey::from_string(name_s);
            match eval_expr_named(
              vm,
              host,
              hooks,
              env,
              strict,
              this,
              home_object,
              derived_constructor,
              this_initialized,
              &mut elem_scope,
              default_expr,
              key,
            ) {
              Ok(v) => v,
              Err(err) => {
                return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err)
              }
            }
          } else {
            match eval_expr(
              vm,
              host,
              hooks,
              env,
              strict,
              this,
              home_object,
              derived_constructor,
              this_initialized,
              &mut elem_scope,
              default_expr,
            ) {
              Ok(v) => v,
              Err(err) => {
                return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err)
              }
            }
          }
        } else {
          match eval_expr(
            vm,
            host,
            hooks,
            env,
            strict,
            this,
            home_object,
            derived_constructor,
            this_initialized,
            &mut elem_scope,
            default_expr,
          ) {
            Ok(v) => v,
            Err(err) => {
              return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err)
            }
          }
        };

        // `SingleNameBinding` name inference (`IsAnonymousFunctionDefinition` / `SetFunctionName`).
        //
        // Important: only apply this when the *default initializer* is actually evaluated (i.e.
        // when the iterator value is `undefined`).
        if is_anonymous_function_definition(default_expr) {
          // Binding patterns (let/const/var/params): infer from the binding identifier.
          if !matches!(kind, BindingKind::Assignment) {
            if let Pat::Id(id) = &*elem.target.stx {
              if let Err(err) =
                maybe_set_anonymous_function_name(&mut elem_scope, item, id.stx.name.as_str())
              {
                return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err);
              }
            }
          } else if let Some(ElementAssignmentTarget::Binding(binding)) = assignment_target.as_ref()
          {
            if let Err(err) = maybe_set_anonymous_function_name(&mut elem_scope, item, binding.name()) {
              return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err);
            }
          }
        }
      }
    }

    if let Some(target) = assignment_target {
      // Root the element value across any allocations while constructing the property key and
      // performing the assignment. Iterator values may be freshly-allocated objects that are only
      // reachable from this local binding.
      let item = match elem_scope.push_root(item) {
        Ok(v) => v,
        Err(err) => {
          return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err)
        }
      };

      let res = match target {
        ElementAssignmentTarget::Binding(binding) => {
          env.set_resolved_binding(vm, host, hooks, &mut elem_scope, binding, item, strict)
        }
        ElementAssignmentTarget::Member { base, key } => {
          let key_s = match elem_scope.alloc_string(key) {
            Ok(s) => s,
            Err(err) => {
              return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err)
            }
          };
          if let Err(err) = elem_scope.push_root(Value::String(key_s)) {
            return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err);
          }
          let key = PropertyKey::from_string(key_s);
          assign_to_property_key(vm, host, hooks, &mut elem_scope, base, key, item, strict)
        }
        ElementAssignmentTarget::ComputedMember { base, key_value } => {
          let key = match elem_scope.to_property_key(vm, host, hooks, key_value) {
            Ok(key) => key,
            Err(VmError::TypeError(msg)) => {
              let err = match throw_type_error(vm, &mut elem_scope, msg) {
                Ok(e) => e,
                Err(e) => e,
              };
              return iterator_close_on_err(
                vm,
                host,
                hooks,
                &mut elem_scope,
                &iterator_record,
                err,
              );
            }
            Err(err) => {
              return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err)
            }
          };
          if let Err(err) = root_property_key(&mut elem_scope, key) {
            return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err);
          }
          assign_to_property_key(vm, host, hooks, &mut elem_scope, base, key, item, strict)
        }
        ElementAssignmentTarget::SuperMember { super_base, key } => {
          assign_to_super_member(
            vm,
            host,
            hooks,
            &mut elem_scope,
            super_base,
            key,
            item,
            strict,
            this,
            derived_constructor,
            this_initialized,
          )
        }
        ElementAssignmentTarget::SuperComputedMember {
          super_base,
          key_value,
        } => assign_to_super_computed_member(
          vm,
          host,
          hooks,
          &mut elem_scope,
          super_base,
          key_value,
          item,
          strict,
          this,
          derived_constructor,
          this_initialized,
        ),
      };
      if let Err(err) = res {
        return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err);
      }
    } else if let Err(err) = bind_pattern(
      vm,
      host,
      hooks,
      &mut elem_scope,
      env,
      &elem.target.stx,
      item,
      kind,
      strict,
      this,
      home_object,
      derived_constructor,
      this_initialized,
    ) {
      return iterator_close_on_err(vm, host, hooks, &mut elem_scope, &iterator_record, err);
    }
  }

  let Some(rest_pat) = &pat.rest else {
    // Iterator binding initialization performs IteratorClose on normal completion when the
    // iterator is not exhausted.
    if !iterator_record.done {
      crate::iterator::iterator_close(
        vm,
        host,
        hooks,
        scope,
        &iterator_record,
        crate::iterator::CloseCompletionKind::NonThrow,
      )?;
    }
    return Ok(());
  };

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;

  // Rest element assignment (e.g. `[...obj[prop]] = iterable`) must evaluate the LHS reference
  // *before* consuming the remainder of the iterator. Otherwise, an infinite iterator could hang
  // the runtime before the (abrupt) LHS evaluation occurs.
  //
  // This ordering is observable in test262 `staging/sm/destructuring/array-iterator-close.js`.
  enum RestAssignmentTarget<'a> {
    Binding(ResolvedBinding<'a>),
    Member { base: Value, key: &'a str },
    ComputedMember { base: Value, key_value: Value },
    SuperMember {
      super_base: Option<GcObject>,
      key: &'a str,
    },
    SuperComputedMember {
      super_base: Option<GcObject>,
      key_value: Value,
    },
  }
 
  let mut rest_assignment_target: Option<RestAssignmentTarget<'_>> = None;
  if matches!(kind, BindingKind::Assignment) {
    match &*rest_pat.stx {
      Pat::Id(id) => {
        match env.resolve_binding_reference(vm, host, hooks, scope, &id.stx.name) {
          Ok(binding) => rest_assignment_target = Some(RestAssignmentTarget::Binding(binding)),
          Err(err) => return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err),
        }
      }
      Pat::AssignTarget(target) => {
        let target_ref = (|| -> Result<Option<RestAssignmentTarget<'_>>, VmError> {
          match &*target.stx {
            Expr::Id(id) => Ok(Some(RestAssignmentTarget::Binding(
              env.resolve_binding_reference(vm, host, hooks, scope, &id.stx.name)?,
            ))),
            Expr::IdPat(id) => Ok(Some(RestAssignmentTarget::Binding(
              env.resolve_binding_reference(vm, host, hooks, scope, &id.stx.name)?,
            ))),
            Expr::Member(member) => {
              if member.stx.optional_chaining {
                return Err(VmError::InvariantViolation(
                  "optional chaining used in assignment target",
                ));
              }

              if matches!(&*member.stx.left.stx, Expr::Super(_)) {
                // `GetThisBinding` (and derived-constructor initialization checks) must happen
                // before consuming the rest of the iterator.
                let _ = get_super_receiver(vm, scope, this, derived_constructor, this_initialized)?;
                let Some(home) = home_object else {
                  return Err(VmError::InvariantViolation(
                    "super property assignment missing [[HomeObject]]",
                  ));
                };
                let super_base = scope.heap().object_prototype(home)?;
                if let Some(base_obj) = super_base {
                  scope.push_root(Value::Object(base_obj))?;
                }
                return Ok(Some(RestAssignmentTarget::SuperMember {
                  super_base,
                  key: &member.stx.right,
                }));
              }

              let base = eval_expr(
                vm,
                host,
                hooks,
                env,
                strict,
                this,
                home_object,
                derived_constructor,
                this_initialized,
                scope,
                &member.stx.left,
              )?;
              let base = scope.push_root(base)?;
              Ok(Some(RestAssignmentTarget::Member {
                base,
                key: &member.stx.right,
              }))
            }
            Expr::ComputedMember(member) => {
              if member.stx.optional_chaining {
                return Err(VmError::InvariantViolation(
                  "optional chaining used in assignment target",
                ));
              }

              if matches!(&*member.stx.object.stx, Expr::Super(_)) {
                // `GetThisBinding` must happen before evaluating the computed key expression so
                // derived constructors throw before observing key side effects.
                let _ = get_super_receiver(vm, scope, this, derived_constructor, this_initialized)?;
                let Some(home) = home_object else {
                  return Err(VmError::InvariantViolation(
                    "super property assignment missing [[HomeObject]]",
                  ));
                };
                let key_value = eval_expr(
                  vm,
                  host,
                  hooks,
                  env,
                  strict,
                  this,
                  home_object,
                  derived_constructor,
                  this_initialized,
                  scope,
                  &member.stx.member,
                )?;
                let key_value = scope.push_root(key_value)?;
                // Spec: key expression value before `GetSuperBase`, and `GetSuperBase` before the
                // deferred `ToPropertyKey` conversion.
                let super_base = scope.heap().object_prototype(home)?;
                if let Some(base_obj) = super_base {
                  scope.push_root(Value::Object(base_obj))?;
                }
                return Ok(Some(RestAssignmentTarget::SuperComputedMember {
                  super_base,
                  key_value,
                }));
              }

              let base = eval_expr(
                vm,
                host,
                hooks,
                env,
                strict,
                this,
                home_object,
                derived_constructor,
                this_initialized,
                scope,
                &member.stx.object,
              )?;
              let base = scope.push_root(base)?;
              let key_value =
                eval_expr(
                  vm,
                  host,
                  hooks,
                  env,
                  strict,
                  this,
                  home_object,
                  derived_constructor,
                  this_initialized,
                  scope,
                  &member.stx.member,
                )?;
              let key_value = scope.push_root(key_value)?;
              Ok(Some(RestAssignmentTarget::ComputedMember { base, key_value }))
            }
            _ => Ok(None),
          }
        })();
        match target_ref {
          Ok(v) => rest_assignment_target = v,
          Err(err) => return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err),
        }
      }
      _ => {}
    }
  }

  // Rest element must produce a real Array exotic object.
  let rest_arr = match scope.alloc_array(0) {
    Ok(arr) => arr,
    Err(err) => return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err),
  };
  if let Err(err) = scope.push_root(Value::Object(rest_arr)) {
    return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err);
  }
  if let Err(err) = scope
    .heap_mut()
    .object_set_prototype(rest_arr, Some(intr.array_prototype()))
  {
    return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err);
  }

  let mut rest_idx: u32 = 0;
  loop {
    // Budget rest-element copying: `...rest` can iterate many remaining indices.
    if let Err(err) = vm.tick() {
      return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err);
    }

    let next = match crate::iterator::iterator_step_value(
      vm,
      host,
      hooks,
      scope,
      &mut iterator_record,
    ) {
      Ok(v) => v,
      Err(err) => return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err),
    };
    let Some(v) = next else {
      break;
    };

    // Root the element value while allocating the property key and defining the property: the
    // iterator's `next` method may return newly-allocated objects that are not reachable from any
    // heap object other than this local binding.
    let create_res = {
      let mut elem_scope = scope.reborrow();
      elem_scope.push_roots(&[Value::Object(rest_arr), v])?;

      let key_s = elem_scope.alloc_u32_index_string(rest_idx)?;
      let key = PropertyKey::from_string(key_s);
      elem_scope.create_data_property_or_throw(rest_arr, key, v)
    };
    if let Err(err) = create_res {
      return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err);
    }
    rest_idx = rest_idx.saturating_add(1);
  }

  if let Some(target) = rest_assignment_target {
    let res = match target {
      RestAssignmentTarget::Binding(binding) => {
        env.set_resolved_binding(vm, host, hooks, scope, binding, Value::Object(rest_arr), strict)
      }
      RestAssignmentTarget::Member { base, key } => {
        let key_s = match scope.alloc_string(key) {
          Ok(s) => s,
          Err(err) => return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err),
        };
        if let Err(err) = scope.push_root(Value::String(key_s)) {
          return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err);
        }
        let key = PropertyKey::from_string(key_s);
        assign_to_property_key(vm, host, hooks, scope, base, key, Value::Object(rest_arr), strict)
      }
      RestAssignmentTarget::ComputedMember { base, key_value } => {
        let key = match scope.to_property_key(vm, host, hooks, key_value) {
          Ok(key) => key,
          Err(VmError::TypeError(msg)) => {
            let err = match throw_type_error(vm, scope, msg) {
              Ok(e) => e,
              Err(e) => e,
            };
            return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err);
          }
          Err(err) => return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err),
        };
        if let Err(err) = root_property_key(scope, key) {
          return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err);
        }
        assign_to_property_key(vm, host, hooks, scope, base, key, Value::Object(rest_arr), strict)
      }
      RestAssignmentTarget::SuperMember { super_base, key } => assign_to_super_member(
        vm,
        host,
        hooks,
        scope,
        super_base,
        key,
        Value::Object(rest_arr),
        strict,
        this,
        derived_constructor,
        this_initialized,
      ),
      RestAssignmentTarget::SuperComputedMember {
        super_base,
        key_value,
      } => assign_to_super_computed_member(
        vm,
        host,
        hooks,
        scope,
        super_base,
        key_value,
        Value::Object(rest_arr),
        strict,
        this,
        derived_constructor,
        this_initialized,
      ),
    };
    return match res {
      Ok(()) => Ok(()),
      Err(err) => iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err),
    };
  }

  let bind_res = bind_pattern(
    vm,
    host,
    hooks,
    scope,
    env,
    &rest_pat.stx,
    Value::Object(rest_arr),
    kind,
    strict,
    this,
    home_object,
    derived_constructor,
    this_initialized,
  );
  match bind_res {
    Ok(()) => Ok(()),
    Err(err) => iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err),
  }
}

fn resolve_obj_pat_key(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  env: &mut RuntimeEnv,
  key: &ClassOrObjKey,
  strict: bool,
  this: Value,
  home_object: Option<GcObject>,
  derived_constructor: bool,
  this_initialized: bool,
) -> Result<PropertyKey, VmError> {
  match key {
    ClassOrObjKey::Direct(direct) => {
      let s = if let Some(units) = literal_string_code_units(&direct.assoc) {
        scope.alloc_string_from_code_units(units)?
      } else if direct.stx.tt == TT::LiteralNumber {
        let mut tick = || vm.tick();
        let n = crate::ops::parse_ascii_decimal_to_f64_str(&direct.stx.key, &mut tick)?
          .ok_or(VmError::Unimplemented("numeric literal property name parse"))?;
        scope.heap_mut().to_string(Value::Number(n))?
      } else {
        scope.alloc_string(&direct.stx.key)?
      };
      Ok(PropertyKey::from_string(s))
    }
    ClassOrObjKey::Computed(expr) => {
      let value = eval_expr(
        vm,
        host,
        hooks,
        env,
        strict,
        this,
        home_object,
        derived_constructor,
        this_initialized,
        scope,
        expr,
      )?;
      // Root the computed value until `to_property_key` completes.
      let value = scope.push_root(value)?;
      let key = match scope.to_property_key(vm, host, hooks, value) {
        Ok(key) => key,
        Err(VmError::TypeError(msg)) => return Err(throw_type_error(vm, scope, msg)?),
        Err(err) => return Err(err),
      };
      Ok(key)
    }
  }
}

fn assign_to_property_key(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  base: Value,
  key: PropertyKey,
  value: Value,
  strict: bool,
) -> Result<(), VmError> {
  // Root `base`/`key`/`value` across `ToObject(base)` and `[[Set]]`, both of which can allocate and
  // invoke user code (via accessors / host hooks).
  let mut set_scope = scope.reborrow();
  let key_root = match key {
    PropertyKey::String(s) => Value::String(s),
    PropertyKey::Symbol(s) => Value::Symbol(s),
  };
  let roots = [base, key_root, value];
  set_scope.push_roots(&roots)?;
  maybe_set_anonymous_function_name_for_assignment_key(&mut set_scope, value, key)?;

  // `PutValue` for property references uses `ToObject(base)` for the target object, but uses the
  // original base value (which may be a primitive) as the receiver.
  let object = match set_scope.to_object(vm, host, hooks, base) {
    Ok(obj) => obj,
    Err(VmError::TypeError(msg)) => return Err(throw_type_error(vm, &mut set_scope, msg)?),
    Err(err) => return Err(err),
  };
  // Root the boxed object so host hooks/accessors can allocate freely.
  set_scope.push_root(Value::Object(object))?;

  let ok = crate::spec_ops::internal_set_with_host_and_hooks(
    vm,
    &mut set_scope,
    host,
    hooks,
    object,
    key,
    value,
    base,
  )?;
  if ok {
    Ok(())
  } else if strict {
    Err(throw_type_error(vm, &mut set_scope, "Cannot assign to read-only property")?)
  } else {
    // Sloppy-mode assignment to a non-writable/non-extensible target fails silently.
    Ok(())
  }
}

fn assign_to_super_member(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  super_base: Option<GcObject>,
  key: &str,
  value: Value,
  strict: bool,
  this: Value,
  derived_constructor: bool,
  this_initialized: bool,
) -> Result<(), VmError> {
  // Root receiver/value/super base across key allocation and `[[Set]]`.
  let mut set_scope = scope.reborrow();
  set_scope.push_roots(&[this, value])?;
  if let Some(base_obj) = super_base {
    set_scope.push_root(Value::Object(base_obj))?;
  }

  let receiver = get_super_receiver(vm, &mut set_scope, this, derived_constructor, this_initialized)?;
  set_scope.push_root(receiver)?;

  let key_s = set_scope.alloc_string(key)?;
  set_scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  maybe_set_anonymous_function_name_for_assignment_key(&mut set_scope, value, key)?;

  let Some(base_obj) = super_base else {
    // Mirror `PutValue` null/undefined base behaviour by using `ToObject(null)` for the error.
    match set_scope.to_object(vm, host, hooks, Value::Null) {
      Err(VmError::TypeError(msg)) => return Err(throw_type_error(vm, &mut set_scope, msg)?),
      Err(err) => return Err(err),
      Ok(_) => unreachable!("ToObject(null) should throw"),
    }
  };
  set_scope.push_root(Value::Object(base_obj))?;

  let ok = crate::spec_ops::internal_set_with_host_and_hooks(
    vm,
    &mut set_scope,
    host,
    hooks,
    base_obj,
    key,
    value,
    receiver,
  )?;
  if ok {
    Ok(())
  } else if strict {
    Err(throw_type_error(vm, &mut set_scope, "Cannot assign to read-only property")?)
  } else {
    Ok(())
  }
}

fn assign_to_super_computed_member(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  super_base: Option<GcObject>,
  key_value: Value,
  value: Value,
  strict: bool,
  this: Value,
  derived_constructor: bool,
  this_initialized: bool,
) -> Result<(), VmError> {
  // Root receiver/value/key_value/super base across `ToPropertyKey`/`[[Set]]`.
  let mut set_scope = scope.reborrow();
  set_scope.push_roots(&[this, key_value, value])?;
  if let Some(base_obj) = super_base {
    set_scope.push_root(Value::Object(base_obj))?;
  }

  let receiver = get_super_receiver(vm, &mut set_scope, this, derived_constructor, this_initialized)?;
  set_scope.push_root(receiver)?;

  let key = match set_scope.to_property_key(vm, host, hooks, key_value) {
    Ok(key) => key,
    Err(VmError::TypeError(msg)) => return Err(throw_type_error(vm, &mut set_scope, msg)?),
    Err(err) => return Err(err),
  };
  root_property_key(&mut set_scope, key)?;
  maybe_set_anonymous_function_name_for_assignment_key(&mut set_scope, value, key)?;

  let Some(base_obj) = super_base else {
    // Ensure `ToPropertyKey` happens before throwing for a `null` super base.
    match set_scope.to_object(vm, host, hooks, Value::Null) {
      Err(VmError::TypeError(msg)) => return Err(throw_type_error(vm, &mut set_scope, msg)?),
      Err(err) => return Err(err),
      Ok(_) => unreachable!("ToObject(null) should throw"),
    }
  };
  set_scope.push_root(Value::Object(base_obj))?;

  let ok = crate::spec_ops::internal_set_with_host_and_hooks(
    vm,
    &mut set_scope,
    host,
    hooks,
    base_obj,
    key,
    value,
    receiver,
  )?;
  if ok {
    Ok(())
  } else if strict {
    Err(throw_type_error(vm, &mut set_scope, "Cannot assign to read-only property")?)
  } else {
    Ok(())
  }
}

fn assign_to_member(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  env: &mut RuntimeEnv,
  member: &MemberExpr,
  value: Value,
  strict: bool,
  this: Value,
  home_object: Option<GcObject>,
  derived_constructor: bool,
  this_initialized: bool,
) -> Result<(), VmError> {
  if member.optional_chaining {
    return Err(VmError::InvariantViolation(
      "optional chaining used in assignment target",
    ));
  }

  if matches!(&*member.left.stx, Expr::Super(_)) {
    // `GetThisBinding` must happen before evaluating the property set (derived constructors throw
    // before any observable side effects).
    let _ = get_super_receiver(vm, scope, this, derived_constructor, this_initialized)?;
    let Some(home) = home_object else {
      return Err(VmError::InvariantViolation(
        "super property assignment missing [[HomeObject]]",
      ));
    };
    let super_base = scope.heap().object_prototype(home)?;
    if let Some(base_obj) = super_base {
      scope.push_root(Value::Object(base_obj))?;
    }
    return assign_to_super_member(
      vm,
      host,
      hooks,
      scope,
      super_base,
      &member.right,
      value,
      strict,
      this,
      derived_constructor,
      this_initialized,
    );
  }

  // Root the RHS across evaluation of the LHS object.
  let mut rhs_scope = scope.reborrow();
  rhs_scope.push_root(value)?;
  let base = eval_expr(
    vm,
    host,
    hooks,
    env,
    strict,
    this,
    home_object,
    derived_constructor,
    this_initialized,
    &mut rhs_scope,
    &member.left,
  )?;
  // Root the base value across property-key allocation and `ToObject(base)` boxing.
  let base = rhs_scope.push_root(base)?;

  let key_s = rhs_scope.alloc_string(&member.right)?;
  let key = PropertyKey::from_string(key_s);
  assign_to_property_key(vm, host, hooks, &mut rhs_scope, base, key, value, strict)
}

fn assign_to_computed_member(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  env: &mut RuntimeEnv,
  member: &ComputedMemberExpr,
  value: Value,
  strict: bool,
  this: Value,
  home_object: Option<GcObject>,
  derived_constructor: bool,
  this_initialized: bool,
) -> Result<(), VmError> {
  if member.optional_chaining {
    return Err(VmError::InvariantViolation(
      "optional chaining used in assignment target",
    ));
  }

  if matches!(&*member.object.stx, Expr::Super(_)) {
    // `super[expr]` assignment target.
    let mut key_scope = scope.reborrow();
    key_scope.push_roots(&[this, value])?;

    // `GetThisBinding` must happen before evaluating the computed key expression so derived
    // constructors throw before observing key side effects.
    let _ = get_super_receiver(
      vm,
      &mut key_scope,
      this,
      derived_constructor,
      this_initialized,
    )?;

    let Some(home) = home_object else {
      return Err(VmError::InvariantViolation(
        "super property assignment missing [[HomeObject]]",
      ));
    };
    let key_value = eval_expr(
      vm,
      host,
      hooks,
      env,
      strict,
      this,
      home_object,
      derived_constructor,
      this_initialized,
      &mut key_scope,
      &member.member,
    )?;
    let key_value = key_scope.push_root(key_value)?;
    // Spec: key expression value before `GetSuperBase`, and `GetSuperBase` before `ToPropertyKey`.
    let super_base = key_scope.heap().object_prototype(home)?;
    if let Some(base_obj) = super_base {
      key_scope.push_root(Value::Object(base_obj))?;
    }

    return assign_to_super_computed_member(
      vm,
      host,
      hooks,
      &mut key_scope,
      super_base,
      key_value,
      value,
      strict,
      this,
      derived_constructor,
      this_initialized,
    );
  }

  // Root the RHS across evaluation of the LHS object/key.
  let mut rhs_scope = scope.reborrow();
  rhs_scope.push_root(value)?;

  let base = eval_expr(
    vm,
    host,
    hooks,
    env,
    strict,
    this,
    home_object,
    derived_constructor,
    this_initialized,
    &mut rhs_scope,
    &member.object,
  )?;
  // Root the base across evaluation/conversion of the computed key.
  let base = rhs_scope.push_root(base)?;
  let key_value = eval_expr(
    vm,
    host,
    hooks,
    env,
    strict,
    this,
    home_object,
    derived_constructor,
    this_initialized,
    &mut rhs_scope,
    &member.member,
  )?;
  let key_value = rhs_scope.push_root(key_value)?;
  let key = match rhs_scope.to_property_key(vm, host, hooks, key_value) {
    Ok(key) => key,
    Err(VmError::TypeError(msg)) => return Err(throw_type_error(vm, &mut rhs_scope, msg)?),
    Err(err) => return Err(err),
  };
  assign_to_property_key(vm, host, hooks, &mut rhs_scope, base, key, value, strict)
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
