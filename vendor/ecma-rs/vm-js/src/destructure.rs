use crate::exec::{eval_expr, RuntimeEnv};
use crate::property::{PropertyDescriptor, PropertyKey, PropertyKind};
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
  let Value::Object(obj) = value else {
    return Err(throw_type_error(vm, scope, "object destructuring requires object")?);
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

    let mut prop_value =
      scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
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

  let rest_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(rest_obj))?;

  let keys = scope.ordinary_own_property_keys_with_tick(obj, || vm.tick())?;
  for key in keys {
    // Budget rest-property copying: `...rest` can iterate many keys even for a small pattern.
    vm.tick()?;
    if excluded
      .iter()
      .any(|excluded_key| scope.heap().property_key_eq(excluded_key, &key))
    {
      continue;
    }

    let Some(desc) = scope.ordinary_get_own_property(obj, key)? else {
      continue;
    };
    if !desc.enumerable {
      continue;
    }

    // `CopyDataProperties` uses `Get` even though we already have the descriptor.
    let v = scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
    let _ = scope.create_data_property(rest_obj, key, v)?;
  }

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
  let Value::Object(obj) = value else {
    return Err(throw_type_error(vm, scope, "array destructuring requires object")?);
  };
  scope.push_root(Value::Object(obj))?;

  let len = array_like_length(vm, host, hooks, scope, obj)?;
  let mut idx: u32 = 0;

  for elem in &pat.elements {
    // Budget array destructuring by pattern size: holes and identifiers don't evaluate nested
    // expressions, but still advance the iterator/index.
    vm.tick()?;
    let Some(elem) = elem else {
      idx = idx.saturating_add(1);
      continue;
    };

    let mut item = if idx < len {
      array_like_get(vm, host, hooks, scope, obj, idx)?
    } else {
      Value::Undefined
    };

    if matches!(item, Value::Undefined) {
      if let Some(default_expr) = &elem.default_value {
        item = eval_expr(vm, host, hooks, env, strict, this, scope, default_expr)?;
      }
    }

    bind_pattern(
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
    )?;
    idx = idx.saturating_add(1);
  }

  let Some(rest_pat) = &pat.rest else {
    return Ok(());
  };

  let rest_arr = scope.alloc_object()?;
  scope.push_root(Value::Object(rest_arr))?;

  let mut rest_idx: u32 = 0;
  while idx < len {
    // Budget rest-element copying: `...rest` can iterate many remaining indices.
    vm.tick()?;
    let v = array_like_get(vm, host, hooks, scope, obj, idx)?;
    {
      // Root the element value while allocating the property key and defining the property:
      // `array_like_get` can invoke getters which may return newly-allocated objects that are not
      // reachable from `obj` itself.
      let mut elem_scope = scope.reborrow();
      let v = elem_scope.push_root(v)?;
      let key_str = rest_idx.to_string();
      let key_s = elem_scope.alloc_string(&key_str)?;
      let key = PropertyKey::from_string(key_s);
      let _ = elem_scope.create_data_property(rest_arr, key, v)?;
    }
    idx += 1;
    rest_idx += 1;
  }

  // Define `length` as non-enumerable to match real arrays closely enough for rest patterns.
  let length_s = scope.alloc_string("length")?;
  let length_key = PropertyKey::from_string(length_s);
  let length_desc = PropertyDescriptor {
    enumerable: false,
    configurable: true,
    kind: PropertyKind::Data {
      value: Value::Number(rest_idx as f64),
      writable: true,
    },
  };
  scope.define_property(rest_arr, length_key, length_desc)?;

  bind_pattern(
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
  )
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
        let n = direct
          .stx
          .key
          .parse::<f64>()
          .map_err(|_| VmError::Unimplemented("numeric literal property name parse"))?;
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

fn array_like_length(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  obj: GcObject,
) -> Result<u32, VmError> {
  let key_s = scope.alloc_string("length")?;
  let key = PropertyKey::from_string(key_s);
  let v = scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))?;
  match v {
    Value::Number(n) if n.is_finite() && n >= 0.0 => Ok(n as u32),
    Value::Undefined => Ok(0),
    _ => Err(VmError::Unimplemented("array-like length")),
  }
}

fn array_like_get(
  vm: &mut Vm,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  scope: &mut Scope<'_>,
  obj: GcObject,
  idx: u32,
) -> Result<Value, VmError> {
  let key_str = idx.to_string();
  let key_s = scope.alloc_string(&key_str)?;
  let key = PropertyKey::from_string(key_s);
  scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, Value::Object(obj))
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

  let ok = set_scope.ordinary_set_with_host_and_hooks(vm, host, hooks, object, key, value, base)?;
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
