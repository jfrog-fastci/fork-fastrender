use vm_js::{
  GcObject, Heap, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError,
  VmHost, VmHostHooks,
};

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn illegal_constructor_call(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("Illegal constructor"))
}

fn illegal_constructor_construct(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _args: &[Value],
  _new_target: Value,
) -> Result<Value, VmError> {
  Err(VmError::TypeError("Illegal constructor"))
}

fn non_configurable_read_only_data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: false,
    configurable: false,
    kind: PropertyKind::Data {
      value,
      writable: false,
    },
  }
}

/// Install a minimal `ShadowRoot` interface object (`globalThis.ShadowRoot`) for the vm-js WebIDL
/// backend.
///
/// The WebIDL bindings generator does not currently emit a `ShadowRoot` installer, but the
/// WebIDL-first DOM backend still needs a real interface object so:
/// - `DomPlatform::new_from_global_prototypes` can adopt `ShadowRoot.prototype` for wrapper
///   inheritance, and
/// - scripts can observe `ShadowRoot` as a distinct platform-object interface (`instanceof`).
///
/// This installer is **non-clobbering**: if `globalThis.ShadowRoot` already exists and is an
/// object, the global binding is preserved. The installer may still **patch** the existing
/// constructor/prototype objects to ensure `ShadowRoot.prototype` exists and follows the expected
/// `DocumentFragment` inheritance chain (mirroring the behavior of generated installers).
pub fn install_shadow_root_bindings_vm_js(
  vm: &mut Vm,
  heap: &mut Heap,
  realm: &Realm,
) -> Result<(), VmError> {
  let mut scope = heap.scope();
  let base = scope.heap().stack_root_len();

  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let shadow_root_key = alloc_key(&mut scope, "ShadowRoot")?;

  // Determine the parent interface objects (if present): ShadowRoot inherits from DocumentFragment.
  let mut document_fragment_ctor: Option<GcObject> = None;
  let mut document_fragment_proto: Option<GcObject> = None;
  {
    let df_key = alloc_key(&mut scope, "DocumentFragment")?;
    if let Ok(Some(Value::Object(ctor))) =
      scope
        .heap()
        .object_get_own_data_property_value(global, &df_key)
    {
      scope.push_root(Value::Object(ctor))?;
      document_fragment_ctor = Some(ctor);

      let proto_key = alloc_key(&mut scope, "prototype")?;
      if let Ok(Some(Value::Object(proto))) =
        scope
          .heap()
          .object_get_own_data_property_value(ctor, &proto_key)
      {
        scope.push_root(Value::Object(proto))?;
        document_fragment_proto = Some(proto);
      }
    }
  }

  // If `globalThis.ShadowRoot` already exists (e.g. installed by generated bindings), patch it in
  // place and avoid clobbering the global.
  if let Ok(Some(Value::Object(existing_ctor))) = scope
    .heap()
    .object_get_own_data_property_value(global, &shadow_root_key)
  {
    scope.push_root(Value::Object(existing_ctor))?;

    let prototype_key = alloc_key(&mut scope, "prototype")?;
    let shadow_root_proto = match scope
      .heap()
      .object_get_own_data_property_value(existing_ctor, &prototype_key)
    {
      Ok(Some(Value::Object(proto))) => proto,
      _ => {
        let proto = scope.alloc_object()?;
        scope.push_root(Value::Object(proto))?;
        scope.define_property(
          existing_ctor,
          prototype_key,
          non_configurable_read_only_data_desc(Value::Object(proto)),
        )?;
        proto
      }
    };
    scope.push_root(Value::Object(shadow_root_proto))?;

    // Ensure `ShadowRoot.prototype.constructor` exists.
    let constructor_key = alloc_key(&mut scope, "constructor")?;
    if scope
      .heap()
      .object_get_own_property(shadow_root_proto, &constructor_key)?
      .is_none()
    {
      scope.define_property(
        shadow_root_proto,
        constructor_key,
        non_configurable_read_only_data_desc(Value::Object(existing_ctor)),
      )?;
    }

    // Patch prototype inheritance: `ShadowRoot.prototype` inherits from `DocumentFragment.prototype`.
    if let Some(parent_proto) = document_fragment_proto {
      scope
        .heap_mut()
        .object_set_prototype(shadow_root_proto, Some(parent_proto))?;
    }

    // Patch interface object inheritance: `Object.getPrototypeOf(ShadowRoot) === DocumentFragment`.
    if let Some(parent_ctor) = document_fragment_ctor {
      scope
        .heap_mut()
        .object_set_prototype(existing_ctor, Some(parent_ctor))?;
    }

    scope.heap_mut().truncate_stack_roots(base);
    return Ok(());
  }

  // Create `ShadowRoot.prototype`.
  let shadow_root_proto = scope.alloc_object()?;
  scope.push_root(Value::Object(shadow_root_proto))?;
  let parent_proto = document_fragment_proto.or_else(|| Some(realm.intrinsics().object_prototype()));
  scope
    .heap_mut()
    .object_set_prototype(shadow_root_proto, parent_proto)?;

  // Create an illegal constructor function.
  let call_id = vm.register_native_call(illegal_constructor_call)?;
  let construct_id = vm.register_native_construct(illegal_constructor_construct)?;

  let name = scope.alloc_string("ShadowRoot")?;
  scope.push_root(Value::String(name))?;

  let ctor = scope.alloc_native_function(call_id, Some(construct_id), name, 0)?;
  scope.push_root(Value::Object(ctor))?;
  scope.heap_mut().object_set_prototype(
    ctor,
    Some(realm.intrinsics().function_prototype()),
  )?;

  // Link `ctor.prototype`.
  let prototype_key = alloc_key(&mut scope, "prototype")?;
  scope.define_property(
    ctor,
    prototype_key,
    non_configurable_read_only_data_desc(Value::Object(shadow_root_proto)),
  )?;

  // Link `prototype.constructor`.
  let constructor_key = alloc_key(&mut scope, "constructor")?;
  scope.define_property(
    shadow_root_proto,
    constructor_key,
    non_configurable_read_only_data_desc(Value::Object(ctor)),
  )?;

  // Web-compatible global constructor attributes: writable, configurable, non-enumerable.
  scope.define_property(
    global,
    shadow_root_key,
    PropertyDescriptor {
      enumerable: false,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(ctor),
        writable: true,
      },
    },
  )?;

  // Match WebIDL interface object inheritance (`Object.getPrototypeOf(ShadowRoot) === DocumentFragment`)
  // when the parent constructor is available. WindowRealm also patches the constructor chain later,
  // but doing it here keeps the installer self-contained.
  if let Some(parent_ctor) = document_fragment_ctor {
    scope
      .heap_mut()
      .object_set_prototype(ctor, Some(parent_ctor))?;
  }

  scope.heap_mut().truncate_stack_roots(base);
  Ok(())
}
