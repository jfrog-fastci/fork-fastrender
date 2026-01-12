use crate::env::{DeclarativeEnvRecord, EnvBinding, EnvBindingValue, EnvRecord, ObjectEnvRecord};
use crate::function::{
  CallHandler, ConstructHandler, EcmaFunctionId, FunctionData, JsFunction, NativeConstructId,
  NativeFunctionId, ThisMode,
};
use crate::property::{PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind};
use crate::promise::{PromiseReaction, PromiseReactionType, PromiseState};
use crate::string::JsString;
use crate::symbol::JsSymbol;
use crate::CompiledFunctionRef;
use crate::{
  EnvRootId, GcEnv, GcObject, GcString, GcSymbol, HeapId, RealmId, RootId, Value, Vm, VmError,
  VmHost, VmHostHooks, WeakGcObject,
};
use std::cell::Cell;
use core::mem;
use semantic_js::js::SymbolId;
use std::rc::Rc;
use std::collections::HashSet;

/// Hard upper bound for `[[Prototype]]` chain traversals.
///
/// This is a DoS resistance measure. Even though `object_set_prototype` prevents cycles,
/// embeddings (or unsafe internal helpers) can violate invariants.
pub const MAX_PROTOTYPE_CHAIN: usize = 10_000;

/// Lightweight copy of a Proxy's internal slots.
///
/// This exists so callers can query proxy state without holding a borrow of the underlying heap
/// allocation across further heap operations (which may need `&mut Heap`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProxyData {
  pub target: Option<GcObject>,
  pub handler: Option<GcObject>,
}

/// Minimum non-zero capacity for heap-internal vectors that can grow due to hostile input.
///
/// Keeping a small floor avoids pathological "grow by 1" patterns while still being conservative
/// about over-allocation.
const MIN_VEC_CAPACITY: usize = 1;

/// Per-object host slots for embeddings (e.g. DOM/WebIDL bindings).
///
/// This is deliberately:
/// - **JS-unobservable** (not stored as a property)
/// - **Copy** (small, inline payload)
/// - **Non-GC-traced** (must not contain `Gc*` handles)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostSlots {
  pub a: u64,
  pub b: u64,
}

/// Heap configuration and memory limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapLimits {
  /// Hard memory limit for heap memory usage, in bytes.
  ///
  /// This is enforced against [`Heap::estimated_total_bytes`], which includes live heap payload
  /// bytes, external allocations owned by heap objects (e.g. `ArrayBuffer` backing stores), and
  /// GC/heap metadata overhead (slot table, mark bits, root stacks, etc).
  ///
  /// It also includes VM-managed off-heap allocations charged via [`Heap::charge_external`], such
  /// as `SourceText`, compiled code caches, and module metadata.
  pub max_bytes: usize,
  /// When an allocation would cause [`Heap::estimated_total_bytes`] to exceed this threshold, the
  /// heap will trigger a GC cycle before attempting the allocation.
  pub gc_threshold: usize,
}

impl HeapLimits {
  /// Creates a new set of heap limits.
  pub fn new(max_bytes: usize, gc_threshold: usize) -> Self {
    Self {
      max_bytes,
      gc_threshold,
    }
  }
}

#[derive(Debug, Clone, Copy)]
struct SymbolRegistryEntry {
  key: GcString,
  sym: GcSymbol,
}

#[derive(Debug)]
struct ExternalMemoryTracker {
  bytes: Cell<usize>,
}

impl ExternalMemoryTracker {
  fn new() -> Self {
    Self { bytes: Cell::new(0) }
  }

  fn bytes(&self) -> usize {
    self.bytes.get()
  }

  fn add(&self, bytes: usize) {
    self.bytes.set(self.bytes.get().saturating_add(bytes));
  }

  fn sub(&self, bytes: usize) {
    // Never panic in a destructor path; be conservative and saturate.
    self.bytes.set(self.bytes.get().saturating_sub(bytes));
  }
}

/// RAII token returned by [`Heap::charge_external`].
///
/// While this token is alive, its charged bytes contribute to [`Heap::estimated_total_bytes`].
/// Dropping the token releases the charge.
#[derive(Debug)]
pub struct ExternalMemoryToken {
  tracker: Rc<ExternalMemoryTracker>,
  bytes: usize,
}

impl ExternalMemoryToken {
  /// The number of bytes charged by this token.
  #[inline]
  pub fn bytes(&self) -> usize {
    self.bytes
  }
}

impl Drop for ExternalMemoryToken {
  fn drop(&mut self) {
    self.tracker.sub(self.bytes);
  }
}

/// A non-moving mark/sweep GC heap.
///
/// The heap stores objects in a `Vec` of slots. GC handles store the slot `index` and a
/// per-slot `generation`, which makes handles stable across `Vec` reallocations and allows
/// detection of stale handles when slots are reused.
pub struct Heap {
  limits: HeapLimits,

  /// Default `[[Prototype]]` used for objects created as `F.prototype` by
  /// [`crate::function_properties::make_constructor`].
  ///
  /// When a realm is initialized, this is set to that realm's `%Object.prototype%` so that objects
  /// constructed via user-defined functions/classes inherit `Object.prototype` methods like
  /// `toString`/`hasOwnProperty` by default.
  ///
  /// When no realm has been initialized (e.g. unit tests that use the heap/VM without creating a
  /// realm), this remains `None` and `F.prototype` objects are created with a null `[[Prototype]]`.
  default_object_prototype: Option<GcObject>,

  /// Bytes used by live heap object payloads.
  ///
  /// This intentionally excludes heap metadata overhead (slot table, mark bits, roots, etc) which
  /// is tracked via [`Heap::estimated_total_bytes`].
  used_bytes: usize,
  /// Bytes used by external allocations owned by heap objects.
  ///
  /// This is tracked separately from [`Heap::used_bytes`] so the GC can account for non-GC memory
  /// (e.g. `ArrayBuffer` backing stores) when deciding when to collect.
  external_bytes: usize,
  /// Bytes used by VM-managed allocations that live outside the GC heap.
  ///
  /// This covers large off-heap structures such as source text, compiled code caches, and module
  /// graph metadata. Bytes are added via [`Heap::charge_external`], and are released when the
  /// returned [`ExternalMemoryToken`] is dropped.
  vm_external_bytes: Rc<ExternalMemoryTracker>,
  gc_runs: u64,

  // GC-managed allocations.
  slots: Vec<Slot>,
  marks: Vec<u8>,
  free_list: Vec<u32>,
  /// Worklist used during GC marking.
  ///
  /// Stored on the heap (rather than allocated per-GC) so collection does not need to allocate.
  gc_worklist: Vec<HeapId>,

  next_symbol_id: u64,

  // Root sets.
  pub(crate) root_stack: Vec<Value>,
  pub(crate) env_root_stack: Vec<GcEnv>,
  persistent_roots: Vec<Option<Value>>,
  persistent_roots_free: Vec<u32>,
  persistent_env_roots: Vec<Option<GcEnv>>,
  persistent_env_roots_free: Vec<u32>,

  // Global symbol registry for `Symbol.for`-like behaviour.
  //
  // The registry is scanned during GC (as an additional root set) to keep
  // interned symbols alive.
  symbol_registry: Vec<SymbolRegistryEntry>,
}

/// RAII wrapper for a persistent GC root created by [`Heap::add_root`].
///
/// This is intended for host embeddings that need to keep VM values alive across calls but want to
/// avoid leaking roots on early returns.
///
/// While this guard is alive it holds a mutable borrow of the [`Heap`]. For long-lived roots stored
/// in host state, prefer storing the returned [`RootId`] from [`Heap::add_root`] directly.
pub struct PersistentRoot<'a> {
  heap: &'a mut Heap,
  id: RootId,
}

impl<'a> PersistentRoot<'a> {
  /// Adds `value` to the heap's persistent root set and returns a guard that removes it on drop.
  pub fn new(heap: &'a mut Heap, value: Value) -> Result<Self, VmError> {
    let id = heap.add_root(value)?;
    Ok(Self { heap, id })
  }

  /// The underlying [`RootId`].
  #[inline]
  pub fn id(&self) -> RootId {
    self.id
  }

  /// Returns the current rooted value.
  #[inline]
  pub fn get(&self) -> Option<Value> {
    self.heap.get_root(self.id)
  }

  /// Updates the rooted value.
  #[inline]
  pub fn set(&mut self, value: Value) {
    self.heap.set_root(self.id, value);
  }

  /// Borrows the underlying heap immutably.
  #[inline]
  pub fn heap(&self) -> &Heap {
    &*self.heap
  }

  /// Borrows the underlying heap mutably.
  #[inline]
  pub fn heap_mut(&mut self) -> &mut Heap {
    &mut *self.heap
  }
}

impl Drop for PersistentRoot<'_> {
  fn drop(&mut self) {
    self.heap.remove_root(self.id);
  }
}

impl Heap {
  /// Creates a new heap with the provided memory limits.
  pub fn new(limits: HeapLimits) -> Self {
    debug_assert!(
      limits.gc_threshold <= limits.max_bytes,
      "gc_threshold should be <= max_bytes"
    );

    Self {
      limits,
      default_object_prototype: None,
      used_bytes: 0,
      external_bytes: 0,
      vm_external_bytes: Rc::new(ExternalMemoryTracker::new()),
      gc_runs: 0,
      slots: Vec::new(),
      marks: Vec::new(),
      free_list: Vec::new(),
      gc_worklist: Vec::new(),
      next_symbol_id: 1,
      root_stack: Vec::new(),
      env_root_stack: Vec::new(),
      persistent_roots: Vec::new(),
      persistent_roots_free: Vec::new(),
      persistent_env_roots: Vec::new(),
      persistent_env_roots_free: Vec::new(),
      symbol_registry: Vec::new(),
    }
  }

  pub(crate) fn set_default_object_prototype(&mut self, proto: Option<GcObject>) {
    self.default_object_prototype = proto;
  }

  pub(crate) fn default_object_prototype(&self) -> Option<GcObject> {
    self.default_object_prototype
  }

