use crate::exec::{eval_expr, RuntimeEnv};
use crate::property::PropertyKey;
use crate::{Scope, Value, Vm, VmError, VmHost, VmHostHooks};
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

  // `IteratorClose` suppression rules (ECMA-262):
  // - If the original completion is a throw completion, iterator closing is best-effort and any
  //   *catchable* closing error is suppressed.
  // - Never allow JS-visible closing errors to replace fatal VM failures (termination, OOM, etc).
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
    Err(close_err) => {
      if original_is_throw && close_err.is_throw_completion() {
        // Suppress JS-visible `IteratorClose` errors when we are already throwing.
        Err(err)
      } else if original_is_throw {
        // Never suppress fatal VM errors (OOM/termination/etc).
        Err(close_err)
      } else {
        // Preserve fatal/non-catchable original errors even if closing throws.
        Err(err)
      }
    }
  }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum BindingKind {
  Var,
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
  // `SetFunctionName`-like behaviour: when binding an anonymous function/class to an identifier,
  // infer its `name` from the identifier.
  //
  // In ECMAScript this applies in a variety of binding/assignment contexts; `vm-js` approximates it
  // here for identifier targets.
  maybe_set_anonymous_function_name(scope, value, name)?;

  match kind {
    BindingKind::Var => env.set_var(vm, host, hooks, scope, name, value),
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
  let current_name = match scope.heap().get_function(func_obj) {
    Ok(f) => f.name,
    Err(VmError::NotCallable) => return Ok(()),
    Err(err) => return Err(err),
  };
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
  crate::function_properties::set_function_name(&mut scope, func_obj, PropertyKey::String(name_s), None)?;
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
    )?;
    root_property_key(scope, key)?;
    excluded.push(key);

    let mut prop_value = scope.get_with_host_and_hooks(vm, host, hooks, obj, key, src_value)?;
    if matches!(prop_value, Value::Undefined) {
      if let Some(default_expr) = &prop.stx.default_value {
        prop_value = eval_expr(vm, host, hooks, env, strict, this, scope, default_expr)?;
      }
    }

    bind_pattern(
      vm,
      host,
      hooks,
      scope,
      env,
      &prop.stx.target.stx,
      prop_value,
      kind,
      strict,
      this,
    )?;
  }

  let Some(rest_pat) = &pat.rest else {
    return Ok(());
  };

  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
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
  )
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

    let Some(elem) = elem else {
      // Elision: still advance the iterator but do not read `value`.
      //
      // Spec: `IteratorBindingInitialization` uses `IteratorStep` for elisions, *not*
      // `IteratorStepValue`. This avoids observable access to the iterator result's `value`
      // property (e.g. a throwing getter) when the element is skipped.
      if let Err(err) = crate::iterator::iterator_step(vm, host, hooks, scope, &mut iterator_record) {
        return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err);
      }
      continue;
    };

    let mut item = match crate::iterator::iterator_step_value(
      vm,
      host,
      hooks,
      scope,
      &mut iterator_record,
    ) {
      Ok(Some(v)) => v,
      Ok(None) => Value::Undefined,
      Err(err) => return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err),
    };

    if matches!(item, Value::Undefined) {
      if let Some(default_expr) = &elem.default_value {
        item = match eval_expr(vm, host, hooks, env, strict, this, scope, default_expr) {
          Ok(v) => v,
          Err(err) => {
            return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err)
          }
        };
      }
    }

    if let Err(err) = bind_pattern(
      vm,
      host,
      hooks,
      scope,
      env,
      &elem.target.stx,
      item,
      kind,
      strict,
      this,
    ) {
      return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err);
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
    Id(&'a str),
    Property { base: Value, key: PropertyKey },
  }

  let mut rest_assignment_target: Option<RestAssignmentTarget<'_>> = None;
  if matches!(kind, BindingKind::Assignment) {
    if let Pat::AssignTarget(target) = &*rest_pat.stx {
      let target_ref = (|| -> Result<Option<RestAssignmentTarget<'_>>, VmError> {
        match &*target.stx {
          Expr::Id(id) => Ok(Some(RestAssignmentTarget::Id(&id.stx.name))),
          Expr::IdPat(id) => Ok(Some(RestAssignmentTarget::Id(&id.stx.name))),
          Expr::Member(member) => {
            if member.stx.optional_chaining {
              return Err(VmError::Unimplemented("optional chaining assignment target"));
            }
 
            let base = eval_expr(vm, host, hooks, env, strict, this, scope, &member.stx.left)?;
            let base = scope.push_root(base)?;
 
            let key_s = scope.alloc_string(&member.stx.right)?;
            scope.push_root(Value::String(key_s))?;
            let key = PropertyKey::from_string(key_s);
            Ok(Some(RestAssignmentTarget::Property { base, key }))
          }
          Expr::ComputedMember(member) => {
            if member.stx.optional_chaining {
              return Err(VmError::Unimplemented("optional chaining assignment target"));
            }
 
            let base = eval_expr(vm, host, hooks, env, strict, this, scope, &member.stx.object)?;
            let base = scope.push_root(base)?;
            let key_value =
              eval_expr(vm, host, hooks, env, strict, this, scope, &member.stx.member)?;
            let key_value = scope.push_root(key_value)?;
            let key = match scope.to_property_key(vm, host, hooks, key_value) {
              Ok(key) => key,
              Err(VmError::TypeError(msg)) => return Err(throw_type_error(vm, scope, msg)?),
              Err(err) => return Err(err),
            };
            root_property_key(scope, key)?;
            Ok(Some(RestAssignmentTarget::Property { base, key }))
          }
          _ => Ok(None),
        }
      })();
      match target_ref {
        Ok(v) => rest_assignment_target = v,
        Err(err) => return iterator_close_on_err(vm, host, hooks, scope, &iterator_record, err),
      }
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

      let key_s = elem_scope.alloc_string(&rest_idx.to_string())?;
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
      RestAssignmentTarget::Id(name) => bind_identifier(
        vm,
        host,
        hooks,
        env,
        scope,
        name,
        Value::Object(rest_arr),
        BindingKind::Assignment,
        strict,
      ),
      RestAssignmentTarget::Property { base, key } => {
        assign_to_property_key(vm, host, hooks, scope, base, key, Value::Object(rest_arr), strict)
      }
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
      let value = eval_expr(vm, host, hooks, env, strict, this, scope, expr)?;
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
) -> Result<(), VmError> {
  if member.optional_chaining {
    return Err(VmError::Unimplemented("optional chaining assignment target"));
  }

  // Root the RHS across evaluation of the LHS object.
  let mut rhs_scope = scope.reborrow();
  rhs_scope.push_root(value)?;
  let base = eval_expr(vm, host, hooks, env, strict, this, &mut rhs_scope, &member.left)?;
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
) -> Result<(), VmError> {
  if member.optional_chaining {
    return Err(VmError::Unimplemented("optional chaining assignment target"));
  }

  // Root the RHS across evaluation of the LHS object/key.
  let mut rhs_scope = scope.reborrow();
  rhs_scope.push_root(value)?;

  let base = eval_expr(vm, host, hooks, env, strict, this, &mut rhs_scope, &member.object)?;
  // Root the base across evaluation/conversion of the computed key.
  let base = rhs_scope.push_root(base)?;
  let key_value = eval_expr(vm, host, hooks, env, strict, this, &mut rhs_scope, &member.member)?;
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