  /// Enters a stack-rooting scope.
  ///
  /// Stack roots pushed via [`Scope::push_root`] are removed when the returned `Scope` is dropped.
  pub fn scope(&mut self) -> Scope<'_> {
    let root_stack_len_at_entry = self.root_stack.len();
    let env_root_stack_len_at_entry = self.env_root_stack.len();
    Scope {
      heap: self,
      root_stack_len_at_entry,
      env_root_stack_len_at_entry,
    }
  }

  /// Returns the current length of the value stack root set.
  ///
  /// Values pushed via [`Heap::push_stack_root`] are traced during GC.
  pub fn stack_root_len(&self) -> usize {
    self.root_stack.len()
  }

  /// Returns the number of active persistent roots registered via [`Heap::add_root`].
  pub fn persistent_root_count(&self) -> usize {
    self.persistent_roots.iter().filter(|slot| slot.is_some()).count()
  }

  /// Returns the number of active persistent environment roots registered via [`Heap::add_env_root`].
  pub fn persistent_env_root_count(&self) -> usize {
    self
      .persistent_env_roots
      .iter()
      .filter(|slot| slot.is_some())
      .count()
  }

  /// Pushes a value stack root.
  ///
  /// Stack roots are traced during GC until removed (typically via
  /// [`Heap::truncate_stack_roots`]).
  pub fn push_stack_root(&mut self, value: Value) -> Result<(), VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(value));
    let new_len = self
      .root_stack
      .len()
      .checked_add(1)
      .ok_or(VmError::OutOfMemory)?;
    let growth_bytes = vec_capacity_growth_bytes::<Value>(self.root_stack.capacity(), new_len);
    if growth_bytes != 0 {
      // Ensure `value` is treated as a root if this triggers a GC while we grow `root_stack`.
      let values = [value];
      self.ensure_can_allocate_with_extra_roots(|_| growth_bytes, &values, &[], &[], &[])?;
      reserve_vec_to_len::<Value>(&mut self.root_stack, new_len)?;
    }
    self.root_stack.push(value);
    Ok(())
  }

  /// Pushes multiple value stack roots in one operation.
  ///
  /// This is equivalent to calling [`Heap::push_stack_root`] repeatedly, but ensures *all* values
  /// are treated as roots if the root stack needs to grow (and therefore potentially trigger GC).
  pub fn push_stack_roots(&mut self, values: &[Value]) -> Result<(), VmError> {
    if values.is_empty() {
      return Ok(());
    }

    for value in values {
      debug_assert!(self.debug_value_is_valid_or_primitive(*value));
    }

    let new_len = self
      .root_stack
      .len()
      .checked_add(values.len())
      .ok_or(VmError::OutOfMemory)?;
    let growth_bytes = vec_capacity_growth_bytes::<Value>(self.root_stack.capacity(), new_len);
    if growth_bytes != 0 {
      // Ensure `values` are treated as roots if this triggers a GC while we grow `root_stack`.
      self.ensure_can_allocate_with_extra_roots(|_| growth_bytes, values, &[], &[], &[])?;
      reserve_vec_to_len::<Value>(&mut self.root_stack, new_len)?;
    }
    for value in values {
      self.root_stack.push(*value);
    }
    Ok(())
  }

  /// Truncates the value stack root set.
  pub fn truncate_stack_roots(&mut self, len: usize) {
    self.root_stack.truncate(len);
  }

  /// Bytes currently used by live heap object payloads.
  ///
  /// This excludes heap metadata overhead; see [`Heap::estimated_total_bytes`].
  pub fn used_bytes(&self) -> usize {
    self.used_bytes
  }

  /// Bytes currently used by external allocations owned by heap objects.
  ///
  /// This includes allocations that live outside the GC heap but are still owned by GC objects
  /// (e.g. `ArrayBuffer` backing stores).
  pub fn external_bytes(&self) -> usize {
    self.external_bytes
  }

  /// Bytes currently charged for VM-managed off-heap allocations.
  ///
  /// This does **not** include external allocations owned by heap objects (see
  /// [`Heap::external_bytes`]).
  pub fn vm_external_bytes(&self) -> usize {
    self.vm_external_bytes.bytes()
  }

  /// Increments the external allocation counter.
  ///
  /// Callers should pair this with [`Heap::sub_external_bytes`] when the external allocation is
  /// released (typically via a GC finalizer).
  pub fn add_external_bytes(&mut self, bytes: usize) {
    self.external_bytes = self.external_bytes.saturating_add(bytes);
  }

  /// Decrements the external allocation counter.
  pub fn sub_external_bytes(&mut self, bytes: usize) {
    debug_assert!(
      bytes <= self.external_bytes,
      "attempted to subtract more external bytes than currently tracked (tracked={}, sub={})",
      self.external_bytes,
      bytes
    );
    self.external_bytes = self.external_bytes.saturating_sub(bytes);
  }

  /// Charges `bytes` against this heap's [`HeapLimits`], returning a token that releases the charge
  /// on drop.
  ///
  /// This is intended for:
  /// - VM-internal allocations that live outside the GC heap (e.g. `SourceText`, compiled code
  ///   caches, module metadata).
  /// - Embedder allocations that should count toward the VM's memory limit (DOM wrapper caches,
  ///   module source caches, etc).
  ///
  /// If charging would exceed [`HeapLimits::gc_threshold`], this triggers a GC cycle before failing
  /// with [`VmError::OutOfMemory`].
  pub fn charge_external(&mut self, bytes: usize) -> Result<ExternalMemoryToken, VmError> {
    let additional = bytes;
    let after = self.estimated_total_bytes().saturating_add(additional);
    if after > self.limits.gc_threshold {
      self.collect_garbage();
    }

    let after = self.estimated_total_bytes().saturating_add(additional);
    if after > self.limits.max_bytes {
      return Err(VmError::OutOfMemory);
    }

    self.vm_external_bytes.add(bytes);
    Ok(ExternalMemoryToken {
      tracker: self.vm_external_bytes.clone(),
      bytes,
    })
  }

  /// The heap's configured memory limits.
  pub fn limits(&self) -> HeapLimits {
    self.limits
  }

  /// Estimated total bytes used by the heap, including GC metadata overhead.
  ///
  /// This is the value used to enforce [`HeapLimits::max_bytes`] and trigger collection at
  /// [`HeapLimits::gc_threshold`].
  pub fn estimated_total_bytes(&self) -> usize {
    let mut total = 0usize;

    // Live payload bytes (dynamic allocations owned by live heap objects).
    total = total.saturating_add(self.used_bytes);

    // External allocations owned by live heap objects (e.g. `ArrayBuffer` backing stores).
    total = total.saturating_add(self.external_bytes);

    // VM-managed off-heap allocations (source text, compiled code, module metadata, embedder
    // caches, etc).
    total = total.saturating_add(self.vm_external_bytes.bytes());

    // Slot table + mark bits + free lists + GC worklist.
    total = total.saturating_add(self.slots.capacity().saturating_mul(mem::size_of::<Slot>()));
    total = total.saturating_add(self.marks.capacity()); // Vec<u8>
    total = total.saturating_add(self.free_list.capacity().saturating_mul(mem::size_of::<u32>()));
    total = total.saturating_add(
      self
        .gc_worklist
        .capacity()
        .saturating_mul(mem::size_of::<HeapId>()),
    );

    // Root sets.
    total = total.saturating_add(
      self
        .root_stack
        .capacity()
        .saturating_mul(mem::size_of::<Value>()),
    );
    total = total.saturating_add(
      self
        .env_root_stack
        .capacity()
        .saturating_mul(mem::size_of::<GcEnv>()),
    );
    total = total.saturating_add(
      self
        .persistent_roots
        .capacity()
        .saturating_mul(mem::size_of::<Option<Value>>()),
    );
    total = total.saturating_add(
      self
        .persistent_roots_free
        .capacity()
        .saturating_mul(mem::size_of::<u32>()),
    );
    total = total.saturating_add(
      self
        .persistent_env_roots
        .capacity()
        .saturating_mul(mem::size_of::<Option<GcEnv>>()),
    );
    total = total.saturating_add(
      self
        .persistent_env_roots_free
        .capacity()
        .saturating_mul(mem::size_of::<u32>()),
    );

    // Symbol registry overhead. (The key payload bytes are already included because the registry
    // stores `GcString` handles to heap strings.)
    total = total.saturating_add(
      self
        .symbol_registry
        .capacity()
        .saturating_mul(mem::size_of::<SymbolRegistryEntry>()),
    );

    total
  }

  #[cfg(debug_assertions)]
  fn debug_recompute_used_bytes(&self) -> usize {
    self
      .slots
      .iter()
      .filter(|slot| slot.value.is_some())
      .fold(0usize, |acc, slot| acc.saturating_add(slot.bytes))
  }

  #[cfg(debug_assertions)]
  fn debug_assert_used_bytes_is_correct(&self) {
    let recomputed = self.debug_recompute_used_bytes();
    debug_assert_eq!(
      self.used_bytes, recomputed,
      "Heap::used_bytes mismatch: used_bytes={}, recomputed={}",
      self.used_bytes, recomputed
    );
  }

  /// Total number of GC cycles that have run.
  pub fn gc_runs(&self) -> u64 {
    self.gc_runs
  }

  /// Explicitly runs a GC cycle.
  pub fn collect_garbage(&mut self) {
    self.collect_garbage_with_extra_roots(&[], &[], &[], &[]);
  }

  fn collect_garbage_with_extra_roots(
    &mut self,
    extra_value_roots_a: &[Value],
    extra_value_roots_b: &[Value],
    extra_env_roots_a: &[GcEnv],
    extra_env_roots_b: &[GcEnv],
  ) {
    self.gc_runs += 1;

    // Mark.
    {
      debug_assert_eq!(self.slots.len(), self.marks.len());

      let slots = &self.slots;
      let marks = &mut self.marks[..];

      self.gc_worklist.clear();
      let mut tracer = Tracer::new(slots, marks, &mut self.gc_worklist);
      for value in extra_value_roots_a {
        tracer.trace_value(*value);
      }
      for value in extra_value_roots_b {
        tracer.trace_value(*value);
      }
      for env in extra_env_roots_a {
        tracer.trace_env(*env);
      }
      for env in extra_env_roots_b {
        tracer.trace_env(*env);
      }
      for value in &self.root_stack {
        tracer.trace_value(*value);
      }
      for env in &self.env_root_stack {
        tracer.trace_env(*env);
      }
      for value in self.persistent_roots.iter().flatten() {
        tracer.trace_value(*value);
      }
      for env in self.persistent_env_roots.iter().flatten() {
        tracer.trace_env(*env);
      }
      for entry in &self.symbol_registry {
        // The registry roots both the key (string) and the interned symbol.
        tracer.trace_value(Value::String(entry.key));
        tracer.trace_value(Value::Symbol(entry.sym));
      }

      while let Some(id) = tracer.pop_work() {
        let Some(idx) = tracer.validate(id) else {
          continue;
        };
        if tracer.marks[idx] == 2 {
          continue;
        }
        tracer.marks[idx] = 2;

        let Some(obj) = tracer.slots[idx].value.as_ref() else {
          debug_assert!(false, "validated heap id points to a free slot: {id:?}");
          continue;
        };
        obj.trace(&mut tracer);
      }
    }

    // Sweep.
    for (idx, slot) in self.slots.iter_mut().enumerate() {
      let marked = self.marks[idx] != 0;
      // Reset mark bits for next cycle.
      self.marks[idx] = 0;

      if slot.value.is_none() {
        debug_assert!(!marked);
        continue;
      }

      if marked {
        continue;
      }

      // Unreachable: drop the object and free the slot.
      if let Some(obj) = slot.value.as_mut() {
        obj.finalize(&mut self.external_bytes);
      }
      self.used_bytes = self.used_bytes.saturating_sub(slot.bytes);
      slot.value = None;
      slot.bytes = 0;
      slot.host_slots = None;
      slot.generation = slot.generation.wrapping_add(1);
      self.free_list.push(idx as u32);
    }

    // WeakSet hygiene: remove dead keys from live WeakSet objects.
    //
    // Even though `WeakSet` operations treat dead keys as absent, failing to prune them causes the
    // internal entry list to grow without bound across GC cycles.
    //
    // This is intentionally in-place (no allocation): we move each entry vector out temporarily so
    // we can call back into `&self` for liveness checks without holding a borrow into `self.slots`.
    for idx in 0..self.slots.len() {
      let mut entries = {
        let Some(HeapObject::WeakSet(ws)) = self.slots[idx].value.as_mut() else {
          continue;
        };
        mem::take(&mut ws.entries)
      };
      entries.retain(|entry| entry.upgrade(&*self).is_some());
      let Some(HeapObject::WeakSet(ws)) = self.slots[idx].value.as_mut() else {
        continue;
      };
      ws.entries = entries;
      // Note: we do not shrink the underlying allocation here; `retain` only updates length.
      // Slot `bytes` accounting remains unchanged because the allocation capacity is unchanged.
    }

    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
  }

  /// Adds a persistent root and returns an RAII guard that removes it on drop.
  #[inline]
  pub fn persistent_root(&mut self, value: Value) -> Result<PersistentRoot<'_>, VmError> {
    PersistentRoot::new(self, value)
  }

  /// Adds a persistent root, keeping `value` live until the returned [`RootId`] is removed.
  pub fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    // Root sets should not contain stale handles; detect issues early in debug builds.
    debug_assert!(self.debug_value_is_valid_or_primitive(value));

    // Fast path: reuse a previously-freed root slot.
    let idx = if let Some(idx) = self.persistent_roots_free.pop() {
      idx as usize
    } else {
      // Slow path: grow the root table (and ensure the free list is large enough that
      // `remove_root` never needs to allocate).
      let extra_roots = [value];
      self.ensure_can_allocate_with_extra_roots(
        |heap| heap.additional_bytes_for_new_persistent_root_slot(),
        &extra_roots,
        &[],
        &[],
        &[],
      )?;
      self.reserve_for_new_persistent_root_slot()?;
      self.persistent_roots.push(None);
      self.persistent_roots.len() - 1
    };
    debug_assert!(self.persistent_roots[idx].is_none());
    self.persistent_roots[idx] = Some(value);
    Ok(RootId(idx as u32))
  }

  /// Returns the current value of a persistent root.
  pub fn get_root(&self, id: RootId) -> Option<Value> {
    self
      .persistent_roots
      .get(id.0 as usize)
      .and_then(|slot| *slot)
  }

  /// Updates a persistent root's value.
  ///
  /// Panics only in debug builds if `id` is invalid.
  pub fn set_root(&mut self, id: RootId, value: Value) {
    // Root sets should not contain stale handles; detect issues early in debug builds.
    debug_assert!(self.debug_value_is_valid_or_primitive(value));

    let idx = id.0 as usize;
    debug_assert!(idx < self.persistent_roots.len(), "invalid RootId");
    if idx >= self.persistent_roots.len() {
      return;
    }
    debug_assert!(
      self.persistent_roots[idx].is_some(),
      "RootId already removed"
    );
    if self.persistent_roots[idx].is_some() {
      self.persistent_roots[idx] = Some(value);
    }
  }

  /// Removes a persistent root previously created by [`Heap::add_root`].
  pub fn remove_root(&mut self, id: RootId) {
    let idx = id.0 as usize;
    debug_assert!(idx < self.persistent_roots.len(), "invalid RootId");
    if idx >= self.persistent_roots.len() {
      return;
    }
    debug_assert!(
      self.persistent_roots[idx].is_some(),
      "RootId already removed"
    );
    if self.persistent_roots[idx].take().is_some() {
      self.persistent_roots_free.push(id.0);
    }
  }

  /// Adds a persistent environment root, keeping `env` live until the returned [`EnvRootId`] is
  /// removed.
  pub fn add_env_root(&mut self, env: GcEnv) -> Result<EnvRootId, VmError> {
    debug_assert!(self.is_valid_env(env));

    // Root `env` during allocation in case growing the env-root table triggers GC.
    let mut scope = self.scope();
    scope.push_env_root(env)?;

    // Fast path: reuse a previously-freed root slot.
    let idx = if let Some(idx) = scope.heap.persistent_env_roots_free.pop() {
      idx as usize
    } else {
      // Slow path: grow the env-root table (and ensure the free list is large enough that
      // `remove_env_root` never needs to allocate).
      scope
        .heap
        .ensure_can_allocate_with(|heap| heap.additional_bytes_for_new_persistent_env_root_slot())?;
      scope.heap.reserve_for_new_persistent_env_root_slot()?;
      scope.heap.persistent_env_roots.push(None);
      scope.heap.persistent_env_roots.len() - 1
    };
    debug_assert!(scope.heap.persistent_env_roots[idx].is_none());
    scope.heap.persistent_env_roots[idx] = Some(env);
    Ok(EnvRootId(idx as u32))
  }

  /// Returns the current value of a persistent env root.
  pub fn get_env_root(&self, id: EnvRootId) -> Option<GcEnv> {
    self
      .persistent_env_roots
      .get(id.0 as usize)
      .and_then(|slot| *slot)
  }

  /// Updates a persistent env root's value.
  ///
  /// Panics only in debug builds if `id` is invalid.
  pub fn set_env_root(&mut self, id: EnvRootId, env: GcEnv) {
    debug_assert!(self.is_valid_env(env));

    let idx = id.0 as usize;
    debug_assert!(
      idx < self.persistent_env_roots.len(),
      "invalid EnvRootId"
    );
    if idx >= self.persistent_env_roots.len() {
      return;
    }
    debug_assert!(
      self.persistent_env_roots[idx].is_some(),
      "EnvRootId already removed"
    );
    if self.persistent_env_roots[idx].is_some() {
      self.persistent_env_roots[idx] = Some(env);
    }
  }

  /// Removes a persistent env root previously created by [`Heap::add_env_root`].
  pub fn remove_env_root(&mut self, id: EnvRootId) {
    let idx = id.0 as usize;
    debug_assert!(
      idx < self.persistent_env_roots.len(),
      "invalid EnvRootId"
    );
    if idx >= self.persistent_env_roots.len() {
      return;
    }
    debug_assert!(
      self.persistent_env_roots[idx].is_some(),
      "EnvRootId already removed"
    );
    if self.persistent_env_roots[idx].take().is_some() {
      self.persistent_env_roots_free.push(id.0);
    }
  }

  /// Returns `true` if `obj` currently points to a live object allocation.
  pub fn is_valid_object(&self, obj: GcObject) -> bool {
    matches!(
      self.get_heap_object(obj.0),
      Ok(
        HeapObject::Object(_)
          | HeapObject::ArrayBuffer(_)
          | HeapObject::TypedArray(_)
          | HeapObject::DataView(_)
          | HeapObject::Function(_)
          | HeapObject::Proxy(_)
          | HeapObject::Promise(_)
          | HeapObject::WeakSet(_)
          | HeapObject::Generator(_)
      )
    )
  }

  /// Returns `true` if `obj` currently points to a live Proxy object allocation.
  ///
  /// This is the spec-shaped "brand check" used by `IsRevokedProxy`-like operations: an object is a
  /// Proxy if it has Proxy internal slots (represented here by the `HeapObject::Proxy` variant).
  pub fn is_proxy_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::Proxy(_)))
  }

  /// Returns the `[[ProxyTarget]]` internal slot of `proxy`.
  ///
  /// Returns `Ok(None)` for revoked proxies.
  pub fn proxy_target(&self, proxy: GcObject) -> Result<Option<GcObject>, VmError> {
    Ok(self.get_proxy(proxy)?.target)
  }

  /// Returns the `[[ProxyHandler]]` internal slot of `proxy`.
  ///
  /// Returns `Ok(None)` for revoked proxies.
  pub fn proxy_handler(&self, proxy: GcObject) -> Result<Option<GcObject>, VmError> {
    Ok(self.get_proxy(proxy)?.handler)
  }

  /// Revokes a Proxy object by clearing its `[[ProxyTarget]]` and `[[ProxyHandler]]` internal slots.
  pub fn proxy_revoke(&mut self, proxy: GcObject) -> Result<(), VmError> {
    let p = self.get_proxy_mut(proxy)?;
    p.target = None;
    p.handler = None;
    Ok(())
  }

  /// Returns `true` if `obj` currently points to a live Promise object allocation.
  ///
  /// This is the spec-shaped "brand check" used by `IsPromise`: an object is a Promise if it has
  /// Promise internal slots (represented here by the `HeapObject::Promise` variant).
  pub fn is_promise_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::Promise(_)))
  }

  pub(crate) fn is_generator_object(&self, _obj: GcObject) -> bool {
    false
  }

  /// Returns `true` if `obj` currently points to a live ArrayBuffer object allocation.
  pub fn is_array_buffer_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::ArrayBuffer(_)))
  }

  /// Returns `true` if `obj` is a detached `ArrayBuffer`.
  ///
  /// `vm-js` represents detachment by clearing the backing store (`data: None`), which also makes
  /// `ArrayBufferByteLength` report `0`.
  pub fn is_detached_array_buffer(&self, obj: GcObject) -> Result<bool, VmError> {
    Ok(self.get_array_buffer(obj)?.data.is_none())
  }

  /// Returns `true` if `obj` currently points to a live Uint8Array object allocation.
  pub fn is_uint8_array_object(&self, obj: GcObject) -> bool {
    matches!(
      self.get_heap_object(obj.0),
      Ok(HeapObject::TypedArray(arr)) if arr.kind == TypedArrayKind::Uint8
    )
  }

  /// Returns `true` if `obj` currently points to a live typed array object allocation.
  pub fn is_typed_array_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::TypedArray(_)))
  }

  /// Returns `true` if `obj` currently points to a live DataView object allocation.
  pub fn is_data_view_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::DataView(_)))
  }

  /// Returns `true` if `obj` currently points to a live ArrayBuffer view (typed array or DataView).
  pub fn is_array_buffer_view_object(&self, obj: GcObject) -> bool {
    matches!(
      self.get_heap_object(obj.0),
      Ok(HeapObject::TypedArray(_) | HeapObject::DataView(_))
    )
  }

  /// Returns `true` if `obj` currently points to a live Date object allocation.
  pub fn is_date_object(&self, obj: GcObject) -> bool {
    matches!(self.date_value(obj), Ok(Some(_)))
  }

  /// Returns the `[[DateValue]]` internal slot for a Date object.
  ///
  /// Returns `Ok(None)` if `obj` is not a Date object.
  pub fn date_value(&self, obj: GcObject) -> Result<Option<f64>, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::Object(o) => match &o.base.kind {
        ObjectKind::Date(d) => Ok(Some(d.value)),
        _ => Ok(None),
      },
      _ => Ok(None),
    }
  }

  pub(crate) fn array_buffer_byte_length(&self, obj: GcObject) -> Result<usize, VmError> {
    Ok(self.get_array_buffer(obj)?.byte_length())
  }

  pub(crate) fn array_buffer_is_detached(&self, obj: GcObject) -> Result<bool, VmError> {
    Ok(self.get_array_buffer(obj)?.data.is_none())
  }

  /// Returns a borrowed view of the bytes backing an `ArrayBuffer` object.
  ///
  /// This is intended for host bindings that need to read `ArrayBuffer` contents (e.g. `TextDecoder`).
  /// The returned slice is valid as long as the underlying `ArrayBuffer` remains live and the heap is
  /// not mutably borrowed.
  pub fn array_buffer_data(&self, obj: GcObject) -> Result<&[u8], VmError> {
    let buf = self.get_array_buffer(obj)?;
    buf
      .data
      .as_deref()
      // Detachment is user-observable (e.g. ArrayBuffer transfer / structured clone), so accessing
      // the backing store must fail as a JS-catchable TypeError rather than an engine-bug error.
      .ok_or(VmError::TypeError("ArrayBuffer is detached"))
  }

  /// Detaches an `ArrayBuffer` by dropping its backing store.
  ///
  /// A detached `ArrayBuffer` has `byteLength === 0` and attempts to access its underlying bytes
  /// should throw (see [`Heap::array_buffer_data`]).
  ///
  /// This is intended for host embeddings implementing "transfer"/structured clone semantics.
  pub fn detach_array_buffer(&mut self, obj: GcObject) -> Result<(), VmError> {
    let buf = self.get_array_buffer_mut(obj)?;
    let Some(data) = buf.data.take() else {
      // Already detached.
      return Ok(());
    };
    let len = data.len();
    drop(data);
    self.sub_external_bytes(len);
    Ok(())
  }

  /// Detaches an `ArrayBuffer` and returns its backing store.
  ///
  /// This models transfer-list semantics: after detachment, the `ArrayBuffer` remains live but has
  /// no backing store (`[[ArrayBufferData]]` is `null`), and `byteLength` becomes `0`.
  pub fn detach_array_buffer_take_data(
    &mut self,
    obj: GcObject,
  ) -> Result<Option<Box<[u8]>>, VmError> {
    let buf = self.get_array_buffer_mut(obj)?;
    let data = buf.data.take();
    if let Some(data) = &data {
      self.sub_external_bytes(data.len());
    }
    Ok(data)
  }

  pub(crate) fn array_buffer_write(&mut self, obj: GcObject, offset: usize, bytes: &[u8]) -> Result<(), VmError> {
    if bytes.is_empty() {
      return Ok(());
    }
    // Validate and bounds-check before mutably borrowing the backing store.
    let buf_len = self.get_array_buffer(obj)?.byte_length();
    let end = offset
      .checked_add(bytes.len())
      .ok_or(VmError::OutOfMemory)?;
    if end > buf_len {
      return Err(VmError::TypeError("ArrayBuffer write out of bounds"));
    }

    let buf = self.get_array_buffer_mut(obj)?;
    let data = buf
      .data
      .as_deref_mut()
      .ok_or(VmError::TypeError("ArrayBuffer is detached"))?;
    data[offset..end].copy_from_slice(bytes);
    Ok(())
  }

  pub(crate) fn typed_array_kind(&self, obj: GcObject) -> Result<TypedArrayKind, VmError> {
    Ok(self.get_typed_array(obj)?.kind)
  }

  pub(crate) fn typed_array_length(&self, obj: GcObject) -> Result<usize, VmError> {
    let view = self.get_typed_array(obj)?;
    if self.get_array_buffer(view.viewed_array_buffer)?.data.is_none() {
      return Ok(0);
    }
    Ok(view.length)
  }

  pub(crate) fn typed_array_byte_length(&self, obj: GcObject) -> Result<usize, VmError> {
    let view = self.get_typed_array(obj)?;
    if self.get_array_buffer(view.viewed_array_buffer)?.data.is_none() {
      return Ok(0);
    }
    view.byte_length()
  }

  pub(crate) fn typed_array_byte_offset(&self, obj: GcObject) -> Result<usize, VmError> {
    let view = self.get_typed_array(obj)?;
    if self.get_array_buffer(view.viewed_array_buffer)?.data.is_none() {
      return Ok(0);
    }
    Ok(view.byte_offset)
  }

  pub(crate) fn typed_array_buffer(&self, obj: GcObject) -> Result<GcObject, VmError> {
    Ok(self.get_typed_array(obj)?.viewed_array_buffer)
  }

  pub(crate) fn typed_array_get_element_value(
    &self,
    obj: GcObject,
    index: usize,
  ) -> Result<Option<Value>, VmError> {
    let view = self.get_typed_array(obj)?;
    if self.get_array_buffer(view.viewed_array_buffer)?.data.is_none() {
      return Ok(None);
    }
    if index >= view.length {
      return Ok(None);
    }
    Ok(Some(self.typed_array_get_value(view, index)?))
  }

  pub(crate) fn data_view_byte_length(&self, obj: GcObject) -> Result<usize, VmError> {
    Ok(self.get_data_view(obj)?.byte_length)
  }

  pub(crate) fn data_view_byte_offset(&self, obj: GcObject) -> Result<usize, VmError> {
    Ok(self.get_data_view(obj)?.byte_offset)
  }

  pub(crate) fn data_view_buffer(&self, obj: GcObject) -> Result<GcObject, VmError> {
    Ok(self.get_data_view(obj)?.viewed_array_buffer)
  }

  /// Returns a borrowed view of the bytes visible through a `Uint8Array` view.
  ///
  /// This is intended for host bindings that need to read `Uint8Array` contents without round-tripping
  /// through JS (e.g. `TextDecoder`).
  ///
  /// The returned slice is valid as long as the underlying `Uint8Array` and its backing `ArrayBuffer`
  /// remain live and the heap is not mutably borrowed.
  pub fn uint8_array_data(&self, obj: GcObject) -> Result<&[u8], VmError> {
    let view = self.get_typed_array(obj)?;
    if view.kind != TypedArrayKind::Uint8 {
      return Err(VmError::invalid_handle());
    }
    let start = view.byte_offset;
    let end = start
      .checked_add(view.byte_length()?)
      .ok_or(VmError::InvariantViolation("Uint8Array byte offset overflow"))?;
    let buf = self.get_array_buffer(view.viewed_array_buffer)?;
    let data = buf
      .data
      .as_deref()
      .ok_or(VmError::TypeError("ArrayBuffer is detached"))?;
    data.get(start..end).ok_or(VmError::InvariantViolation(
      "Uint8Array view references out-of-bounds ArrayBuffer data",
    ))
  }

  /// Writes `bytes` into a `Uint8Array` view starting at element index `index`.
  ///
  /// This is intended for host bindings that need to efficiently populate typed arrays without
  /// round-tripping through JS property sets.
  ///
  /// Returns the number of bytes written, which is `min(bytes.len(), view.length - index)`. If
  /// `index` is out of bounds, this returns `Ok(0)` (mirroring typed array out-of-bounds write
  /// semantics).
  ///
  /// # Errors
  ///
  /// Returns an error if `obj` is not a live `Uint8Array` object or if the view's `byteOffset` +
  /// `length` does not fit within its backing `ArrayBuffer`.
  pub fn uint8_array_write(&mut self, obj: GcObject, index: usize, bytes: &[u8]) -> Result<usize, VmError> {
    // Extract view fields without holding a mutable borrow across ArrayBuffer access.
    let (buffer, byte_offset, length) = {
      let view = self.get_typed_array(obj)?;
      if view.kind != TypedArrayKind::Uint8 {
        return Err(VmError::invalid_handle());
      }
      (view.viewed_array_buffer, view.byte_offset, view.length)
    };

    if index >= length || bytes.is_empty() {
      return Ok(0);
    }
    let max_write = bytes.len().min(length - index);

    let abs_start = byte_offset
      .checked_add(index)
      .ok_or(VmError::InvariantViolation("Uint8Array byte offset overflow"))?;
    let abs_end = abs_start
      .checked_add(max_write)
      .ok_or(VmError::InvariantViolation("Uint8Array byte offset overflow"))?;

    let buf_len = self
      .get_array_buffer(buffer)?
      .data
      .as_deref()
      .ok_or(VmError::TypeError("ArrayBuffer is detached"))?
      .len();
    if abs_end > buf_len {
      return Err(VmError::InvariantViolation(
        "Uint8Array view references out-of-bounds ArrayBuffer data",
      ));
    }

    let buf = self.get_array_buffer_mut(buffer)?;
    let data = buf
      .data
      .as_deref_mut()
      .ok_or(VmError::TypeError("ArrayBuffer is detached"))?;
    data[abs_start..abs_end].copy_from_slice(&bytes[..max_write]);
    Ok(max_write)
  }

  /// Alias for [`Heap::is_promise_object`].
  pub fn is_promise(&self, obj: GcObject) -> bool {
    self.is_promise_object(obj)
  }

  /// Sets a host-only internal slot payload on an object.
  ///
  /// This is intended for platform bindings (e.g. DOM wrappers) to attach small metadata
  /// such as a `NodeId` or wrapper kind to a `GcObject` without exposing it to JS.
  pub fn object_set_host_slots(&mut self, obj: GcObject, slots: HostSlots) -> Result<(), VmError> {
    let idx = self
      .validate(obj.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    match self.slots[idx].value {
      Some(
        HeapObject::Object(_)
        | HeapObject::ArrayBuffer(_)
        | HeapObject::TypedArray(_)
        | HeapObject::DataView(_)
        | HeapObject::Function(_)
        | HeapObject::Proxy(_)
        | HeapObject::Promise(_)
        | HeapObject::WeakSet(_)
        | HeapObject::Generator(_),
      ) => {
        self.slots[idx].host_slots = Some(slots);
        Ok(())
      }
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Gets a host-only internal slot payload from an object, if set.
  pub fn object_host_slots(&self, obj: GcObject) -> Result<Option<HostSlots>, VmError> {
    let idx = self
      .validate(obj.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    match self.slots[idx].value {
      Some(
        HeapObject::Object(_)
        | HeapObject::ArrayBuffer(_)
        | HeapObject::TypedArray(_)
        | HeapObject::DataView(_)
        | HeapObject::Function(_)
        | HeapObject::Proxy(_)
        | HeapObject::Promise(_)
        | HeapObject::WeakSet(_)
        | HeapObject::Generator(_),
      ) => Ok(self.slots[idx].host_slots),
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Clears any host-only internal slot payload from an object.
  pub fn object_clear_host_slots(&mut self, obj: GcObject) -> Result<(), VmError> {
    let idx = self
      .validate(obj.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    match self.slots[idx].value {
      Some(
        HeapObject::Object(_)
        | HeapObject::ArrayBuffer(_)
        | HeapObject::TypedArray(_)
        | HeapObject::DataView(_)
        | HeapObject::Function(_)
        | HeapObject::Proxy(_)
        | HeapObject::Promise(_)
        | HeapObject::WeakSet(_)
        | HeapObject::Generator(_),
      ) => {
        self.slots[idx].host_slots = None;
        Ok(())
      }
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Returns `true` if `s` currently points to a live string allocation.
  pub fn is_valid_string(&self, s: GcString) -> bool {
    matches!(self.get_heap_object(s.0), Ok(HeapObject::String(_)))
  }

  /// Returns `true` if `sym` currently points to a live symbol allocation.
  pub fn is_valid_symbol(&self, sym: GcSymbol) -> bool {
    matches!(self.get_heap_object(sym.0), Ok(HeapObject::Symbol(_)))
  }

  pub fn is_valid_env(&self, env: GcEnv) -> bool {
    matches!(self.get_heap_object(env.0), Ok(HeapObject::Env(_)))
  }

  /// Returns `true` if `value` is callable (i.e. has an ECMAScript `[[Call]]` internal method).
  pub fn is_callable(&self, value: Value) -> Result<bool, VmError> {
    let Value::Object(mut obj) = value else {
      return Ok(false);
    };

    // Follow Proxy chains iteratively to avoid recursion.
    for _ in 0..MAX_PROTOTYPE_CHAIN {
      match self.get_heap_object(obj.0)? {
        HeapObject::Function(_) => return Ok(true),
        HeapObject::Proxy(p) => {
          let (Some(target), Some(_handler)) = (p.target, p.handler) else {
            // Revoked proxy.
            return Ok(false);
          };
          obj = target;
        }
        _ => return Ok(false),
      }
    }

    // If we hit the hard traversal limit, treat it as non-callable rather than surfacing an
    // internal error. (In particular, `typeof` must not throw.)
    Ok(false)
  }

  /// Returns `true` if `value` is a constructor (i.e. has an ECMAScript `[[Construct]]` internal
  /// method).
  pub fn is_constructor(&self, value: Value) -> Result<bool, VmError> {
    let Value::Object(mut obj) = value else {
      return Ok(false);
    };

    // Follow Proxy chains iteratively to avoid recursion.
    for _ in 0..MAX_PROTOTYPE_CHAIN {
      match self.get_heap_object(obj.0)? {
        HeapObject::Function(f) => return Ok(f.construct.is_some()),
        HeapObject::Proxy(p) => {
          let (Some(target), Some(_handler)) = (p.target, p.handler) else {
            // Revoked proxy.
            return Ok(false);
          };
          obj = target;
        }
        _ => return Ok(false),
      }
    }

    // Avoid surfacing internal errors to user code; see `Heap::is_callable`.
    Ok(false)
  }

  /// Calls `callee` with the provided `this` value and arguments.
  ///
  /// This is a convenience wrapper around [`Vm::call`] for host embeddings: it creates a temporary
  /// stack-rooting [`Scope`] to keep `callee`, `this`, and `args` alive for the duration of the
  /// call.
  ///
  /// Invalid handles are rejected up-front with [`VmError::InvalidHandle`] (rather than tripping
  /// debug assertions when rooting).
  pub fn call(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    callee: Value,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    if !self.debug_value_is_valid_or_primitive(callee) {
      return Err(VmError::invalid_handle());
    }
    if !self.debug_value_is_valid_or_primitive(this) {
      return Err(VmError::invalid_handle());
    }
    for &arg in args {
      if !self.debug_value_is_valid_or_primitive(arg) {
        return Err(VmError::invalid_handle());
      }
    }

    let mut scope = self.scope();
    vm.call(host, &mut scope, callee, this, args)
  }

  /// Gets the string contents for `s`.
  pub fn get_string(&self, s: GcString) -> Result<&JsString, VmError> {
    match self.get_heap_object(s.0)? {
      HeapObject::String(s) => Ok(s),
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Gets the (optional) description for `sym`.
  pub fn get_symbol_description(&self, sym: GcSymbol) -> Result<Option<GcString>, VmError> {
    match self.get_heap_object(sym.0)? {
      HeapObject::Symbol(sym) => Ok(sym.description()),
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Convenience: returns the (optional) description for `sym`, treating invalid handles as
  /// "no description".
  pub fn symbol_description(&self, sym: GcSymbol) -> Option<GcString> {
    self.get_symbol_description(sym).ok().flatten()
  }

  /// Returns the debug/introspection id for `sym`.
  pub fn get_symbol_id(&self, sym: GcSymbol) -> Result<u64, VmError> {
    match self.get_heap_object(sym.0)? {
      HeapObject::Symbol(sym) => Ok(sym.id()),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_object_base(&self, obj: GcObject) -> Result<&ObjectBase, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::Object(o) => Ok(&o.base),
      HeapObject::ArrayBuffer(b) => Ok(&b.base),
      HeapObject::TypedArray(a) => Ok(&a.base),
      HeapObject::DataView(v) => Ok(&v.base),
      HeapObject::Function(f) => Ok(&f.base),
      HeapObject::Promise(p) => Ok(&p.object.base),
      HeapObject::WeakSet(ws) => Ok(&ws.base),
      HeapObject::Generator(g) => Ok(&g.object.base),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_object_base_mut(&mut self, obj: GcObject) -> Result<&mut ObjectBase, VmError> {
    match self.get_heap_object_mut(obj.0)? {
      HeapObject::Object(o) => Ok(&mut o.base),
      HeapObject::ArrayBuffer(b) => Ok(&mut b.base),
      HeapObject::TypedArray(a) => Ok(&mut a.base),
      HeapObject::DataView(v) => Ok(&mut v.base),
      HeapObject::Function(f) => Ok(&mut f.base),
      HeapObject::Promise(p) => Ok(&mut p.object.base),
      HeapObject::WeakSet(ws) => Ok(&mut ws.base),
      HeapObject::Generator(g) => Ok(&mut g.object.base),
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Returns `true` if `key` is present in `set`.
  ///
  /// Dead/stale keys are treated as absent.
  pub fn weak_set_has(&self, set: GcObject, key: GcObject) -> Result<bool, VmError> {
    if !self.is_valid_object(key) {
      return Ok(false);
    }

    let HeapObject::WeakSet(ws) = self.get_heap_object(set.0)? else {
      return Err(VmError::invalid_handle());
    };

    for entry in ws.entries.iter() {
      let Some(obj) = entry.upgrade(self) else {
        continue;
      };
      if obj == key {
        return Ok(true);
      }
    }
    Ok(false)
  }

  /// Inserts `key` into `set`.
  ///
  /// Dead/stale keys are ignored.
  pub fn weak_set_add(&mut self, set: GcObject, key: GcObject) -> Result<(), VmError> {
    if !self.is_valid_object(set) {
      return Err(VmError::invalid_handle());
    }
    if !self.is_valid_object(key) {
      return Ok(());
    }

    // Root inputs across any potential GC while growing the entry vector.
    let mut scope = self.scope();
    scope.push_roots(&[Value::Object(set), Value::Object(key)])?;
    scope.heap.weak_set_add_rooted(set, key)
  }

  fn weak_set_add_rooted(&mut self, set: GcObject, key: GcObject) -> Result<(), VmError> {
    let slot_idx = self
      .validate(set.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let key_weak = WeakGcObject::from(key);

    let (entry_len, entry_cap, property_count, old_bytes) = {
      let slot = &self.slots[slot_idx];
      let Some(HeapObject::WeakSet(ws)) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };

      // WeakSet operations must treat dead keys as absent. Entries are expected to be GC-pruned,
      // but this check prevents stale keys from being observed even if pruning falls behind.
      for entry in ws.entries.iter() {
        if *entry == key_weak && entry.upgrade(self).is_some() {
          return Ok(());
        }
      }

      (
        ws.entries.len(),
        ws.entries.capacity(),
        ws.base.properties.len(),
        slot.bytes,
      )
    };

    let required_len = entry_len.checked_add(1).ok_or(VmError::OutOfMemory)?;
    let desired_capacity = grown_capacity(entry_cap, required_len);
    if desired_capacity == usize::MAX {
      return Err(VmError::OutOfMemory);
    }

    let expected_new_bytes =
      JsWeakSet::heap_size_bytes_for_counts(property_count, desired_capacity);
    let grow_by = expected_new_bytes.saturating_sub(old_bytes);
    if grow_by != 0 {
      self.ensure_can_allocate(grow_by)?;
      let Some(HeapObject::WeakSet(ws)) = self.slots[slot_idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      reserve_vec_to_len::<WeakGcObject>(&mut ws.entries, required_len)?;
    }

    let Some(HeapObject::WeakSet(ws)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    ws.entries.push(key_weak);
    let new_bytes = ws.heap_size_bytes();
    self.update_slot_bytes(slot_idx, new_bytes);

    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
    Ok(())
  }

  /// Removes `key` from `set`, returning whether it was present.
  ///
  /// Dead/stale keys are treated as absent.
  pub fn weak_set_delete(&mut self, set: GcObject, key: GcObject) -> Result<bool, VmError> {
    if !self.is_valid_object(key) {
      return Ok(false);
    }

    let slot_idx = self
      .validate(set.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let key_weak = WeakGcObject::from(key);
    let mut removed = false;

    // No allocation: `retain` is in-place.
    let mut entries = {
      let Some(HeapObject::WeakSet(ws)) = self.slots[slot_idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      mem::take(&mut ws.entries)
    };

    entries.retain(|entry| {
      let Some(obj) = entry.upgrade(&*self) else {
        return false;
      };
      if *entry == key_weak && obj == key {
        removed = true;
        return false;
      }
      true
    });

    let Some(HeapObject::WeakSet(ws)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    ws.entries = entries;

    Ok(removed)
  }

  /// Returns the number of entries currently stored in `set`.
  ///
  /// Note: this counts *weak* entries and is intended for engine tests/introspection.
  pub fn weak_set_entry_count(&self, set: GcObject) -> Result<usize, VmError> {
    match self.get_heap_object(set.0)? {
      HeapObject::WeakSet(ws) => Ok(ws.entries.len()),
      _ => Err(VmError::invalid_handle()),
    }
  }
  fn get_env(&self, env: GcEnv) -> Result<&EnvRecord, VmError> {
    match self.get_heap_object(env.0)? {
      HeapObject::Env(e) => Ok(e),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_env_mut(&mut self, env: GcEnv) -> Result<&mut EnvRecord, VmError> {
    match self.get_heap_object_mut(env.0)? {
      HeapObject::Env(e) => Ok(e),
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Gets the environment record for `env`.
  ///
  /// Returns [`VmError::InvalidHandle`] if `env` is stale or points to a non-environment heap
  /// object.
  pub(crate) fn get_env_record(&self, env: GcEnv) -> Result<&EnvRecord, VmError> {
    self.get_env(env)
  }

  /// Mutably gets the environment record for `env`.
  ///
  /// Returns [`VmError::InvalidHandle`] if `env` is stale or points to a non-environment heap
  /// object.
  #[allow(dead_code)]
  pub(crate) fn get_env_record_mut(&mut self, env: GcEnv) -> Result<&mut EnvRecord, VmError> {
    self.get_env_mut(env)
  }

  fn get_declarative_env(&self, env: GcEnv) -> Result<&DeclarativeEnvRecord, VmError> {
    match self.get_env(env)? {
      EnvRecord::Declarative(env) => Ok(env),
      EnvRecord::Object(_) => Err(VmError::Unimplemented("object environment record")),
    }
  }

  fn get_declarative_env_mut(&mut self, env: GcEnv) -> Result<&mut DeclarativeEnvRecord, VmError> {
    match self.get_env_mut(env)? {
      EnvRecord::Declarative(env) => Ok(env),
      EnvRecord::Object(_) => Err(VmError::Unimplemented("object environment record")),
    }
  }
  /// Gets an object's `[[Prototype]]`.
  pub fn object_prototype(&self, obj: GcObject) -> Result<Option<GcObject>, VmError> {
    Ok(self.get_object_base(obj)?.prototype)
  }

  /// Sets an object's `[[Prototype]]`.
  pub fn object_set_prototype(
    &mut self,
    obj: GcObject,
    prototype: Option<GcObject>,
  ) -> Result<(), VmError> {
    // Validate `obj` early so we don't silently accept stale handles.
    let _ = self.get_object_base(obj)?;

    // Direct self-cycle.
    if prototype == Some(obj) {
      return Err(VmError::PrototypeCycle);
    }

    // Reject indirect cycles by walking `prototype`'s chain and checking whether it contains `obj`.
    //
    // Also guard against hostile chains (very deep or cyclic) even if an invariant was violated.
    let mut current = prototype;
    let mut steps = 0usize;
    let mut visited: HashSet<GcObject> = HashSet::new();
    while let Some(p) = current {
      if steps >= MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;

      if !visited.insert(p) {
        return Err(VmError::PrototypeCycle);
      }
      if p == obj {
        return Err(VmError::PrototypeCycle);
      }

      current = self.object_prototype(p)?;
    }

    self.get_object_base_mut(obj)?.prototype = prototype;
    Ok(())
  }

  /// Forcefully sets an object's `[[Prototype]]` without cycle checks.
  ///
  /// # Safety
  ///
  /// This can violate VM invariants (create prototype cycles, etc). Intended for low-level host
  /// embeddings and tests.
  pub unsafe fn object_set_prototype_unchecked(
    &mut self,
    obj: GcObject,
    prototype: Option<GcObject>,
  ) -> Result<(), VmError> {
    self.get_object_base_mut(obj)?.prototype = prototype;
    Ok(())
  }

  pub(crate) fn object_is_extensible(&self, obj: GcObject) -> Result<bool, VmError> {
    Ok(self.get_object_base(obj)?.extensible)
  }

  pub(crate) fn object_set_extensible(
    &mut self,
    obj: GcObject,
    extensible: bool,
  ) -> Result<(), VmError> {
    self.get_object_base_mut(obj)?.extensible = extensible;
    Ok(())
  }

  /// Gets an own property descriptor from an object.
  pub fn object_get_own_property(
    &self,
    obj: GcObject,
    key: &PropertyKey,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    self.object_get_own_property_with_tick(obj, key, || Ok(()))
  }

  pub fn object_get_own_property_with_tick(
    &self,
    obj: GcObject,
    key: &PropertyKey,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    // Integer-indexed exotic behaviour for typed arrays:
    // - numeric index properties are not stored in the object's property table
    // - they are materialized on demand from the view's `[[ViewedArrayBuffer]]`.
    if let PropertyKey::String(s) = key {
      if let Some(index) = self.string_to_array_index(*s) {
        if let HeapObject::TypedArray(view) = self.get_heap_object(obj.0)? {
          // If the backing buffer is detached, integer-indexed properties are treated as absent.
          if self
            .get_array_buffer(view.viewed_array_buffer)?
            .data
            .is_none()
          {
            return Ok(None);
          }
          let idx = index as usize;
          if idx < view.length {
            return Ok(Some(PropertyDescriptor {
              enumerable: true,
              configurable: false,
              kind: PropertyKind::Data {
                value: self.typed_array_get_value(view, idx)?,
                writable: true,
              },
            }));
          }
          // Out-of-bounds index: no own property.
          return Ok(None);
        }
      }
    }
 
    let obj = self.get_object_base(obj)?;
    // Property lookups can scan very large property tables (especially for missing keys). Budget
    // the scan so deadline/interrupt checks can be observed inside a single `Get(O, P)` operation.
    //
    // Note: avoid ticking on the first iteration (`i == 0`) so small property tables do not
    // effectively double-charge fuel (property access expressions are already charged via the
    // enclosing expression/statement tick).
    const TICK_EVERY: usize = 1024;
    for (i, prop) in obj.properties.iter().enumerate() {
      if i != 0 && i % TICK_EVERY == 0 {
        tick()?;
      }
      if self.property_key_eq(&prop.key, key) {
        return Ok(Some(prop.desc));
      }
    }
    Ok(None)
  }

  pub(crate) fn object_delete_own_property(
    &mut self,
    obj: GcObject,
    key: &PropertyKey,
  ) -> Result<bool, VmError> {
    let slot_idx = self
      .validate(obj.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    // Two-phase borrow to avoid holding `&mut HeapObject` while calling back into `&self` for
    // string comparisons in `property_key_eq`.
    #[derive(Clone, Copy)]
    enum TargetKind {
      OrdinaryObject,
      ArrayBuffer,
      TypedArray,
      DataView,
      Function {
        bound_args_len: usize,
        native_slots_len: usize,
      },
      Promise {
        fulfill_reaction_count: usize,
        reject_reaction_count: usize,
      },
      WeakSet {
        entry_capacity: usize,
      },
      Generator {
        args_len: usize,
        continuation_len: usize,
      },
    }

    let (idx, target_kind, property_count) = {
      let slot = &self.slots[slot_idx];
      let Some(obj) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };
      match obj {
        HeapObject::Object(obj) => (
          obj
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::OrdinaryObject,
          obj.base.properties.len(),
        ),
        HeapObject::ArrayBuffer(buf) => (
          buf
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::ArrayBuffer,
          buf.base.properties.len(),
        ),
        HeapObject::TypedArray(arr) => (
          arr
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::TypedArray,
          arr.base.properties.len(),
        ),
        HeapObject::DataView(view) => (
          view
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::DataView,
          view.base.properties.len(),
        ),
        HeapObject::Function(func) => (
          func
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::Function {
            bound_args_len: func.bound_args.as_ref().map(|args| args.len()).unwrap_or(0),
            native_slots_len: func.native_slots.as_ref().map(|slots| slots.len()).unwrap_or(0),
          },
          func.base.properties.len(),
        ),
        HeapObject::Promise(p) => (
          p.object
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::Promise {
            fulfill_reaction_count: p.fulfill_reactions.as_deref().map(|r| r.len()).unwrap_or(0),
            reject_reaction_count: p.reject_reactions.as_deref().map(|r| r.len()).unwrap_or(0),
          },
          p.object.base.properties.len(),
        ),
        HeapObject::WeakSet(ws) => (
          ws
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::WeakSet {
            entry_capacity: ws.entries.capacity(),
          },
          ws.base.properties.len(),
        ),
        HeapObject::Generator(g) => (
          g.object
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::Generator {
            args_len: g.args.as_deref().map(|args| args.len()).unwrap_or(0),
            continuation_len: g.continuation.as_deref().map(|c| c.len()).unwrap_or(0),
          },
          g.object.base.properties.len(),
        ),
        _ => return Err(VmError::invalid_handle()),
      }
    };

    let Some(idx) = idx else {
      return Ok(false);
    };

    let new_property_count = property_count.saturating_sub(1);
    let new_bytes = match target_kind {
      TargetKind::OrdinaryObject => JsObject::heap_size_bytes_for_property_count(new_property_count),
      TargetKind::ArrayBuffer => JsArrayBuffer::heap_size_bytes_for_property_count(new_property_count),
      TargetKind::TypedArray => JsTypedArray::heap_size_bytes_for_property_count(new_property_count),
      TargetKind::DataView => JsDataView::heap_size_bytes_for_property_count(new_property_count),
      TargetKind::Function {
        bound_args_len,
        native_slots_len,
      } => JsFunction::heap_size_bytes_for_counts(bound_args_len, native_slots_len, new_property_count),
      TargetKind::Promise {
        fulfill_reaction_count,
        reject_reaction_count,
      } => JsPromise::heap_size_bytes_for_counts(
        new_property_count,
        fulfill_reaction_count,
        reject_reaction_count,
      ),
      TargetKind::WeakSet { entry_capacity } => {
        JsWeakSet::heap_size_bytes_for_counts(new_property_count, entry_capacity)
      }
      TargetKind::Generator {
        args_len,
        continuation_len,
      } => JsGenerator::heap_size_bytes_for_counts(new_property_count, args_len, continuation_len),
    };

    // Allocate the new property table fallibly so hostile inputs cannot abort the host process
    // on allocator OOM (even though this is a net-shrinking operation).
    let mut buf: Vec<PropertyEntry> = Vec::new();
    buf
      .try_reserve_exact(new_property_count)
      .map_err(|_| VmError::OutOfMemory)?;

    {
      let slot = &self.slots[slot_idx];
      match slot.value.as_ref() {
        Some(HeapObject::Object(obj)) => {
          buf.extend_from_slice(&obj.base.properties[..idx]);
          buf.extend_from_slice(&obj.base.properties[idx + 1..]);
        }
        Some(HeapObject::ArrayBuffer(obj)) => {
          buf.extend_from_slice(&obj.base.properties[..idx]);
          buf.extend_from_slice(&obj.base.properties[idx + 1..]);
        }
        Some(HeapObject::TypedArray(obj)) => {
          buf.extend_from_slice(&obj.base.properties[..idx]);
          buf.extend_from_slice(&obj.base.properties[idx + 1..]);
        }
        Some(HeapObject::DataView(obj)) => {
          buf.extend_from_slice(&obj.base.properties[..idx]);
          buf.extend_from_slice(&obj.base.properties[idx + 1..]);
        }
        Some(HeapObject::Function(func)) => {
          buf.extend_from_slice(&func.base.properties[..idx]);
          buf.extend_from_slice(&func.base.properties[idx + 1..]);
        }
        Some(HeapObject::Promise(p)) => {
          buf.extend_from_slice(&p.object.base.properties[..idx]);
          buf.extend_from_slice(&p.object.base.properties[idx + 1..]);
        }
        Some(HeapObject::WeakSet(ws)) => {
          buf.extend_from_slice(&ws.base.properties[..idx]);
          buf.extend_from_slice(&ws.base.properties[idx + 1..]);
        }
        Some(HeapObject::Generator(g)) => {
          buf.extend_from_slice(&g.object.base.properties[..idx]);
          buf.extend_from_slice(&g.object.base.properties[idx + 1..]);
        }
        _ => return Err(VmError::invalid_handle()),
      }
    }

    let properties = buf.into_boxed_slice();
    let Some(obj) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    match obj {
      HeapObject::Object(obj) => obj.base.properties = properties,
      HeapObject::ArrayBuffer(obj) => obj.base.properties = properties,
      HeapObject::TypedArray(obj) => obj.base.properties = properties,
      HeapObject::DataView(obj) => obj.base.properties = properties,
      HeapObject::Function(func) => func.base.properties = properties,
      HeapObject::Promise(p) => p.object.base.properties = properties,
      HeapObject::WeakSet(ws) => ws.base.properties = properties,
      HeapObject::Generator(g) => g.object.base.properties = properties,
      _ => return Err(VmError::invalid_handle()),
    }

    // This is a net-shrinking operation, so no `ensure_can_allocate` call is needed.
    self.update_slot_bytes(slot_idx, new_bytes);

    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();

    Ok(true)
  }

  pub(crate) fn property_key_is_length(&self, key: &PropertyKey) -> bool {
    const LENGTH_UNITS: [u16; 6] = [108, 101, 110, 103, 116, 104]; // "length"
    let PropertyKey::String(s) = key else {
      return false;
    };
    let Ok(js) = self.get_string(*s) else {
      return false;
    };
    js.as_code_units() == LENGTH_UNITS
  }

  /// Returns whether `obj` is an Array exotic object.
  ///
  /// This is the engine-side equivalent of `Array.isArray` (without consulting prototypes).
  pub fn object_is_array(&self, obj: GcObject) -> Result<bool, VmError> {
    Ok(self.get_object_base(obj)?.array_length().is_some())
  }

  pub(crate) fn array_length(&self, obj: GcObject) -> Result<u32, VmError> {
    self
      .get_object_base(obj)?
      .array_length()
      .ok_or(VmError::InvariantViolation("expected array object"))
  }

  pub(crate) fn array_length_key(&self, obj: GcObject) -> Result<PropertyKey, VmError> {
    let base = self.get_object_base(obj)?;
    if base.array_length().is_none() {
      return Err(VmError::InvariantViolation("expected array object"));
    }
    let entry = base
      .properties
      .get(0)
      .ok_or(VmError::InvariantViolation("array missing length property"))?;
    if !self.property_key_is_length(&entry.key) {
      return Err(VmError::InvariantViolation(
        "array length property is not at index 0",
      ));
    }
    Ok(entry.key)
  }

  pub(crate) fn array_length_writable(&self, obj: GcObject) -> Result<bool, VmError> {
    let base = self.get_object_base(obj)?;
    if base.array_length().is_none() {
      return Err(VmError::InvariantViolation("expected array object"));
    }
    let entry = base
      .properties
      .get(0)
      .ok_or(VmError::InvariantViolation("array missing length property"))?;
    if !self.property_key_is_length(&entry.key) {
      return Err(VmError::InvariantViolation(
        "array length property is not at index 0",
      ));
    }
    match entry.desc.kind {
      PropertyKind::Data { writable, .. } => Ok(writable),
      PropertyKind::Accessor { .. } => Err(VmError::InvariantViolation(
        "array length property is not a data descriptor",
      )),
    }
  }

  pub(crate) fn array_set_length(&mut self, obj: GcObject, new_len: u32) -> Result<(), VmError> {
    let base = self.get_object_base_mut(obj)?;
    if base.array_length().is_none() {
      return Err(VmError::InvariantViolation("expected array object"));
    }
    base.set_array_length(new_len);
    Ok(())
  }

  pub(crate) fn array_set_length_writable(
    &mut self,
    obj: GcObject,
    writable: bool,
  ) -> Result<(), VmError> {
    // Validate with an immutable borrow first so we don't need to borrow `self` immutably while
    // holding a mutable borrow into the object.
    {
      let base = self.get_object_base(obj)?;
      if base.array_length().is_none() {
        return Err(VmError::InvariantViolation("expected array object"));
      }
      let entry = base
        .properties
        .get(0)
        .ok_or(VmError::InvariantViolation("array missing length property"))?;
      if !self.property_key_is_length(&entry.key) {
        return Err(VmError::InvariantViolation(
          "array length property is not at index 0",
        ));
      }
    }

    let base = self.get_object_base_mut(obj)?;
    let entry = base
      .properties
      .get_mut(0)
      .ok_or(VmError::InvariantViolation("array missing length property"))?;
    match &mut entry.desc.kind {
      PropertyKind::Data {
        writable: slot_writable,
        ..
      } => {
        *slot_writable = writable;
        Ok(())
      }
      PropertyKind::Accessor { .. } => Err(VmError::InvariantViolation(
        "array length property is not a data descriptor",
      )),
    }
  }

  /// Convenience: returns the value of an own data property, if present.
  pub fn object_get_own_data_property_value(
    &self,
    obj: GcObject,
    key: &PropertyKey,
  ) -> Result<Option<Value>, VmError> {
    let Some(desc) = self.object_get_own_property(obj, key)? else {
      return Ok(None);
    };
    match desc.kind {
      PropertyKind::Data { value, .. } => Ok(Some(value)),
      PropertyKind::Accessor { .. } => Err(VmError::PropertyNotData),
    }
  }

  /// Updates the `value` of an existing own data property.
  pub fn object_set_existing_data_property_value(
    &mut self,
    obj: GcObject,
    key: &PropertyKey,
    value: Value,
  ) -> Result<(), VmError> {
    let key_is_length = self.property_key_is_length(key);
    let key_array_index = match key {
      PropertyKey::String(s) => self.string_to_array_index(*s),
      PropertyKey::Symbol(_) => None,
    };

    // Two-phase borrow to avoid holding `&mut ObjectBase` while calling back into `&self` for
    // string comparisons in `property_key_eq`.
    let idx = {
      let obj = self.get_object_base(obj)?;
      obj
        .properties
        .iter()
        .position(|prop| self.property_key_eq(&prop.key, key))
    };

    let Some(idx) = idx else {
      return Err(VmError::PropertyNotFound);
    };

    let obj = self.get_object_base_mut(obj)?;

    // Array exotic `length` handling.
    if key_is_length {
      if obj.array_length().is_some() {
        let Value::Number(n) = value else {
          return Err(VmError::TypeError("Invalid array length"));
        };
        let new_len = array_length_from_f64(n).ok_or(VmError::TypeError("Invalid array length"))?;
        obj.set_array_length(new_len);
        return Ok(());
      }
    }

    let prop = obj
      .properties
      .get_mut(idx)
      .ok_or(VmError::PropertyNotFound)?;
    match &mut prop.desc.kind {
      PropertyKind::Data { value: slot, .. } => {
        *slot = value;
      }
      PropertyKind::Accessor { .. } => return Err(VmError::PropertyNotData),
    }

    // Array exotic index semantics: writing an array index extends `length`.
    if let Some(index) = key_array_index {
      if let Some(current_len) = obj.array_length() {
        let new_len = index.wrapping_add(1);
        if new_len > current_len {
          obj.set_array_length(new_len);
        }
      }
    }

    Ok(())
  }

  pub fn define_own_property(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
  ) -> Result<bool, VmError> {
    let mut scope = self.scope();
    scope.define_own_property(obj, key, desc)
  }

  pub fn define_own_property_or_throw(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
  ) -> Result<(), VmError> {
    let ok = self.define_own_property(obj, key, desc)?;
    if ok {
      Ok(())
    } else {
      Err(VmError::TypeError("DefineOwnProperty rejected"))
    }
  }

  /// ECMAScript `DefinePropertyOrThrow`.
  ///
  /// This is a convenience wrapper around [`Heap::define_own_property`]. If the definition is
  /// rejected (`false`), this returns a `TypeError`.
  pub fn define_property_or_throw(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptorPatch,
  ) -> Result<(), VmError> {
    self.define_own_property_or_throw(obj, key, desc)
  }

  pub fn create_data_property(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    value: Value,
  ) -> Result<bool, VmError> {
    let mut scope = self.scope();
    scope.create_data_property(obj, key, value)
  }

  pub fn create_data_property_or_throw(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    value: Value,
  ) -> Result<(), VmError> {
    let ok = self.create_data_property(obj, key, value)?;
    if ok {
      Ok(())
    } else {
      Err(VmError::TypeError("CreateDataProperty rejected"))
    }
  }

  /// ECMAScript `DeletePropertyOrThrow`.
  pub fn delete_property_or_throw(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<(), VmError> {
    let mut scope = self.scope();
    scope.delete_property_or_throw(obj, key)
  }

  /// Gets a property descriptor from `obj` or its prototype chain.
  pub fn get_property(
    &self,
    obj: GcObject,
    key: &PropertyKey,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    self.get_property_with_tick(obj, key, || Ok(()))
  }

  pub fn get_property_with_tick(
    &self,
    obj: GcObject,
    key: &PropertyKey,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    self.get_property_from_prototype_with_tick_impl(Some(obj), None, key, &mut tick)
  }

  pub(crate) fn get_property_from_prototype_with_tick(
    &self,
    obj: GcObject,
    key: &PropertyKey,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    self.get_property_from_prototype_with_tick_impl(
      self.object_prototype(obj)?,
      Some(obj),
      key,
      &mut tick,
    )
  }

  fn get_property_from_prototype_with_tick_impl(
    &self,
    mut current: Option<GcObject>,
    initial_visited: Option<GcObject>,
    key: &PropertyKey,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    let mut steps = 0usize;
    let mut visited: HashSet<GcObject> = HashSet::new();
    if let Some(obj) = initial_visited {
      if visited.try_reserve(1).is_err() {
        return Err(VmError::OutOfMemory);
      }
      visited.insert(obj);
    }
 
    while let Some(obj) = current {
      if steps >= MAX_PROTOTYPE_CHAIN {
        return Err(VmError::PrototypeChainTooDeep);
      }
      steps += 1;
 
      if visited.try_reserve(1).is_err() {
        return Err(VmError::OutOfMemory);
      }
      if !visited.insert(obj) {
        return Err(VmError::PrototypeCycle);
      }
 
      if let Some(desc) = self.object_get_own_property_with_tick(obj, key, &mut *tick)? {
        return Ok(Some(desc));
      }
 
      current = self.object_prototype(obj)?;
    }
 
    Ok(None)
  }

  /// Returns whether a property exists on `obj` or its prototype chain.
  pub fn has_property(&self, obj: GcObject, key: &PropertyKey) -> Result<bool, VmError> {
    Ok(self.get_property(obj, key)?.is_some())
  }

  /// Implements a minimal `[[Get]]` internal method for objects.
  ///
  /// This is currently limited to data properties (sufficient for WebIDL sequence/record
  /// conversions and early scaffolding). Accessor properties return
  /// [`VmError::Unimplemented`], except that an accessor with an `undefined` getter returns
  /// `undefined`.
  pub fn get(&self, obj: GcObject, key: &PropertyKey) -> Result<Value, VmError> {
    let Some(desc) = self.get_property(obj, key)? else {
      return Ok(Value::Undefined);
    };
    match desc.kind {
      PropertyKind::Data { value, .. } => Ok(value),
      PropertyKind::Accessor { get, .. } => {
        if matches!(get, Value::Undefined) {
          Ok(Value::Undefined)
        } else {
          Err(VmError::Unimplemented(
            "Heap::get accessor properties require a VM to call getters",
          ))
        }
      }
    }
  }

  /// ECMAScript `[[Get]]` for ordinary objects (full semantics, including accessors).
  ///
  /// This is a convenience wrapper around [`Scope::ordinary_get`]. It:
  /// - walks the prototype chain (bounded by [`MAX_PROTOTYPE_CHAIN`]),
  /// - returns data property values,
  /// - and invokes accessor getters using `vm.call` with `receiver` as `this`.
  ///
  /// ## ⚠️ Dummy `VmHost` context
  ///
  /// Accessor getters are invoked using a **dummy host context** (`()`). Host embeddings that need
  /// native handlers to observe real host state should prefer
  /// [`Heap::ordinary_get_with_host_and_hooks`].
  ///
  /// # Rooting
  ///
  /// The returned [`Value`] is **not automatically rooted**. If the caller will perform any
  /// additional allocations that could trigger GC, it must root the returned value itself (for
  /// example with [`Scope::push_root`]).
  pub fn ordinary_get(
    &mut self,
    vm: &mut Vm,
    obj: GcObject,
    key: PropertyKey,
    receiver: Value,
  ) -> Result<Value, VmError> {
    let mut scope = self.scope();
    scope.ordinary_get(vm, obj, key, receiver)
  }

  /// ECMAScript `[[Get]]` for ordinary objects (full semantics, including accessors), using an
  /// explicit embedder host context and host hook implementation.
  ///
  /// This is a convenience wrapper around [`Scope::ordinary_get_with_host_and_hooks`].
  pub fn ordinary_get_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    receiver: Value,
  ) -> Result<Value, VmError> {
    let mut scope = self.scope();
    scope.ordinary_get_with_host_and_hooks(vm, host, hooks, obj, key, receiver)
  }

  /// ECMAScript `[[Set]]` for ordinary objects (full semantics, including accessors).
  ///
  /// This is a convenience wrapper around [`Scope::ordinary_set`].
  ///
  /// ## ⚠️ Dummy `VmHost` context
  ///
  /// Accessor setters are invoked using a **dummy host context** (`()`). Host embeddings that need
  /// native handlers to observe real host state should prefer
  /// [`Heap::ordinary_set_with_host_and_hooks`].
  ///
  /// # Rooting
  ///
  /// This method does not automatically root `value`/`receiver` beyond the scope of the call.
  pub fn ordinary_set(
    &mut self,
    vm: &mut Vm,
    obj: GcObject,
    key: PropertyKey,
    value: Value,
    receiver: Value,
  ) -> Result<bool, VmError> {
    let mut scope = self.scope();
    scope.ordinary_set(vm, obj, key, value, receiver)
  }

  /// ECMAScript `[[Set]]` for ordinary objects (full semantics, including accessors), using an
  /// explicit embedder host context and host hook implementation.
  ///
  /// This is a convenience wrapper around [`Scope::ordinary_set_with_host_and_hooks`].
  pub fn ordinary_set_with_host_and_hooks(
    &mut self,
    vm: &mut Vm,
    host: &mut dyn VmHost,
    hooks: &mut dyn VmHostHooks,
    obj: GcObject,
    key: PropertyKey,
    value: Value,
    receiver: Value,
  ) -> Result<bool, VmError> {
    let mut scope = self.scope();
    scope.ordinary_set_with_host_and_hooks(vm, host, hooks, obj, key, value, receiver)
  }

  /// Implements the `OwnPropertyKeys` internal method (ECMA-262) for ordinary objects.
  ///
  /// This orders keys as:
  /// 1. array index keys, in ascending numeric order,
  /// 2. other string keys, in insertion order,
  /// 3. symbol keys, in insertion order.
  pub fn own_property_keys(&self, obj: GcObject) -> Result<Vec<PropertyKey>, VmError> {
    let props = &self.get_object_base(obj)?.properties;

    // This operation allocates temporary vectors sized proportionally to the number of properties.
    //
    // Use `try_reserve*` so hostile inputs cannot trigger a process abort via allocator OOM.
    let (mut array_count, mut string_count, mut symbol_count) = (0usize, 0usize, 0usize);
    for prop in props.iter() {
      match prop.key {
        PropertyKey::String(s) => {
          if self.string_to_array_index(s).is_some() {
            array_count = array_count.saturating_add(1);
          } else {
            string_count = string_count.saturating_add(1);
          }
        }
        PropertyKey::Symbol(_) => {
          symbol_count = symbol_count.saturating_add(1);
        }
      }
    }

    let mut array_keys: Vec<(u32, PropertyKey)> = Vec::new();
    array_keys
      .try_reserve_exact(array_count)
      .map_err(|_| VmError::OutOfMemory)?;
    let mut string_keys: Vec<PropertyKey> = Vec::new();
    string_keys
      .try_reserve_exact(string_count)
      .map_err(|_| VmError::OutOfMemory)?;
    let mut symbol_keys: Vec<PropertyKey> = Vec::new();
    symbol_keys
      .try_reserve_exact(symbol_count)
      .map_err(|_| VmError::OutOfMemory)?;

    for prop in props.iter() {
      match prop.key {
        PropertyKey::String(s) => {
          if let Some(idx) = self.string_to_array_index(s) {
            array_keys.push((idx, prop.key));
          } else {
            string_keys.push(prop.key);
          }
        }
        PropertyKey::Symbol(_) => symbol_keys.push(prop.key),
      }
    }

    array_keys.sort_by_key(|(idx, _)| *idx);

    let out_len = array_keys
      .len()
      .checked_add(string_keys.len())
      .and_then(|n| n.checked_add(symbol_keys.len()))
      .ok_or(VmError::OutOfMemory)?;
    let mut out: Vec<PropertyKey> = Vec::new();
    out
      .try_reserve_exact(out_len)
      .map_err(|_| VmError::OutOfMemory)?;
    out.extend(array_keys.into_iter().map(|(_, k)| k));
    out.extend(string_keys);
    out.extend(symbol_keys);
    Ok(out)
  }

  fn string_to_array_index(&self, s: GcString) -> Option<u32> {
    let js = self.get_string(s).ok()?;
    let units = js.as_code_units();
    if units.is_empty() {
      return None;
    }
    if units.len() > 1 && units[0] == b'0' as u16 {
      return None;
    }
    let mut n: u64 = 0;
    for &u in units {
      if !(b'0' as u16..=b'9' as u16).contains(&u) {
        return None;
      }
      n = n.checked_mul(10)?;
      n = n.checked_add((u - b'0' as u16) as u64)?;
      if n > u32::MAX as u64 {
        return None;
      }
    }
    // Array index is uint32 < 2^32 - 1.
    if n == u32::MAX as u64 {
      return None;
    }
    Some(n as u32)
  }

  fn get_array_buffer(&self, obj: GcObject) -> Result<&JsArrayBuffer, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::ArrayBuffer(b) => Ok(b),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_array_buffer_mut(&mut self, obj: GcObject) -> Result<&mut JsArrayBuffer, VmError> {
    match self.get_heap_object_mut(obj.0)? {
      HeapObject::ArrayBuffer(b) => Ok(b),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_typed_array(&self, obj: GcObject) -> Result<&JsTypedArray, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::TypedArray(a) => Ok(a),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_proxy(&self, obj: GcObject) -> Result<&JsProxy, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::Proxy(p) => Ok(p),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_proxy_mut(&mut self, obj: GcObject) -> Result<&mut JsProxy, VmError> {
    match self.get_heap_object_mut(obj.0)? {
      HeapObject::Proxy(p) => Ok(p),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_data_view(&self, obj: GcObject) -> Result<&JsDataView, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::DataView(v) => Ok(v),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn typed_array_get_value(&self, view: &JsTypedArray, index: usize) -> Result<Value, VmError> {
    debug_assert!(index < view.length);

    let bytes_per_element = view.kind.bytes_per_element();
    let rel = index
      .checked_mul(bytes_per_element)
      .ok_or(VmError::InvariantViolation("TypedArray index overflow"))?;
    let abs = view
      .byte_offset
      .checked_add(rel)
      .ok_or(VmError::InvariantViolation("TypedArray byte offset overflow"))?;

    let buf = self.get_array_buffer(view.viewed_array_buffer)?;
    let data = buf.data.as_deref().ok_or(VmError::InvariantViolation(
      "TypedArray view references missing ArrayBuffer backing store",
    ))?;

    let value = match view.kind {
      TypedArrayKind::Int8 => {
        let b = *data.get(abs).ok_or(VmError::InvariantViolation(
          "TypedArray view references out-of-bounds ArrayBuffer data",
        ))?;
        (b as i8) as f64
      }
      TypedArrayKind::Uint8 | TypedArrayKind::Uint8Clamped => {
        let b = *data.get(abs).ok_or(VmError::InvariantViolation(
          "TypedArray view references out-of-bounds ArrayBuffer data",
        ))?;
        b as f64
      }
      TypedArrayKind::Int16 => {
        let bytes: [u8; 2] = data
          .get(abs..abs + 2)
          .ok_or(VmError::InvariantViolation(
            "TypedArray view references out-of-bounds ArrayBuffer data",
          ))?
          .try_into()
          .map_err(|_| VmError::InvariantViolation("TypedArray slice length mismatch"))?;
        i16::from_le_bytes(bytes) as f64
      }
      TypedArrayKind::Uint16 => {
        let bytes: [u8; 2] = data
          .get(abs..abs + 2)
          .ok_or(VmError::InvariantViolation(
            "TypedArray view references out-of-bounds ArrayBuffer data",
          ))?
          .try_into()
          .map_err(|_| VmError::InvariantViolation("TypedArray slice length mismatch"))?;
        u16::from_le_bytes(bytes) as f64
      }
      TypedArrayKind::Int32 => {
        let bytes: [u8; 4] = data
          .get(abs..abs + 4)
          .ok_or(VmError::InvariantViolation(
            "TypedArray view references out-of-bounds ArrayBuffer data",
          ))?
          .try_into()
          .map_err(|_| VmError::InvariantViolation("TypedArray slice length mismatch"))?;
        i32::from_le_bytes(bytes) as f64
      }
      TypedArrayKind::Uint32 => {
        let bytes: [u8; 4] = data
          .get(abs..abs + 4)
          .ok_or(VmError::InvariantViolation(
            "TypedArray view references out-of-bounds ArrayBuffer data",
          ))?
          .try_into()
          .map_err(|_| VmError::InvariantViolation("TypedArray slice length mismatch"))?;
        u32::from_le_bytes(bytes) as f64
      }
      TypedArrayKind::Float32 => {
        let bytes: [u8; 4] = data
          .get(abs..abs + 4)
          .ok_or(VmError::InvariantViolation(
            "TypedArray view references out-of-bounds ArrayBuffer data",
          ))?
          .try_into()
          .map_err(|_| VmError::InvariantViolation("TypedArray slice length mismatch"))?;
        f32::from_bits(u32::from_le_bytes(bytes)) as f64
      }
      TypedArrayKind::Float64 => {
        let bytes: [u8; 8] = data
          .get(abs..abs + 8)
          .ok_or(VmError::InvariantViolation(
            "TypedArray view references out-of-bounds ArrayBuffer data",
          ))?
          .try_into()
          .map_err(|_| VmError::InvariantViolation("TypedArray slice length mismatch"))?;
        f64::from_bits(u64::from_le_bytes(bytes))
      }
    };

    Ok(Value::Number(value))
  }

  pub(crate) fn typed_array_set_element_value(
    &mut self,
    view_obj: GcObject,
    index: usize,
    value: Value,
  ) -> Result<bool, VmError> {
    // Extract view fields without holding a mutable borrow across ArrayBuffer access.
    let (buffer, byte_offset, length, kind) = {
      let view = self.get_typed_array(view_obj)?;
      (view.viewed_array_buffer, view.byte_offset, view.length, view.kind)
    };

    if index >= length {
      return Ok(false);
    }

    let bytes_per_element = kind.bytes_per_element();
    let rel = index
      .checked_mul(bytes_per_element)
      .ok_or(VmError::InvariantViolation("TypedArray index overflow"))?;
    let abs_start = byte_offset
      .checked_add(rel)
      .ok_or(VmError::InvariantViolation("TypedArray byte offset overflow"))?;
    let abs_end = abs_start
      .checked_add(bytes_per_element)
      .ok_or(VmError::InvariantViolation("TypedArray byte offset overflow"))?;

    // Detached buffers behave as though the typed array is out-of-bounds.
    let buf = self.get_array_buffer(buffer)?;
    let Some(data) = buf.data.as_deref() else {
      return Ok(false);
    };
    let buf_len = data.len();
    if abs_end > buf_len {
      return Err(VmError::InvariantViolation(
        "TypedArray view references out-of-bounds ArrayBuffer data",
      ));
    }

    // Convert via ToNumber; supports string/boolean/null/undefined, throws on Symbol.
    let n = self.to_number(value)?;

    fn to_uint8_clamp(n: f64) -> u8 {
      if n.is_nan() || n <= 0.0 {
        return 0;
      }
      if n >= 255.0 {
        return 255;
      }
      let f = n.floor();
      // Spec: `if f + 0.5 < n` round up.
      if f + 0.5 < n {
        return (f as u8).saturating_add(1);
      }
      // Spec: `if n < f + 0.5` round down.
      if n < f + 0.5 {
        return f as u8;
      }
      // Exactly halfway: ties to even.
      if (f as u64) % 2 == 1 {
        (f as u8).saturating_add(1)
      } else {
        f as u8
      }
    }

    let buf = self.get_array_buffer_mut(buffer)?;
    let Some(data) = buf.data.as_deref_mut() else {
      return Ok(false);
    };

    match kind {
      TypedArrayKind::Int8 => {
        let v = if !n.is_finite() { 0 } else { n.trunc().rem_euclid(256.0) as u8 as i8 };
        data[abs_start] = v as u8;
      }
      TypedArrayKind::Uint8 => {
        let v = if !n.is_finite() { 0 } else { n.trunc().rem_euclid(256.0) as u8 };
        data[abs_start] = v;
      }
      TypedArrayKind::Uint8Clamped => {
        data[abs_start] = to_uint8_clamp(n);
      }
      TypedArrayKind::Int16 => {
        let v = if !n.is_finite() { 0 } else { n.trunc().rem_euclid(65_536.0) as u16 as i16 };
        data[abs_start..abs_end].copy_from_slice(&v.to_le_bytes());
      }
      TypedArrayKind::Uint16 => {
        let v = if !n.is_finite() { 0 } else { n.trunc().rem_euclid(65_536.0) as u16 };
        data[abs_start..abs_end].copy_from_slice(&v.to_le_bytes());
      }
      TypedArrayKind::Int32 => {
        let v = if !n.is_finite() { 0 } else { n.trunc().rem_euclid(4_294_967_296.0) as u32 as i32 };
        data[abs_start..abs_end].copy_from_slice(&v.to_le_bytes());
      }
      TypedArrayKind::Uint32 => {
        let v = if !n.is_finite() { 0 } else { n.trunc().rem_euclid(4_294_967_296.0) as u32 };
        data[abs_start..abs_end].copy_from_slice(&v.to_le_bytes());
      }
      TypedArrayKind::Float32 => {
        let v = n as f32;
        data[abs_start..abs_end].copy_from_slice(&v.to_bits().to_le_bytes());
      }
      TypedArrayKind::Float64 => {
        data[abs_start..abs_end].copy_from_slice(&n.to_bits().to_le_bytes());
      }
    }

    Ok(true)
  }

  fn get_promise(&self, promise: GcObject) -> Result<&JsPromise, VmError> {
    match self.get_heap_object(promise.0)? {
      HeapObject::Promise(p) => Ok(p),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_promise_mut(&mut self, promise: GcObject) -> Result<&mut JsPromise, VmError> {
    match self.get_heap_object_mut(promise.0)? {
      HeapObject::Promise(p) => Ok(p),
      _ => Err(VmError::invalid_handle()),
    }
  }

  #[allow(dead_code)]
  pub(crate) fn get_generator(&self, obj: GcObject) -> Result<&JsGenerator, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::Generator(g) => Ok(g),
      _ => Err(VmError::invalid_handle()),
    }
  }

  #[allow(dead_code)]
  pub(crate) fn get_generator_mut(&mut self, obj: GcObject) -> Result<&mut JsGenerator, VmError> {
    match self.get_heap_object_mut(obj.0)? {
      HeapObject::Generator(g) => Ok(g),
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Returns `promise.[[PromiseState]]`.
  pub fn promise_state(&self, promise: GcObject) -> Result<PromiseState, VmError> {
    Ok(self.get_promise(promise)?.state)
  }

  /// Returns `promise.[[PromiseResult]]`.
  pub fn promise_result(&self, promise: GcObject) -> Result<Option<Value>, VmError> {
    Ok(self.get_promise(promise)?.result)
  }

  /// Returns `promise.[[PromiseIsHandled]]`.
  pub fn promise_is_handled(&self, promise: GcObject) -> Result<bool, VmError> {
    Ok(self.get_promise(promise)?.is_handled)
  }

  /// Sets `promise.[[PromiseIsHandled]]`.
  pub fn promise_set_is_handled(&mut self, promise: GcObject, handled: bool) -> Result<(), VmError> {
    self.get_promise_mut(promise)?.is_handled = handled;
    Ok(())
  }

  /// Returns the length of `promise.[[PromiseFulfillReactions]]`.
  pub fn promise_fulfill_reactions_len(&self, promise: GcObject) -> Result<usize, VmError> {
    Ok(
      self
        .get_promise(promise)?
        .fulfill_reactions
        .as_deref()
        .map(|r| r.len())
        .unwrap_or(0),
    )
  }

  /// Returns the length of `promise.[[PromiseRejectReactions]]`.
  pub fn promise_reject_reactions_len(&self, promise: GcObject) -> Result<usize, VmError> {
    Ok(
      self
        .get_promise(promise)?
        .reject_reactions
        .as_deref()
        .map(|r| r.len())
        .unwrap_or(0),
    )
  }

  /// Sets `promise.[[PromiseState]]` and `promise.[[PromiseResult]]`.
  ///
  /// If `state` is not [`PromiseState::Pending`], this is a settlement operation and the Promise's
  /// reaction lists are cleared as required by ECMA-262.
  pub fn promise_set_state_and_result(
    &mut self,
    promise: GcObject,
    state: PromiseState,
    result: Option<Value>,
  ) -> Result<(), VmError> {
    debug_assert!(
      result.map_or(true, |v| self.debug_value_is_valid_or_primitive(v)),
      "promise_set_state_and_result given invalid GC handle"
    );

    let idx = self
      .validate(promise.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let new_bytes = {
      let promise = match self.slots[idx].value.as_mut() {
        Some(HeapObject::Promise(p)) => p,
        _ => return Err(VmError::invalid_handle()),
      };

      promise.state = state;
      match state {
        PromiseState::Pending => {
          promise.result = None;
          if promise.fulfill_reactions.is_none() {
            promise.fulfill_reactions = Some(Box::default());
          }
          if promise.reject_reactions.is_none() {
            promise.reject_reactions = Some(Box::default());
          }
        }
        PromiseState::Fulfilled | PromiseState::Rejected => {
          promise.result = result;
          promise.fulfill_reactions = None;
          promise.reject_reactions = None;
        }
      }

      promise.heap_size_bytes()
    };

    self.update_slot_bytes(idx, new_bytes);
    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
    Ok(())
  }

  /// Settles a pending Promise and returns its previous reaction lists.
  ///
  /// This is a convenience for implementing `FulfillPromise`/`RejectPromise` in the JS Promise
  /// built-in: the reaction lists must be observed in order to enqueue reaction jobs, but they are
  /// cleared as part of the settlement step.
  pub(crate) fn promise_settle_and_take_reactions(
    &mut self,
    promise: GcObject,
    state: PromiseState,
    result: Value,
  ) -> Result<(Box<[PromiseReaction]>, Box<[PromiseReaction]>), VmError> {
    if state == PromiseState::Pending {
      return Err(VmError::Unimplemented(
        "promise_settle_and_take_reactions requires a non-pending state",
      ));
    }

    let idx = self
      .validate(promise.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let (fulfill_reactions, reject_reactions, new_bytes) = {
      let promise = match self.slots[idx].value.as_mut() {
        Some(HeapObject::Promise(p)) => p,
        _ => return Err(VmError::invalid_handle()),
      };

      // Per spec, subsequent resolves/rejects of an already-settled promise are no-ops.
      if promise.state != PromiseState::Pending {
        return Ok((Box::default(), Box::default()));
      }

      promise.state = state;
      promise.result = Some(result);

      let fulfill_reactions = mem::take(&mut promise.fulfill_reactions).unwrap_or_default();
      let reject_reactions = mem::take(&mut promise.reject_reactions).unwrap_or_default();

      (fulfill_reactions, reject_reactions, promise.heap_size_bytes())
    };

    self.update_slot_bytes(idx, new_bytes);
    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
    Ok((fulfill_reactions, reject_reactions))
  }

  /// Settles a pending Promise as fulfilled.
  pub fn promise_fulfill(&mut self, promise: GcObject, value: Value) -> Result<(), VmError> {
    self.promise_set_state_and_result(promise, PromiseState::Fulfilled, Some(value))
  }

  /// Settles a pending Promise as rejected.
  pub fn promise_reject(&mut self, promise: GcObject, reason: Value) -> Result<(), VmError> {
    self.promise_set_state_and_result(promise, PromiseState::Rejected, Some(reason))
  }

  /// Pushes a reaction onto `promise.[[PromiseFulfillReactions]]`.
  pub fn promise_push_fulfill_reaction(
    &mut self,
    promise: GcObject,
    reaction: PromiseReaction,
  ) -> Result<(), VmError> {
    let mut scope = self.scope();
    scope.promise_append_fulfill_reaction(promise, reaction)
  }

  /// Pushes a reaction onto `promise.[[PromiseRejectReactions]]`.
  pub fn promise_push_reject_reaction(
    &mut self,
    promise: GcObject,
    reaction: PromiseReaction,
  ) -> Result<(), VmError> {
    let mut scope = self.scope();
    scope.promise_append_reject_reaction(promise, reaction)
  }

  fn promise_append_reaction(
    &mut self,
    promise: GcObject,
    is_fulfill_list: bool,
    reaction: PromiseReaction,
  ) -> Result<(), VmError> {
    let idx = self
      .validate(promise.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let (property_count, fulfill_count, reject_count, old_bytes, state) = {
      let slot = &self.slots[idx];
      let Some(obj) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };
      let HeapObject::Promise(p) = obj else {
        return Err(VmError::invalid_handle());
      };
      (
        p.object.base.properties.len(),
        p.fulfill_reactions.as_deref().map(|r| r.len()).unwrap_or(0),
        p.reject_reactions.as_deref().map(|r| r.len()).unwrap_or(0),
        slot.bytes,
        p.state,
      )
    };

    if state != PromiseState::Pending {
      return Err(VmError::invalid_handle());
    }

    let new_fulfill_count = if is_fulfill_list {
      fulfill_count.checked_add(1).ok_or(VmError::OutOfMemory)?
    } else {
      fulfill_count
    };
    let new_reject_count = if is_fulfill_list {
      reject_count
    } else {
      reject_count.checked_add(1).ok_or(VmError::OutOfMemory)?
    };

    let new_bytes =
      JsPromise::heap_size_bytes_for_counts(property_count, new_fulfill_count, new_reject_count);

    // Before allocating, enforce heap limits based on the net growth of this object.
    let grow_by = new_bytes.saturating_sub(old_bytes);
    self.ensure_can_allocate(grow_by)?;

    // Allocate the new reaction list fallibly so hostile inputs cannot abort the host process on
    // allocator OOM.
    let new_list_len = if is_fulfill_list {
      new_fulfill_count
    } else {
      new_reject_count
    };

    let mut buf: Vec<PromiseReaction> = Vec::new();
    buf
      .try_reserve_exact(new_list_len)
      .map_err(|_| VmError::OutOfMemory)?;

    {
      let slot = &self.slots[idx];
      let Some(HeapObject::Promise(p)) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };
      if is_fulfill_list {
        let Some(list) = p.fulfill_reactions.as_deref() else {
          return Err(VmError::invalid_handle());
        };
        buf.extend_from_slice(list);
      } else {
        let Some(list) = p.reject_reactions.as_deref() else {
          return Err(VmError::invalid_handle());
        };
        buf.extend_from_slice(list);
      }
    }

    buf.push(reaction);
    let new_list = buf.into_boxed_slice();

    {
      let promise = match self.slots[idx].value.as_mut() {
        Some(HeapObject::Promise(p)) => p,
        _ => return Err(VmError::invalid_handle()),
      };
      if is_fulfill_list {
        promise.fulfill_reactions = Some(new_list);
      } else {
        promise.reject_reactions = Some(new_list);
      }
    }

    self.update_slot_bytes(idx, new_bytes);
    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
    Ok(())
  }

  /// Implements `Symbol.for`-like behaviour using a deterministic global registry.
  ///
  /// The registry is scanned by the GC, so registered symbols remain live even if they are not
  /// referenced from the stack or persistent roots.
  pub fn symbol_for(&mut self, key: GcString) -> Result<GcSymbol, VmError> {
    self.symbol_for_with_tick(key, || Ok::<(), VmError>(()))
  }

  /// Like [`Heap::symbol_for`], but budgets linear-time registry insertion work via `tick`.
  ///
  /// This is intended for the ECMAScript `Symbol.for` builtin, where hostile scripts can grow the
  /// registry arbitrarily until heap limits are reached.
  pub fn symbol_for_with_tick(
    &mut self,
    key: GcString,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<GcSymbol, VmError> {
    let key_contents = self.get_string(key)?;
    if let Some(sym) = self.symbol_registry_get(key_contents)? {
      return Ok(sym);
    }

    // Pre-flight the allocation: creating a new registry entry may require growing both the heap
    // (new symbol allocation) and the registry vector itself.
    let extra_roots = [Value::String(key)];
    self.ensure_can_allocate_with_extra_roots(|heap| {
      let mut bytes = 0usize;
      // New symbol allocation: payload size is 0 (symbols have no external payload bytes), but may
      // require growing the heap slot table.
      bytes = bytes.saturating_add(heap.additional_bytes_for_heap_alloc(0));
      // Registry entry vector growth.
      bytes = bytes.saturating_add(heap.additional_bytes_for_symbol_registry_insert(1));
      bytes
    }, &extra_roots, &[], &[], &[])?;

    // Root `key` and the newly-created symbol while inserting into the registry in case the
    // allocation paths trigger GC.
    let mut scope = self.scope();
    scope.push_root(Value::String(key))?;
    let sym = scope.new_symbol(Some(key))?;
    scope.push_root(Value::Symbol(sym))?;

    scope
      .heap_mut()
      .symbol_registry_insert_with_tick(key, sym, &mut tick)?;
    Ok(sym)
  }

  /// Implements `Symbol.keyFor` lookup using the global symbol registry.
  ///
  /// Returns the registry key for `sym` if it is present; otherwise returns `None`.
  pub fn symbol_key_for(&self, sym: GcSymbol) -> Result<Option<GcString>, VmError> {
    // Per spec, global registry symbols always have descriptions equal to their registry keys, so
    // we can avoid scanning the registry by:
    // 1. reading `sym.[[Description]]`
    // 2. looking up that key in the registry
    // 3. ensuring the registry entry points back to `sym`.
    let Some(desc) = self.get_symbol_description(sym)? else {
      return Ok(None);
    };
    let desc_contents = self.get_string(desc)?;
    match self.symbol_registry_binary_search(desc_contents)? {
      Ok(idx) => {
        let entry = self.symbol_registry[idx];
        if entry.sym == sym {
          Ok(Some(entry.key))
        } else {
          Ok(None)
        }
      }
      Err(_) => Ok(None),
    }
  }

  pub(crate) fn internal_string_data_symbol(&self) -> Option<GcSymbol> {
    const STRING_DATA_KEY: [u16; 25] = [
      118, 109, 45, 106, 115, 46, 105, 110, 116, 101, 114, 110, 97, 108, 46, 83, 116, 114, 105,
      110, 103, 68, 97, 116, 97,
    ];

    // The registry is kept sorted by key contents, so this can use binary search instead of an
    // O(n) scan (important: scripts can grow the global symbol registry arbitrarily).
    match self.symbol_registry_binary_search_code_units(&STRING_DATA_KEY) {
      Ok(Ok(idx)) => Some(self.symbol_registry[idx].sym),
      _ => None,
    }
  }

  pub(crate) fn internal_boolean_data_symbol(&self) -> Option<GcSymbol> {
    const BOOLEAN_DATA_KEY: [u16; 26] = [
      118, 109, 45, 106, 115, 46, 105, 110, 116, 101, 114, 110, 97, 108, 46, 66, 111, 111, 108,
      101, 97, 110, 68, 97, 116, 97,
    ];
    for entry in &self.symbol_registry {
      let Ok(js) = self.get_string(entry.key) else {
        continue;
      };
      if js.as_code_units() == BOOLEAN_DATA_KEY {
        return Some(entry.sym);
      }
    }
    None
  }

  pub(crate) fn internal_number_data_symbol(&self) -> Option<GcSymbol> {
    const NUMBER_DATA_KEY: [u16; 25] = [
      118, 109, 45, 106, 115, 46, 105, 110, 116, 101, 114, 110, 97, 108, 46, 78, 117, 109, 98,
      101, 114, 68, 97, 116, 97,
    ];
    for entry in &self.symbol_registry {
      let Ok(js) = self.get_string(entry.key) else {
        continue;
      };
      if js.as_code_units() == NUMBER_DATA_KEY {
        return Some(entry.sym);
      }
    }
    None
  }

  pub(crate) fn internal_symbol_data_symbol(&self) -> Option<GcSymbol> {
    const SYMBOL_DATA_KEY: [u16; 25] = [
      118, 109, 45, 106, 115, 46, 105, 110, 116, 101, 114, 110, 97, 108, 46, 83, 121, 109, 98,
      111, 108, 68, 97, 116, 97,
    ];
    for entry in &self.symbol_registry {
      let Ok(js) = self.get_string(entry.key) else {
        continue;
      };
      if js.as_code_units() == SYMBOL_DATA_KEY {
        return Some(entry.sym);
      }
    }
    None
  }

  pub(crate) fn internal_bigint_data_symbol(&self) -> Option<GcSymbol> {
    const BIGINT_DATA_KEY: [u16; 25] = [
      118, 109, 45, 106, 115, 46, 105, 110, 116, 101, 114, 110, 97, 108, 46, 66, 105, 103, 73,
      110, 116, 68, 97, 116, 97,
    ];
    for entry in &self.symbol_registry {
      let Ok(js) = self.get_string(entry.key) else {
        continue;
      };
      if js.as_code_units() == BIGINT_DATA_KEY {
        return Some(entry.sym);
      }
    }
    None
  }

  /// Gets an object's own property descriptor.
  ///
  /// This does not currently walk the prototype chain.
  pub fn get_own_property(
    &self,
    obj: GcObject,
    key: PropertyKey,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    self.object_get_own_property(obj, &key)
  }

  /// ECMAScript `OrdinaryDelete` / `[[Delete]]` for ordinary objects.
  ///
  /// Spec: https://tc39.es/ecma262/#sec-ordinarydelete
  pub fn ordinary_delete(&mut self, obj: GcObject, key: PropertyKey) -> Result<bool, VmError> {
    let Some(current) = self.get_own_property(obj, key)? else {
      return Ok(true);
    };

    if !current.configurable {
      return Ok(false);
    }

    let _deleted = self.object_delete_own_property(obj, &key)?;
    Ok(true)
  }

  /// ECMAScript `OrdinaryOwnPropertyKeys` / `[[OwnPropertyKeys]]` for ordinary objects.
  ///
  /// Spec: https://tc39.es/ecma262/#sec-ordinaryownpropertykeys
  pub fn ordinary_own_property_keys_with_tick(
    &self,
    obj: GcObject,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<Vec<PropertyKey>, VmError> {
    let properties = &self.get_object_base(obj)?.properties;

    let property_count = properties.len();

    // `[[OwnPropertyKeys]]` can be invoked from native builtins (`Object.keys`,
    // destructuring/object spread) and can traverse very large property tables. Budget it so the
    // runtime can still observe fuel/interrupt/deadline limits while enumerating keys.
    const TICK_EVERY: usize = 1024;

    // 1. Array indices (String keys that are array indices) in ascending numeric order.
    let mut index_keys: Vec<(u32, PropertyKey)> = Vec::new();
    index_keys
      .try_reserve_exact(property_count)
      .map_err(|_| VmError::OutOfMemory)?;
    for (i, prop) in properties.iter().enumerate() {
      if i % TICK_EVERY == 0 {
        tick()?;
      }
      if matches!(prop.key, PropertyKey::String(_)) {
        if let Some(idx) = self.array_index(&prop.key) {
          index_keys.push((idx, prop.key));
        }
      }
    }

    // Charge a little fuel and re-check interrupt/deadline state before and after the sort, which
    // can be relatively expensive for very large index sets.
    if !index_keys.is_empty() {
      tick()?;
    }
    index_keys.sort_by_key(|(idx, _)| *idx);
    if !index_keys.is_empty() {
      tick()?;
    }

    // 2. String keys that are not array indices, in chronological creation order.
    // 3. Symbol keys, in chronological creation order.
    let mut out: Vec<PropertyKey> = Vec::new();
    out
      .try_reserve_exact(property_count)
      .map_err(|_| VmError::OutOfMemory)?;

    for (i, (_, key)) in index_keys.iter().enumerate() {
      if i % TICK_EVERY == 0 {
        tick()?;
      }
      out.push(*key);
    }

    for (i, prop) in properties.iter().enumerate() {
      if i % TICK_EVERY == 0 {
        tick()?;
      }
      let PropertyKey::String(_) = prop.key else {
        continue;
      };
      if self.array_index(&prop.key).is_none() {
        out.push(prop.key);
      }
    }

    for (i, prop) in properties.iter().enumerate() {
      if i % TICK_EVERY == 0 {
        tick()?;
      }
      if matches!(prop.key, PropertyKey::Symbol(_)) {
        out.push(prop.key);
      }
    }

    Ok(out)
  }

  pub fn ordinary_own_property_keys(&self, obj: GcObject) -> Result<Vec<PropertyKey>, VmError> {
    self.ordinary_own_property_keys_with_tick(obj, || Ok(()))
  }

  pub(crate) fn set_function_name_metadata(
    &mut self,
    func: GcObject,
    name: GcString,
  ) -> Result<(), VmError> {
    let idx = self
      .validate(func.0)
      .ok_or_else(|| VmError::invalid_handle())?;
    let Some(obj) = self.slots[idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    let HeapObject::Function(func) = obj else {
      return Err(VmError::invalid_handle());
    };
    func.name = name;
    Ok(())
  }

  pub(crate) fn set_function_length_metadata(
    &mut self,
    func: GcObject,
    length: u32,
  ) -> Result<(), VmError> {
    let idx = self
      .validate(func.0)
      .ok_or_else(|| VmError::invalid_handle())?;
    let Some(obj) = self.slots[idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    let HeapObject::Function(func) = obj else {
      return Err(VmError::invalid_handle());
    };
    func.length = length;
    Ok(())
  }

  pub(crate) fn env_outer(&self, env: GcEnv) -> Result<Option<GcEnv>, VmError> {
    Ok(self.get_env(env)?.outer())
  }

  pub(crate) fn env_has_binding(&self, env: GcEnv, name: &str) -> Result<bool, VmError> {
    Ok(
      self
        .get_declarative_env(env)?
        .find_binding_index(self, name)?
        .is_some(),
    )
  }

  pub(crate) fn env_initialize_binding(
    &mut self,
    env: GcEnv,
    name: &str,
    value: Value,
  ) -> Result<(), VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(value));

    let idx = {
      let rec = self.get_declarative_env(env)?;
      rec
        .find_binding_index(self, name)?
        .ok_or(VmError::Unimplemented("unbound identifier"))?
    };

    let rec = self.get_declarative_env_mut(env)?;
    let binding = rec
      .bindings
      .get_mut(idx)
      .ok_or(VmError::Unimplemented(
        "environment record binding index out of bounds",
      ))?;

    if binding.initialized {
      return Err(VmError::Unimplemented("binding already initialized"));
    }

    if let EnvBindingValue::Direct(slot) = &mut binding.value {
      *slot = value;
    } else {
      return Err(VmError::InvariantViolation(
        "cannot initialize an indirect env binding",
      ));
    }
    binding.initialized = true;
    Ok(())
  }

  pub(crate) fn env_get_binding_value(
    &self,
    env: GcEnv,
    name: &str,
    _strict: bool,
  ) -> Result<Value, VmError> {
    let rec = self.get_declarative_env(env)?;
    let Some(idx) = rec.find_binding_index(self, name)? else {
      return Err(VmError::Unimplemented("unbound identifier"));
    };
    let binding = rec.bindings.get(idx).ok_or(VmError::Unimplemented(
      "environment record binding index out of bounds",
    ))?;
    if !binding.initialized {
      // TDZ.
      //
      // Note: environment records do not currently have access to a `Realm` to construct proper
      // `ReferenceError` objects. We return a sentinel throw value that higher-level execution code
      // can translate into a realm-aware error object.
      return Err(VmError::Throw(Value::Null));
    }
    binding.value.get(self)
  }

  pub(crate) fn env_get_binding_value_by_gc_string(
    &self,
    env: GcEnv,
    name: GcString,
  ) -> Result<Value, VmError> {
    let rec = self.get_declarative_env(env)?;
    let needle_units = self.get_string(name)?.as_code_units();
    let mut found: Option<&crate::env::EnvBinding> = None;
    for binding in rec.bindings.iter() {
      let Some(binding_name) = binding.name else {
        continue;
      };
      if self.get_string(binding_name)?.as_code_units() == needle_units {
        found = Some(binding);
        break;
      }
    }
    let binding = found.ok_or(VmError::Unimplemented("unbound identifier"))?;
    if !binding.initialized {
      // TDZ sentinel; see `env_get_binding_value`.
      return Err(VmError::Throw(Value::Null));
    }
    binding.value.get(self)
  }

  pub(crate) fn env_set_mutable_binding(
    &mut self,
    env: GcEnv,
    name: &str,
    value: Value,
    _strict: bool,
  ) -> Result<(), VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(value));

    let idx = {
      let rec = self.get_declarative_env(env)?;
      rec
        .find_binding_index(self, name)?
        .ok_or(VmError::Unimplemented("unbound identifier"))?
    };

    let rec = self.get_declarative_env_mut(env)?;
    let binding = rec
      .bindings
      .get_mut(idx)
      .ok_or(VmError::Unimplemented(
        "environment record binding index out of bounds",
      ))?;

    if !binding.initialized {
      // TDZ.
      // See `env_get_binding_value` for why this is a sentinel.
      return Err(VmError::Throw(Value::Null));
    }

    if !binding.mutable {
      // Assignment to const.
      //
      // Like TDZ, this is currently a sentinel throw value that is later mapped to a TypeError
      // object by higher-level evaluation code.
      return Err(VmError::Throw(Value::Undefined));
    }

    if let EnvBindingValue::Direct(slot) = &mut binding.value {
      *slot = value;
    } else {
      return Err(VmError::InvariantViolation(
        "cannot assign through an indirect env binding",
      ));
    }
    Ok(())
  }

  pub fn env_has_symbol_binding(&self, env: GcEnv, symbol: SymbolId) -> Result<bool, VmError> {
    Ok(self.get_declarative_env(env)?.has_symbol_binding(symbol))
  }

  pub fn env_get_symbol_binding_value(
    &self,
    env: GcEnv,
    symbol: SymbolId,
  ) -> Result<Value, VmError> {
    self
      .get_declarative_env(env)?
      .get_symbol_binding_value(self, symbol)
  }

  pub fn env_initialize_symbol_binding(
    &mut self,
    env: GcEnv,
    symbol: SymbolId,
    value: Value,
  ) -> Result<(), VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(value));
    self
      .get_declarative_env_mut(env)?
      .initialize_symbol_binding(symbol, value)
  }

  pub fn env_set_mutable_symbol_binding(
    &mut self,
    env: GcEnv,
    symbol: SymbolId,
    value: Value,
  ) -> Result<(), VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(value));
    self
      .get_declarative_env_mut(env)?
      .set_mutable_symbol_binding(symbol, value)
  }

  fn env_add_binding(&mut self, env: GcEnv, binding: EnvBinding) -> Result<(), VmError> {
    let idx = self
      .validate(env.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let (binding_count, old_bytes) = {
      let slot = &self.slots[idx];
      let Some(HeapObject::Env(env)) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };
      let EnvRecord::Declarative(env) = env else {
        return Err(VmError::Unimplemented("object environment record"));
      };
      (env.bindings.len(), slot.bytes)
    };

    let new_binding_count = binding_count.checked_add(1).ok_or(VmError::OutOfMemory)?;
    let new_bytes = EnvRecord::heap_size_bytes_for_binding_count(new_binding_count);

    // Before allocating, enforce heap limits based on the net growth of this environment record.
    let grow_by = new_bytes.saturating_sub(old_bytes);
    self.ensure_can_allocate(grow_by)?;

    // Allocate the new binding table fallibly so hostile inputs cannot abort the host process
    // on allocator OOM.
    let mut buf: Vec<EnvBinding> = Vec::new();
    buf
      .try_reserve_exact(new_binding_count)
      .map_err(|_| VmError::OutOfMemory)?;

    {
      let slot = &self.slots[idx];
      let Some(HeapObject::Env(env)) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };
      let EnvRecord::Declarative(env) = env else {
        return Err(VmError::Unimplemented("object environment record"));
      };
      buf.extend_from_slice(&env.bindings);
    }

    let insert_at = match buf.binary_search_by_key(&binding.symbol, |b| b.symbol) {
      Ok(_) => return Err(VmError::Unimplemented("duplicate env binding")),
      Err(idx) => idx,
    };
    buf.insert(insert_at, binding);
    let bindings = buf.into_boxed_slice();

    let Some(HeapObject::Env(env)) = self.slots[idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    let EnvRecord::Declarative(env) = env else {
      return Err(VmError::Unimplemented("object environment record"));
    };
    env.bindings = bindings;

    self.update_slot_bytes(idx, new_bytes);

    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
    Ok(())
  }

  fn define_property(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptor,
  ) -> Result<(), VmError> {
    let key_is_length = self.property_key_is_length(&key);
    let key_array_index = match key {
      PropertyKey::String(s) => self.string_to_array_index(s),
      PropertyKey::Symbol(_) => None,
    };

    let idx = self
      .validate(obj.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    // Integer-indexed exotic behaviour for typed arrays:
    // - writing an in-bounds integer index updates the view's underlying ArrayBuffer
    // - out-of-bounds writes are silently ignored
    //
    // Note: we intentionally do **not** store integer indices in the object's property table.
    if let Some(index) = key_array_index {
      if matches!(self.slots[idx].value.as_ref(), Some(HeapObject::TypedArray(_))) {
        let value = match desc.kind {
          PropertyKind::Data { value, .. } => value,
          PropertyKind::Accessor { .. } => {
            return Err(VmError::TypeError(
              "TypedArray integer index properties must be data descriptors",
            ))
          }
        };

        let _ = self.typed_array_set_element_value(obj, index as usize, value)?;
        return Ok(());
      }
    }

    #[derive(Clone, Copy)]
    enum TargetKind {
      OrdinaryObject,
      ArrayBuffer,
      TypedArray,
      DataView,
      Function {
        bound_args_len: usize,
        native_slots_len: usize,
      },
      Promise {
        fulfill_reaction_count: usize,
        reject_reaction_count: usize,
      },
      WeakSet {
        entry_capacity: usize,
      },
      Generator {
        args_len: usize,
        continuation_len: usize,
      },
    }

    let (target_kind, property_count, old_bytes, existing_idx, array_len) = {
      let slot = &self.slots[idx];
      let Some(obj) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };
      match obj {
        HeapObject::Object(obj) => {
          let existing_idx = obj
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::OrdinaryObject,
            obj.base.properties.len(),
            slot.bytes,
            existing_idx,
            obj.array_length(),
          )
        }
        HeapObject::ArrayBuffer(obj) => {
          let existing_idx = obj
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::ArrayBuffer,
            obj.base.properties.len(),
            slot.bytes,
            existing_idx,
            None,
          )
        }
        HeapObject::TypedArray(obj) => {
          let existing_idx = obj
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::TypedArray,
            obj.base.properties.len(),
            slot.bytes,
            existing_idx,
            None,
          )
        }
        HeapObject::DataView(obj) => {
          let existing_idx = obj
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::DataView,
            obj.base.properties.len(),
            slot.bytes,
            existing_idx,
            None,
          )
        }
        HeapObject::Function(func) => {
          let existing_idx = func
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          let bound_args_len = func.bound_args.as_ref().map(|args| args.len()).unwrap_or(0);
          let native_slots_len = func.native_slots.as_ref().map(|slots| slots.len()).unwrap_or(0);
          (
            TargetKind::Function {
              bound_args_len,
              native_slots_len,
            },
            func.base.properties.len(),
            slot.bytes,
            existing_idx,
            None,
          )
        }
        HeapObject::Promise(p) => {
          let existing_idx = p
            .object
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::Promise {
              fulfill_reaction_count: p.fulfill_reactions.as_deref().map(|r| r.len()).unwrap_or(0),
              reject_reaction_count: p.reject_reactions.as_deref().map(|r| r.len()).unwrap_or(0),
            },
            p.object.base.properties.len(),
            slot.bytes,
            existing_idx,
            None,
          )
        }
        HeapObject::WeakSet(obj) => {
          let existing_idx = obj
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::WeakSet {
              entry_capacity: obj.entries.capacity(),
            },
            obj.base.properties.len(),
            slot.bytes,
            existing_idx,
            None,
          )
        }
        HeapObject::Generator(g) => {
          let existing_idx = g
            .object
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::Generator {
              args_len: g.args.as_deref().map(|args| args.len()).unwrap_or(0),
              continuation_len: g.continuation.as_deref().map(|c| c.len()).unwrap_or(0),
            },
            g.object.base.properties.len(),
            slot.bytes,
            existing_idx,
            None,
          )
        }
        _ => return Err(VmError::invalid_handle()),
      }
    };

    // If defining the `length` property on an Array, keep the internal `length` slot in sync with
    // the property descriptor's `[[Value]]`.
    let new_array_length = if key_is_length && array_len.is_some() {
      if existing_idx != Some(0) {
        return Err(VmError::InvariantViolation(
          "array length property is missing or not at index 0",
        ));
      }
      match desc.kind {
        PropertyKind::Data {
          value: Value::Number(n),
          ..
        } => Some(array_length_from_f64(n).ok_or(VmError::InvariantViolation(
          "array length must be a uint32 number",
        ))?),
        _ => {
          return Err(VmError::InvariantViolation(
            "array length property must be a data descriptor with a numeric value",
          ));
        }
      }
    } else {
      None
    };

    match existing_idx {
      Some(existing_idx) => {
        // Replace in-place (no change to heap size).
        match self.slots[idx].value.as_mut() {
          Some(HeapObject::Object(obj)) => obj.base.properties[existing_idx].desc = desc,
          Some(HeapObject::ArrayBuffer(obj)) => obj.base.properties[existing_idx].desc = desc,
          Some(HeapObject::TypedArray(obj)) => obj.base.properties[existing_idx].desc = desc,
          Some(HeapObject::DataView(obj)) => obj.base.properties[existing_idx].desc = desc,
          Some(HeapObject::Function(func)) => func.base.properties[existing_idx].desc = desc,
          Some(HeapObject::Promise(p)) => p.object.base.properties[existing_idx].desc = desc,
          Some(HeapObject::WeakSet(ws)) => ws.base.properties[existing_idx].desc = desc,
          Some(HeapObject::Generator(g)) => g.object.base.properties[existing_idx].desc = desc,
          _ => return Err(VmError::invalid_handle()),
        }
      }
      None => {
        let new_property_count = property_count
          .checked_add(1)
          .ok_or(VmError::OutOfMemory)?;
        let new_bytes = match target_kind {
          TargetKind::OrdinaryObject => JsObject::heap_size_bytes_for_property_count(new_property_count),
          TargetKind::ArrayBuffer => JsArrayBuffer::heap_size_bytes_for_property_count(new_property_count),
          TargetKind::TypedArray => JsTypedArray::heap_size_bytes_for_property_count(new_property_count),
          TargetKind::DataView => JsDataView::heap_size_bytes_for_property_count(new_property_count),
          TargetKind::Function {
            bound_args_len,
            native_slots_len,
          } => JsFunction::heap_size_bytes_for_counts(bound_args_len, native_slots_len, new_property_count),
          TargetKind::Promise {
            fulfill_reaction_count,
            reject_reaction_count,
          } => JsPromise::heap_size_bytes_for_counts(
            new_property_count,
            fulfill_reaction_count,
            reject_reaction_count,
          ),
          TargetKind::WeakSet { entry_capacity } => {
            JsWeakSet::heap_size_bytes_for_counts(new_property_count, entry_capacity)
          }
          TargetKind::Generator {
            args_len,
            continuation_len,
          } => JsGenerator::heap_size_bytes_for_counts(new_property_count, args_len, continuation_len),
        };

        // Before allocating, enforce heap limits based on the net growth of this object.
        let grow_by = new_bytes.saturating_sub(old_bytes);
        self.ensure_can_allocate(grow_by)?;

        // Allocate the new property table fallibly so hostile inputs cannot abort the host process
        // on allocator OOM.
        let mut buf: Vec<PropertyEntry> = Vec::new();
        buf
          .try_reserve_exact(new_property_count)
          .map_err(|_| VmError::OutOfMemory)?;

        {
          let slot = &self.slots[idx];
          match slot.value.as_ref() {
            Some(HeapObject::Object(obj)) => buf.extend_from_slice(&obj.base.properties),
            Some(HeapObject::ArrayBuffer(obj)) => buf.extend_from_slice(&obj.base.properties),
            Some(HeapObject::TypedArray(obj)) => buf.extend_from_slice(&obj.base.properties),
            Some(HeapObject::DataView(obj)) => buf.extend_from_slice(&obj.base.properties),
            Some(HeapObject::Function(func)) => buf.extend_from_slice(&func.base.properties),
            Some(HeapObject::Promise(p)) => buf.extend_from_slice(&p.object.base.properties),
            Some(HeapObject::WeakSet(ws)) => buf.extend_from_slice(&ws.base.properties),
            Some(HeapObject::Generator(g)) => buf.extend_from_slice(&g.object.base.properties),
            _ => return Err(VmError::invalid_handle()),
          }
        }

        buf.push(PropertyEntry { key, desc });
        let properties = buf.into_boxed_slice();

        match self.slots[idx].value.as_mut() {
          Some(HeapObject::Object(obj)) => obj.base.properties = properties,
          Some(HeapObject::ArrayBuffer(obj)) => obj.base.properties = properties,
          Some(HeapObject::TypedArray(obj)) => obj.base.properties = properties,
          Some(HeapObject::DataView(obj)) => obj.base.properties = properties,
          Some(HeapObject::Function(func)) => func.base.properties = properties,
          Some(HeapObject::Promise(p)) => p.object.base.properties = properties,
          Some(HeapObject::WeakSet(ws)) => ws.base.properties = properties,
          Some(HeapObject::Generator(g)) => g.object.base.properties = properties,
          _ => return Err(VmError::invalid_handle()),
        }

        self.update_slot_bytes(idx, new_bytes);

        #[cfg(debug_assertions)]
        self.debug_assert_used_bytes_is_correct();
      }
    };

    if let Some(new_len) = new_array_length {
      let Some(HeapObject::Object(obj)) = self.slots[idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      obj.set_array_length(new_len);
    }

    // Array exotic index semantics: writing an array index extends `length`.
    if let Some(index) = key_array_index {
      if array_len.is_some() {
        let new_len = index.wrapping_add(1);
        let Some(HeapObject::Object(obj)) = self.slots[idx].value.as_mut() else {
          return Err(VmError::invalid_handle());
        };
        if let Some(current_len) = obj.array_length() {
          if new_len > current_len {
            obj.set_array_length(new_len);
          }
        }
      }
    }

    Ok(())
  }

  fn get_heap_object(&self, id: HeapId) -> Result<&HeapObject, VmError> {
    let idx = self.validate(id).ok_or_else(|| VmError::invalid_handle())?;
    self
      .slots[idx]
      .value
      .as_ref()
      .ok_or_else(|| VmError::invalid_handle())
  }

  fn get_heap_object_mut(&mut self, id: HeapId) -> Result<&mut HeapObject, VmError> {
    let idx = self.validate(id).ok_or_else(|| VmError::invalid_handle())?;
    self
      .slots[idx]
      .value
      .as_mut()
      .ok_or_else(|| VmError::invalid_handle())
  }
  fn validate(&self, id: HeapId) -> Option<usize> {
    let idx = id.index() as usize;
    let slot = self.slots.get(idx)?;
    if slot.generation != id.generation() {
      return None;
    }
    if slot.value.is_none() {
      return None;
    }
    Some(idx)
  }

  fn ensure_can_allocate(&mut self, additional_bytes: usize) -> Result<(), VmError> {
    self.ensure_can_allocate_with(|_| additional_bytes)
  }

  fn ensure_can_allocate_with<F>(&mut self, additional_bytes: F) -> Result<(), VmError>
  where
    F: FnMut(&Heap) -> usize,
  {
    self.ensure_can_allocate_with_extra_roots(additional_bytes, &[], &[], &[], &[])
  }

  fn ensure_can_allocate_with_extra_roots<F>(
    &mut self,
    mut additional_bytes: F,
    extra_value_roots_a: &[Value],
    extra_value_roots_b: &[Value],
    extra_env_roots_a: &[GcEnv],
    extra_env_roots_b: &[GcEnv],
  ) -> Result<(), VmError>
  where
    F: FnMut(&Heap) -> usize,
  {
    let after = self
      .estimated_total_bytes()
      .saturating_add(additional_bytes(self));
    if after > self.limits.gc_threshold {
      self.collect_garbage_with_extra_roots(
        extra_value_roots_a,
        extra_value_roots_b,
        extra_env_roots_a,
        extra_env_roots_b,
      );
    }

    let after = self
      .estimated_total_bytes()
      .saturating_add(additional_bytes(self));
    if after > self.limits.max_bytes {
      return Err(VmError::OutOfMemory);
    }
    Ok(())
  }

  fn update_slot_bytes(&mut self, idx: usize, new_bytes: usize) {
    let slot = &mut self.slots[idx];
    let old_bytes = slot.bytes;

    if new_bytes >= old_bytes {
      self.used_bytes = self.used_bytes.saturating_add(new_bytes - old_bytes);
    } else {
      self.used_bytes = self.used_bytes.saturating_sub(old_bytes - new_bytes);
    }

    slot.bytes = new_bytes;
  }

  fn alloc_unchecked_inner(
    &mut self,
    obj: HeapObject,
    new_bytes: usize,
    preflight: bool,
  ) -> Result<HeapId, VmError> {
    if preflight {
      // Pre-flight allocation with a dynamic cost model because running GC can populate
      // `free_list`, which avoids slot-table growth.
      self.ensure_can_allocate_with(|heap| heap.additional_bytes_for_heap_alloc(new_bytes))?;
    }
    let idx = match self.free_list.pop() {
      Some(idx) => idx as usize,
      None => {
        self.reserve_for_new_slot()?;
        let idx = self.slots.len();
        self.slots.push(Slot::new());
        self.marks.push(0);
        idx
      }
    };

    let slot = &mut self.slots[idx];
    debug_assert!(slot.value.is_none(), "free list returned an occupied slot");

    slot.value = Some(obj);
    slot.host_slots = None;
    slot.bytes = new_bytes;
    self.used_bytes = self.used_bytes.saturating_add(new_bytes);

    let idx_u32: u32 = idx.try_into().map_err(|_| VmError::OutOfMemory)?;
    let id = HeapId::from_parts(idx_u32, slot.generation);

    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();

    Ok(id)
  }

  fn alloc_unchecked(&mut self, obj: HeapObject, new_bytes: usize) -> Result<HeapId, VmError> {
    self.alloc_unchecked_inner(obj, new_bytes, true)
  }

  fn alloc_unchecked_after_ensure(
    &mut self,
    obj: HeapObject,
    new_bytes: usize,
  ) -> Result<HeapId, VmError> {
    self.alloc_unchecked_inner(obj, new_bytes, false)
  }

  fn symbol_registry_get(&self, key: &JsString) -> Result<Option<GcSymbol>, VmError> {
    match self.symbol_registry_binary_search(key)? {
      Ok(idx) => Ok(Some(self.symbol_registry[idx].sym)),
      Err(_) => Ok(None),
    }
  }

  fn symbol_registry_insert_with_tick(
    &mut self,
    key: GcString,
    sym: GcSymbol,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<(), VmError> {
    self.reserve_for_symbol_registry_insert(1)?;

    let key_contents = self.get_string(key)?;
    let insert_at = match self.symbol_registry_binary_search(key_contents)? {
      Ok(_) => return Ok(()), // Idempotent if called twice.
      Err(idx) => idx,
    };

    let entry = SymbolRegistryEntry { key, sym };

    // Fast path: insert at the end.
    let len = self.symbol_registry.len();
    if insert_at == len {
      self.symbol_registry.push(entry);
      return Ok(());
    }

    // Manual insertion to allow budgeting the O(n) shift.
    self.symbol_registry.push(entry);

    const TICK_EVERY: usize = 1024;
    for (step, i) in (insert_at..len).rev().enumerate() {
      if step % TICK_EVERY == 0 {
        tick()?;
      }
      self.symbol_registry[i + 1] = self.symbol_registry[i];
    }
    self.symbol_registry[insert_at] = entry;

    Ok(())
  }

  fn symbol_registry_binary_search(
    &self,
    key: &JsString,
  ) -> Result<Result<usize, usize>, VmError> {
    // Manual binary search so we can compare by string contents (not by handle identity).
    let mut low = 0usize;
    let mut high = self.symbol_registry.len();
    while low < high {
      let mid = low + (high - low) / 2;
      let mid_key = self.get_string(self.symbol_registry[mid].key)?;
      match mid_key.cmp(key) {
        std::cmp::Ordering::Less => {
          low = mid + 1;
        }
        std::cmp::Ordering::Greater => {
          high = mid;
        }
        std::cmp::Ordering::Equal => return Ok(Ok(mid)),
      }
    }
    Ok(Err(low))
  }

  fn symbol_registry_binary_search_code_units(
    &self,
    key: &[u16],
  ) -> Result<Result<usize, usize>, VmError> {
    // Manual binary search so we can compare by string contents (not by handle identity), without
    // constructing a temporary `JsString`.
    let mut low = 0usize;
    let mut high = self.symbol_registry.len();
    while low < high {
      let mid = low + (high - low) / 2;
      let mid_key = self.get_string(self.symbol_registry[mid].key)?;
      match mid_key.as_code_units().cmp(key) {
        std::cmp::Ordering::Less => {
          low = mid + 1;
        }
        std::cmp::Ordering::Greater => {
          high = mid;
        }
        std::cmp::Ordering::Equal => return Ok(Ok(mid)),
      }
    }
    Ok(Err(low))
  }

  fn additional_bytes_for_heap_alloc(&self, payload_bytes: usize) -> usize {
    let mut bytes = payload_bytes;
    if self.free_list.is_empty() {
      bytes = bytes.saturating_add(self.additional_bytes_for_new_slot());
    }
    bytes
  }

  fn additional_bytes_for_new_slot(&self) -> usize {
    let new_len = self.slots.len().saturating_add(1);
    let mut bytes = 0usize;

    bytes = bytes.saturating_add(vec_capacity_growth_bytes::<Slot>(
      self.slots.capacity(),
      new_len,
    ));
    bytes = bytes.saturating_add(vec_capacity_growth_bytes::<u8>(
      self.marks.capacity(),
      new_len,
    ));

    // Ensure GC sweep and marking can never allocate.
    bytes = bytes.saturating_add(vec_capacity_growth_bytes::<u32>(
      self.free_list.capacity(),
      new_len,
    ));
    bytes = bytes.saturating_add(vec_capacity_growth_bytes::<HeapId>(
      self.gc_worklist.capacity(),
      new_len,
    ));

    bytes
  }

  fn reserve_for_new_slot(&mut self) -> Result<(), VmError> {
    let new_len = self.slots.len().checked_add(1).ok_or(VmError::OutOfMemory)?;

    reserve_vec_to_len::<Slot>(&mut self.slots, new_len)?;
    reserve_vec_to_len::<u8>(&mut self.marks, new_len)?;
    reserve_vec_to_len::<u32>(&mut self.free_list, new_len)?;
    reserve_vec_to_len::<HeapId>(&mut self.gc_worklist, new_len)?;
    Ok(())
  }

  fn additional_bytes_for_new_persistent_root_slot(&self) -> usize {
    let new_len = self.persistent_roots.len().saturating_add(1);
    let mut bytes = 0usize;
    bytes = bytes.saturating_add(vec_capacity_growth_bytes::<Option<Value>>(
      self.persistent_roots.capacity(),
      new_len,
    ));
    // Ensure `remove_root` never needs to allocate.
    bytes = bytes.saturating_add(vec_capacity_growth_bytes::<u32>(
      self.persistent_roots_free.capacity(),
      new_len,
    ));
    bytes
  }

  fn reserve_for_new_persistent_root_slot(&mut self) -> Result<(), VmError> {
    let new_len = self
      .persistent_roots
      .len()
      .checked_add(1)
      .ok_or(VmError::OutOfMemory)?;
    reserve_vec_to_len::<Option<Value>>(&mut self.persistent_roots, new_len)?;
    reserve_vec_to_len::<u32>(&mut self.persistent_roots_free, new_len)?;
    Ok(())
  }

  fn additional_bytes_for_new_persistent_env_root_slot(&self) -> usize {
    let new_len = self.persistent_env_roots.len().saturating_add(1);
    let mut bytes = 0usize;
    bytes = bytes.saturating_add(vec_capacity_growth_bytes::<Option<GcEnv>>(
      self.persistent_env_roots.capacity(),
      new_len,
    ));
    // Ensure `remove_env_root` never needs to allocate.
    bytes = bytes.saturating_add(vec_capacity_growth_bytes::<u32>(
      self.persistent_env_roots_free.capacity(),
      new_len,
    ));
    bytes
  }

  fn reserve_for_new_persistent_env_root_slot(&mut self) -> Result<(), VmError> {
    let new_len = self
      .persistent_env_roots
      .len()
      .checked_add(1)
      .ok_or(VmError::OutOfMemory)?;
    reserve_vec_to_len::<Option<GcEnv>>(&mut self.persistent_env_roots, new_len)?;
    reserve_vec_to_len::<u32>(&mut self.persistent_env_roots_free, new_len)?;
    Ok(())
  }

  fn additional_bytes_for_symbol_registry_insert(&self, additional: usize) -> usize {
    let required = self.symbol_registry.len().saturating_add(additional);
    vec_capacity_growth_bytes::<SymbolRegistryEntry>(self.symbol_registry.capacity(), required)
  }

  fn reserve_for_symbol_registry_insert(&mut self, additional: usize) -> Result<(), VmError> {
    let required = self
      .symbol_registry
      .len()
      .checked_add(additional)
      .ok_or(VmError::OutOfMemory)?;
    self.ensure_can_allocate_with(|heap| heap.additional_bytes_for_symbol_registry_insert(additional))?;
    reserve_vec_to_len::<SymbolRegistryEntry>(&mut self.symbol_registry, required)?;
    Ok(())
  }

  fn debug_value_is_valid_or_primitive(&self, value: Value) -> bool {
    match value {
      Value::Undefined | Value::Null | Value::Bool(_) | Value::Number(_) | Value::BigInt(_) => true,
      Value::String(s) => self.is_valid_string(s),
      Value::Symbol(s) => self.is_valid_symbol(s),
      Value::Object(o) => self.is_valid_object(o),
    }
  }

  pub(crate) fn get_function_call_handler(&self, func: GcObject) -> Result<CallHandler, VmError> {
    match self.get_heap_object(func.0)? {
      HeapObject::Function(f) => Ok(f.call.clone()),
      _ => Err(VmError::NotCallable),
    }
  }

  pub(crate) fn get_proxy_data(&self, obj: GcObject) -> Result<Option<ProxyData>, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::Proxy(p) => Ok(Some(ProxyData {
        target: p.target,
        handler: p.handler,
      })),
      _ => Ok(None),
    }
  }

  /// Returns the captured native slots for a function object.
  ///
  /// If the function has no native slots, this returns an empty slice.
  pub fn get_function_native_slots(&self, func: GcObject) -> Result<&[Value], VmError> {
    match self.get_heap_object(func.0)? {
      HeapObject::Function(f) => Ok(f.native_slots.as_deref().unwrap_or(&[])),
      _ => Err(VmError::NotCallable),
    }
  }

  pub(crate) fn get_function_construct_handler(
    &self,
    func: GcObject,
  ) -> Result<Option<ConstructHandler>, VmError> {
    match self.get_heap_object(func.0)? {
      HeapObject::Function(f) => Ok(f.construct),
      _ => Err(VmError::NotConstructable),
    }
  }

  pub(crate) fn get_function_name(&self, func: GcObject) -> Result<GcString, VmError> {
    match self.get_heap_object(func.0)? {
      HeapObject::Function(f) => Ok(f.name),
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn get_function(&self, func: GcObject) -> Result<&JsFunction, VmError> {
    match self.get_heap_object(func.0)? {
      HeapObject::Function(f) => Ok(f),
      _ => Err(VmError::NotCallable),
    }
  }

  pub(crate) fn get_function_data(&self, func: GcObject) -> Result<FunctionData, VmError> {
    match self.get_heap_object(func.0)? {
      HeapObject::Function(f) => Ok(f.data),
      _ => Err(VmError::invalid_handle()),
    }
  }

  #[allow(dead_code)]
  pub(crate) fn get_function_this_mode(&self, func: GcObject) -> Result<ThisMode, VmError> {
    match self.get_heap_object(func.0)? {
      HeapObject::Function(f) => Ok(f.this_mode),
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn get_function_realm(&self, func: GcObject) -> Result<Option<GcObject>, VmError> {
    match self.get_heap_object(func.0)? {
      HeapObject::Function(f) => Ok(f.realm),
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn get_function_job_realm(&self, func: GcObject) -> Option<RealmId> {
    // Promise job callbacks may be Proxy objects; follow the proxy chain to the underlying target.
    let mut obj = func;
    for _ in 0..MAX_PROTOTYPE_CHAIN {
      match self.get_heap_object(obj.0) {
        Ok(HeapObject::Function(f)) => return f.job_realm,
        Ok(HeapObject::Proxy(p)) => {
          let (Some(target), Some(_handler)) = (p.target, p.handler) else {
            return None;
          };
          obj = target;
        }
        _ => return None,
      }
    }
    None
  }

  pub(crate) fn get_function_closure_env(&self, func: GcObject) -> Result<Option<GcEnv>, VmError> {
    match self.get_heap_object(func.0)? {
      HeapObject::Function(f) => Ok(f.closure_env),
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn set_function_bound_this(
    &mut self,
    func: GcObject,
    bound_this: Value,
  ) -> Result<(), VmError> {
    match self.get_heap_object_mut(func.0)? {
      HeapObject::Function(f) => {
        f.bound_this = Some(bound_this);
        Ok(())
      }
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn set_function_bound_new_target(
    &mut self,
    func: GcObject,
    bound_new_target: Value,
  ) -> Result<(), VmError> {
    match self.get_heap_object_mut(func.0)? {
      HeapObject::Function(f) => {
        f.bound_new_target = Some(bound_new_target);
        Ok(())
      }
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn set_function_realm(&mut self, func: GcObject, realm: GcObject) -> Result<(), VmError> {
    match self.get_heap_object_mut(func.0)? {
      HeapObject::Function(f) => {
        f.realm = Some(realm);
        Ok(())
      }
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn set_function_job_realm(&mut self, func: GcObject, realm: RealmId) -> Result<(), VmError> {
    match self.get_heap_object_mut(func.0)? {
      HeapObject::Function(f) => {
        f.job_realm = Some(realm);
        Ok(())
      }
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn set_function_data(
    &mut self,
    func: GcObject,
    data: FunctionData,
  ) -> Result<(), VmError> {
    match self.get_heap_object_mut(func.0)? {
      HeapObject::Function(f) => {
        f.data = data;
        Ok(())
      }
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn set_function_closure_env(
    &mut self,
    func: GcObject,
    env: Option<GcEnv>,
  ) -> Result<(), VmError> {
    match self.get_heap_object_mut(func.0)? {
      HeapObject::Function(f) => {
        f.closure_env = env;
        Ok(())
      }
      _ => Err(VmError::invalid_handle()),
    }
  }
}

/// A stack-rooting scope.
///
/// All stack roots pushed via [`Scope::push_root`] are removed when the scope is dropped.
pub struct Scope<'a> {
  heap: &'a mut Heap,
  root_stack_len_at_entry: usize,
  env_root_stack_len_at_entry: usize,
}

impl Drop for Scope<'_> {
  fn drop(&mut self) {
    self.heap.root_stack.truncate(self.root_stack_len_at_entry);
    self
      .heap
      .env_root_stack
      .truncate(self.env_root_stack_len_at_entry);
  }
}

impl<'a> Scope<'a> {
  /// Pushes a stack root.
  ///
  /// The returned `Value` is the same as the input, allowing call sites to write
  /// `let v = scope.push_root(v)?;` if desired.
  pub fn push_root(&mut self, value: Value) -> Result<Value, VmError> {
    let values = [value];
    self.push_roots(&values)?;
    Ok(value)
  }

  /// Pushes multiple stack roots in one operation.
  pub fn push_roots(&mut self, values: &[Value]) -> Result<(), VmError> {
    self.push_roots_with_extra_roots(values, &[], &[])
  }

  pub(crate) fn push_roots_with_extra_roots(
    &mut self,
    values: &[Value],
    extra_roots: &[Value],
    extra_env_roots: &[GcEnv],
  ) -> Result<(), VmError> {
    if values.is_empty() {
      return Ok(());
    }

    for value in values {
      debug_assert!(self.heap.debug_value_is_valid_or_primitive(*value));
    }

    let new_len = self
      .heap
      .root_stack
      .len()
      .checked_add(values.len())
      .ok_or(VmError::OutOfMemory)?;
    let growth_bytes = vec_capacity_growth_bytes::<Value>(self.heap.root_stack.capacity(), new_len);

    if growth_bytes != 0 {
      // Ensure `values` (and `extra_roots`) are treated as roots if this triggers a GC.
      self.heap.ensure_can_allocate_with_extra_roots(
        |_| growth_bytes,
        values,
        extra_roots,
        extra_env_roots,
        &[],
      )?;
      reserve_vec_to_len::<Value>(&mut self.heap.root_stack, new_len)?;
    }
    for value in values {
      self.heap.root_stack.push(*value);
    }
    Ok(())
  }

  pub fn push_env_root(&mut self, env: GcEnv) -> Result<GcEnv, VmError> {
    debug_assert!(self.heap.is_valid_env(env));

    let new_len = self
      .heap
      .env_root_stack
      .len()
      .checked_add(1)
      .ok_or(VmError::OutOfMemory)?;
    let growth_bytes = vec_capacity_growth_bytes::<GcEnv>(self.heap.env_root_stack.capacity(), new_len);

    if growth_bytes != 0 {
      // Ensure `env` is treated as a root if this triggers a GC while we grow `env_root_stack`.
      let envs = [env];
      self.heap.ensure_can_allocate_with_extra_roots(|_| growth_bytes, &[], &[], &envs, &[])?;
      reserve_vec_to_len::<GcEnv>(&mut self.heap.env_root_stack, new_len)?;
    }

    self.heap.env_root_stack.push(env);
    Ok(env)
  }

  /// Creates a nested child scope that borrows the same heap.
  pub fn reborrow(&mut self) -> Scope<'_> {
    let root_stack_len_at_entry = self.heap.root_stack.len();
    let env_root_stack_len_at_entry = self.heap.env_root_stack.len();
    Scope {
      heap: &mut *self.heap,
      root_stack_len_at_entry,
      env_root_stack_len_at_entry,
    }
  }

  /// Borrows the underlying heap immutably.
  pub fn heap(&self) -> &Heap {
    &*self.heap
  }

  /// Borrows the underlying heap mutably.
  pub fn heap_mut(&mut self) -> &mut Heap {
    &mut *self.heap
  }

  /// Allocates a JavaScript string on the heap from UTF-8.
  pub fn alloc_string_from_utf8(&mut self, s: &str) -> Result<GcString, VmError> {
    let units_len = s.encode_utf16().count();
    let new_bytes = JsString::heap_size_bytes_for_len(units_len);
    self.heap.ensure_can_allocate(new_bytes)?;

    // Allocate the backing buffer fallibly so hostile input cannot abort the
    // host process on allocator OOM.
    let mut units: Vec<u16> = Vec::new();
    units
      .try_reserve_exact(units_len)
      .map_err(|_| VmError::OutOfMemory)?;
    units.extend(s.encode_utf16());
    let js = JsString::from_u16_vec(units);
    debug_assert_eq!(new_bytes, js.heap_size_bytes());
    let obj = HeapObject::String(js);
    Ok(GcString(self.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates a JavaScript string on the heap from UTF-16 code units.
  pub fn alloc_string_from_code_units(&mut self, units: &[u16]) -> Result<GcString, VmError> {
    let new_bytes = JsString::heap_size_bytes_for_len(units.len());
    self.heap.ensure_can_allocate(new_bytes)?;

    // Fallible allocation for the backing buffer (avoid process abort on OOM).
    let mut buf: Vec<u16> = Vec::new();
    buf
      .try_reserve_exact(units.len())
      .map_err(|_| VmError::OutOfMemory)?;
    buf.extend_from_slice(units);

    let js = JsString::from_u16_vec(buf);
    debug_assert_eq!(new_bytes, js.heap_size_bytes());
    let obj = HeapObject::String(js);
    Ok(GcString(self.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Ensures that allocating a string with `units_len` UTF-16 code units would not exceed heap
  /// limits.
  ///
  /// This is useful for built-ins that need to allocate an intermediate `Vec<u16>` of the final
  /// string size: without this pre-check, attacker-controlled length arguments could force the host
  /// process to allocate large buffers outside the GC heap before we notice the heap limit would be
  /// exceeded.
  ///
  /// Callers must ensure any values that should remain live across a potential GC are rooted before
  /// invoking this method.
  pub fn ensure_can_alloc_string_units(&mut self, units_len: usize) -> Result<(), VmError> {
    let new_bytes = JsString::heap_size_bytes_for_len(units_len);
    self.heap.ensure_can_allocate(new_bytes)
  }

  /// Allocates a JavaScript string on the heap from a UTF-16 code unit buffer.
  pub fn alloc_string_from_u16_vec(&mut self, units: Vec<u16>) -> Result<GcString, VmError> {
    let new_bytes = JsString::heap_size_bytes_for_len(units.len());
    self.heap.ensure_can_allocate(new_bytes)?;

    let js = JsString::from_u16_vec(units);
    debug_assert_eq!(new_bytes, js.heap_size_bytes());
    let obj = HeapObject::String(js);
    Ok(GcString(self.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Convenience alias for [`Scope::alloc_string_from_utf8`].
  pub fn alloc_string(&mut self, s: &str) -> Result<GcString, VmError> {
    self.alloc_string_from_utf8(s)
  }

  /// Allocates a JavaScript symbol on the heap.
  pub fn new_symbol(&mut self, description: Option<GcString>) -> Result<GcSymbol, VmError> {
    // Root the description string during allocation in case `ensure_can_allocate` triggers a GC.
    //
    // Note: `description` does not need to remain rooted after allocation; the symbol itself
    // retains a handle and will trace it.
    let mut scope = self.reborrow();
    if let Some(desc) = description {
      scope.push_root(Value::String(desc))?;
    }

    let new_bytes = 0;
    scope.heap.ensure_can_allocate(new_bytes)?;

    let id = scope.heap.next_symbol_id;
    scope.heap.next_symbol_id = scope.heap.next_symbol_id.wrapping_add(1);

    let obj = HeapObject::Symbol(JsSymbol::new(id, description));
    Ok(GcSymbol(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Convenience allocation for `Symbol(description)` where `description` is UTF-8.
  pub fn alloc_symbol(&mut self, description: Option<&str>) -> Result<GcSymbol, VmError> {
    let description = match description {
      Some(s) => Some(self.alloc_string(s)?),
      None => None,
    };
    self.new_symbol(description)
  }

  /// Allocates an empty JavaScript object on the heap.
  pub fn alloc_object(&mut self) -> Result<GcObject, VmError> {
    let new_bytes = JsObject::heap_size_bytes_for_property_count(0);
    self.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Object(JsObject::new(None));
    Ok(GcObject(self.heap.alloc_unchecked(obj, new_bytes)?))
  }
  /// Allocates an empty `WeakSet` object on the heap.
  pub fn alloc_weak_set(&mut self) -> Result<GcObject, VmError> {
    let new_bytes = JsWeakSet::heap_size_bytes_for_counts(0, 0);
    self.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::WeakSet(JsWeakSet::new(None));
    Ok(GcObject(self.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates a JavaScript Date object on the heap.
  pub fn alloc_date(&mut self, value: f64) -> Result<GcObject, VmError> {
    let new_bytes = JsObject::heap_size_bytes_for_property_count(0);
    self.heap.ensure_can_allocate(new_bytes)?;

    let obj = JsObject {
      base: ObjectBase {
        prototype: None,
        extensible: true,
        properties: Box::default(),
        kind: ObjectKind::Date(DateObject { value }),
      },
    };
    Ok(GcObject(self.heap.alloc_unchecked(HeapObject::Object(obj), new_bytes)?))
  }
  /// Allocates an ordinary object with the provided `[[Prototype]]` and own properties.
  pub fn alloc_object_with_properties(
    &mut self,
    proto: Option<GcObject>,
    props: &[(PropertyKey, PropertyDescriptor)],
  ) -> Result<GcObject, VmError> {
    // Root the prototype and all keys/values during allocation in case `ensure_can_allocate`
    // triggers a GC cycle.
    //
    // Note: these roots are temporary; once the object is allocated, it will retain handles and
    // trace them.
    let mut scope = self.reborrow();
    let max_roots = proto.is_some() as usize + props.len().saturating_mul(3);
    let mut roots: Vec<Value> = Vec::new();
    roots
      .try_reserve_exact(max_roots)
      .map_err(|_| VmError::OutOfMemory)?;
    if let Some(proto) = proto {
      roots.push(Value::Object(proto));
    }
    for (key, desc) in props {
      roots.push(match key {
        PropertyKey::String(s) => Value::String(*s),
        PropertyKey::Symbol(s) => Value::Symbol(*s),
      });
      match desc.kind {
        PropertyKind::Data { value, .. } => {
          roots.push(value);
        }
        PropertyKind::Accessor { get, set } => {
          roots.push(get);
          roots.push(set);
        }
      }
    }
    scope.push_roots(&roots)?;

    let new_bytes = JsObject::heap_size_bytes_for_property_count(props.len());
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Object(JsObject::from_property_slice(proto, props)?);
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates an empty JavaScript object on the heap with an explicit internal prototype.
  pub fn alloc_object_with_prototype(
    &mut self,
    prototype: Option<GcObject>,
  ) -> Result<GcObject, VmError> {
    self.alloc_object_with_properties(prototype, &[])
  }

  /// Allocates a JavaScript array exotic object on the heap.
  ///
  /// The array's `length` internal slot is initialised to `len`.
  ///
  /// Note: `[[Prototype]]` is initialised to `None` and should be set by the caller.
  pub fn alloc_array(&mut self, len: usize) -> Result<GcObject, VmError> {
    let len_u32 =
      u32::try_from(len).map_err(|_| VmError::Unimplemented("array length exceeds u32"))?;

    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    let length_key = scope.alloc_string("length")?;
    scope.push_root(Value::String(length_key))?;

    // Build the initial property table containing the (non-enumerable) `length` data property.
    //
    // This gives arrays the expected `Reflect.ownKeys`/`[[OwnPropertyKeys]]`-style key order:
    // indices first, then `length`.
    let mut buf: Vec<PropertyEntry> = Vec::new();
    buf.try_reserve_exact(1).map_err(|_| VmError::OutOfMemory)?;
    buf.push(PropertyEntry {
      key: PropertyKey::from_string(length_key),
      desc: PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Number(len_u32 as f64),
          writable: true,
        },
      },
    });

    let properties = buf.into_boxed_slice();
    let new_bytes = JsObject::heap_size_bytes_for_property_count(properties.len());
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = JsObject {
      base: ObjectBase {
        prototype: None,
        extensible: true,
        properties,
        kind: ObjectKind::Array(ArrayObject { length: len_u32 }),
      },
    };
    Ok(GcObject(
      scope
        .heap
        .alloc_unchecked(HeapObject::Object(obj), new_bytes)?,
    ))
  }

  /// Allocates a Proxy exotic object on the heap.
  ///
  /// If either `target` or `handler` is `None`, the Proxy is considered **revoked**.
  pub fn alloc_proxy(
    &mut self,
    target: Option<GcObject>,
    handler: Option<GcObject>,
  ) -> Result<GcObject, VmError> {
    if target.is_some() != handler.is_some() {
      return Err(VmError::InvariantViolation(
        "Proxy allocation requires both target and handler (or neither for a revoked proxy)",
      ));
    }

    // Validate handles up-front.
    if let Some(target) = target {
      if !self.heap.is_valid_object(target) {
        return Err(VmError::invalid_handle());
      }
    }
    if let Some(handler) = handler {
      if !self.heap.is_valid_object(handler) {
        return Err(VmError::invalid_handle());
      }
    }

    // Root inputs during allocation in case root-stack growth, `ensure_can_allocate`, or slot-table
    // growth triggers GC.
    //
    let mut roots = [Value::Undefined; 2];
    let mut root_count: usize = 0;
    if let Some(target) = target {
      roots[root_count] = Value::Object(target);
      root_count += 1;
    }
    if let Some(handler) = handler {
      roots[root_count] = Value::Object(handler);
      root_count += 1;
    }

    // Push all roots in one `push_roots` call so root-stack growth can't trigger a GC between them
    // (which could collect e.g. `handler` before it is rooted).
    let mut scope = self.reborrow();
    scope.push_roots(&roots[..root_count])?;

    // Proxies have no heap-owned payload allocations today; they store only GC handles inline.
    let new_bytes = 0usize;
    scope.heap.ensure_can_allocate(new_bytes)?;
    let obj = HeapObject::Proxy(JsProxy { target, handler });
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }
  /// Revokes a Proxy exotic object by clearing its target and handler.
  pub fn revoke_proxy(&mut self, proxy: GcObject) -> Result<(), VmError> {
    self.heap.proxy_revoke(proxy)
  }

  /// Allocates a new `ArrayBuffer` object with `byte_length` zero-initialized bytes.
  ///
  /// Note: `[[Prototype]]` is initialised to `None` and should be set by the caller.
  pub fn alloc_array_buffer(&mut self, byte_length: usize) -> Result<GcObject, VmError> {
    let new_bytes = JsArrayBuffer::heap_size_bytes_for_property_count(0);
    self.heap.ensure_can_allocate_with(|heap| {
      heap
        .additional_bytes_for_heap_alloc(new_bytes)
        .saturating_add(byte_length)
    })?;
  
    // Allocate the backing buffer fallibly so hostile input cannot abort the host process on OOM.
    let mut buf: Vec<u8> = Vec::new();
    buf
      .try_reserve_exact(byte_length)
      .map_err(|_| VmError::OutOfMemory)?;
    buf.resize(byte_length, 0);
    let data = buf.into_boxed_slice();
 
    let obj = HeapObject::ArrayBuffer(JsArrayBuffer::new(None, data));
    let id = self.heap.alloc_unchecked_after_ensure(obj, new_bytes)?;
    self.heap.add_external_bytes(byte_length);
    Ok(GcObject(id))
  }

  /// Allocates a new `ArrayBuffer` object backed by the provided bytes.
  ///
  /// Note: `[[Prototype]]` is initialised to `None` and should be set by the caller.
  pub fn alloc_array_buffer_from_u8_vec(&mut self, bytes: Vec<u8>) -> Result<GcObject, VmError> {
    let byte_length = bytes.len();
    let new_bytes = JsArrayBuffer::heap_size_bytes_for_property_count(0);
    self.heap.ensure_can_allocate_with(|heap| {
      heap
        .additional_bytes_for_heap_alloc(new_bytes)
        .saturating_add(byte_length)
    })?;

    // Avoid process abort on allocator OOM: if `bytes` has spare capacity, converting it to a boxed
    // slice may reallocate. Preserve the original allocation only when it's already exact-sized.
    let data: Box<[u8]> = if bytes.capacity() == bytes.len() {
      bytes.into_boxed_slice()
    } else {
      let mut buf: Vec<u8> = Vec::new();
      buf
        .try_reserve_exact(byte_length)
        .map_err(|_| VmError::OutOfMemory)?;
      buf.extend_from_slice(&bytes);
      buf.into_boxed_slice()
    };

    let obj = HeapObject::ArrayBuffer(JsArrayBuffer::new(None, data));
    let id = self.heap.alloc_unchecked_after_ensure(obj, new_bytes)?;
    self.heap.add_external_bytes(byte_length);
    Ok(GcObject(id))
  }

  pub(crate) fn alloc_typed_array(
    &mut self,
    kind: TypedArrayKind,
    viewed_array_buffer: GcObject,
    byte_offset: usize,
    length: usize,
  ) -> Result<GcObject, VmError> {
    // Root the buffer during validation/allocation in case `alloc_unchecked` triggers a GC.
    let mut scope = self.reborrow();
    scope.push_root(Value::Object(viewed_array_buffer))?;

    let buf = scope.heap.get_array_buffer(viewed_array_buffer)?;
    if buf.data.is_none() {
      return Err(VmError::TypeError("TypedArray view over detached ArrayBuffer"));
    }
    let buf_len = buf.byte_length();
    let bytes_per_element = kind.bytes_per_element();
    if byte_offset % bytes_per_element != 0 {
      return Err(VmError::TypeError("TypedArray byteOffset must be aligned"));
    }
    let byte_length = length
      .checked_mul(bytes_per_element)
      .ok_or(VmError::OutOfMemory)?;
    let end = byte_offset.checked_add(byte_length).ok_or(VmError::OutOfMemory)?;
    if end > buf_len {
      return Err(VmError::TypeError("TypedArray view out of bounds"));
    }

    let new_bytes = JsTypedArray::heap_size_bytes_for_property_count(0);
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::TypedArray(JsTypedArray::new(
      None,
      kind,
      viewed_array_buffer,
      byte_offset,
      length,
    ));
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates a new `Uint8Array` view over `viewed_array_buffer`.
  ///
  /// Note: `[[Prototype]]` is initialised to `None` and should be set by the caller.
  pub fn alloc_uint8_array(
    &mut self,
    viewed_array_buffer: GcObject,
    byte_offset: usize,
    length: usize,
  ) -> Result<GcObject, VmError> {
    self.alloc_typed_array(
      TypedArrayKind::Uint8,
      viewed_array_buffer,
      byte_offset,
      length,
    )
  }

  /// Allocates a new `DataView` view over `viewed_array_buffer`.
  ///
  /// Note: `[[Prototype]]` is initialised to `None` and should be set by the caller.
  pub(crate) fn alloc_data_view(
    &mut self,
    viewed_array_buffer: GcObject,
    byte_offset: usize,
    byte_length: usize,
  ) -> Result<GcObject, VmError> {
    // Root the buffer during validation/allocation in case `alloc_unchecked` triggers a GC.
    let mut scope = self.reborrow();
    scope.push_root(Value::Object(viewed_array_buffer))?;

    let buf_len = scope.heap.get_array_buffer(viewed_array_buffer)?.byte_length();
    let end = byte_offset
      .checked_add(byte_length)
      .ok_or(VmError::OutOfMemory)?;
    if end > buf_len {
      return Err(VmError::TypeError("DataView view out of bounds"));
    }

    let new_bytes = JsDataView::heap_size_bytes_for_property_count(0);
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::DataView(JsDataView::new(
      None,
      viewed_array_buffer,
      byte_offset,
      byte_length,
    ));
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates a new Generator object on the heap.
  pub fn alloc_generator(
    &mut self,
    this_value: Value,
    args: &[Value],
    env: Option<GcEnv>,
  ) -> Result<GcObject, VmError> {
    self.alloc_generator_with_prototype(None, this_value, args, env)
  }

  /// Allocates a new Generator object on the heap with an explicit `[[Prototype]]`.
  pub fn alloc_generator_with_prototype(
    &mut self,
    prototype: Option<GcObject>,
    this_value: Value,
    args: &[Value],
    env: Option<GcEnv>,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    if let Some(proto) = prototype {
      if !scope.heap().is_valid_object(proto) {
        return Err(VmError::invalid_handle());
      }
    }
    if !scope.heap().debug_value_is_valid_or_primitive(this_value) {
      return Err(VmError::invalid_handle());
    }
    for &arg in args {
      if !scope.heap().debug_value_is_valid_or_primitive(arg) {
        return Err(VmError::invalid_handle());
      }
    }
    if let Some(env) = env {
      if !scope.heap().is_valid_env(env) {
        return Err(VmError::invalid_handle());
      }
    }

    let max_roots = prototype.is_some() as usize
      + 1usize // this
      + args.len();
    let mut roots: Vec<Value> = Vec::new();
    roots
      .try_reserve_exact(max_roots)
      .map_err(|_| VmError::OutOfMemory)?;
    if let Some(proto) = prototype {
      roots.push(Value::Object(proto));
    }
    roots.push(this_value);
    roots.extend_from_slice(args);

    if let Some(env) = env {
      // Treat `env` as an extra env root while growing the value root stack.
      scope.push_roots_with_extra_roots(&roots, &[], &[env])?;
      scope.push_env_root(env)?;
    } else {
      scope.push_roots(&roots)?;
    }

    let args: Option<Box<[Value]>> = if args.is_empty() {
      None
    } else {
      // Allocate the args buffer fallibly so hostile inputs cannot abort the host process on OOM.
      let mut buf: Vec<Value> = Vec::new();
      buf
        .try_reserve_exact(args.len())
        .map_err(|_| VmError::OutOfMemory)?;
      buf.extend_from_slice(args);
      Some(buf.into_boxed_slice())
    };

    let gen = JsGenerator::new(prototype, this_value, args, env);
    let new_bytes = gen.heap_size_bytes();
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Generator(gen);
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates a new pending Promise object on the heap.
  pub fn alloc_promise(&mut self) -> Result<GcObject, VmError> {
    self.alloc_promise_with_prototype(None)
  }

  /// Allocates a new pending Promise object on the heap with an explicit `[[Prototype]]`.
  pub fn alloc_promise_with_prototype(
    &mut self,
    prototype: Option<GcObject>,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    if let Some(proto) = prototype {
      scope.push_root(Value::Object(proto))?;
    }

    let new_bytes = JsPromise::heap_size_bytes_for_counts(0, 0, 0);
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Promise(JsPromise::new(prototype));
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }
  /// Defines (adds or replaces) an own property on `obj`.
  pub fn define_property(
    &mut self,
    obj: GcObject,
    key: PropertyKey,
    desc: PropertyDescriptor,
  ) -> Result<(), VmError> {
    // Root inputs for the duration of the operation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    let mut roots = [Value::Undefined; 4];
    let mut root_count = 0usize;
    roots[root_count] = Value::Object(obj);
    root_count += 1;
    roots[root_count] = match key {
      PropertyKey::String(s) => Value::String(s),
      PropertyKey::Symbol(s) => Value::Symbol(s),
    };
    root_count += 1;
    match desc.kind {
      PropertyKind::Data { value, .. } => {
        roots[root_count] = value;
        root_count += 1;
      }
      PropertyKind::Accessor { get, set } => {
        roots[root_count] = get;
        root_count += 1;
        roots[root_count] = set;
        root_count += 1;
      }
    };
    scope.push_roots(&roots[..root_count])?;

    scope.heap.define_property(obj, key, desc)
  }

  /// Appends a reaction record to `promise.[[PromiseFulfillReactions]]`.
  pub fn promise_append_fulfill_reaction(
    &mut self,
    promise: GcObject,
    reaction: PromiseReaction,
  ) -> Result<(), VmError> {
    debug_assert_eq!(reaction.type_, PromiseReactionType::Fulfill);

    // Root inputs for the duration of the operation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    let mut roots = [Value::Undefined; 5];
    let mut root_count = 0usize;
    roots[root_count] = Value::Object(promise);
    root_count += 1;
    if let Some(handler) = &reaction.handler {
      roots[root_count] = Value::Object(handler.callback_object());
      root_count += 1;
    }
    if let Some(cap) = &reaction.capability {
      roots[root_count] = cap.promise;
      root_count += 1;
      roots[root_count] = cap.resolve;
      root_count += 1;
      roots[root_count] = cap.reject;
      root_count += 1;
    }
    scope.push_roots(&roots[..root_count])?;

    scope.heap.promise_append_reaction(promise, true, reaction)
  }

  /// Appends a reaction record to `promise.[[PromiseRejectReactions]]`.
  pub fn promise_append_reject_reaction(
    &mut self,
    promise: GcObject,
    reaction: PromiseReaction,
  ) -> Result<(), VmError> {
    debug_assert_eq!(reaction.type_, PromiseReactionType::Reject);

    // Root inputs for the duration of the operation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    let mut roots = [Value::Undefined; 5];
    let mut root_count = 0usize;
    roots[root_count] = Value::Object(promise);
    root_count += 1;
    if let Some(handler) = &reaction.handler {
      roots[root_count] = Value::Object(handler.callback_object());
      root_count += 1;
    }
    if let Some(cap) = &reaction.capability {
      roots[root_count] = cap.promise;
      root_count += 1;
      roots[root_count] = cap.resolve;
      root_count += 1;
      roots[root_count] = cap.reject;
      root_count += 1;
    }
    scope.push_roots(&roots[..root_count])?;

    scope.heap.promise_append_reaction(promise, false, reaction)
  }

  /// Allocates a native JavaScript function object on the heap.
  pub fn alloc_native_function(
    &mut self,
    call: NativeFunctionId,
    construct: Option<NativeConstructId>,
    name: GcString,
    length: u32,
  ) -> Result<GcObject, VmError> {
    self.alloc_native_function_with_slots_and_env(call, construct, name, length, &[], None)
  }

  /// Allocates a native JavaScript function object with a captured Environment Record.
  pub fn alloc_native_function_with_env(
    &mut self,
    call: NativeFunctionId,
    construct: Option<NativeConstructId>,
    name: GcString,
    length: u32,
    closure_env: Option<GcEnv>,
  ) -> Result<GcObject, VmError> {
    self.alloc_native_function_with_slots_and_env(call, construct, name, length, &[], closure_env)
  }

  /// Allocates a native JavaScript function object with captured native slots.
  pub fn alloc_native_function_with_slots(
    &mut self,
    call: NativeFunctionId,
    construct: Option<NativeConstructId>,
    name: GcString,
    length: u32,
    slots: &[Value],
  ) -> Result<GcObject, VmError> {
    self.alloc_native_function_with_slots_and_env(call, construct, name, length, slots, None)
  }

  pub(crate) fn alloc_native_function_with_slots_and_env(
    &mut self,
    call: NativeFunctionId,
    construct: Option<NativeConstructId>,
    name: GcString,
    length: u32,
    slots: &[Value],
    closure_env: Option<GcEnv>,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    // Root `name` and `slots` in a way that's robust against GC triggering while we grow the root
    // stack.
    //
    // `push_root`/`push_roots` can trigger GC when growing `root_stack`, so ensure any not-yet-pushed
    // values are treated as extra roots during that operation.
    let name_root = [Value::String(name)];
    if let Some(env) = closure_env {
      scope.push_roots_with_extra_roots(&name_root, slots, &[env])?;
      scope.push_roots_with_extra_roots(slots, &[], &[env])?;
      scope.push_env_root(env)?;
    } else {
      scope.push_roots_with_extra_roots(&name_root, slots, &[])?;
      scope.push_roots(slots)?;
    }

    let native_slots: Option<Box<[Value]>> = if slots.is_empty() {
      None
    } else {
      // Allocate the slot buffer fallibly so hostile inputs cannot abort the host process on
      // allocator OOM.
      let mut buf: Vec<Value> = Vec::new();
      buf
        .try_reserve_exact(slots.len())
        .map_err(|_| VmError::OutOfMemory)?;
      buf.extend_from_slice(slots);
      Some(buf.into_boxed_slice())
    };

    let func = JsFunction::new_native_with_slots_and_env(
      call,
      construct,
      name,
      length,
      native_slots,
      closure_env,
    );
    let new_bytes = func.heap_size_bytes();
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Function(func);
    let func = GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?);

    // Define standard function properties.
    crate::function_properties::set_function_name(
      &mut scope,
      func,
      PropertyKey::String(name),
      None,
    )?;
    crate::function_properties::set_function_length(&mut scope, func, length)?;

    // Constructors get a `.prototype` object.
    if construct.is_some() {
      crate::function_properties::make_constructor(&mut scope, func)?;
    }

    Ok(func)
  }

  pub fn alloc_declarative_env_record(
    &mut self,
    outer: Option<GcEnv>,
    bindings: &[EnvBinding],
  ) -> Result<GcEnv, VmError> {
    let mut scope = self.reborrow();
    // Root inputs for the duration of allocation in case `ensure_can_allocate` triggers a GC.
    //
    // Rooting must be robust against GC triggering while we grow root stacks: if GC runs while
    // we're pushing roots, any not-yet-pushed binding values could otherwise be collected.
    if let Some(outer) = outer {
      if !scope.heap().is_valid_env(outer) {
        return Err(VmError::invalid_handle());
      }
    }

    // Collect all `Value` roots (binding names + direct values) and env roots (outer + indirect
    // target environments) so we can push them in GC-safe batches.
    let max_value_roots = bindings.len().saturating_mul(2);
    let mut value_roots: Vec<Value> = Vec::new();
    value_roots
      .try_reserve_exact(max_value_roots)
      .map_err(|_| VmError::OutOfMemory)?;

    let mut env_roots: Vec<GcEnv> = Vec::new();
    if let Some(outer) = outer {
      env_roots.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
      env_roots.push(outer);
    }

    for binding in bindings {
      if let Some(name) = binding.name {
        value_roots.push(Value::String(name));
      }
      match binding.value {
        EnvBindingValue::Direct(value) => value_roots.push(value),
        EnvBindingValue::Indirect { env, name } => {
          value_roots.push(Value::String(name));
          if !scope.heap().is_valid_env(env) {
            return Err(VmError::invalid_handle());
          }
          env_roots.try_reserve(1).map_err(|_| VmError::OutOfMemory)?;
          env_roots.push(env);
        }
      }
    }

    scope.push_roots_with_extra_roots(&value_roots, &[], &env_roots)?;

    if !env_roots.is_empty() {
      // Reserve `env_root_stack` capacity in one go so pushing individual env roots cannot allocate
      // and trigger GC while some roots are still only held in local variables.
      let new_len = scope
        .heap
        .env_root_stack
        .len()
        .checked_add(env_roots.len())
        .ok_or(VmError::OutOfMemory)?;
      let growth_bytes = vec_capacity_growth_bytes::<GcEnv>(scope.heap.env_root_stack.capacity(), new_len);
      if growth_bytes != 0 {
        scope.heap.ensure_can_allocate_with_extra_roots(|_| growth_bytes, &value_roots, &[], &env_roots, &[])?;
        reserve_vec_to_len::<GcEnv>(&mut scope.heap.env_root_stack, new_len)?;
      }
      for env in &env_roots {
        scope.heap.env_root_stack.push(*env);
      }
    }

    let new_bytes = EnvRecord::heap_size_bytes_for_binding_count(bindings.len());
    scope.heap.ensure_can_allocate(new_bytes)?;

    // Allocate the backing buffer fallibly so hostile inputs cannot abort the host process
    // on allocator OOM.
    let mut buf: Vec<EnvBinding> = Vec::new();
    buf
      .try_reserve_exact(bindings.len())
      .map_err(|_| VmError::OutOfMemory)?;
    buf.extend_from_slice(bindings);

    // Keep the table deterministic by sorting by `SymbolId`.
    buf.sort_by_key(|binding| binding.symbol);
    if buf
      .windows(2)
      .any(|pair| pair[0].symbol == pair[1].symbol)
    {
      return Err(VmError::Unimplemented("duplicate env binding"));
    }

    let bindings = buf.into_boxed_slice();
    let env = EnvRecord::new_with_bindings(outer, bindings);

    let obj = HeapObject::Env(env);
    Ok(GcEnv(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Convenience alias for [`Scope::alloc_declarative_env_record`].
  pub fn alloc_env_record(&mut self, outer: Option<GcEnv>, bindings: &[EnvBinding]) -> Result<GcEnv, VmError> {
    self.alloc_declarative_env_record(outer, bindings)
  }

  pub fn alloc_object_env_record(
    &mut self,
    binding_object: GcObject,
    outer: Option<GcEnv>,
    with_environment: bool,
  ) -> Result<GcEnv, VmError> {
    let mut scope = self.reborrow();
    // Root inputs for the duration of allocation in case `ensure_can_allocate` triggers a GC.
    //
    // Rooting must be robust against GC triggering while we grow root stacks: if GC runs while
    // we're pushing one root, any not-yet-pushed roots could otherwise be collected.
    if !scope.heap().is_valid_object(binding_object) {
      return Err(VmError::invalid_handle());
    }
    if let Some(outer) = outer {
      if !scope.heap().is_valid_env(outer) {
        return Err(VmError::invalid_handle());
      }
    }

    let value_roots = [Value::Object(binding_object)];
    if let Some(outer) = outer {
      // Treat `outer` as an extra env root while growing the value root stack.
      scope.push_roots_with_extra_roots(&value_roots, &[], &[outer])?;
      scope.push_env_root(outer)?;
    } else {
      scope.push_roots(&value_roots)?;
    }

    let new_bytes = ObjectEnvRecord::heap_size_bytes();
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Env(EnvRecord::Object(ObjectEnvRecord {
      outer,
      binding_object,
      with_environment,
    }));
    Ok(GcEnv(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  pub fn env_create(&mut self, outer: Option<GcEnv>) -> Result<GcEnv, VmError> {
    // Root `outer` during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    if let Some(outer) = outer {
      scope.push_env_root(outer)?;
    }

    let new_bytes = EnvRecord::heap_size_bytes_for_binding_count(0);
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Env(EnvRecord::new(outer));
    Ok(GcEnv(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  pub fn env_create_mutable_binding(
    &mut self,
    env: GcEnv,
    name: &str,
  ) -> Result<(), VmError> {
    if self.heap().env_has_binding(env, name)? {
      return Err(VmError::Unimplemented("duplicate binding"));
    }

    let mut scope = self.reborrow();
    scope.push_env_root(env)?;

    let name = scope.alloc_string(name)?;
    scope.push_root(Value::String(name))?;

    scope.heap.env_add_binding(
      env,
      EnvBinding {
        symbol: SymbolId::from_raw(name.id().0),
        name: Some(name),
        value: EnvBindingValue::Direct(Value::Undefined),
        mutable: true,
        initialized: false,
        strict: false,
      },
    )
  }

  pub(crate) fn env_create_immutable_binding(
    &mut self,
    env: GcEnv,
    name: &str,
  ) -> Result<(), VmError> {
    if self.heap().env_has_binding(env, name)? {
      return Err(VmError::Unimplemented("duplicate binding"));
    }

    let mut scope = self.reborrow();
    scope.push_env_root(env)?;

    let name = scope.alloc_string(name)?;
    scope.push_root(Value::String(name))?;

    scope.heap.env_add_binding(
      env,
      EnvBinding {
        symbol: SymbolId::from_raw(name.id().0),
        name: Some(name),
        value: EnvBindingValue::Direct(Value::Undefined),
        mutable: false,
        initialized: false,
        strict: false,
      },
    )
  }

  pub(crate) fn env_create_import_binding(
    &mut self,
    env: GcEnv,
    name: &str,
    target_env: GcEnv,
    target_name: &str,
  ) -> Result<(), VmError> {
    if self.heap().env_has_binding(env, name)? {
      return Err(VmError::Unimplemented("duplicate binding"));
    }

    let mut scope = self.reborrow();
    if !scope.heap().is_valid_env(env) || !scope.heap().is_valid_env(target_env) {
      return Err(VmError::invalid_handle());
    }
    // Root inputs across allocations/GC while creating the strings and growing the binding table.
    scope.push_env_root(env)?;
    scope.push_env_root(target_env)?;

    let local_name = scope.alloc_string(name)?;
    scope.push_root(Value::String(local_name))?;
    let target_name = scope.alloc_string(target_name)?;
    scope.push_root(Value::String(target_name))?;

    scope.heap.env_add_binding(
      env,
      EnvBinding {
        symbol: SymbolId::from_raw(local_name.id().0),
        name: Some(local_name),
        value: EnvBindingValue::Indirect {
          env: target_env,
          name: target_name,
        },
        mutable: false,
        // Import bindings are initialized immediately; they forward reads to the exporting module.
        initialized: true,
        strict: true,
      },
    )
  }

  /// Allocates an ECMAScript (user-defined) function object on the heap.
  pub fn alloc_ecma_function(
    &mut self,
    code: EcmaFunctionId,
    is_constructable: bool,
    name: GcString,
    length: u32,
    this_mode: ThisMode,
    is_strict: bool,
    closure_env: Option<GcEnv>,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    if let Some(env) = closure_env {
      let roots = [Value::String(name)];
      scope.push_roots_with_extra_roots(&roots, &[], &[env])?;
      scope.push_env_root(env)?;
    } else {
      scope.push_root(Value::String(name))?;
    }

    let func = JsFunction::new_ecma(
      code,
      is_constructable,
      name,
      length,
      this_mode,
      is_strict,
      closure_env,
    );
    let new_bytes = func.heap_size_bytes();
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Function(func);
    let func = GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?);

    // Define standard function properties.
    crate::function_properties::set_function_name(
      &mut scope,
      func,
      PropertyKey::String(name),
      None,
    )?;
    crate::function_properties::set_function_length(&mut scope, func, length)?;

    // Constructors get a `.prototype` object.
    if is_constructable {
      crate::function_properties::make_constructor(&mut scope, func)?;
    }

    Ok(func)
  }

  /// Allocates a JavaScript bound function object on the heap.
  ///
  /// This creates an ordinary function object with the `[[BoundTargetFunction]]`,
  /// `[[BoundThis]]`, and `[[BoundArguments]]` internal slots populated.
  ///
  /// Note: we intentionally do not define standard function properties (`name`, `length`,
  /// `prototype`) here; callers are expected to define `name`/`length` per ECMA-262 as needed.
  pub(crate) fn alloc_bound_function_raw(
    &mut self,
    call: CallHandler,
    construct: Option<ConstructHandler>,
    name: GcString,
    length: u32,
    bound_target: GcObject,
    bound_this: Value,
    bound_args: Option<Box<[Value]>>,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    let bound_args_len = bound_args.as_deref().map(|args| args.len()).unwrap_or(0);
    let max_roots = 3usize.saturating_add(bound_args_len);
    let mut roots: Vec<Value> = Vec::new();
    roots
      .try_reserve_exact(max_roots)
      .map_err(|_| VmError::OutOfMemory)?;
    roots.push(Value::String(name));
    roots.push(Value::Object(bound_target));
    roots.push(bound_this);
    if let Some(bound_args) = bound_args.as_deref() {
      roots.extend_from_slice(bound_args);
    }
    scope.push_roots(&roots)?;

    let mut func = JsFunction::new_with_handlers(call, construct, name, length);
    func.bound_target = Some(bound_target);
    func.bound_this = Some(bound_this);
    func.bound_args = bound_args;

    let new_bytes = func.heap_size_bytes();
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Function(func);
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates a user-defined JavaScript function object on the heap.
  pub fn alloc_user_function(
    &mut self,
    func: CompiledFunctionRef,
    name: GcString,
    length: u32,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    scope.push_root(Value::String(name))?;

    let func = JsFunction::new_user(
      func,
      name,
      length,
      ThisMode::Global,
      /* is_strict */ false,
      /* closure_env */ None,
    );
    let new_bytes = func.heap_size_bytes();
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Function(func);
    let func = GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?);

    // Define standard function metadata properties (`name`, `length`).
    crate::function_properties::set_function_name(
      &mut scope,
      func,
      PropertyKey::String(name),
      None,
    )?;
    crate::function_properties::set_function_length(&mut scope, func, length)?;

    Ok(func)
  }

  pub(crate) fn alloc_user_function_with_env(
    &mut self,
    func: CompiledFunctionRef,
    name: GcString,
    length: u32,
    this_mode: ThisMode,
    is_strict: bool,
    closure_env: Option<GcEnv>,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    if let Some(env) = closure_env {
      let roots = [Value::String(name)];
      scope.push_roots_with_extra_roots(&roots, &[], &[env])?;
      scope.push_env_root(env)?;
    } else {
      scope.push_root(Value::String(name))?;
    }

    let func = JsFunction::new_user(func, name, length, this_mode, is_strict, closure_env);
    let new_bytes = func.heap_size_bytes();
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Function(func);
    let func = GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?);

    // Define standard function metadata properties (`name`, `length`).
    crate::function_properties::set_function_name(
      &mut scope,
      func,
      PropertyKey::String(name),
      None,
    )?;
    crate::function_properties::set_function_length(&mut scope, func, length)?;

    Ok(func)
  }

  pub fn alloc_bound_function(
    &mut self,
    target: GcObject,
    bound_this: Value,
    bound_args: &[Value],
    name: GcString,
    length: u32,
  ) -> Result<GcObject, VmError> {
    // Extract call/construct handlers from `target` without holding a heap borrow across
    // allocations.
    let (target_call, target_construct) = {
      let f = self.heap().get_function(target)?;
      (f.call.clone(), f.construct)
    };

    let bound_args_len = bound_args.len();
    let bound_args = if bound_args.is_empty() {
      None
    } else {
      // Allocate the bound args buffer fallibly so hostile inputs cannot abort the host process
      // on allocator OOM.
      let mut buf: Vec<Value> = Vec::new();
      buf
        .try_reserve_exact(bound_args_len)
        .map_err(|_| VmError::OutOfMemory)?;
      buf.extend_from_slice(bound_args);
      Some(buf.into_boxed_slice())
    };

    self.alloc_bound_function_raw(
      target_call,
      target_construct,
      name,
      length,
      target,
      bound_this,
      bound_args,
    )
  }
}

#[derive(Debug)]
struct Slot {
  generation: u32,
  value: Option<HeapObject>,
  host_slots: Option<HostSlots>,
  bytes: usize,
}

impl Slot {
  fn new() -> Self {
    Self {
      generation: 0,
      value: None,
      host_slots: None,
      bytes: 0,
    }
  }
}

#[derive(Debug)]
enum HeapObject {
  String(JsString),
  Symbol(JsSymbol),
  Object(JsObject),
  ArrayBuffer(JsArrayBuffer),
  TypedArray(JsTypedArray),
  DataView(JsDataView),
  Function(JsFunction),
  Proxy(JsProxy),
  Env(EnvRecord),
  Promise(JsPromise),
  WeakSet(JsWeakSet),
  Generator(JsGenerator),
}

impl Trace for HeapObject {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    match self {
      HeapObject::String(s) => s.trace(tracer),
      HeapObject::Symbol(s) => s.trace(tracer),
      HeapObject::Object(o) => o.trace(tracer),
      HeapObject::ArrayBuffer(b) => b.trace(tracer),
      HeapObject::TypedArray(a) => a.trace(tracer),
      HeapObject::DataView(v) => v.trace(tracer),
      HeapObject::Function(f) => f.trace(tracer),
      HeapObject::Proxy(p) => p.trace(tracer),
      HeapObject::Env(e) => e.trace(tracer),
      HeapObject::Promise(p) => p.trace(tracer),
      HeapObject::WeakSet(ws) => ws.trace(tracer),
      HeapObject::Generator(g) => g.trace(tracer),
    }
  }
}

impl HeapObject {
  fn finalize(&mut self, external_bytes: &mut usize) {
    let freed = match self {
      HeapObject::ArrayBuffer(buf) => buf.finalize(),
      _ => 0,
    };
    if freed == 0 {
      return;
    }
    debug_assert!(
      freed <= *external_bytes,
      "finalizer freed more external bytes than currently tracked (tracked={}, freed={})",
      *external_bytes,
      freed
    );
    *external_bytes = (*external_bytes).saturating_sub(freed);
  }
}

impl Trace for JsString {
  fn trace(&self, _tracer: &mut Tracer<'_>) {
    // Strings have no outgoing GC references.
  }
}

impl Trace for JsSymbol {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    if let Some(desc) = self.description() {
      tracer.trace_heap_id(desc.0);
    }
  }
}

#[derive(Debug)]
struct JsProxy {
  target: Option<GcObject>,
  handler: Option<GcObject>,
}

impl Trace for JsProxy {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    if let Some(target) = self.target {
      tracer.trace_value(Value::Object(target));
    }
    if let Some(handler) = self.handler {
      tracer.trace_value(Value::Object(handler));
    }
  }
}

#[derive(Debug)]
pub(crate) struct ObjectBase {
  prototype: Option<GcObject>,
  extensible: bool,
  properties: Box<[PropertyEntry]>,
  kind: ObjectKind,
}

impl ObjectBase {
  pub(crate) fn new(prototype: Option<GcObject>) -> Self {
    Self {
      prototype,
      extensible: true,
      properties: Box::default(),
      kind: ObjectKind::Ordinary,
    }
  }

  fn from_property_slice(
    prototype: Option<GcObject>,
    props: &[(PropertyKey, PropertyDescriptor)],
  ) -> Result<Self, VmError> {
    // Avoid process abort on allocator OOM: allocate the property buffer fallibly.
    let mut buf: Vec<PropertyEntry> = Vec::new();
    buf
      .try_reserve_exact(props.len())
      .map_err(|_| VmError::OutOfMemory)?;
    buf.extend(props.iter().map(|(key, desc)| PropertyEntry {
      key: *key,
      desc: *desc,
    }));

    Ok(Self {
      prototype,
      extensible: true,
      properties: buf.into_boxed_slice(),
      kind: ObjectKind::Ordinary,
    })
  }

  pub(crate) fn property_count(&self) -> usize {
    self.properties.len()
  }

  pub(crate) fn properties_heap_size_bytes_for_count(count: usize) -> usize {
    // Payload bytes owned by this object allocation (the property table).
    //
    // Note: object headers are stored inline in the heap slot table, so this size intentionally
    // excludes the header size and only counts heap-owned payload allocations.
    count
      .checked_mul(mem::size_of::<PropertyEntry>())
      .unwrap_or(usize::MAX)
  }

  fn array_length(&self) -> Option<u32> {
    match &self.kind {
      ObjectKind::Array(a) => Some(a.length),
      ObjectKind::Ordinary | ObjectKind::Date(_) => None,
    }
  }

  fn set_array_length(&mut self, new_len: u32) {
    let ObjectKind::Array(a) = &mut self.kind else {
      return;
    };
    a.length = new_len;

    // Arrays always carry an own `length` data property at index 0 in their property table.
    if let Some(entry) = self.properties.get_mut(0) {
      if let PropertyKind::Data { value, .. } = &mut entry.desc.kind {
        *value = Value::Number(new_len as f64);
      } else {
        debug_assert!(false, "array length property is not a data descriptor");
      }
    } else {
      debug_assert!(false, "array missing length property entry");
    }
  }
}

impl Trace for ObjectBase {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    if let Some(proto) = self.prototype {
      tracer.trace_value(Value::Object(proto));
    }
    for prop in self.properties.iter() {
      prop.trace(tracer);
    }
  }
}

#[derive(Debug)]
struct JsObject {
  base: ObjectBase,
}

impl JsObject {
  fn new(prototype: Option<GcObject>) -> Self {
    Self {
      base: ObjectBase::new(prototype),
    }
  }

  fn from_property_slice(
    prototype: Option<GcObject>,
    props: &[(PropertyKey, PropertyDescriptor)],
  ) -> Result<Self, VmError> {
    Ok(Self {
      base: ObjectBase::from_property_slice(prototype, props)?,
    })
  }

  fn heap_size_bytes_for_property_count(count: usize) -> usize {
    ObjectBase::properties_heap_size_bytes_for_count(count)
  }

  fn array_length(&self) -> Option<u32> {
    self.base.array_length()
  }

  fn set_array_length(&mut self, new_len: u32) {
    self.base.set_array_length(new_len);
  }
}

impl Trace for JsObject {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.base.trace(tracer);
  }
}

#[derive(Debug)]
struct JsArrayBuffer {
  base: ObjectBase,
  data: Option<Box<[u8]>>,
}

impl JsArrayBuffer {
  fn new(prototype: Option<GcObject>, data: Box<[u8]>) -> Self {
    Self {
      base: ObjectBase::new(prototype),
      data: Some(data),
    }
  }

  fn byte_length(&self) -> usize {
    // If `data` is missing, the buffer has been detached (transfer-list semantics).
    self.data.as_deref().map(|d| d.len()).unwrap_or(0)
  }

  #[allow(dead_code)]
  fn heap_size_bytes(&self) -> usize {
    Self::heap_size_bytes_for_property_count(self.base.properties.len())
  }

  fn heap_size_bytes_for_property_count(property_count: usize) -> usize {
    ObjectBase::properties_heap_size_bytes_for_count(property_count)
  }

  fn finalize(&mut self) -> usize {
    // Detached buffers have no backing store to free.
    let Some(data) = self.data.take() else {
      return 0;
    };
    data.len()
  }
}

impl Trace for JsArrayBuffer {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.base.trace(tracer);
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypedArrayKind {
  Int8,
  Uint8,
  Uint8Clamped,
  Int16,
  Uint16,
  Int32,
  Uint32,
  Float32,
  Float64,
}

impl TypedArrayKind {
  pub(crate) fn bytes_per_element(self) -> usize {
    match self {
      TypedArrayKind::Int8 | TypedArrayKind::Uint8 | TypedArrayKind::Uint8Clamped => 1,
      TypedArrayKind::Int16 | TypedArrayKind::Uint16 => 2,
      TypedArrayKind::Int32 | TypedArrayKind::Uint32 | TypedArrayKind::Float32 => 4,
      TypedArrayKind::Float64 => 8,
    }
  }
}

#[derive(Debug)]
struct JsTypedArray {
  base: ObjectBase,
  kind: TypedArrayKind,
  viewed_array_buffer: GcObject,
  byte_offset: usize,
  /// Length in elements (not bytes).
  length: usize,
}

impl JsTypedArray {
  fn new(
    prototype: Option<GcObject>,
    kind: TypedArrayKind,
    viewed_array_buffer: GcObject,
    byte_offset: usize,
    length: usize,
  ) -> Self {
    Self {
      base: ObjectBase::new(prototype),
      kind,
      viewed_array_buffer,
      byte_offset,
      length,
    }
  }

  fn byte_length(&self) -> Result<usize, VmError> {
    self
      .length
      .checked_mul(self.kind.bytes_per_element())
      .ok_or(VmError::OutOfMemory)
  }

  fn heap_size_bytes_for_property_count(property_count: usize) -> usize {
    ObjectBase::properties_heap_size_bytes_for_count(property_count)
  }
}

impl Trace for JsTypedArray {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.base.trace(tracer);
    tracer.trace_value(Value::Object(self.viewed_array_buffer));
  }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeneratorState {
  SuspendedStart,
  SuspendedYield,
  Executing,
  Completed,
}

#[derive(Debug)]
pub(crate) struct JsGenerator {
  object: JsObject,
  #[allow(dead_code)]
  pub(crate) state: GeneratorState,
  pub(crate) this_value: Value,
  pub(crate) args: Option<Box<[Value]>>,
  pub(crate) env: Option<GcEnv>,
  pub(crate) continuation: Option<Box<[Value]>>,
}

impl JsGenerator {
  fn new(
    prototype: Option<GcObject>,
    this_value: Value,
    args: Option<Box<[Value]>>,
    env: Option<GcEnv>,
  ) -> Self {
    Self {
      object: JsObject::new(prototype),
      state: GeneratorState::SuspendedStart,
      this_value,
      args,
      env,
      continuation: None,
    }
  }

  fn heap_size_bytes(&self) -> usize {
    let property_count = self.object.base.properties.len();
    let args_len = self.args.as_deref().map(|args| args.len()).unwrap_or(0);
    let continuation_len = self
      .continuation
      .as_deref()
      .map(|cont| cont.len())
      .unwrap_or(0);
    Self::heap_size_bytes_for_counts(property_count, args_len, continuation_len)
  }

  fn heap_size_bytes_for_counts(property_count: usize, args_len: usize, continuation_len: usize) -> usize {
    let props_bytes = ObjectBase::properties_heap_size_bytes_for_count(property_count);
    let args_bytes = args_len.checked_mul(mem::size_of::<Value>()).unwrap_or(usize::MAX);
    let continuation_bytes = continuation_len
      .checked_mul(mem::size_of::<Value>())
      .unwrap_or(usize::MAX);

    // Payload bytes owned by this generator allocation.
    //
    // Note: `JsGenerator` headers are stored inline in the heap slot table, so this size
    // intentionally excludes `mem::size_of::<JsGenerator>()` and only counts heap-owned payload
    // allocations.
    props_bytes
      .checked_add(args_bytes)
      .and_then(|b| b.checked_add(continuation_bytes))
      .unwrap_or(usize::MAX)
  }
}

impl Trace for JsGenerator {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.object.trace(tracer);
    tracer.trace_value(self.this_value);
    if let Some(env) = self.env {
      tracer.trace_env(env);
    }
    if let Some(args) = &self.args {
      for value in args.iter().copied() {
        tracer.trace_value(value);
      }
    }
    if let Some(cont) = &self.continuation {
      for value in cont.iter().copied() {
        tracer.trace_value(value);
      }
    }
  }
}

#[derive(Debug)]
struct JsDataView {
  base: ObjectBase,
  viewed_array_buffer: GcObject,
  byte_offset: usize,
  byte_length: usize,
}

impl JsDataView {
  fn new(
    prototype: Option<GcObject>,
    viewed_array_buffer: GcObject,
    byte_offset: usize,
    byte_length: usize,
  ) -> Self {
    Self {
      base: ObjectBase::new(prototype),
      viewed_array_buffer,
      byte_offset,
      byte_length,
    }
  }

  fn heap_size_bytes_for_property_count(property_count: usize) -> usize {
    ObjectBase::properties_heap_size_bytes_for_count(property_count)
  }
}

impl Trace for JsDataView {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.base.trace(tracer);
    tracer.trace_value(Value::Object(self.viewed_array_buffer));
  }
}

#[derive(Debug)]
struct JsWeakSet {
  base: ObjectBase,
  entries: Vec<WeakGcObject>,
}

impl JsWeakSet {
  fn new(prototype: Option<GcObject>) -> Self {
    Self {
      base: ObjectBase::new(prototype),
      entries: Vec::new(),
    }
  }

  fn heap_size_bytes(&self) -> usize {
    Self::heap_size_bytes_for_counts(self.base.properties.len(), self.entries.capacity())
  }

  fn heap_size_bytes_for_counts(property_count: usize, entry_capacity: usize) -> usize {
    let props_bytes = ObjectBase::properties_heap_size_bytes_for_count(property_count);
    let entries_bytes = entry_capacity
      .checked_mul(mem::size_of::<WeakGcObject>())
      .unwrap_or(usize::MAX);
    props_bytes.saturating_add(entries_bytes)
  }
}

impl Trace for JsWeakSet {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    // WeakSet keys are weak: do not trace `entries`.
    self.base.trace(tracer);
  }
}

#[derive(Debug)]
struct JsPromise {
  object: JsObject,
  state: PromiseState,
  /// `[[PromiseResult]]` is either a value (when settled) or *undefined* (when pending).
  ///
  /// The spec models "undefined" distinctly from "empty"; at this layer we represent it as `None`.
  result: Option<Value>,
  /// `[[PromiseFulfillReactions]]` is present only while pending (spec: set to `undefined` on
  /// settlement).
  fulfill_reactions: Option<Box<[PromiseReaction]>>,
  /// `[[PromiseRejectReactions]]` is present only while pending (spec: set to `undefined` on
  /// settlement).
  reject_reactions: Option<Box<[PromiseReaction]>>,
  is_handled: bool,
}

impl JsPromise {
  fn new(prototype: Option<GcObject>) -> Self {
    Self {
      object: JsObject::new(prototype),
      state: PromiseState::Pending,
      result: None,
      fulfill_reactions: Some(Box::default()),
      reject_reactions: Some(Box::default()),
      is_handled: false,
    }
  }

  fn heap_size_bytes(&self) -> usize {
    Self::heap_size_bytes_for_counts(
      self.object.base.properties.len(),
      self.fulfill_reactions.as_deref().map(|r| r.len()).unwrap_or(0),
      self.reject_reactions.as_deref().map(|r| r.len()).unwrap_or(0),
    )
  }

  fn heap_size_bytes_for_counts(
    property_count: usize,
    fulfill_reaction_count: usize,
    reject_reaction_count: usize,
  ) -> usize {
    let props_bytes = property_count
      .checked_mul(mem::size_of::<PropertyEntry>())
      .unwrap_or(usize::MAX);
    let fulfill_bytes = fulfill_reaction_count
      .checked_mul(mem::size_of::<PromiseReaction>())
      .unwrap_or(usize::MAX);
    let reject_bytes = reject_reaction_count
      .checked_mul(mem::size_of::<PromiseReaction>())
      .unwrap_or(usize::MAX);

    mem::size_of::<Self>()
      .checked_add(props_bytes)
      .and_then(|v| v.checked_add(fulfill_bytes))
      .and_then(|v| v.checked_add(reject_bytes))
      .unwrap_or(usize::MAX)
  }
}

impl Trace for JsPromise {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.object.trace(tracer);
    if let Some(result) = self.result {
      tracer.trace_value(result);
    }
    if let Some(reactions) = &self.fulfill_reactions {
      for reaction in reactions.iter() {
        reaction.trace(tracer);
      }
    }
    if let Some(reactions) = &self.reject_reactions {
      for reaction in reactions.iter() {
        reaction.trace(tracer);
      }
    }
  }
}

#[derive(Debug)]
enum ObjectKind {
  Ordinary,
  Array(ArrayObject),
  Date(DateObject),
}

#[derive(Debug)]
struct ArrayObject {
  length: u32,
}

#[derive(Debug)]
struct DateObject {
  value: f64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PropertyEntry {
  key: PropertyKey,
  desc: PropertyDescriptor,
}

impl Trace for PropertyEntry {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.key.trace(tracer);
    self.desc.trace(tracer);
  }
}

pub(crate) trait Trace {
  fn trace(&self, tracer: &mut Tracer<'_>);
}

pub(crate) struct Tracer<'a> {
  slots: &'a [Slot],
  marks: &'a mut [u8],
  worklist: &'a mut Vec<HeapId>,
}

impl<'a> Tracer<'a> {
  fn new(slots: &'a [Slot], marks: &'a mut [u8], worklist: &'a mut Vec<HeapId>) -> Self {
    Self { slots, marks, worklist }
  }

  fn pop_work(&mut self) -> Option<HeapId> {
    self.worklist.pop()
  }

  pub(crate) fn trace_value(&mut self, value: Value) {
    match value {
      Value::Undefined | Value::Null | Value::Bool(_) | Value::Number(_) | Value::BigInt(_) => {}
      Value::String(s) => self.trace_heap_id(s.0),
      Value::Symbol(s) => self.trace_heap_id(s.0),
      Value::Object(o) => self.trace_heap_id(o.0),
    }
  }

  pub(crate) fn trace_env(&mut self, env: GcEnv) {
    self.trace_heap_id(env.0);
  }

  fn trace_heap_id(&mut self, id: HeapId) {
    let Some(idx) = self.validate(id) else {
      return;
    };
    if self.marks[idx] != 0 {
      return;
    }
    // Mark as "discovered" before pushing to avoid unbounded worklist growth due to duplicates.
    // We treat 0 = white, 1 = gray (queued), 2 = black (scanned).
    self.marks[idx] = 1;
    self.worklist.push(id);
  }

  fn validate(&self, id: HeapId) -> Option<usize> {
    let idx = id.index() as usize;
    let slot = self.slots.get(idx)?;
    if slot.generation != id.generation() {
      debug_assert!(false, "stale handle during GC: {id:?}");
      return None;
    }
    if slot.value.is_none() {
      debug_assert!(false, "handle points at a free slot during GC: {id:?}");
      return None;
    }
    Some(idx)
  }
}

fn array_length_from_f64(n: f64) -> Option<u32> {
  if !n.is_finite() {
    return None;
  }
  if n < 0.0 {
    return None;
  }
  if n.fract() != 0.0 {
    return None;
  }
  if n > u32::MAX as f64 {
    return None;
  }
  Some(n as u32)
}

fn grown_capacity(current_capacity: usize, required_len: usize) -> usize {
  if required_len <= current_capacity {
    return current_capacity;
  }
  let mut cap = current_capacity.max(MIN_VEC_CAPACITY);
  while cap < required_len {
    cap = match cap.checked_mul(2) {
      Some(next) => next,
      None => return usize::MAX,
    };
  }
  cap
}

fn vec_capacity_growth_bytes<T>(current_capacity: usize, required_len: usize) -> usize {
  let elem_size = mem::size_of::<T>();
  if elem_size == 0 {
    return 0;
  }
  let new_capacity = grown_capacity(current_capacity, required_len);
  if new_capacity == usize::MAX {
    return usize::MAX;
  }
  new_capacity
    .saturating_sub(current_capacity)
    .saturating_mul(elem_size)
}

fn reserve_vec_to_len<T>(vec: &mut Vec<T>, required_len: usize) -> Result<(), VmError> {
  if required_len <= vec.capacity() {
    return Ok(());
  }
  let desired_capacity = grown_capacity(vec.capacity(), required_len);
  if desired_capacity == usize::MAX {
    return Err(VmError::OutOfMemory);
  }

  let additional = desired_capacity
    .checked_sub(vec.len())
    .ok_or(VmError::OutOfMemory)?;
  vec
    .try_reserve_exact(additional)
    .map_err(|_| VmError::OutOfMemory)?;
  Ok(())
}

#[cfg(test)]
mod external_memory_accounting_tests {
  use super::*;

  #[test]
  fn charge_external_blocks_heap_allocations_until_dropped() -> Result<(), VmError> {
    let max_bytes = 1024;
    let mut heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));

    let token = heap.charge_external(max_bytes)?;
    assert!(
      heap.estimated_total_bytes() >= max_bytes,
      "expected charged bytes to contribute to estimated_total_bytes"
    );

    {
      let mut scope = heap.scope();
      match scope.alloc_object() {
        Err(VmError::OutOfMemory) => {}
        Ok(_) => panic!("expected allocation to fail due to charged external bytes"),
        Err(e) => return Err(e),
      }
    }

    drop(token);
    assert!(
      heap.estimated_total_bytes() < max_bytes,
      "expected dropping the token to release charged bytes"
    );

    {
      let mut scope = heap.scope();
      scope.alloc_object()?;
    }

    Ok(())
  }
}
