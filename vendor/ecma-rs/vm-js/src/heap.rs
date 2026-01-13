use crate::env::{DeclarativeEnvRecord, EnvBinding, EnvBindingValue, EnvRecord, ObjectEnvRecord, PrivateNameEntry};
use crate::exec::{GenFrame, RuntimeEnv};
use crate::function::{
  CallHandler, ConstructHandler, EcmaFunctionId, FunctionData, JsFunction, NativeConstructId,
  NativeFunctionId, ThisMode,
};
use crate::property::{PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind};
use crate::promise::{PromiseCapability, PromiseReaction, PromiseReactionType, PromiseState};
use crate::regexp::{RegExpFlags, RegExpProgram};
use crate::bigint::JsBigInt;
use crate::string::JsString;
use crate::symbol::JsSymbol;
use crate::CompiledFunctionRef;
use crate::handle::GcModuleNamespaceExports;
use crate::{
  EnvRootId, GcBigInt, GcEnv, GcObject, GcString, GcSymbol, HeapId, Job, JobKind, RealmId, RootId,
  Value, Vm, WellKnownSymbols,
  VmError, VmHost, VmHostHooks, VmJobContext, WeakGcObject,
  WeakGcSymbol,
};
use std::cell::Cell;
use core::mem;
use core::num::NonZeroU32;
use parse_js::ast::func::Func;
use parse_js::ast::node::Node;
use semantic_js::js::SymbolId;
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;
use crate::tick;
use std::sync::Arc;

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

/// Engine-private symbols used to model spec internal slots as hidden symbol-keyed properties.
///
/// These symbols must:
/// - be **stable within a heap** (so objects can cross realms)
/// - be **unreachable from user code** (i.e. *not* obtainable via `Symbol.for`)
/// - be **unobservable via key enumeration** (see `OrdinaryOwnPropertyKeys` filtering)
#[derive(Debug, Default, Clone, Copy)]
struct InternalSymbols {
  // Primitive wrapper internal slots (`[[StringData]]`, etc).
  string_data: Option<GcSymbol>,
  symbol_data: Option<GcSymbol>,
  boolean_data: Option<GcSymbol>,
  number_data: Option<GcSymbol>,
  bigint_data: Option<GcSymbol>,

  // Array iterator internal slots.
  array_iterator_array: Option<GcSymbol>,
  array_iterator_index: Option<GcSymbol>,
  array_iterator_kind: Option<GcSymbol>,

  // Map iterator internal slots.
  map_iterator_map: Option<GcSymbol>,
  map_iterator_index: Option<GcSymbol>,
  map_iterator_kind: Option<GcSymbol>,

  // Set iterator internal slots.
  set_iterator_set: Option<GcSymbol>,
  set_iterator_index: Option<GcSymbol>,
  set_iterator_kind: Option<GcSymbol>,

  // String iterator internal slots.
  string_iterator_iterated_string: Option<GcSymbol>,
  string_iterator_next_index: Option<GcSymbol>,

  // RegExp string iterator (`/re/g[Symbol.matchAll](...)`) internal slots.
  regexp_string_iterator_iterating_regexp: Option<GcSymbol>,
  regexp_string_iterator_iterated_string: Option<GcSymbol>,
  regexp_string_iterator_global: Option<GcSymbol>,
  regexp_string_iterator_unicode: Option<GcSymbol>,
  regexp_string_iterator_done: Option<GcSymbol>,

  // Proposal-era `JSON.rawJSON` internal slot marker (`[[IsRawJSON]]`).
  is_raw_json: Option<GcSymbol>,
}

/// Minimum non-zero capacity for heap-internal vectors that can grow due to hostile input.
///
/// Keeping a small floor avoids pathological "grow by 1" patterns while still being conservative
/// about over-allocation.
const MIN_VEC_CAPACITY: usize = 1;

/// Size of the small-integer string cache (`"0".."9999"`).
///
/// This is tuned for test262's `regExpUtils.js::buildString`, which frequently uses indices in
/// `[0, 10000)` when chunking Unicode code points for `String.fromCodePoint.apply(...)`.
const SMALL_INT_STRING_CACHE_SIZE: usize = 10_000;

/// Maximum array index stored in the fast elements table for Array exotic objects.
///
/// Larger indices are represented in the ordinary property table to avoid allocating enormous
/// sparse vectors for hostile inputs like `a[2**32-2] = 1`.
pub(crate) const MAX_FAST_ARRAY_INDEX: u32 = 100_000;

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

/// Generator object state (ECMA-262 shaped).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GeneratorState {
  SuspendedStart,
  SuspendedYield,
  Executing,
  Completed,
}

/// Async generator object state (ECMA-262 shaped).
///
/// Note: async generator *runtime* semantics are implemented incrementally; this enum models the
/// spec-visible internal slot `[[AsyncGeneratorState]]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AsyncGeneratorState {
  SuspendedStart,
  SuspendedYield,
  Executing,
  AwaitingReturn,
  Completed,
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

  // Engine-private symbols used to model spec internal slots.
  //
  // These are **not** stored in the global symbol registry so scripts cannot obtain them via
  // `Symbol.for("vm-js.internal.*")`.
  internal_symbols: InternalSymbols,

  /// Cached ECMAScript well-known symbols (e.g. `Symbol.iterator`) for this heap.
  ///
  /// These are treated as **agent-wide identities** while they are live: all realms created in
  /// this heap observe the same `Symbol.*` values.
  ///
  /// The heap does **not** automatically keep these alive. Instead, well-known symbols are rooted
  /// by realms (and any other live objects that reference them), allowing them to be collected once
  /// all realms and host roots are torn down.
  well_known_symbols: Option<WellKnownSymbols>,

  // Commonly-used property key strings (interned for memory efficiency).
  common_key_name: Option<GcString>,
  common_key_message: Option<GcString>,
  common_key_length: Option<GcString>,
  common_key_constructor: Option<GcString>,
  common_key_prototype: Option<GcString>,

  /// Cached decimal string representations for small non-negative integers.
  ///
  /// This is primarily a performance optimization for:
  /// - `ToString` on small integer `Number` values used as array indices, and
  /// - `[[OwnPropertyKeys]]` / other algorithms that need to allocate index strings.
  ///
  /// The cache is traced during GC so entries remain valid across collections.
  small_int_strings: Vec<Option<GcString>>,

  /// True if at least one `FinalizationRegistry` has pending cleanup work.
  ///
  /// This is set during GC when an unreachable target is discovered and cleared from a registry's
  /// internal cell list. The corresponding cleanup jobs are enqueued outside of GC to avoid
  /// allocating during collection.
  finalization_registry_cleanup_jobs_pending: bool,
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
      internal_symbols: InternalSymbols::default(),
      well_known_symbols: None,
      common_key_name: None,
      common_key_message: None,
      common_key_length: None,
      common_key_constructor: None,
      common_key_prototype: None,
      small_int_strings: vec![None; SMALL_INT_STRING_CACHE_SIZE],
      finalization_registry_cleanup_jobs_pending: false,
    }
  }

  /// Sets the heap-level default `%Object.prototype%` for newly-created constructor `.prototype`
  /// objects.
  ///
  /// This is initialized by [`Realm::new`](crate::Realm::new) and cleared by
  /// [`Realm::teardown`](crate::Realm::teardown).
  ///
  /// Embeddings that manage multiple realms on a shared heap (e.g. test262's `$262.createRealm`)
  /// may need to temporarily swap this value when switching the active realm.
  pub fn set_default_object_prototype(&mut self, proto: Option<GcObject>) {
    self.default_object_prototype = proto;
  }

  /// Returns the heap-level default `%Object.prototype%`, if any.
  pub fn default_object_prototype(&self) -> Option<GcObject> {
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

  /// Debug-only GC invariant checks for internal strong references stored in heap objects.
  ///
  /// These checks complement `Tracer::validate`:
  /// - `Tracer::validate` only checks slot bounds + generation and does not validate reference kinds.
  /// - If a heap object holds an internal `GcObject` pointing at the wrong kind of allocation (e.g.
  ///   a TypedArray whose `viewed_array_buffer` points at a non-ArrayBuffer object), the VM can
  ///   surface `VmError::InvalidHandle` at unrelated call sites.
  ///
  /// Running this after GC helps catch corruption early and with useful context.
  #[cfg(any(debug_assertions, feature = "gc_validate"))]
  fn debug_validate_no_stale_internal_handles(&self) {
    for (owner_idx, owner_slot) in self.slots.iter().enumerate() {
      let Some(owner_obj) = owner_slot.value.as_ref() else {
        continue;
      };
      match owner_obj {
        HeapObject::Object(obj) => {
          // Validate internal-slot-like references stored in `ObjectKind` variants.
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("Object(kind={:?})", obj.base.kind);
          match &obj.base.kind {
            ObjectKind::ModuleNamespace(ns) => {
              self.debug_validate_heap_id_expected(
                owner_kind,
                owner_id,
                format_args!("[[Exports]]"),
                ns.exports.id(),
                "live ModuleNamespaceExports",
                debug_expected_is_module_namespace_exports,
              );
            }
            ObjectKind::Array(_) | ObjectKind::Ordinary | ObjectKind::Date(_) | ObjectKind::Error | ObjectKind::Arguments => {}
          }
        }
        HeapObject::TypedArray(arr) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("TypedArray(kind={:?})", arr.kind);
          self.debug_validate_heap_id_expected(
            owner_kind,
            owner_id,
            format_args!("viewed_array_buffer"),
            arr.viewed_array_buffer.0,
            "live ArrayBuffer",
            debug_expected_is_array_buffer,
          );
        }
        HeapObject::DataView(view) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("DataView");
          self.debug_validate_heap_id_expected(
            owner_kind,
            owner_id,
            format_args!("viewed_array_buffer"),
            view.viewed_array_buffer.0,
            "live ArrayBuffer",
            debug_expected_is_array_buffer,
          );
        }
        HeapObject::Proxy(p) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("Proxy");

          if p.target.is_some() != p.handler.is_some() {
            panic!(
              "GC invariant violated: {owner_kind} {owner_id:?} has inconsistent proxy slots: target={:?} handler={:?}",
              p.target.map(|o| o.id()),
              p.handler.map(|o| o.id()),
            );
          }

          if let Some(target) = p.target {
            self.debug_validate_heap_id_expected(
              owner_kind,
              owner_id,
              format_args!("target"),
              target.0,
              "live Object",
              debug_expected_is_object,
            );
          }
          if let Some(handler) = p.handler {
            self.debug_validate_heap_id_expected(
              owner_kind,
              owner_id,
              format_args!("handler"),
              handler.0,
              "live Object",
              debug_expected_is_object,
            );
          }
        }
        HeapObject::Function(f) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("Function");

          // Internal name metadata is a required strong string reference.
          self.debug_validate_heap_id_expected(
            owner_kind,
            owner_id,
            format_args!("name"),
            f.name.0,
            "live String",
            debug_expected_is_string,
          );

          // FunctionData slots are internal strong edges.
          match f.data {
            FunctionData::None | FunctionData::EcmaFallback { .. } | FunctionData::PromiseCapabilityExecutor => {}
            FunctionData::ClassConstructorBody { class_constructor } => {
              self.debug_validate_heap_id_expected(
                owner_kind,
                owner_id,
                format_args!("data.class_constructor"),
                class_constructor.0,
                "live Object",
                debug_expected_is_object,
              );
            }
            FunctionData::PromiseResolvingFunction { promise, .. } => {
              // This is expected to point at a Promise object, but keep the invariant lightweight
              // and only require a live object allocation.
              self.debug_validate_heap_id_expected(
                owner_kind,
                owner_id,
                format_args!("data.promise"),
                promise.0,
                "live Object",
                debug_expected_is_object,
              );
            }
            FunctionData::PromiseFinallyHandler {
              on_finally,
              constructor,
              ..
            } => {
              self.debug_validate_value(owner_kind, owner_id, format_args!("data.on_finally"), on_finally);
              self.debug_validate_value(
                owner_kind,
                owner_id,
                format_args!("data.constructor"),
                constructor,
              );
            }
            FunctionData::PromiseFinallyThunk { value, .. } => {
              self.debug_validate_value(owner_kind, owner_id, format_args!("data.value"), value);
            }
          }

          // Bound functions: `bound_target` is a strong edge and should always be callable.
          if let Some(target) = f.bound_target {
            self.debug_validate_heap_id_expected(
              owner_kind,
              owner_id,
              format_args!("bound_target"),
              target.0,
              "live Object",
              debug_expected_is_object,
            );
            // Callability is a spec-level invariant for bound functions; catch violations early.
            match self.is_callable(Value::Object(target)) {
              Ok(true) => {}
              Ok(false) => {
                panic!(
                  "GC invariant violated: {owner_kind} {owner_id:?} has non-callable bound_target={:?}",
                  target.id()
                );
              }
              Err(err) => {
                panic!(
                  "GC invariant violated: {owner_kind} {owner_id:?} failed to validate bound_target={:?}: {err:?}",
                  target.id()
                );
              }
            }
          }
          if let Some(bound_this) = f.bound_this {
            self.debug_validate_value(owner_kind, owner_id, format_args!("bound_this"), bound_this);
          }
          if let Some(bound_new_target) = f.bound_new_target {
            self.debug_validate_value(
              owner_kind,
              owner_id,
              format_args!("bound_new_target"),
              bound_new_target,
            );
          }
          if let Some(bound_args) = &f.bound_args {
            for (i, v) in bound_args.iter().copied().enumerate() {
              self.debug_validate_value(
                owner_kind,
                owner_id,
                format_args!("bound_args[{i}]"),
                v,
              );
            }
          }
          if let Some(native_slots) = &f.native_slots {
            for (i, v) in native_slots.iter().copied().enumerate() {
              self.debug_validate_value(
                owner_kind,
                owner_id,
                format_args!("native_slots[{i}]"),
                v,
              );
            }
          }
          if let Some(realm) = f.realm {
            self.debug_validate_heap_id_expected(
              owner_kind,
              owner_id,
              format_args!("realm"),
              realm.0,
              "live Object",
              debug_expected_is_object,
            );
          }
          if let Some(env) = f.closure_env {
            self.debug_validate_heap_id_expected(
              owner_kind,
              owner_id,
              format_args!("closure_env"),
              env.0,
              "live Env",
              debug_expected_is_env,
            );
          }
        }
        HeapObject::Promise(p) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("Promise");

          if let Some(result) = p.result {
            self.debug_validate_value(owner_kind, owner_id, format_args!("result"), result);
          }

          if let Some(reactions) = p.fulfill_reactions.as_deref() {
            for (i, reaction) in reactions.iter().enumerate() {
              if let Some(cap) = &reaction.capability {
                self.debug_validate_value(
                  owner_kind,
                  owner_id,
                  format_args!("fulfill_reactions[{i}].capability.promise"),
                  cap.promise,
                );
                self.debug_validate_value(
                  owner_kind,
                  owner_id,
                  format_args!("fulfill_reactions[{i}].capability.resolve"),
                  cap.resolve,
                );
                self.debug_validate_value(
                  owner_kind,
                  owner_id,
                  format_args!("fulfill_reactions[{i}].capability.reject"),
                  cap.reject,
                );
              }
              if let Some(handler) = &reaction.handler {
                let cb = handler.callback_object();
                self.debug_validate_heap_id_expected(
                  owner_kind,
                  owner_id,
                  format_args!("fulfill_reactions[{i}].handler.callback"),
                  cb.0,
                  "live Object",
                  debug_expected_is_object,
                );
              }
            }
          }

          if let Some(reactions) = p.reject_reactions.as_deref() {
            for (i, reaction) in reactions.iter().enumerate() {
              if let Some(cap) = &reaction.capability {
                self.debug_validate_value(
                  owner_kind,
                  owner_id,
                  format_args!("reject_reactions[{i}].capability.promise"),
                  cap.promise,
                );
                self.debug_validate_value(
                  owner_kind,
                  owner_id,
                  format_args!("reject_reactions[{i}].capability.resolve"),
                  cap.resolve,
                );
                self.debug_validate_value(
                  owner_kind,
                  owner_id,
                  format_args!("reject_reactions[{i}].capability.reject"),
                  cap.reject,
                );
              }
              if let Some(handler) = &reaction.handler {
                let cb = handler.callback_object();
                self.debug_validate_heap_id_expected(
                  owner_kind,
                  owner_id,
                  format_args!("reject_reactions[{i}].handler.callback"),
                  cb.0,
                  "live Object",
                  debug_expected_is_object,
                );
              }
            }
          }
        }
        HeapObject::FinalizationRegistry(fr) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("FinalizationRegistry");

          self.debug_validate_value(owner_kind, owner_id, format_args!("cleanup_callback"), fr.cleanup_callback);

          for (i, cell) in fr.cells.iter().enumerate() {
            self.debug_validate_value(
              owner_kind,
              owner_id,
              format_args!("cells[{i}].held_value"),
              cell.held_value,
            );
            if let Some(target) = cell.target {
              if target.upgrade_value(self).is_none() {
                panic!(
                  "GC invariant violated: {owner_kind} {owner_id:?} has cells[{i}].target={:?} which does not upgrade to a live value (expected GC hygiene to clear dead targets)",
                  target.id()
                );
              }
            }
          }
        }
        HeapObject::Map(m) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("Map");

          let live_count = m.entries.iter().filter(|e| e.key.is_some()).count();
          if live_count != m.size {
            panic!(
              "GC invariant violated: {owner_kind} {owner_id:?} has inconsistent size: size={}, live_entry_count={}",
              m.size, live_count
            );
          }

          for (i, entry) in m.entries.iter().enumerate() {
            match (entry.key, entry.value) {
              (None, None) => {}
              (None, Some(v)) => {
                panic!(
                  "GC invariant violated: {owner_kind} {owner_id:?} has MapEntry[{i}] with value={v:?} but key=None"
                );
              }
              (Some(k), None) => {
                panic!(
                  "GC invariant violated: {owner_kind} {owner_id:?} has MapEntry[{i}] with key={k:?} but value=None"
                );
              }
              (Some(k), Some(v)) => {
                self.debug_validate_value(owner_kind, owner_id, format_args!("entries[{i}].key"), k);
                self.debug_validate_value(owner_kind, owner_id, format_args!("entries[{i}].value"), v);
              }
            }
          }
        }
        HeapObject::Set(s) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("Set");

          let live_count = s.entries.iter().filter(|e| e.is_some()).count();
          if live_count != s.size {
            panic!(
              "GC invariant violated: {owner_kind} {owner_id:?} has inconsistent size: size={}, live_entry_count={}",
              s.size, live_count
            );
          }

          for (i, entry) in s.entries.iter().enumerate() {
            if let Some(v) = *entry {
              self.debug_validate_value(owner_kind, owner_id, format_args!("entries[{i}]"), v);
            }
          }
        }
        HeapObject::WeakMap(wm) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("WeakMap");

          for (i, entry) in wm.entries.iter().enumerate() {
            if entry.key.upgrade_value(self).is_none() {
              panic!(
                "GC invariant violated: {owner_kind} {owner_id:?} has WeakMapEntry[{i}] with dead key={:?} (expected GC hygiene to remove dead keys)",
                entry.key.id()
              );
            }
            self.debug_validate_value(owner_kind, owner_id, format_args!("entries[{i}].value"), entry.value);
          }
        }
        HeapObject::WeakSet(ws) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("WeakSet");

          for (i, entry) in ws.entries.iter().enumerate() {
            if entry.upgrade_value(self).is_none() {
              panic!(
                "GC invariant violated: {owner_kind} {owner_id:?} has dead key at entries[{i}]={:?} (expected GC hygiene to remove dead keys)",
                entry.id()
              );
            }
          }
        }
        HeapObject::ModuleNamespaceExports(exports) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("ModuleNamespaceExports");

          for (i, export) in exports.exports.iter().enumerate() {
            self.debug_validate_heap_id_expected(
              owner_kind,
              owner_id,
              format_args!("exports[{i}].name"),
              export.name.0,
              "live String",
              debug_expected_is_string,
            );
            self.debug_validate_heap_id_expected(
              owner_kind,
              owner_id,
              format_args!("exports[{i}].getter"),
              export.getter.0,
              "live Object",
              debug_expected_is_object,
            );
            match export.value {
              ModuleNamespaceExportValue::Binding { env, name } => {
                self.debug_validate_heap_id_expected(
                  owner_kind,
                  owner_id,
                  format_args!("exports[{i}].binding.env"),
                  env.0,
                  "live Env",
                  debug_expected_is_env,
                );
                self.debug_validate_heap_id_expected(
                  owner_kind,
                  owner_id,
                  format_args!("exports[{i}].binding.name"),
                  name.0,
                  "live String",
                  debug_expected_is_string,
                );
              }
              ModuleNamespaceExportValue::Namespace { namespace } => {
                self.debug_validate_heap_id_expected(
                  owner_kind,
                  owner_id,
                  format_args!("exports[{i}].namespace"),
                  namespace.0,
                  "live Object",
                  debug_expected_is_object,
                );
              }
            }
          }
        }
        HeapObject::Env(env) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("Env");

          if let Some(outer) = env.outer() {
            self.debug_validate_heap_id_expected(
              owner_kind,
              owner_id,
              format_args!("outer"),
              outer.0,
              "live Env",
              debug_expected_is_env,
            );
          }

          match env {
            EnvRecord::Declarative(env) => {
              for (i, binding) in env.bindings.iter().enumerate() {
                if let Some(name) = binding.name {
                  self.debug_validate_heap_id_expected(
                    owner_kind,
                    owner_id,
                    format_args!("bindings[{i}].name"),
                    name.0,
                    "live String",
                    debug_expected_is_string,
                  );
                }
                match binding.value {
                  EnvBindingValue::Direct(v) => {
                    self.debug_validate_value(owner_kind, owner_id, format_args!("bindings[{i}].value"), v);
                  }
                  EnvBindingValue::Indirect { env, name } => {
                    self.debug_validate_heap_id_expected(
                      owner_kind,
                      owner_id,
                      format_args!("bindings[{i}].indirect.env"),
                      env.0,
                      "live Env",
                      debug_expected_is_env,
                    );
                    self.debug_validate_heap_id_expected(
                      owner_kind,
                      owner_id,
                      format_args!("bindings[{i}].indirect.name"),
                      name.0,
                      "live String",
                      debug_expected_is_string,
                    );
                  }
                }
              }
              if let Some(this_value) = env.this_value {
                self.debug_validate_value(owner_kind, owner_id, format_args!("this_value"), this_value);
              }
              if let Some(new_target) = env.new_target {
                self.debug_validate_value(owner_kind, owner_id, format_args!("new_target"), new_target);
              }
              if let Some(private_names) = env.private_names.as_deref() {
                for (i, entry) in private_names.iter().enumerate() {
                  self.debug_validate_heap_id_expected(
                    owner_kind,
                    owner_id,
                    format_args!("private_names[{i}].sym"),
                    entry.sym.0,
                    "live Symbol",
                    debug_expected_is_symbol,
                  );
                }
              }
            }
            EnvRecord::Object(env) => {
              self.debug_validate_heap_id_expected(
                owner_kind,
                owner_id,
                format_args!("binding_object"),
                env.binding_object.0,
                "live Object",
                debug_expected_is_object,
              );
            }
          }
        }
        HeapObject::RegExp(r) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("RegExp");
          self.debug_validate_heap_id_expected(
            owner_kind,
            owner_id,
            format_args!("original_source"),
            r.original_source.0,
            "live String",
            debug_expected_is_string,
          );
          self.debug_validate_heap_id_expected(
            owner_kind,
            owner_id,
            format_args!("original_flags"),
            r.original_flags.0,
            "live String",
            debug_expected_is_string,
          );
        }
        HeapObject::Symbol(sym) => {
          let owner_id = HeapId::from_parts(owner_idx as u32, owner_slot.generation);
          let owner_kind = format_args!("Symbol");
          if let Some(desc) = sym.description() {
            self.debug_validate_heap_id_expected(
              owner_kind,
              owner_id,
              format_args!("description"),
              desc.0,
              "live String",
              debug_expected_is_string,
            );
          }
        }
        _ => {}
      }
    }
  }

  #[cfg(any(debug_assertions, feature = "gc_validate"))]
  fn debug_validate_heap_id_expected(
    &self,
    owner_kind: core::fmt::Arguments<'_>,
    owner_id: HeapId,
    field: core::fmt::Arguments<'_>,
    referenced_id: HeapId,
    expected: &'static str,
    expected_pred: fn(&HeapObject) -> bool,
  ) {
    let idx = referenced_id.index() as usize;
    let Some(slot) = self.slots.get(idx) else {
      panic!(
        "GC invariant violated: {owner_kind} {owner_id:?} has invalid {field}={referenced_id:?}; \
referenced index {idx} is out of bounds (heap slots len={})",
        self.slots.len()
      );
    };

    if slot.generation == referenced_id.generation() {
      if let Some(obj) = slot.value.as_ref() {
        if expected_pred(obj) {
          return;
        }
      }
    }

    let current_kind = slot
      .value
      .as_ref()
      .map(|obj| obj.debug_kind())
      .unwrap_or("Free");

    panic!(
      "GC invariant violated: {owner_kind} {owner_id:?} has invalid {field}={referenced_id:?}; \
referenced slot currently has generation={} and kind={current_kind} (expected {expected} with matching generation)",
      slot.generation
    );
  }

  #[cfg(any(debug_assertions, feature = "gc_validate"))]
  fn debug_validate_value(
    &self,
    owner_kind: core::fmt::Arguments<'_>,
    owner_id: HeapId,
    field: core::fmt::Arguments<'_>,
    value: Value,
  ) {
    match value {
      Value::Undefined | Value::Null | Value::Bool(_) | Value::Number(_) => {}
      Value::BigInt(b) => self.debug_validate_heap_id_expected(
        owner_kind,
        owner_id,
        field,
        b.0,
        "live BigInt",
        debug_expected_is_bigint,
      ),
      Value::String(s) => self.debug_validate_heap_id_expected(
        owner_kind,
        owner_id,
        field,
        s.0,
        "live String",
        debug_expected_is_string,
      ),
      Value::Symbol(s) => self.debug_validate_heap_id_expected(
        owner_kind,
        owner_id,
        field,
        s.0,
        "live Symbol",
        debug_expected_is_symbol,
      ),
      Value::Object(o) => self.debug_validate_heap_id_expected(
        owner_kind,
        owner_id,
        field,
        o.0,
        "live Object",
        debug_expected_is_object,
      ),
    }
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
      // Engine-private internal-slot symbols.
      let internal = &self.internal_symbols;
      let internal_syms = [
        internal.string_data,
        internal.symbol_data,
        internal.boolean_data,
        internal.number_data,
        internal.bigint_data,
        internal.array_iterator_array,
        internal.array_iterator_index,
        internal.array_iterator_kind,
        internal.map_iterator_map,
        internal.map_iterator_index,
        internal.map_iterator_kind,
        internal.set_iterator_set,
        internal.set_iterator_index,
        internal.set_iterator_kind,
        internal.string_iterator_iterated_string,
        internal.string_iterator_next_index,
        internal.regexp_string_iterator_iterating_regexp,
        internal.regexp_string_iterator_iterated_string,
        internal.regexp_string_iterator_global,
        internal.regexp_string_iterator_unicode,
        internal.regexp_string_iterator_done,
        internal.is_raw_json,
      ];
      for sym in internal_syms.into_iter().flatten() {
        tracer.trace_value(Value::Symbol(sym));
      }

      // Cached small integer strings used for fast `Number` -> `String` conversions.
      //
      // These are treated as roots so cached entries remain valid across GC, which avoids repeated
      // allocation churn in tight loops that repeatedly convert small indices to strings.
      for s in self.small_int_strings.iter().flatten() {
        tracer.trace_value(Value::String(*s));
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

    // WeakMap ephemeron processing.
    //
    // WeakMap keys are weak (they do not keep their keys alive), but as long as a WeakMap itself is
    // reachable, each entry behaves like an ephemeron: if the key is reachable, then the value is
    // reachable.
    //
    // This is a fixpoint computation because marking a value can make additional keys reachable,
    // which can in turn activate additional WeakMap entries.
    {
      debug_assert_eq!(self.slots.len(), self.marks.len());

      let slots = &self.slots;
      let marks = &mut self.marks[..];

      self.gc_worklist.clear();
      let mut tracer = Tracer::new(slots, marks, &mut self.gc_worklist);

      loop {
        let mut did_mark = false;

        for (wm_idx, slot) in slots.iter().enumerate() {
          // Ignore unreachable WeakMaps.
          if tracer.marks[wm_idx] == 0 {
            continue;
          }
          let Some(HeapObject::WeakMap(wm)) = slot.value.as_ref() else {
            continue;
          };

          for entry in wm.entries.iter() {
            // Ignore stale/out-of-bounds keys and treat unmarked keys as absent.
            let key_id = entry.key.id();
            let key_idx = key_id.index() as usize;
            let Some(key_slot) = slots.get(key_idx) else {
              continue;
            };
            if key_slot.generation != key_id.generation() {
              continue;
            }
            if key_slot.value.is_none() {
              continue;
            }
            if tracer.marks[key_idx] == 0 {
              continue;
            }

            let before = tracer.worklist.len();
            tracer.trace_value(entry.value);
            if tracer.worklist.len() != before {
              did_mark = true;
            }
          }
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

        if !did_mark {
          break;
        }
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
      entries.retain(|entry| entry.upgrade_value(&*self).is_some());
      let Some(HeapObject::WeakSet(ws)) = self.slots[idx].value.as_mut() else {
        continue;
      };
      ws.entries = entries;
      // Note: we do not shrink the underlying allocation here; `retain` only updates length.
      // Slot `bytes` accounting remains unchanged because the allocation capacity is unchanged.
    }

    // WeakMap hygiene: remove entries whose keys are dead.
    //
    // Like WeakSet hygiene, this prevents the internal entry list from growing without bound across
    // GC cycles.
    for idx in 0..self.slots.len() {
      let mut entries = {
        let Some(HeapObject::WeakMap(wm)) = self.slots[idx].value.as_mut() else {
          continue;
        };
        mem::take(&mut wm.entries)
      };
      entries.retain(|entry| entry.key.upgrade_value(&*self).is_some());
      let Some(HeapObject::WeakMap(wm)) = self.slots[idx].value.as_mut() else {
        continue;
      };
      wm.entries = entries;
      // Note: we do not shrink the underlying allocation here; `retain` only updates length.
      // Slot `bytes` accounting remains unchanged because the allocation capacity is unchanged.
    }

    // FinalizationRegistry hygiene: clear dead targets from live registries.
    //
    // This uses the same "take the vector out, operate without holding a slot borrow" pattern as
    // WeakMap/WeakSet hygiene so we can query liveness via `WeakGcKey::upgrade_value`.
    //
    // We do **not** enqueue cleanup jobs during GC (which must not allocate). Instead we mark
    // registries as having `cleanup_pending` and set a heap-global flag that will be observed by
    // microtask checkpoints.
    let mut any_pending_cleanup = false;
    for idx in 0..self.slots.len() {
      let mut cells = {
        let Some(HeapObject::FinalizationRegistry(fr)) = self.slots[idx].value.as_mut() else {
          continue;
        };
        mem::take(&mut fr.cells)
      };

      let mut registry_pending = false;
      for cell in cells.iter_mut() {
        match cell.target {
          Some(target) => {
            if target.upgrade_value(&*self).is_none() {
              cell.target = None;
              registry_pending = true;
            }
          }
          None => registry_pending = true,
        }
      }

      let Some(HeapObject::FinalizationRegistry(fr)) = self.slots[idx].value.as_mut() else {
        continue;
      };
      fr.cells = cells;
      fr.cleanup_pending = registry_pending;
      if registry_pending {
        any_pending_cleanup = true;
      }
      // Note: we do not shrink the underlying allocation here; `retain` only updates length.
      // Slot `bytes` accounting remains unchanged because the allocation capacity is unchanged.
    }

    self.finalization_registry_cleanup_jobs_pending = any_pending_cleanup;

    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();

    #[cfg(any(debug_assertions, feature = "gc_validate"))]
    self.debug_validate_no_stale_internal_handles();
  }

  /// Enqueues `FinalizationRegistry` cleanup jobs into a host job queue.
  ///
  /// GC clears dead `FinalizationRegistry` targets during collection, but does **not** allocate job
  /// objects. Instead it sets [`Heap::finalization_registry_cleanup_jobs_pending`], and hosts (or
  /// the VM's microtask checkpoint loop) are expected to call this method to translate pending work
  /// into concrete jobs.
  ///
  /// The enqueued jobs invoke `%FinalizationRegistry.prototype.cleanupSome%` on each registry with
  /// pending cleanup, ensuring cleanup callbacks run through the normal job/microtask machinery.
  pub(crate) fn enqueue_finalization_registry_cleanup_jobs(
    &mut self,
    vm: &mut Vm,
    hooks: &mut dyn VmHostHooks,
  ) -> Result<(), VmError> {
    if !self.finalization_registry_cleanup_jobs_pending {
      return Ok(());
    }
    let Some(intr) = vm.intrinsics() else {
      // If there is no realm/intrinsics, `FinalizationRegistry` instances should not be observable.
      // Avoid surfacing internal errors in this low-level helper.
      //
      // Also clear the heap-level pending flag so callers that poll for pending work do not loop
      // forever.
      self.finalization_registry_cleanup_jobs_pending = false;
      return Ok(());
    };

    // The cleanup job calls the intrinsic builtin `FinalizationRegistry.prototype.cleanupSome` on
    // the registry. Capture the function object here so the job closure doesn't need to look it up.
    let cleanup_some = intr.finalization_registry_prototype_cleanup_some();

    const TICK_EVERY: usize = 256;
    let mut any_pending = false;

    for idx in 0..self.slots.len() {
      if idx % TICK_EVERY == 0 {
        vm.tick()?;
      }

      let (pending, realm, generation) = {
        let slot = &self.slots[idx];
        let Some(HeapObject::FinalizationRegistry(fr)) = slot.value.as_ref() else {
          continue;
        };
        (fr.cleanup_pending, fr.realm, slot.generation)
      };
      if !pending {
        continue;
      }
      any_pending = true;

      let registry = GcObject(HeapId::from_parts(idx as u32, generation));

      // Root captured handles for the lifetime of the queued job.
      let registry_root = self.add_root(Value::Object(registry))?;
      let cleanup_some_root = match self.add_root(Value::Object(cleanup_some)) {
        Ok(root) => root,
        Err(err) => {
          self.remove_root(registry_root);
          return Err(err);
        }
      };

      let mut roots: Vec<RootId> = Vec::new();
      if roots.try_reserve_exact(2).is_err() {
        self.remove_root(registry_root);
        self.remove_root(cleanup_some_root);
        return Err(VmError::OutOfMemory);
      }
      roots.push(registry_root);
      roots.push(cleanup_some_root);

      let job = match Job::new(JobKind::FinalizationRegistryCleanup, move |ctx, host| {
        // `FinalizationRegistryCleanupJob` calls `cleanupSome` with no argument, which uses the
        // registry's own `[[CleanupCallback]]`.
        let _ = ctx.call(host, Value::Object(cleanup_some), Value::Object(registry), &[])?;
        Ok(())
      }) {
        Ok(job) => job.with_roots(roots),
        Err(err) => {
          // If we fail to allocate the job closure, ensure we do not leak the persistent roots we
          // created for it.
          for root in roots {
            self.remove_root(root);
          }
          return Err(err);
        }
      };

      struct EnqueueCtx<'a> {
        heap: &'a mut Heap,
      }

      impl VmJobContext for EnqueueCtx<'_> {
        fn call(
          &mut self,
          _hooks: &mut dyn VmHostHooks,
          _callee: Value,
          _this: Value,
          _args: &[Value],
        ) -> Result<Value, VmError> {
          Err(VmError::Unimplemented("EnqueueCtx::call"))
        }

        fn construct(
          &mut self,
          _hooks: &mut dyn VmHostHooks,
          _callee: Value,
          _args: &[Value],
          _new_target: Value,
        ) -> Result<Value, VmError> {
          Err(VmError::Unimplemented("EnqueueCtx::construct"))
        }

        fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
          self.heap.add_root(value)
        }

        fn remove_root(&mut self, id: RootId) {
          self.heap.remove_root(id);
        }
      }

      let mut ctx = EnqueueCtx { heap: &mut *self };
      hooks.host_enqueue_promise_job_fallible(&mut ctx, job, realm)?;
    }

    // Keep the heap-level flag in sync with whether any registry still has pending cleanup work.
    self.finalization_registry_cleanup_jobs_pending = any_pending;
    Ok(())
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
          | HeapObject::DerivedConstructorState(_)
          | HeapObject::ArrayBuffer(_)
          | HeapObject::TypedArray(_)
          | HeapObject::DataView(_)
          | HeapObject::Function(_)
          | HeapObject::Proxy(_)
          | HeapObject::RegExp(_)
          | HeapObject::Promise(_)
          | HeapObject::Map(_)
          | HeapObject::Set(_)
          | HeapObject::WeakRef(_)
          | HeapObject::WeakMap(_)
          | HeapObject::WeakSet(_)
          | HeapObject::FinalizationRegistry(_)
          | HeapObject::Generator(_)
          | HeapObject::AsyncGenerator(_)
      )
    )
  }

  pub(crate) fn is_derived_constructor_state(&self, obj: GcObject) -> bool {
    matches!(
      self.get_heap_object(obj.0),
      Ok(HeapObject::DerivedConstructorState(_))
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

  /// Returns `true` if `obj` currently points to a live Generator object allocation.
  ///
  /// This is the spec-shaped "brand check" for generator objects (i.e. objects with generator
  /// internal slots).
  pub fn is_generator_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::Generator(_)))
  }

  /// Returns `true` if `obj` currently points to a live AsyncGenerator object allocation.
  ///
  /// This is the spec-shaped "brand check" for async generator objects (i.e. objects with async
  /// generator internal slots).
  ///
  pub fn is_async_generator_object(&self, obj: GcObject) -> bool {
    matches!(
      self.get_heap_object(obj.0),
      Ok(HeapObject::AsyncGenerator(_))
    )
  }

  /// Returns `true` if `obj` currently points to a live ArrayBuffer object allocation.
  pub fn is_array_buffer_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::ArrayBuffer(_)))
  }

  /// Returns `true` if `obj` is a detached `ArrayBuffer`.
  ///
  /// This corresponds to ECMA-262 `IsDetachedBuffer` (`[[ArrayBufferData]] == null`).
  ///
  /// `vm-js` represents detachment by clearing the backing store (`data: None`), which also makes
  /// `byteLength` report `0`.
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

  /// Returns the `%TypedArray%.prototype[@@toStringTag]` name for `obj` when it is a typed array.
  ///
  /// This corresponds to the ECMA-262 `TypedArrayName` internal slot used by WebIDL overload
  /// resolution and BufferSource conversions.
  pub fn typed_array_name(&self, obj: GcObject) -> Option<&'static str> {
    match self.get_heap_object(obj.0) {
      Ok(HeapObject::TypedArray(arr)) => Some(match arr.kind {
        TypedArrayKind::Int8 => "Int8Array",
        TypedArrayKind::Uint8 => "Uint8Array",
        TypedArrayKind::Uint8Clamped => "Uint8ClampedArray",
        TypedArrayKind::Int16 => "Int16Array",
        TypedArrayKind::Uint16 => "Uint16Array",
        TypedArrayKind::Int32 => "Int32Array",
        TypedArrayKind::Uint32 => "Uint32Array",
        TypedArrayKind::Float32 => "Float32Array",
        TypedArrayKind::Float64 => "Float64Array",
      }),
      Ok(_) | Err(_) => None,
    }
  }

  /// Returns `true` if `obj` currently points to a live DataView object allocation.
  pub fn is_data_view_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::DataView(_)))
  }

  /// Returns `true` if `obj` currently points to a live WeakMap object allocation.
  pub fn is_weak_map_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::WeakMap(_)))
  }

  /// Returns `true` if `obj` currently points to a live Map object allocation.
  pub fn is_map_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::Map(_)))
  }

  /// Returns `true` if `obj` currently points to a live Set object allocation.
  pub fn is_set_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::Set(_)))
  }

  /// Returns `true` if `obj` currently points to a live WeakSet object allocation.
  pub fn is_weak_set_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::WeakSet(_)))
  }

  /// Returns `true` if `obj` currently points to a live WeakRef object allocation.
  pub fn is_weak_ref_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::WeakRef(_)))
  }

  /// Returns `true` if `obj` currently points to a live FinalizationRegistry object allocation.
  pub fn is_finalization_registry_object(&self, obj: GcObject) -> bool {
    matches!(
      self.get_heap_object(obj.0),
      Ok(HeapObject::FinalizationRegistry(_))
    )
  }

  /// Returns `true` if `obj` currently points to a live ArrayBuffer view (typed array or DataView).
  pub fn is_array_buffer_view_object(&self, obj: GcObject) -> bool {
    matches!(
      self.get_heap_object(obj.0),
      Ok(HeapObject::TypedArray(_) | HeapObject::DataView(_))
    )
  }

  /// Returns `true` if `obj` currently points to a live RegExp object allocation.
  pub fn is_regexp_object(&self, obj: GcObject) -> bool {
    matches!(self.get_heap_object(obj.0), Ok(HeapObject::RegExp(_)))
  }

  /// Returns `true` if `obj` currently points to a live Error object allocation.
  ///
  /// This is the `[[ErrorData]]` brand check used by `Object.prototype.toString` builtin-tag
  /// selection. It must distinguish real Error instances from ordinary objects that merely inherit
  /// from `%Error.prototype%` (e.g. `Object.create(Error.prototype)`).
  pub fn is_error_object(&self, obj: GcObject) -> bool {
    match self.get_heap_object(obj.0) {
      Ok(HeapObject::Object(o)) => matches!(o.base.kind, ObjectKind::Error),
      Ok(_) | Err(_) => false,
    }
  }

  /// Returns `true` if `obj` currently points to a live Arguments object allocation.
  ///
  /// This is the `[[ParameterMap]]` brand check used by `Object.prototype.toString` builtin-tag
  /// selection. `vm-js` currently uses a minimal arguments object implementation (not mapped), but
  /// it still must be branded for spec-correct `Object.prototype.toString`.
  pub fn is_arguments_object(&self, obj: GcObject) -> bool {
    match self.get_heap_object(obj.0) {
      Ok(HeapObject::Object(o)) => matches!(o.base.kind, ObjectKind::Arguments),
      Ok(_) | Err(_) => false,
    }
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

  pub(crate) fn array_buffer_max_byte_length(&self, obj: GcObject) -> Result<usize, VmError> {
    Ok(self.get_array_buffer(obj)?.max_byte_length())
  }

  pub(crate) fn array_buffer_is_resizable(&self, obj: GcObject) -> Result<bool, VmError> {
    Ok(self.get_array_buffer(obj)?.is_resizable())
  }

  pub(crate) fn array_buffer_is_immutable(&self, obj: GcObject) -> Result<bool, VmError> {
    Ok(self.get_array_buffer(obj)?.is_immutable())
  }

  pub(crate) fn array_buffer_set_immutable(&mut self, obj: GcObject, immutable: bool) -> Result<(), VmError> {
    self.get_array_buffer_mut(obj)?.immutable = immutable;
    Ok(())
  }

  pub(crate) fn array_buffer_set_resizable(
    &mut self,
    obj: GcObject,
    resizable: bool,
    max_byte_length: usize,
  ) -> Result<(), VmError> {
    let buf = self.get_array_buffer_mut(obj)?;
    buf.resizable = resizable;
    buf.max_byte_length = max_byte_length;
    Ok(())
  }

  pub(crate) fn array_buffer_is_detached(&self, obj: GcObject) -> Result<bool, VmError> {
    self.is_detached_array_buffer(obj)
  }

  /// Transfers the backing store of `src` into a new `ArrayBuffer` object without copying bytes.
  ///
  /// On success:
  /// - `src` becomes detached (`src.byteLength === 0`),
  /// - the returned `ArrayBuffer` owns the original backing store,
  /// - and the heap's `external_bytes` accounting is unchanged (bytes remain tracked exactly once).
  ///
  /// This is intended for host implementations of HTML structured clone transfer semantics.
  pub fn transfer_array_buffer(&mut self, src: GcObject) -> Result<GcObject, VmError> {
    // Validate up-front so we don't allocate a destination buffer if `src` is not transferable.
    // Also snapshot metadata so the transferred buffer preserves `resizable` / `maxByteLength` /
    // immutability state.
    let (src_max_byte_length, src_resizable, src_immutable) = {
      let buf = self.get_array_buffer(src)?;
      if buf.data.is_none() {
        return Err(VmError::TypeError("Cannot transfer detached ArrayBuffer"));
      }
      (buf.max_byte_length(), buf.is_resizable(), buf.is_immutable())
    };

    // Root `src` across any allocation/GC triggered by `ensure_can_allocate`.
    let mut scope = self.scope();
    scope.push_root(Value::Object(src))?;

    // Only require room for the new heap allocation (object metadata); the external backing store
    // bytes are already tracked and are moved without allocation.
    let new_bytes = JsArrayBuffer::heap_size_bytes_for_property_count(0);
    scope
      .heap
      .ensure_can_allocate_with(|heap| heap.additional_bytes_for_heap_alloc(new_bytes))?;

    // Allocate the destination buffer in a detached state, then attach the transferred data. This
    // avoids any need to "undo" a detachment if the allocation fails.
    let obj = HeapObject::ArrayBuffer(JsArrayBuffer::new_detached(
      None,
      src_max_byte_length,
      src_resizable,
      src_immutable,
    ));
    let dst = GcObject(scope.heap.alloc_unchecked_after_ensure(obj, new_bytes)?);

    let data = {
      let src_buf = scope.heap.get_array_buffer_mut(src)?;
      src_buf
        .data
        .take()
        .ok_or(VmError::TypeError("Cannot transfer detached ArrayBuffer"))?
    };
    let dst_buf = scope.heap.get_array_buffer_mut(dst)?;
    debug_assert!(dst_buf.data.is_none());
    dst_buf.data = Some(data);

    Ok(dst)
  }
  /// Returns a borrowed view of the bytes backing an `ArrayBuffer` object.
  ///
  /// This is intended for host bindings that need to read `ArrayBuffer` contents (e.g. `TextDecoder`).
  /// The returned slice is valid as long as the underlying `ArrayBuffer` remains live and the heap is
  /// not mutably borrowed.
  ///
  /// # Errors
  ///
  /// Returns a `TypeError` if the `ArrayBuffer` is detached.
  pub fn array_buffer_data(&self, obj: GcObject) -> Result<&[u8], VmError> {
    let buf = self.get_array_buffer(obj)?;
    buf
      .data
      .as_deref()
      .ok_or(VmError::TypeError("ArrayBuffer is detached"))
  }

  /// Detaches an `ArrayBuffer` by dropping its backing store.
  ///
  /// A detached `ArrayBuffer` has `byteLength === 0` and attempts to access its underlying bytes
  /// should throw (see [`Heap::array_buffer_data`]).
  ///
  /// This is intended for host embeddings implementing "transfer"/structured clone semantics.
  pub fn detach_array_buffer(&mut self, obj: GcObject) -> Result<(), VmError> {
    let _ = self.detach_array_buffer_take_data(obj)?;
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

  pub(crate) fn resize_array_buffer(&mut self, obj: GcObject, new_byte_length: usize) -> Result<(), VmError> {
    // Validate without holding a mutable borrow across allocation.
    let (old_len, immutable) = {
      let buf = self.get_array_buffer(obj)?;
      (buf.byte_length(), buf.is_immutable())
    };
    if immutable {
      return Err(VmError::TypeError("ArrayBuffer is immutable"));
    }
    if self.is_detached_array_buffer(obj)? {
      return Err(VmError::TypeError("ArrayBuffer is detached"));
    }

    if new_byte_length == old_len {
      return Ok(());
    }

    if new_byte_length > old_len {
      let additional = new_byte_length - old_len;
      self.ensure_can_allocate_with(|_| additional)?;
    }

    let buf = self.get_array_buffer_mut(obj)?;
    let Some(data) = buf.data.take() else {
      return Err(VmError::TypeError("ArrayBuffer is detached"));
    };

    // Resize via a Vec so we can grow with fallible allocation and then re-box to an exact-sized
    // slice (matching our external-bytes accounting model).
    let mut v = data.into_vec();
    if new_byte_length > v.len() {
      if v.try_reserve_exact(new_byte_length - v.len()).is_err() {
        // Preserve the observable state of the ArrayBuffer if allocation fails.
        buf.data = Some(v.into_boxed_slice());
        return Err(VmError::OutOfMemory);
      };
    }
    v.resize(new_byte_length, 0);
    let new_data = v.into_boxed_slice();

    buf.data = Some(new_data);

    if new_byte_length > old_len {
      self.add_external_bytes(new_byte_length - old_len);
    } else {
      self.sub_external_bytes(old_len - new_byte_length);
    }
    Ok(())
  }

  pub(crate) fn array_buffer_write(&mut self, obj: GcObject, offset: usize, bytes: &[u8]) -> Result<(), VmError> {
    if bytes.is_empty() {
      return Ok(());
    }
    // Validate and bounds-check before mutably borrowing the backing store.
    let buf = self.get_array_buffer(obj)?;
    if buf.is_immutable() {
      return Err(VmError::TypeError("ArrayBuffer is immutable"));
    }
    let buf_len = buf.byte_length();
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

  /// Copies `len` bytes from `src[src_offset..]` into `dst[dst_offset..]`, ticking periodically.
  ///
  /// This is intended for spec algorithms like typed array cloning/copying that need to preserve
  /// underlying byte patterns (e.g. Float32 NaN payloads) while still respecting VM execution
  /// budgets.
  ///
  /// # Errors
  ///
  /// Returns a `TypeError` if either buffer is detached or if the copy range is out of bounds.
  pub(crate) fn array_buffer_copy_with_tick<F>(
    &mut self,
    src: GcObject,
    src_offset: usize,
    dst: GcObject,
    dst_offset: usize,
    len: usize,
    tick_every_bytes: usize,
    mut tick: F,
  ) -> Result<(), VmError>
  where
    F: FnMut() -> Result<(), VmError>,
  {
    if len == 0 {
      return Ok(());
    }

    // Validate and bounds-check before taking any raw pointers.
    let (src_ptr, src_len) = {
      let buf = self.get_array_buffer(src)?;
      let data = buf
        .data
        .as_deref()
        .ok_or(VmError::TypeError("ArrayBuffer is detached"))?;
      (data.as_ptr(), data.len())
    };
    let (_dst_ptr, dst_len) = {
      let buf = self.get_array_buffer(dst)?;
      let data = buf
        .data
        .as_deref()
        .ok_or(VmError::TypeError("ArrayBuffer is detached"))?;
      (data.as_ptr(), data.len())
    };

    let src_end = src_offset.checked_add(len).ok_or(VmError::OutOfMemory)?;
    if src_end > src_len {
      return Err(VmError::TypeError("ArrayBuffer read out of bounds"));
    }
    let dst_end = dst_offset.checked_add(len).ok_or(VmError::OutOfMemory)?;
    if dst_end > dst_len {
      return Err(VmError::TypeError("ArrayBuffer write out of bounds"));
    }

    // Now take a mutable pointer for the destination. We must take this after validation and
    // after any immutable borrows of `self` are dropped.
    let dst_mut_ptr = {
      let buf = self.get_array_buffer_mut(dst)?;
      let data = buf
        .data
        .as_deref_mut()
        .ok_or(VmError::TypeError("ArrayBuffer is detached"))?;
      data.as_mut_ptr()
    };

    // Copy in chunks so hostile inputs cannot perform long stretches of uninterruptible work.
    let tick_every_bytes = tick_every_bytes.max(1);
    let mut copied = 0usize;
    while copied < len {
      if copied % tick_every_bytes == 0 {
        tick()?;
      }

      let remaining = len - copied;
      let chunk_len = remaining.min(tick_every_bytes);

      // Safety: bounds were checked above; the backing buffers are stable (boxed slices) and the
      // heap is non-moving. Copying between potentially overlapping regions is permitted by
      // `ptr::copy`.
      unsafe {
        let src_chunk = src_ptr.add(src_offset + copied);
        let dst_chunk = dst_mut_ptr.add(dst_offset + copied);
        std::ptr::copy(src_chunk, dst_chunk, chunk_len);
      }

      copied = copied.saturating_add(chunk_len);
    }

    Ok(())
  }

  fn require_typed_array(&self, obj: GcObject) -> Result<&JsTypedArray, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::TypedArray(a) => Ok(a),
      _ => Err(VmError::TypeError(
        "Heap typed array operation called on incompatible receiver",
      )),
    }
  }

  /// Returns the [`TypedArrayKind`] for a typed array object.
  ///
  /// This reads the typed array's internal slots directly (it does not consult JS-visible
  /// properties) and therefore remains reliable even if user code mutates the typed array's
  /// prototype chain.
  ///
  /// # Errors
  ///
  /// - Returns [`VmError::InvalidHandle`] for stale handles.
  /// - Returns a catchable [`VmError::TypeError`] if `obj` is not a typed array object.
  pub fn typed_array_kind(&self, obj: GcObject) -> Result<TypedArrayKind, VmError> {
    Ok(self.require_typed_array(obj)?.kind)
  }

  /// Returns `true` if `obj` is an **integer** typed array (Int8/Uint8/Uint8Clamped/Int16/Uint16/Int32/Uint32).
  ///
  /// This is intended for host bindings like WebCrypto `crypto.getRandomValues`, which must reject
  /// float typed arrays.
  ///
  /// # Errors
  ///
  /// Returns an error if `obj` is not a live typed array object.
  pub fn typed_array_is_integer_kind(&self, obj: GcObject) -> Result<bool, VmError> {
    let view = self.get_typed_array(obj)?;
    Ok(matches!(
      view.kind,
      TypedArrayKind::Int8
        | TypedArrayKind::Uint8
        | TypedArrayKind::Uint8Clamped
        | TypedArrayKind::Int16
        | TypedArrayKind::Uint16
        | TypedArrayKind::Int32
        | TypedArrayKind::Uint32
    ))
  }

  /// Returns `(buffer, byte_offset, byte_length)` for a typed array view.
  ///
  /// This is intended for host bindings that need to operate on the raw bytes covered by a typed
  /// array (e.g. WebCrypto `crypto.getRandomValues`), while:
  /// - respecting the view's `byteOffset`/`byteLength` internal slots
  /// - rejecting detached or out-of-bounds views
  ///
  /// # Errors
  ///
  /// Returns:
  /// - `InvalidHandle` if `obj` is not a live typed array object.
  /// - `TypeError` if the backing ArrayBuffer is detached or the view is out of bounds.
  pub fn typed_array_view_bytes(&self, obj: GcObject) -> Result<(GcObject, usize, usize), VmError> {
    let view = self.get_typed_array(obj)?;
    let buffer = view.viewed_array_buffer;
    let byte_offset = view.byte_offset;
    let byte_length = view.byte_length()?;

    let buf = self.get_array_buffer(buffer)?;
    let data = buf
      .data
      .as_deref()
      .ok_or(VmError::TypeError("ArrayBuffer is detached"))?;
    let end = byte_offset
      .checked_add(byte_length)
      .ok_or(VmError::InvariantViolation("TypedArray byte offset overflow"))?;
    if end > data.len() {
      return Err(VmError::TypeError("TypedArray view out of bounds"));
    }

    Ok((buffer, byte_offset, byte_length))
  }

  fn typed_array_view_is_out_of_bounds(&self, view: &JsTypedArray) -> Result<bool, VmError> {
    // Detached buffers count as out-of-bounds.
    let buf = self.get_array_buffer(view.viewed_array_buffer)?;
    let Some(data) = buf.data.as_deref() else {
      return Ok(true);
    };
    let buf_len = data.len();

    let byte_len = match view.length.checked_mul(view.kind.bytes_per_element()) {
      Some(n) => n,
      None => return Ok(true),
    };
    let end = match view.byte_offset.checked_add(byte_len) {
      Some(end) => end,
      None => return Ok(true),
    };

    Ok(end > buf_len)
  }

  /// Returns the typed array `length` in elements.
  ///
  /// If the view is out of bounds for its backing buffer (including detachment), this reports `0`
  /// (mirroring `%TypedArray%.prototype.length` semantics).
  ///
  /// # Errors
  ///
  /// - Returns [`VmError::InvalidHandle`] for stale handles.
  /// - Returns a catchable [`VmError::TypeError`] if `obj` is not a typed array object.
  pub fn typed_array_length(&self, obj: GcObject) -> Result<usize, VmError> {
    let view = self.require_typed_array(obj)?;
    if self.typed_array_view_is_out_of_bounds(view)? {
      return Ok(0);
    }
    Ok(view.length)
  }

  /// Returns the typed array `byteLength`.
  ///
  /// If the view is out of bounds for its backing buffer (including detachment), this reports `0`
  /// (mirroring `%TypedArray%.prototype.byteLength` semantics).
  ///
  /// # Errors
  ///
  /// - Returns [`VmError::InvalidHandle`] for stale handles.
  /// - Returns a catchable [`VmError::TypeError`] if `obj` is not a typed array object.
  pub fn typed_array_byte_length(&self, obj: GcObject) -> Result<usize, VmError> {
    let view = self.require_typed_array(obj)?;
    if self.typed_array_view_is_out_of_bounds(view)? {
      return Ok(0);
    }
    view.byte_length()
  }

  /// Returns the typed array `byteOffset`.
  ///
  /// If the view is out of bounds for its backing buffer (including detachment), this reports `0`
  /// (mirroring `%TypedArray%.prototype.byteOffset` semantics).
  ///
  /// # Errors
  ///
  /// - Returns [`VmError::InvalidHandle`] for stale handles.
  /// - Returns a catchable [`VmError::TypeError`] if `obj` is not a typed array object.
  pub fn typed_array_byte_offset(&self, obj: GcObject) -> Result<usize, VmError> {
    let view = self.require_typed_array(obj)?;
    if self.typed_array_view_is_out_of_bounds(view)? {
      return Ok(0);
    }
    Ok(view.byte_offset)
  }

  /// Returns the typed array `buffer`.
  ///
  /// Unlike `length`/`byteLength`/`byteOffset`, the backing `ArrayBuffer` handle is returned even
  /// if the buffer is detached or the view is out of bounds.
  ///
  /// # Errors
  ///
  /// - Returns [`VmError::InvalidHandle`] for stale handles.
  /// - Returns a catchable [`VmError::TypeError`] if `obj` is not a typed array object.
  pub fn typed_array_buffer(&self, obj: GcObject) -> Result<GcObject, VmError> {
    Ok(self.require_typed_array(obj)?.viewed_array_buffer)
  }

  pub(crate) fn typed_array_get_element_value(
    &self,
    obj: GcObject,
    index: usize,
  ) -> Result<Option<Value>, VmError> {
    let view = self.get_typed_array(obj)?;
    // Integer-indexed element access must treat detached *and* out-of-bounds views as empty.
    //
    // Spec: `IsValidIntegerIndex` returns false when `IsTypedArrayOutOfBounds` is true, which makes
    // element reads return `undefined`.
    if self.typed_array_view_is_out_of_bounds(view)? {
      return Ok(None);
    }
    if index >= view.length {
      return Ok(None);
    }
    Ok(Some(self.typed_array_get_value(view, index)?))
  }

  fn require_data_view(&self, obj: GcObject) -> Result<&JsDataView, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::DataView(v) => Ok(v),
      _ => Err(VmError::TypeError(
        "Heap DataView operation called on incompatible receiver",
      )),
    }
  }

  fn data_view_is_out_of_bounds(&self, view: &JsDataView) -> Result<bool, VmError> {
    // Detached buffers count as out-of-bounds.
    let buf = self.get_array_buffer(view.viewed_array_buffer)?;
    let Some(data) = buf.data.as_deref() else {
      return Ok(true);
    };
    let buf_len = data.len();

    let end = match view.byte_offset.checked_add(view.byte_length) {
      Some(end) => end,
      None => return Ok(true),
    };
    Ok(end > buf_len)
  }

  /// Returns the DataView `byteLength`.
  ///
  /// If the view is out of bounds for its backing buffer (including detachment), this reports `0`
  /// for host-side convenience (similar to typed array introspection helpers).
  ///
  /// # Errors
  ///
  /// - Returns [`VmError::InvalidHandle`] for stale handles.
  /// - Returns a catchable [`VmError::TypeError`] if `obj` is not a DataView object.
  pub fn data_view_byte_length(&self, obj: GcObject) -> Result<usize, VmError> {
    let view = self.require_data_view(obj)?;
    if self.data_view_is_out_of_bounds(view)? {
      return Ok(0);
    }
    Ok(view.byte_length)
  }

  pub(crate) fn data_view_byte_length_slot(&self, obj: GcObject) -> Result<usize, VmError> {
    Ok(self.require_data_view(obj)?.byte_length)
  }

  /// Returns the DataView `byteOffset`.
  ///
  /// If the view is out of bounds for its backing buffer (including detachment), this reports `0`
  /// for host-side convenience (similar to typed array introspection helpers).
  ///
  /// # Errors
  ///
  /// - Returns [`VmError::InvalidHandle`] for stale handles.
  /// - Returns a catchable [`VmError::TypeError`] if `obj` is not a DataView object.
  pub fn data_view_byte_offset(&self, obj: GcObject) -> Result<usize, VmError> {
    let view = self.require_data_view(obj)?;
    if self.data_view_is_out_of_bounds(view)? {
      return Ok(0);
    }
    Ok(view.byte_offset)
  }

  pub(crate) fn data_view_byte_offset_slot(&self, obj: GcObject) -> Result<usize, VmError> {
    Ok(self.require_data_view(obj)?.byte_offset)
  }

  /// Returns the DataView `buffer`.
  ///
  /// The backing `ArrayBuffer` handle is returned even if the buffer is detached or the view is
  /// out of bounds.
  ///
  /// # Errors
  ///
  /// - Returns [`VmError::InvalidHandle`] for stale handles.
  /// - Returns a catchable [`VmError::TypeError`] if `obj` is not a DataView object.
  pub fn data_view_buffer(&self, obj: GcObject) -> Result<GcObject, VmError> {
    Ok(self.require_data_view(obj)?.viewed_array_buffer)
  }

  /// Returns `true` if the typed array's `[[ByteOffset]] + [[ByteLength]]` does not fit within its
  /// backing ArrayBuffer, or if the backing buffer is detached.
  pub(crate) fn typed_array_is_out_of_bounds(&self, obj: GcObject) -> Result<bool, VmError> {
    let view = self.get_typed_array(obj)?;
    let buffer = view.viewed_array_buffer;
    if self.is_detached_array_buffer(buffer)? {
      return Ok(true);
    }

    let buf_len = self.array_buffer_byte_length(buffer)?;
    let byte_length = view.byte_length()?;
    let end = match view.byte_offset.checked_add(byte_length) {
      Some(end) => end,
      None => return Ok(true),
    };
    Ok(end > buf_len)
  }

  /// Returns a borrowed view of the bytes visible through a `Uint8Array` view.
  ///
  /// This is intended for host bindings that need to read `Uint8Array` contents without round-tripping
  /// through JS (e.g. `TextDecoder`).
  ///
  /// The returned slice is valid as long as the underlying `Uint8Array` and its backing `ArrayBuffer`
  /// remain live and the heap is not mutably borrowed.
  ///
  /// # Errors
  ///
  /// Returns a `TypeError` if the backing `ArrayBuffer` is detached or if the view is out of bounds.
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
    data
      .get(start..end)
      .ok_or(VmError::TypeError("Uint8Array view out of bounds"))
  }

  /// Writes `bytes` into a `Uint8Array` view starting at element index `index`.
  ///
  /// This is intended for host bindings that need to efficiently populate typed arrays without
  /// round-tripping through JS property sets.
  ///
  /// Returns the number of bytes written, which is `min(bytes.len(), view.length - index)`. If
  /// `index` is out of bounds or `bytes` is empty, this returns `Ok(0)` (mirroring typed array
  /// out-of-bounds write semantics).
  ///
  /// If the backing `ArrayBuffer` is detached, this returns `Ok(0)` (treating the view as empty),
  /// matching integer-indexed writes on detached typed arrays.
  ///
  /// If the view is out of bounds (e.g. due to internal corruption or a resizable `ArrayBuffer`
  /// shrinking), this returns `Ok(0)` to mirror out-of-bounds write semantics while avoiding host
  /// panics/invariant violations.
  ///
  /// # Errors
  ///
  /// Returns an error if `obj` is not a live `Uint8Array` object.
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

    let view_end = byte_offset
      .checked_add(length)
      .ok_or(VmError::InvariantViolation("Uint8Array byte offset overflow"))?;

    let abs_start = byte_offset
      .checked_add(index)
      .ok_or(VmError::InvariantViolation("Uint8Array byte offset overflow"))?;
    let abs_end = abs_start
      .checked_add(max_write)
      .ok_or(VmError::InvariantViolation("Uint8Array byte offset overflow"))?;

    // Validate the view is in-bounds and the backing buffer is attached.
    let buf = self.get_array_buffer(buffer)?;
    let Some(data) = buf.data.as_deref() else {
      return Ok(0);
    };
    let buf_len = data.len();
    if view_end > buf_len {
      // Out-of-bounds views behave like empty typed arrays for host byte writes.
      return Ok(0);
    }

    let buf = self.get_array_buffer_mut(buffer)?;
    let Some(data) = buf.data.as_deref_mut() else {
      return Ok(0);
    };
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
        | HeapObject::Map(_)
        | HeapObject::Set(_)
        | HeapObject::WeakRef(_)
        | HeapObject::WeakMap(_)
        | HeapObject::WeakSet(_)
        | HeapObject::FinalizationRegistry(_)
        | HeapObject::Generator(_)
        | HeapObject::AsyncGenerator(_),
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
        | HeapObject::Map(_)
        | HeapObject::Set(_)
        | HeapObject::WeakRef(_)
        | HeapObject::WeakMap(_)
        | HeapObject::WeakSet(_)
        | HeapObject::FinalizationRegistry(_)
        | HeapObject::Generator(_)
        | HeapObject::AsyncGenerator(_),
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
        | HeapObject::Map(_)
        | HeapObject::Set(_)
        | HeapObject::WeakRef(_)
        | HeapObject::WeakMap(_)
        | HeapObject::WeakSet(_)
        | HeapObject::FinalizationRegistry(_)
        | HeapObject::Generator(_)
        | HeapObject::AsyncGenerator(_),
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

  /// Returns `true` if `b` currently points to a live BigInt allocation.
  pub fn is_valid_bigint(&self, b: GcBigInt) -> bool {
    matches!(self.get_heap_object(b.0), Ok(HeapObject::BigInt(_)))
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
    let Value::Object(obj) = value else {
      return Ok(false);
    };
    match self.get_heap_object(obj.0)? {
      HeapObject::Function(_) => Ok(true),
      HeapObject::Proxy(p) => Ok(p.callable),
      _ => Ok(false),
    }
  }

  /// Returns `true` if `value` is a constructor (i.e. has an ECMAScript `[[Construct]]` internal
  /// method).
  pub fn is_constructor(&self, value: Value) -> Result<bool, VmError> {
    let Value::Object(obj) = value else {
      return Ok(false);
    };
    match self.get_heap_object(obj.0)? {
      HeapObject::Function(f) => Ok(f.construct.is_some()),
      HeapObject::Proxy(p) => Ok(p.constructable),
      _ => Ok(false),
    }
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

  /// Gets the BigInt contents for `b`.
  pub fn get_bigint(&self, b: GcBigInt) -> Result<&JsBigInt, VmError> {
    match self.get_heap_object(b.0)? {
      HeapObject::BigInt(b) => Ok(b),
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
      HeapObject::RegExp(r) => Ok(&r.base),
      HeapObject::Generator(g) => Ok(&g.object.base),
      HeapObject::AsyncGenerator(g) => Ok(&g.object.base),
      HeapObject::Promise(p) => Ok(&p.object.base),
      HeapObject::Map(m) => Ok(&m.base),
      HeapObject::Set(s) => Ok(&s.base),
      HeapObject::WeakRef(wr) => Ok(&wr.base),
      HeapObject::WeakMap(wm) => Ok(&wm.base),
      HeapObject::WeakSet(ws) => Ok(&ws.base),
      HeapObject::FinalizationRegistry(fr) => Ok(&fr.base),
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
      HeapObject::RegExp(r) => Ok(&mut r.base),
      HeapObject::Generator(g) => Ok(&mut g.object.base),
      HeapObject::AsyncGenerator(g) => Ok(&mut g.object.base),
      HeapObject::Promise(p) => Ok(&mut p.object.base),
      HeapObject::Map(m) => Ok(&mut m.base),
      HeapObject::Set(s) => Ok(&mut s.base),
      HeapObject::WeakRef(wr) => Ok(&mut wr.base),
      HeapObject::WeakMap(wm) => Ok(&mut wm.base),
      HeapObject::WeakSet(ws) => Ok(&mut ws.base),
      HeapObject::FinalizationRegistry(fr) => Ok(&mut fr.base),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn normalize_same_value_zero(value: Value) -> Value {
    // Map/Set use SameValueZero semantics, which treat -0 and +0 as the same key/value.
    // Canonicalize to +0 at insertion/lookup so internal equality checks can reuse SameValue for
    // numbers.
    match value {
      Value::Number(n) if n == 0.0 => Value::Number(0.0),
      other => other,
    }
  }

  fn same_value_zero_with_tick(
    &self,
    a: Value,
    b: Value,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    let a = Self::normalize_same_value_zero(a);
    let b = Self::normalize_same_value_zero(b);
    match (a, b) {
      (Value::Number(a), Value::Number(b)) => {
        if a.is_nan() && b.is_nan() {
          return Ok(true);
        }
        Ok(a == b)
      }
      (Value::String(a), Value::String(b)) => {
        let a_units = self.get_string(a)?.as_code_units();
        let b_units = self.get_string(b)?.as_code_units();
        Ok(crate::tick::code_units_eq_with_ticks(a_units, b_units, || tick())?)
      }
      _ => Ok(a.same_value(b, self)),
    }
  }

  /// Returns the `[[MapData]]` entry list length for `map` (including deleted slots).
  pub fn map_entries_len(&self, map: GcObject) -> Result<usize, VmError> {
    match self.get_heap_object(map.0)? {
      HeapObject::Map(m) => Ok(m.entries.len()),
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Returns the entry at `index` for `map`, if present and not deleted.
  pub fn map_entry_at(&self, map: GcObject, index: usize) -> Result<Option<(Value, Value)>, VmError> {
    let HeapObject::Map(m) = self.get_heap_object(map.0)? else {
      return Err(VmError::invalid_handle());
    };
    let Some(entry) = m.entries.get(index) else {
      return Ok(None);
    };
    let (Some(k), Some(v)) = (entry.key, entry.value) else {
      return Ok(None);
    };
    Ok(Some((k, v)))
  }

  pub fn map_size(&self, map: GcObject) -> Result<usize, VmError> {
    match self.get_heap_object(map.0)? {
      HeapObject::Map(m) => Ok(m.size),
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub fn map_get_with_tick(
    &self,
    map: GcObject,
    key: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<Value>, VmError> {
    let key = Self::normalize_same_value_zero(key);
    let HeapObject::Map(m) = self.get_heap_object(map.0)? else {
      return Err(VmError::invalid_handle());
    };

    for (i, entry) in m.entries.iter().enumerate() {
      tick::tick_every(i, tick::DEFAULT_TICK_EVERY, &mut tick)?;
      let (Some(k), Some(v)) = (entry.key, entry.value) else {
        continue;
      };
      if self.same_value_zero_with_tick(k, key, &mut tick)? {
        return Ok(Some(v));
      }
    }
    Ok(None)
  }

  pub fn map_has_with_tick(
    &self,
    map: GcObject,
    key: Value,
    tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    Ok(self.map_get_with_tick(map, key, tick)?.is_some())
  }

  pub fn map_set_with_tick(
    &mut self,
    map: GcObject,
    key: Value,
    value: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<(), VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(key));
    debug_assert!(self.debug_value_is_valid_or_primitive(value));

    if !self.is_valid_object(map) {
      return Err(VmError::invalid_handle());
    }

    // Canonicalize -0 to +0 at insertion time.
    let key = Self::normalize_same_value_zero(key);

    // Root inputs across any potential GC while growing the entry vector or property table.
    let mut scope = self.scope();
    scope.push_roots(&[Value::Object(map), key, value])?;
    scope.heap.map_set_rooted_with_tick(map, key, value, &mut tick)
  }

  fn map_set_rooted_with_tick(
    &mut self,
    map: GcObject,
    key: Value,
    value: Value,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<(), VmError> {
    let slot_idx = self
      .validate(map.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    // Two-phase borrow: search without holding a mutable borrow of the map allocation while calling
    // `same_value_zero_with_tick` (which needs `&self` for string reads).
    let (existing_idx, entry_len, entry_cap, property_count, old_bytes) = {
      let slot = &self.slots[slot_idx];
      let Some(HeapObject::Map(m)) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };

      let mut existing_idx: Option<usize> = None;
      for (i, entry) in m.entries.iter().enumerate() {
        tick::tick_every(i, tick::DEFAULT_TICK_EVERY, tick)?;
        let Some(k) = entry.key else {
          continue;
        };
        if self.same_value_zero_with_tick(k, key, tick)? {
          existing_idx = Some(i);
          break;
        }
      }

      (
        existing_idx,
        m.entries.len(),
        m.entries.capacity(),
        m.base.properties.len(),
        slot.bytes,
      )
    };

    if let Some(existing_idx) = existing_idx {
      let Some(HeapObject::Map(m)) = self.slots[slot_idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      if let Some(entry) = m.entries.get_mut(existing_idx) {
        // Existing live entry: replace value without changing insertion order.
        entry.value = Some(value);
      }
      return Ok(());
    }

    let required_len = entry_len.checked_add(1).ok_or(VmError::OutOfMemory)?;
    let desired_capacity = grown_capacity(entry_cap, required_len);
    if desired_capacity == usize::MAX {
      return Err(VmError::OutOfMemory);
    }

    let expected_new_bytes = JsMap::heap_size_bytes_for_counts(property_count, desired_capacity);
    let grow_by = expected_new_bytes.saturating_sub(old_bytes);
    if grow_by != 0 {
      self.ensure_can_allocate(grow_by)?;
      let Some(HeapObject::Map(m)) = self.slots[slot_idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      reserve_vec_to_len::<MapEntry>(&mut m.entries, required_len)?;
    }

    let Some(HeapObject::Map(m)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    m.entries.push(MapEntry {
      key: Some(key),
      value: Some(value),
    });
    m.size = m.size.saturating_add(1);
    let new_bytes = m.heap_size_bytes();
    self.update_slot_bytes(slot_idx, new_bytes);

    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
    Ok(())
  }

  pub fn map_delete_with_tick(
    &mut self,
    map: GcObject,
    key: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(key));
    if !self.is_valid_object(map) {
      return Err(VmError::invalid_handle());
    }
    let key = Self::normalize_same_value_zero(key);

    let mut scope = self.scope();
    scope.push_roots(&[Value::Object(map), key])?;
    scope.heap.map_delete_rooted_with_tick(map, key, &mut tick)
  }

  fn map_delete_rooted_with_tick(
    &mut self,
    map: GcObject,
    key: Value,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    let slot_idx = self
      .validate(map.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let existing_idx = {
      let slot = &self.slots[slot_idx];
      let Some(HeapObject::Map(m)) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };

      let mut found: Option<usize> = None;
      for (i, entry) in m.entries.iter().enumerate() {
        tick::tick_every(i, tick::DEFAULT_TICK_EVERY, tick)?;
        let Some(k) = entry.key else {
          continue;
        };
        if self.same_value_zero_with_tick(k, key, tick)? {
          found = Some(i);
          break;
        }
      }
      found
    };

    let Some(idx) = existing_idx else {
      return Ok(false);
    };

    let Some(HeapObject::Map(m)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    if let Some(entry) = m.entries.get_mut(idx) {
      if entry.key.is_some() {
        entry.key = None;
        entry.value = None;
        m.size = m.size.saturating_sub(1);
        return Ok(true);
      }
    }
    Ok(false)
  }

  pub fn map_clear_with_tick(
    &mut self,
    map: GcObject,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<(), VmError> {
    if !self.is_valid_object(map) {
      return Err(VmError::invalid_handle());
    }

    let slot_idx = self
      .validate(map.0)
      .ok_or_else(|| VmError::invalid_handle())?;
    let Some(HeapObject::Map(m)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };

    for (i, entry) in m.entries.iter_mut().enumerate() {
      tick::tick_every(i, tick::DEFAULT_TICK_EVERY, &mut tick)?;
      entry.key = None;
      entry.value = None;
    }
    m.size = 0;
    Ok(())
  }

  pub fn set_entries_len(&self, set: GcObject) -> Result<usize, VmError> {
    match self.get_heap_object(set.0)? {
      HeapObject::Set(s) => Ok(s.entries.len()),
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub fn set_entry_at(&self, set: GcObject, index: usize) -> Result<Option<Value>, VmError> {
    let HeapObject::Set(s) = self.get_heap_object(set.0)? else {
      return Err(VmError::invalid_handle());
    };
    let Some(entry) = s.entries.get(index) else {
      return Ok(None);
    };
    Ok(*entry)
  }

  pub fn set_size(&self, set: GcObject) -> Result<usize, VmError> {
    match self.get_heap_object(set.0)? {
      HeapObject::Set(s) => Ok(s.size),
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub fn set_has_with_tick(
    &self,
    set: GcObject,
    value: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    let value = Self::normalize_same_value_zero(value);
    let HeapObject::Set(s) = self.get_heap_object(set.0)? else {
      return Err(VmError::invalid_handle());
    };
    for (i, entry) in s.entries.iter().enumerate() {
      tick::tick_every(i, tick::DEFAULT_TICK_EVERY, &mut tick)?;
      let Some(v) = *entry else {
        continue;
      };
      if self.same_value_zero_with_tick(v, value, &mut tick)? {
        return Ok(true);
      }
    }
    Ok(false)
  }

  pub fn set_add_with_tick(
    &mut self,
    set: GcObject,
    value: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<(), VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(value));
    if !self.is_valid_object(set) {
      return Err(VmError::invalid_handle());
    }
    let value = Self::normalize_same_value_zero(value);

    let mut scope = self.scope();
    scope.push_roots(&[Value::Object(set), value])?;
    scope.heap.set_add_rooted_with_tick(set, value, &mut tick)
  }

  fn set_add_rooted_with_tick(
    &mut self,
    set: GcObject,
    value: Value,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<(), VmError> {
    let slot_idx = self
      .validate(set.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let (already_present, entry_len, entry_cap, property_count, old_bytes) = {
      let slot = &self.slots[slot_idx];
      let Some(HeapObject::Set(s)) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };

      let mut already_present = false;
      for (i, entry) in s.entries.iter().enumerate() {
        tick::tick_every(i, tick::DEFAULT_TICK_EVERY, tick)?;
        let Some(v) = *entry else {
          continue;
        };
        if self.same_value_zero_with_tick(v, value, tick)? {
          already_present = true;
          break;
        }
      }

      (
        already_present,
        s.entries.len(),
        s.entries.capacity(),
        s.base.properties.len(),
        slot.bytes,
      )
    };

    if already_present {
      return Ok(());
    }

    let required_len = entry_len.checked_add(1).ok_or(VmError::OutOfMemory)?;
    let desired_capacity = grown_capacity(entry_cap, required_len);
    if desired_capacity == usize::MAX {
      return Err(VmError::OutOfMemory);
    }

    let expected_new_bytes = JsSet::heap_size_bytes_for_counts(property_count, desired_capacity);
    let grow_by = expected_new_bytes.saturating_sub(old_bytes);
    if grow_by != 0 {
      self.ensure_can_allocate(grow_by)?;
      let Some(HeapObject::Set(s)) = self.slots[slot_idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      reserve_vec_to_len::<Option<Value>>(&mut s.entries, required_len)?;
    }

    let Some(HeapObject::Set(s)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    s.entries.push(Some(value));
    s.size = s.size.saturating_add(1);
    let new_bytes = s.heap_size_bytes();
    self.update_slot_bytes(slot_idx, new_bytes);

    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
    Ok(())
  }

  pub fn set_delete_with_tick(
    &mut self,
    set: GcObject,
    value: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(value));
    if !self.is_valid_object(set) {
      return Err(VmError::invalid_handle());
    }
    let value = Self::normalize_same_value_zero(value);

    let mut scope = self.scope();
    scope.push_roots(&[Value::Object(set), value])?;
    scope.heap.set_delete_rooted_with_tick(set, value, &mut tick)
  }

  fn set_delete_rooted_with_tick(
    &mut self,
    set: GcObject,
    value: Value,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    let slot_idx = self
      .validate(set.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let existing_idx = {
      let slot = &self.slots[slot_idx];
      let Some(HeapObject::Set(s)) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };

      let mut found: Option<usize> = None;
      for (i, entry) in s.entries.iter().enumerate() {
        tick::tick_every(i, tick::DEFAULT_TICK_EVERY, tick)?;
        let Some(v) = *entry else {
          continue;
        };
        if self.same_value_zero_with_tick(v, value, tick)? {
          found = Some(i);
          break;
        }
      }
      found
    };

    let Some(idx) = existing_idx else {
      return Ok(false);
    };

    let Some(HeapObject::Set(s)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    if let Some(entry) = s.entries.get_mut(idx) {
      if entry.is_some() {
        *entry = None;
        s.size = s.size.saturating_sub(1);
        return Ok(true);
      }
    }
    Ok(false)
  }

  pub fn set_clear_with_tick(
    &mut self,
    set: GcObject,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<(), VmError> {
    if !self.is_valid_object(set) {
      return Err(VmError::invalid_handle());
    }
    let slot_idx = self
      .validate(set.0)
      .ok_or_else(|| VmError::invalid_handle())?;
    let Some(HeapObject::Set(s)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    for (i, entry) in s.entries.iter_mut().enumerate() {
      tick::tick_every(i, tick::DEFAULT_TICK_EVERY, &mut tick)?;
      *entry = None;
    }
    s.size = 0;
    Ok(())
  }

  /// Implements the `CanBeHeldWeakly` abstract operation.
  ///
  /// This is used by WeakMap/WeakSet/WeakRef/FinalizationRegistry to accept Symbols as weak
  /// keys/values/targets while rejecting registered (global) symbols (see the
  /// `symbols-as-weakmap-keys` proposal).
  pub(crate) fn can_be_held_weakly(&self, value: Value) -> Result<bool, VmError> {
    match value {
      Value::Object(_) => Ok(true),
      Value::Symbol(sym) => Ok(self.symbol_key_for(sym)?.is_none()),
      _ => Ok(false),
    }
  }

  /// Returns the value associated with `key` in `map`, if present.
  ///
  /// Dead/stale keys are treated as absent.
  pub fn weak_map_get(&self, map: GcObject, key: Value) -> Result<Option<Value>, VmError> {
    self.weak_map_get_with_tick(map, key, || Ok(()))
  }

  pub fn weak_map_get_with_tick(
    &self,
    map: GcObject,
    key: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<Value>, VmError> {
    let key_weak = match WeakGcKey::from_value(key, self)? {
      Some(k) => k,
      None => return Ok(None),
    };

    let HeapObject::WeakMap(wm) = self.get_heap_object(map.0)? else {
      return Err(VmError::invalid_handle());
    };

    const TICK_EVERY: usize = 1024;
    for (i, entry) in wm.entries.iter().enumerate() {
      if i != 0 && i % TICK_EVERY == 0 {
        tick()?;
      }
      if entry.key != key_weak {
        continue;
      }
      // WeakMap operations must treat dead keys as absent. Entries are expected to be GC-pruned,
      // but this check prevents stale keys from being observed even if pruning falls behind.
      if entry.key.upgrade_value(self).is_none() {
        continue;
      }
      return Ok(Some(entry.value));
    }
    Ok(None)
  }

  /// Returns whether `key` is present in `map`.
  ///
  /// Dead/stale keys are treated as absent.
  pub fn weak_map_has(&self, map: GcObject, key: Value) -> Result<bool, VmError> {
    self.weak_map_has_with_tick(map, key, || Ok(()))
  }

  pub fn weak_map_has_with_tick(
    &self,
    map: GcObject,
    key: Value,
    tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    Ok(self.weak_map_get_with_tick(map, key, tick)?.is_some())
  }

  /// Sets `map[key] = value`.
  ///
  /// Dead/stale keys are ignored.
  pub fn weak_map_set(&mut self, map: GcObject, key: Value, value: Value) -> Result<(), VmError> {
    self.weak_map_set_with_tick(map, key, value, || Ok(()))
  }

  pub fn weak_map_set_with_tick(
    &mut self,
    map: GcObject,
    key: Value,
    value: Value,
    tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<(), VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(value));

    if !self.is_valid_object(map) {
      return Err(VmError::invalid_handle());
    }
    if WeakGcKey::from_value(key, self)?.is_none() {
      return Ok(());
    }

    // Root inputs across any potential GC while growing the entry vector.
    let mut scope = self.scope();
    scope.push_roots(&[Value::Object(map), key, value])?;
    scope.heap.weak_map_set_rooted_with_tick(map, key, value, tick)
  }

  fn weak_map_set_rooted_with_tick(
    &mut self,
    map: GcObject,
    key: Value,
    value: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<(), VmError> {
    let slot_idx = self
      .validate(map.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let Some(key_weak) = WeakGcKey::from_value(key, self)? else {
      return Ok(());
    };

    let (existing_idx, entry_len, entry_cap, property_count, old_bytes) = {
      let slot = &self.slots[slot_idx];
      let Some(HeapObject::WeakMap(wm)) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };

      const TICK_EVERY: usize = 1024;
      let mut existing_idx = None;
      for (i, entry) in wm.entries.iter().enumerate() {
        if i != 0 && i % TICK_EVERY == 0 {
          tick()?;
        }
        if entry.key == key_weak && entry.key.upgrade_value(self).is_some() {
          existing_idx = Some(i);
          break;
        }
      }

      (
        existing_idx,
        wm.entries.len(),
        wm.entries.capacity(),
        wm.base.properties.len(),
        slot.bytes,
      )
    };

    if let Some(existing_idx) = existing_idx {
      let Some(HeapObject::WeakMap(wm)) = self.slots[slot_idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      if let Some(entry) = wm.entries.get_mut(existing_idx) {
        entry.value = value;
      }
      return Ok(());
    }

    let required_len = entry_len.checked_add(1).ok_or(VmError::OutOfMemory)?;
    let desired_capacity = grown_capacity(entry_cap, required_len);
    if desired_capacity == usize::MAX {
      return Err(VmError::OutOfMemory);
    }

    let expected_new_bytes = JsWeakMap::heap_size_bytes_for_counts(property_count, desired_capacity);
    let grow_by = expected_new_bytes.saturating_sub(old_bytes);
    if grow_by != 0 {
      self.ensure_can_allocate(grow_by)?;
      let Some(HeapObject::WeakMap(wm)) = self.slots[slot_idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      reserve_vec_to_len::<WeakMapEntry>(&mut wm.entries, required_len)?;
    }

    let Some(HeapObject::WeakMap(wm)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    wm.entries.push(WeakMapEntry { key: key_weak, value });
    let new_bytes = wm.heap_size_bytes();
    self.update_slot_bytes(slot_idx, new_bytes);

    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
    Ok(())
  }

  /// Removes `key` from `map`, returning whether it was present.
  ///
  /// Dead/stale keys are treated as absent.
  pub fn weak_map_delete(&mut self, map: GcObject, key: Value) -> Result<bool, VmError> {
    self.weak_map_delete_with_tick(map, key, || Ok(()))
  }

  pub fn weak_map_delete_with_tick(
    &mut self,
    map: GcObject,
    key: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    let Some(key_weak) = WeakGcKey::from_value(key, self)? else {
      return Ok(false);
    };

    let slot_idx = self
      .validate(map.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let mut removed = false;

    // No allocation: compact the entry vector in-place.
    let mut entries = {
      let Some(HeapObject::WeakMap(wm)) = self.slots[slot_idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      mem::take(&mut wm.entries)
    };

    const TICK_EVERY: usize = 1024;
    let mut out_len = 0usize;
    let len = entries.len();
    for i in 0..len {
      if i != 0 && i % TICK_EVERY == 0 {
        tick()?;
      }
      let entry = entries[i];

      let Some(live_key) = entry.key.upgrade_value(&*self) else {
        // Drop dead keys.
        continue;
      };
      if entry.key == key_weak && live_key == key {
        removed = true;
        continue;
      }

      entries[out_len] = entry;
      out_len += 1;
    }
    entries.truncate(out_len);

    let Some(HeapObject::WeakMap(wm)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    wm.entries = entries;

    Ok(removed)
  }

  /// Returns the number of entries currently stored in `map`.
  ///
  /// Note: this counts *weak* entries and is intended for engine tests/introspection.
  pub fn weak_map_entry_count(&self, map: GcObject) -> Result<usize, VmError> {
    match self.get_heap_object(map.0)? {
      HeapObject::WeakMap(wm) => Ok(wm.entries.len()),
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Returns `true` if `key` is present in `set`.
  ///
  /// Dead/stale keys are treated as absent.
  pub fn weak_set_has(&self, set: GcObject, key: Value) -> Result<bool, VmError> {
    self.weak_set_has_with_tick(set, key, || Ok(()))
  }

  pub fn weak_set_has_with_tick(
    &self,
    set: GcObject,
    key: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    let Some(key_weak) = WeakGcKey::from_value(key, self)? else {
      return Ok(false);
    };

    let HeapObject::WeakSet(ws) = self.get_heap_object(set.0)? else {
      return Err(VmError::invalid_handle());
    };

    const TICK_EVERY: usize = 1024;
    for (i, entry) in ws.entries.iter().enumerate() {
      if i != 0 && i % TICK_EVERY == 0 {
        tick()?;
      }
      let Some(live_key) = entry.upgrade_value(self) else {
        continue;
      };
      if live_key == key && *entry == key_weak {
        return Ok(true);
      }
    }
    Ok(false)
  }

  /// Inserts `key` into `set`.
  ///
  /// Dead/stale keys are ignored.
  pub fn weak_set_add(&mut self, set: GcObject, key: Value) -> Result<(), VmError> {
    self.weak_set_add_with_tick(set, key, || Ok(()))
  }

  pub fn weak_set_add_with_tick(
    &mut self,
    set: GcObject,
    key: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<(), VmError> {
    if !self.is_valid_object(set) {
      return Err(VmError::invalid_handle());
    }
    if WeakGcKey::from_value(key, self)?.is_none() {
      return Ok(());
    }

    // Root inputs across any potential GC while growing the entry vector.
    let mut scope = self.scope();
    scope.push_roots(&[Value::Object(set), key])?;
    scope.heap.weak_set_add_rooted_with_tick(set, key, &mut tick)
  }

  fn weak_set_add_rooted_with_tick(
    &mut self,
    set: GcObject,
    key: Value,
    tick: &mut impl FnMut() -> Result<(), VmError>,
  ) -> Result<(), VmError> {
    let slot_idx = self
      .validate(set.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let Some(key_weak) = WeakGcKey::from_value(key, self)? else {
      return Ok(());
    };

    let (entry_len, entry_cap, property_count, old_bytes) = {
      let slot = &self.slots[slot_idx];
      let Some(HeapObject::WeakSet(ws)) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };

      // WeakSet operations must treat dead keys as absent. Entries are expected to be GC-pruned,
      // but this check prevents stale keys from being observed even if pruning falls behind.
      const TICK_EVERY: usize = 1024;
      for (i, entry) in ws.entries.iter().enumerate() {
        if i != 0 && i % TICK_EVERY == 0 {
          tick()?;
        }
        if *entry == key_weak && entry.upgrade_value(self).is_some() {
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
      reserve_vec_to_len::<WeakGcKey>(&mut ws.entries, required_len)?;
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
  pub fn weak_set_delete(&mut self, set: GcObject, key: Value) -> Result<bool, VmError> {
    self.weak_set_delete_with_tick(set, key, || Ok(()))
  }

  pub fn weak_set_delete_with_tick(
    &mut self,
    set: GcObject,
    key: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    let Some(key_weak) = WeakGcKey::from_value(key, self)? else {
      return Ok(false);
    };

    let slot_idx = self
      .validate(set.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let mut removed = false;

    // No allocation: compact the entry vector in-place.
    let mut entries = {
      let Some(HeapObject::WeakSet(ws)) = self.slots[slot_idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      mem::take(&mut ws.entries)
    };

    const TICK_EVERY: usize = 1024;
    let mut out_len = 0usize;
    let len = entries.len();
    for i in 0..len {
      if i != 0 && i % TICK_EVERY == 0 {
        tick()?;
      }
      let entry = entries[i];

      let Some(live_key) = entry.upgrade_value(&*self) else {
        // Drop dead keys.
        continue;
      };
      if entry == key_weak && live_key == key {
        removed = true;
        continue;
      }

      entries[out_len] = entry;
      out_len += 1;
    }
    entries.truncate(out_len);

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

  /// Implements `WeakRef.prototype.deref`.
  ///
  /// Returns the target value (Object or Symbol) if it is still alive, otherwise `None`.
  pub fn weak_ref_deref(&self, weak_ref: GcObject) -> Result<Option<Value>, VmError> {
    match self.get_heap_object(weak_ref.0)? {
      HeapObject::WeakRef(wr) => Ok(wr.target.upgrade_value(self)),
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Returns the `[[CleanupCallback]]` internal slot for a `FinalizationRegistry`.
  pub(crate) fn finalization_registry_cleanup_callback(&self, registry: GcObject) -> Result<Value, VmError> {
    match self.get_heap_object(registry.0)? {
      HeapObject::FinalizationRegistry(fr) => Ok(fr.cleanup_callback),
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Adds a new registration to `FinalizationRegistry`.
  pub fn finalization_registry_register(
    &mut self,
    registry: GcObject,
    target: Value,
    held_value: Value,
    unregister_token: Option<Value>,
  ) -> Result<(), VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(held_value));
    if !self.is_valid_object(registry) {
      return Err(VmError::invalid_handle());
    }
    if WeakGcKey::from_value(target, self)?.is_none() {
      return Err(VmError::invalid_handle());
    }
    if let Some(token) = unregister_token {
      if WeakGcKey::from_value(token, self)?.is_none() {
        return Err(VmError::invalid_handle());
      }
    }

    // Root inputs across any potential GC while growing the cell vector.
    let mut scope = self.scope();
    let mut roots = [Value::Undefined; 4];
    let mut root_count = 0usize;
    roots[root_count] = Value::Object(registry);
    root_count += 1;
    roots[root_count] = target;
    root_count += 1;
    roots[root_count] = held_value;
    root_count += 1;
    if let Some(token) = unregister_token {
      roots[root_count] = token;
      root_count += 1;
    }
    scope.push_roots(&roots[..root_count])?;

    scope
      .heap
      .finalization_registry_register_rooted(registry, target, held_value, unregister_token)
  }

  fn finalization_registry_register_rooted(
    &mut self,
    registry: GcObject,
    target: Value,
    held_value: Value,
    unregister_token: Option<Value>,
  ) -> Result<(), VmError> {
    let slot_idx = self
      .validate(registry.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let (cell_len, cell_cap, property_count, old_bytes) = {
      let slot = &self.slots[slot_idx];
      let Some(HeapObject::FinalizationRegistry(fr)) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };
      (
        fr.cells.len(),
        fr.cells.capacity(),
        fr.base.properties.len(),
        slot.bytes,
      )
    };

    let required_len = cell_len.checked_add(1).ok_or(VmError::OutOfMemory)?;
    let desired_capacity = grown_capacity(cell_cap, required_len);
    if desired_capacity == usize::MAX {
      return Err(VmError::OutOfMemory);
    }

    let expected_new_bytes =
      JsFinalizationRegistry::heap_size_bytes_for_counts(property_count, desired_capacity);
    let grow_by = expected_new_bytes.saturating_sub(old_bytes);
    if grow_by != 0 {
      self.ensure_can_allocate(grow_by)?;
      let Some(HeapObject::FinalizationRegistry(fr)) = self.slots[slot_idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      reserve_vec_to_len::<FinalizationRegistryCell>(&mut fr.cells, required_len)?;
    }

    let Some(target_weak) = WeakGcKey::from_value(target, self)? else {
      return Err(VmError::invalid_handle());
    };
    let unregister_token_weak = match unregister_token {
      Some(token) => Some(
        WeakGcKey::from_value(token, self)?.ok_or_else(|| VmError::invalid_handle())?,
      ),
      None => None,
    };

    let Some(HeapObject::FinalizationRegistry(fr)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };
    fr.cells.push(FinalizationRegistryCell {
      target: Some(target_weak),
      held_value,
      unregister_token: unregister_token_weak,
    });

    if grow_by != 0 {
      let new_bytes = fr.heap_size_bytes();
      self.update_slot_bytes(slot_idx, new_bytes);
      #[cfg(debug_assertions)]
      self.debug_assert_used_bytes_is_correct();
    }

    Ok(())
  }

  /// Implements `FinalizationRegistry.prototype.unregister`.
  pub fn finalization_registry_unregister_with_tick(
    &mut self,
    registry: GcObject,
    unregister_token: Value,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<bool, VmError> {
    if !self.is_valid_object(registry) {
      return Err(VmError::invalid_handle());
    }
    let Some(needle) = WeakGcKey::from_value(unregister_token, self)? else {
      return Err(VmError::invalid_handle());
    };

    let slot_idx = self
      .validate(registry.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let Some(HeapObject::FinalizationRegistry(fr)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };

    const TICK_EVERY: usize = 1024;
    let mut removed = false;
    let mut pending = false;
    let mut steps = 0usize;
    let mut i = 0usize;
    while i < fr.cells.len() {
      if steps % TICK_EVERY == 0 {
        tick()?;
      }
      steps = steps.checked_add(1).ok_or(VmError::OutOfMemory)?;

      let cell = fr.cells[i];
      if cell.unregister_token == Some(needle) {
        removed = true;
        fr.cells.swap_remove(i);
        continue;
      }

      if cell.target.is_none() {
        pending = true;
      }

      i = i.checked_add(1).ok_or(VmError::OutOfMemory)?;
    }

    fr.cleanup_pending = pending;
    if pending {
      self.finalization_registry_cleanup_jobs_pending = true;
    }

    Ok(removed)
  }

  /// Removes and returns one held value for a cleared (collected) target in `registry`.
  ///
  /// This is used by `%FinalizationRegistry.prototype.cleanupSome%` and by the GC-scheduled cleanup
  /// job.
  pub(crate) fn finalization_registry_pop_pending_cleanup_value_with_tick(
    &mut self,
    registry: GcObject,
    mut tick: impl FnMut() -> Result<(), VmError>,
  ) -> Result<Option<Value>, VmError> {
    if !self.is_valid_object(registry) {
      return Err(VmError::invalid_handle());
    }

    let slot_idx = self
      .validate(registry.0)
      .ok_or_else(|| VmError::invalid_handle())?;
    let Some(HeapObject::FinalizationRegistry(fr)) = self.slots[slot_idx].value.as_mut() else {
      return Err(VmError::invalid_handle());
    };

    const TICK_EVERY: usize = 1024;
    let mut steps = 0usize;

    let mut i = 0usize;
    while i < fr.cells.len() {
      if steps % TICK_EVERY == 0 {
        tick()?;
      }
      steps = steps.checked_add(1).ok_or(VmError::OutOfMemory)?;

      let cell = fr.cells[i];
      if cell.target.is_none() {
        let held_value = cell.held_value;
        fr.cells.swap_remove(i);

        // Recompute whether there are any other pending cells. We have already verified that cells
        // before `i` were live (their targets were not cleared), so scan from `i` onward (including
        // the element swapped into `i`).
        let mut pending = false;
        for (j, cell) in fr.cells.iter().enumerate().skip(i) {
          if j % TICK_EVERY == 0 {
            tick()?;
          }
          if cell.target.is_none() {
            pending = true;
            break;
          }
        }
        fr.cleanup_pending = pending;
        if pending {
          self.finalization_registry_cleanup_jobs_pending = true;
        }
        return Ok(Some(held_value));
      }

      i = i.checked_add(1).ok_or(VmError::OutOfMemory)?;
    }

    // No pending cells.
    fr.cleanup_pending = false;
    Ok(None)
  }

  fn get_regexp(&self, obj: GcObject) -> Result<&JsRegExp, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::RegExp(r) => Ok(r),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn require_regexp(&self, obj: GcObject) -> Result<&JsRegExp, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::RegExp(r) => Ok(r),
      _ => Err(VmError::TypeError(
        "Heap RegExp operation called on incompatible receiver",
      )),
    }
  }

  /// Returns the `[[OriginalSource]]` internal slot for a RegExp object.
  ///
  /// This reads the internal slot directly (it does not consult JS-visible properties) and
  /// therefore remains reliable even if user code mutates the RegExp's prototype chain.
  ///
  /// # Errors
  ///
  /// - Returns [`VmError::InvalidHandle`] for stale handles.
  /// - Returns a catchable [`VmError::TypeError`] if `obj` is not a RegExp object.
  pub fn regexp_original_source(&self, obj: GcObject) -> Result<GcString, VmError> {
    Ok(self.require_regexp(obj)?.original_source)
  }

  /// Returns the `[[OriginalFlags]]` internal slot for a RegExp object.
  ///
  /// This reads the internal slot directly (it does not consult JS-visible properties) and
  /// therefore remains reliable even if user code mutates the RegExp's prototype chain.
  ///
  /// # Errors
  ///
  /// - Returns [`VmError::InvalidHandle`] for stale handles.
  /// - Returns a catchable [`VmError::TypeError`] if `obj` is not a RegExp object.
  pub fn regexp_original_flags(&self, obj: GcObject) -> Result<GcString, VmError> {
    Ok(self.require_regexp(obj)?.original_flags)
  }

  pub fn regexp_flags(&self, obj: GcObject) -> Result<RegExpFlags, VmError> {
    Ok(self.require_regexp(obj)?.flags)
  }

  pub fn regexp_program(&self, obj: GcObject) -> Result<&RegExpProgram, VmError> {
    Ok(&self.require_regexp(obj)?.program)
  }
  #[track_caller]
  fn get_env(&self, env: GcEnv) -> Result<&EnvRecord, VmError> {
    let invalid = VmError::InvalidHandle {
      location: std::panic::Location::caller(),
    };
    match self.get_heap_object(env.0) {
      Ok(HeapObject::Env(e)) => Ok(e),
      Ok(_) => Err(invalid),
      Err(VmError::InvalidHandle { .. }) => Err(invalid),
      Err(other) => Err(other),
    }
  }

  #[track_caller]
  fn get_env_mut(&mut self, env: GcEnv) -> Result<&mut EnvRecord, VmError> {
    let invalid = VmError::InvalidHandle {
      location: std::panic::Location::caller(),
    };
    match self.get_heap_object_mut(env.0) {
      Ok(HeapObject::Env(e)) => Ok(e),
      Ok(_) => Err(invalid),
      Err(VmError::InvalidHandle { .. }) => Err(invalid),
      Err(other) => Err(other),
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

  #[track_caller]
  fn get_declarative_env(&self, env: GcEnv) -> Result<&DeclarativeEnvRecord, VmError> {
    let invalid = VmError::InvalidHandle {
      location: std::panic::Location::caller(),
    };
    match self.get_env(env) {
      Ok(EnvRecord::Declarative(env)) => Ok(env),
      Ok(EnvRecord::Object(_)) => Err(VmError::Unimplemented("object environment record")),
      Err(VmError::InvalidHandle { .. }) => Err(invalid),
      Err(other) => Err(other),
    }
  }

  #[track_caller]
  fn get_declarative_env_mut(&mut self, env: GcEnv) -> Result<&mut DeclarativeEnvRecord, VmError> {
    let invalid = VmError::InvalidHandle {
      location: std::panic::Location::caller(),
    };
    match self.get_env_mut(env) {
      Ok(EnvRecord::Declarative(env)) => Ok(env),
      Ok(EnvRecord::Object(_)) => Err(VmError::Unimplemented("object environment record")),
      Err(VmError::InvalidHandle { .. }) => Err(invalid),
      Err(other) => Err(other),
    }
  }
  /// Gets an object's `[[Prototype]]`.
  pub fn object_prototype(&self, obj: GcObject) -> Result<Option<GcObject>, VmError> {
    if self.is_proxy_object(obj) {
      let mut current = obj;
      let mut steps = 0usize;
      let mut visited: HashSet<GcObject> = HashSet::new();
      while self.is_proxy_object(current) {
        if steps >= MAX_PROTOTYPE_CHAIN {
          return Err(VmError::PrototypeChainTooDeep);
        }
        steps += 1;
        if visited.try_reserve(1).is_err() {
          return Err(VmError::OutOfMemory);
        }
        if !visited.insert(current) {
          return Err(VmError::PrototypeCycle);
        }
        let Some(target) = self.proxy_target(current)? else {
          return Err(VmError::TypeError(
            "Cannot perform 'getPrototypeOf' on a revoked Proxy",
          ));
        };
        current = target;
      }

      return Ok(self.get_object_base(current)?.prototype);
    }

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

      if visited.try_reserve(1).is_err() {
        return Err(VmError::OutOfMemory);
      }
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
    if let HeapObject::TypedArray(view) = self.get_heap_object(obj.0)? {
      if let PropertyKey::String(s) = key {
        if let Some(numeric_index) = self.canonical_numeric_index_string(*s)? {
          // Integer-indexed properties are only present when the view is in-bounds (and the backing
          // buffer is not detached).
          //
          // Spec: `IsValidIntegerIndex` / `TypedArrayGetElement`.
          if self.typed_array_view_is_out_of_bounds(view)? {
            return Ok(None);
          }
          if !numeric_index.is_finite() || numeric_index.fract() != 0.0 {
            return Ok(None);
          }
          if numeric_index == 0.0 && numeric_index.is_sign_negative() {
            // -0 is a canonical numeric index string but never a valid integer index.
            return Ok(None);
          }
          if numeric_index < 0.0 {
            return Ok(None);
          }
          if numeric_index > usize::MAX as f64 {
            return Ok(None);
          }

          let idx = numeric_index as usize;
          if idx < view.length {
            return Ok(Some(PropertyDescriptor {
              enumerable: true,
              // TypedArray integer-indexed properties are reported as configurable per ECMA-262
              // `TypedArray.[[GetOwnProperty]]`.
              //
              // Note: deletion is still rejected by TypedArray `[[Delete]]`, so these properties are
              // "non-deletable" even though they are reported as configurable.
              configurable: true,
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

    // Module Namespace Exotic Object `[[GetOwnProperty]]` (ECMA-262 §9.4.6).
    //
    // Exported bindings are *virtual* properties backed by the namespace's `[[Exports]]` list and
    // are not stored in the object's ordinary property table.
    if self.object_is_module_namespace(obj)? {
      if let PropertyKey::String(s) = key {
        if let Some(export) = self.module_namespace_export(obj, *s)? {
          return Ok(Some(PropertyDescriptor {
            enumerable: true,
            configurable: false,
            kind: PropertyKind::Accessor {
              get: Value::Object(export.getter),
              set: Value::Undefined,
            },
          }));
        }
        return Ok(None);
      }
    }

    let obj = self.get_object_base(obj)?;

    // Array exotic objects store array-indexed properties in a separate dense table.
    if let ObjectKind::Array(arr) = &obj.kind {
      if let PropertyKey::String(s) = key {
        if let Some(index) = self.string_to_array_index(*s) {
          if let Some(desc) = arr.elements.get(index as usize).copied().flatten() {
            return Ok(Some(desc));
          }
        }
      }
    }

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

    // Fast path for array-indexed properties stored in the array's dense elements table.
    if let PropertyKey::String(s) = key {
      if let Some(index) = self.string_to_array_index(*s) {
        if index <= MAX_FAST_ARRAY_INDEX {
          if let Some(HeapObject::Object(o)) = self.slots[slot_idx].value.as_mut() {
            if let ObjectKind::Array(arr) = &mut o.base.kind {
              if let Some(slot) = arr.elements.get_mut(index as usize) {
                if slot.is_some() {
                  *slot = None;
                  return Ok(true);
                }
              }
            }
          }
        }
      }
    }

    // Two-phase borrow to avoid holding `&mut HeapObject` while calling back into `&self` for
    // string comparisons in `property_key_eq`.
    #[derive(Clone, Copy)]
    enum TargetKind {
      OrdinaryObject,
      RegExp { program_bytes: usize },
      ArrayBuffer,
      TypedArray,
      DataView,
      Map {
        entry_capacity: usize,
      },
      Set {
        entry_capacity: usize,
      },
      WeakMap {
        entry_capacity: usize,
      },
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
      WeakRef,
      FinalizationRegistry {
        cell_capacity: usize,
      },
      Generator {
        continuation_bytes: usize,
      },
      AsyncGenerator {
        continuation_bytes: usize,
        queue_bytes: usize,
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
        HeapObject::RegExp(obj) => (
          obj
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::RegExp {
            program_bytes: obj.program.heap_size_bytes(),
          },
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
        HeapObject::Map(m) => (
          m.base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::Map {
            entry_capacity: m.entries.capacity(),
          },
          m.base.properties.len(),
        ),
        HeapObject::Set(s) => (
          s.base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::Set {
            entry_capacity: s.entries.capacity(),
          },
          s.base.properties.len(),
        ),
        HeapObject::WeakMap(wm) => (
          wm
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::WeakMap {
            entry_capacity: wm.entries.capacity(),
          },
          wm.base.properties.len(),
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
        HeapObject::WeakRef(wr) => (
          wr
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::WeakRef,
          wr.base.properties.len(),
        ),
        HeapObject::FinalizationRegistry(fr) => (
          fr
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::FinalizationRegistry {
            cell_capacity: fr.cells.capacity(),
          },
          fr.base.properties.len(),
        ),
        HeapObject::Generator(g) => (
          g.object
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::Generator {
            continuation_bytes: g
              .continuation
              .as_ref()
              .map(|c| c.heap_size_bytes())
              .unwrap_or(0),
          },
          g.object.base.properties.len(),
        ),
        HeapObject::AsyncGenerator(g) => (
          g.object
            .base
            .properties
            .iter()
            .position(|prop| self.property_key_eq(&prop.key, key)),
          TargetKind::AsyncGenerator {
            continuation_bytes: g
              .continuation
              .as_ref()
              .map(|c| c.heap_size_bytes())
              .unwrap_or(0),
            queue_bytes: g
              .request_queue
              .capacity()
              .checked_mul(mem::size_of::<AsyncGeneratorRequest>())
              .unwrap_or(usize::MAX),
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
      TargetKind::RegExp { program_bytes } => ObjectBase::properties_heap_size_bytes_for_count(new_property_count)
        .saturating_add(program_bytes),
      TargetKind::ArrayBuffer => JsArrayBuffer::heap_size_bytes_for_property_count(new_property_count),
      TargetKind::TypedArray => JsTypedArray::heap_size_bytes_for_property_count(new_property_count),
      TargetKind::DataView => JsDataView::heap_size_bytes_for_property_count(new_property_count),
      TargetKind::Map { entry_capacity } => JsMap::heap_size_bytes_for_counts(new_property_count, entry_capacity),
      TargetKind::Set { entry_capacity } => JsSet::heap_size_bytes_for_counts(new_property_count, entry_capacity),
      TargetKind::WeakMap { entry_capacity } => {
        JsWeakMap::heap_size_bytes_for_counts(new_property_count, entry_capacity)
      }
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
      TargetKind::WeakRef => JsWeakRef::heap_size_bytes_for_property_count(new_property_count),
      TargetKind::FinalizationRegistry { cell_capacity } => {
        JsFinalizationRegistry::heap_size_bytes_for_counts(new_property_count, cell_capacity)
      }
      TargetKind::Generator {
        continuation_bytes,
      } => ObjectBase::properties_heap_size_bytes_for_count(new_property_count)
        .saturating_add(continuation_bytes),
      TargetKind::AsyncGenerator {
        continuation_bytes,
        queue_bytes,
      } => ObjectBase::properties_heap_size_bytes_for_count(new_property_count)
        .saturating_add(continuation_bytes)
        .saturating_add(queue_bytes),
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
        Some(HeapObject::RegExp(obj)) => {
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
        Some(HeapObject::Map(m)) => {
          buf.extend_from_slice(&m.base.properties[..idx]);
          buf.extend_from_slice(&m.base.properties[idx + 1..]);
        }
        Some(HeapObject::Set(s)) => {
          buf.extend_from_slice(&s.base.properties[..idx]);
          buf.extend_from_slice(&s.base.properties[idx + 1..]);
        }
        Some(HeapObject::WeakMap(wm)) => {
          buf.extend_from_slice(&wm.base.properties[..idx]);
          buf.extend_from_slice(&wm.base.properties[idx + 1..]);
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
        Some(HeapObject::WeakRef(wr)) => {
          buf.extend_from_slice(&wr.base.properties[..idx]);
          buf.extend_from_slice(&wr.base.properties[idx + 1..]);
        }
        Some(HeapObject::FinalizationRegistry(fr)) => {
          buf.extend_from_slice(&fr.base.properties[..idx]);
          buf.extend_from_slice(&fr.base.properties[idx + 1..]);
        }
        Some(HeapObject::Generator(g)) => {
          buf.extend_from_slice(&g.object.base.properties[..idx]);
          buf.extend_from_slice(&g.object.base.properties[idx + 1..]);
        }
        Some(HeapObject::AsyncGenerator(g)) => {
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
      HeapObject::RegExp(obj) => obj.base.properties = properties,
      HeapObject::ArrayBuffer(obj) => obj.base.properties = properties,
      HeapObject::TypedArray(obj) => obj.base.properties = properties,
      HeapObject::DataView(obj) => obj.base.properties = properties,
      HeapObject::Map(m) => m.base.properties = properties,
      HeapObject::Set(s) => s.base.properties = properties,
      HeapObject::WeakMap(wm) => wm.base.properties = properties,
      HeapObject::Function(func) => func.base.properties = properties,
      HeapObject::Promise(p) => p.object.base.properties = properties,
      HeapObject::WeakSet(ws) => ws.base.properties = properties,
      HeapObject::WeakRef(wr) => wr.base.properties = properties,
      HeapObject::FinalizationRegistry(fr) => fr.base.properties = properties,
      HeapObject::Generator(g) => g.object.base.properties = properties,
      HeapObject::AsyncGenerator(g) => g.object.base.properties = properties,
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

  pub(crate) fn property_key_is_caller(&self, key: &PropertyKey) -> bool {
    const CALLER_UNITS: [u16; 6] = [99, 97, 108, 108, 101, 114]; // "caller"
    let PropertyKey::String(s) = key else {
      return false;
    };
    let Ok(js) = self.get_string(*s) else {
      return false;
    };
    js.as_code_units() == CALLER_UNITS
  }

  pub(crate) fn property_key_is_arguments(&self, key: &PropertyKey) -> bool {
    const ARGUMENTS_UNITS: [u16; 9] = [97, 114, 103, 117, 109, 101, 110, 116, 115]; // "arguments"
    let PropertyKey::String(s) = key else {
      return false;
    };
    let Ok(js) = self.get_string(*s) else {
      return false;
    };
    js.as_code_units() == ARGUMENTS_UNITS
  }

  /// Returns whether `obj` is an Array exotic object.
  ///
  /// This is the engine-side equivalent of `Array.isArray` (without consulting prototypes).
  pub fn object_is_array(&self, obj: GcObject) -> Result<bool, VmError> {
    Ok(self.get_object_base(obj)?.array_length().is_some())
  }

  /// Returns whether `obj` is a Module Namespace Exotic Object (ECMA-262 §9.4.6).
  pub(crate) fn object_is_module_namespace(&self, obj: GcObject) -> Result<bool, VmError> {
    Ok(matches!(self.get_object_base(obj)?.kind, ObjectKind::ModuleNamespace(_)))
  }

  fn get_module_namespace_exports_data(
    &self,
    exports: GcModuleNamespaceExports,
  ) -> Result<&ModuleNamespaceExportsData, VmError> {
    match self.get_heap_object(exports.0)? {
      HeapObject::ModuleNamespaceExports(data) => Ok(data),
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn module_namespace_export(&self, obj: GcObject, key: GcString) -> Result<Option<ModuleNamespaceExport>, VmError> {
    let base = self.get_object_base(obj)?;
    let ObjectKind::ModuleNamespace(ns) = &base.kind else {
      return Err(VmError::InvariantViolation("expected module namespace object"));
    };

    let key_units = self.get_string(key)?.as_code_units();
    let exports = self.get_module_namespace_exports_data(ns.exports)?;
    for export in exports.exports.iter() {
      let export_units = self.get_string(export.name)?.as_code_units();
      if export_units == key_units {
        return Ok(Some(*export));
      }
    }
    Ok(None)
  }

  pub(crate) fn module_namespace_exports(&self, obj: GcObject) -> Result<&[ModuleNamespaceExport], VmError> {
    let base = self.get_object_base(obj)?;
    let ObjectKind::ModuleNamespace(ns) = &base.kind else {
      return Err(VmError::InvariantViolation("expected module namespace object"));
    };
    Ok(&self.get_module_namespace_exports_data(ns.exports)?.exports)
  }

  pub(crate) fn array_length(&self, obj: GcObject) -> Result<u32, VmError> {
    self
      .get_object_base(obj)?
      .array_length()
      .ok_or(VmError::InvariantViolation("expected array object"))
  }

  /// Fast path: gets the value of an own *data* array index property from the array's element
  /// storage (without allocating an index key string).
  ///
  /// Returns `None` when:
  /// - the index is out of bounds for the fast element table,
  /// - the element property is missing, or
  /// - the element is an accessor property.
  pub(crate) fn array_fast_own_data_element_value(
    &self,
    obj: GcObject,
    index: u32,
  ) -> Result<Option<Value>, VmError> {
    let base = self.get_object_base(obj)?;
    let ObjectKind::Array(arr) = &base.kind else {
      return Err(VmError::InvariantViolation("expected array object"));
    };
    let Some(desc) = arr.elements.get(index as usize).copied().flatten() else {
      return Ok(None);
    };
    match desc.kind {
      PropertyKind::Data { value, .. } => Ok(Some(value)),
      PropertyKind::Accessor { .. } => Ok(None),
    }
  }

  pub(crate) fn array_fast_own_element_descriptor(
    &self,
    obj: GcObject,
    index: u32,
  ) -> Result<Option<PropertyDescriptor>, VmError> {
    let base = self.get_object_base(obj)?;
    let ObjectKind::Array(arr) = &base.kind else {
      return Err(VmError::InvariantViolation("expected array object"));
    };
    Ok(arr.elements.get(index as usize).copied().flatten())
  }

  pub(crate) fn array_fast_elements_len(&self, obj: GcObject) -> Result<usize, VmError> {
    let base = self.get_object_base(obj)?;
    let ObjectKind::Array(arr) = &base.kind else {
      return Err(VmError::InvariantViolation("expected array object"));
    };
    Ok(arr.elements.len())
  }

  pub(crate) fn array_fast_elements_mut(
    &mut self,
    obj: GcObject,
  ) -> Result<&mut Vec<Option<PropertyDescriptor>>, VmError> {
    let base = self.get_object_base_mut(obj)?;
    let ObjectKind::Array(arr) = &mut base.kind else {
      return Err(VmError::InvariantViolation("expected array object"));
    };
    Ok(&mut arr.elements)
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
    // Proxy objects do not have an ordinary property table (`ObjectBase`) in the heap; the only
    // correct way to query their properties is via `Scope` + `Vm` so Proxy traps can run.
    //
    // This helper is used by many built-ins for *internal slot* style checks (symbol-keyed marker
    // properties that are unreachable from user code). Per spec, Proxy objects never have these
    // internal slots, even when their target would.
    //
    // Treat Proxies as "missing property" instead of surfacing `InvalidHandle` so hostile JS like
    // `new Proxy([].values(), {}).next()` throws a TypeError rather than tripping an engine bug.
    if self.is_proxy_object(obj) {
      return Ok(None);
    }
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

    // Fast path for array-indexed properties stored in the array's dense elements table.
    if let Some(index) = key_array_index {
      if index <= MAX_FAST_ARRAY_INDEX {
        let base = self.get_object_base_mut(obj)?;
        if let ObjectKind::Array(arr) = &mut base.kind {
          if let Some(slot) = arr.elements.get_mut(index as usize) {
            if let Some(desc) = slot.as_mut() {
              // Array exotic `length` handling.
              if key_is_length {
                // `length` is not a numeric index, but keep the check robust.
                let Value::Number(n) = value else {
                  return Err(VmError::TypeError("Invalid array length"));
                };
                let new_len =
                  array_length_from_f64(n).ok_or(VmError::TypeError("Invalid array length"))?;
                base.set_array_length(new_len);
                return Ok(());
              }

              match &mut desc.kind {
                PropertyKind::Data { value: slot, .. } => {
                  *slot = value;
                }
                PropertyKind::Accessor { .. } => return Err(VmError::PropertyNotData),
              }

              // Array exotic index semantics: writing an array index extends `length`.
              let new_len = index.wrapping_add(1);
              if new_len > arr.length {
                base.set_array_length(new_len);
              }
              return Ok(());
            }
          }
        }
      }
    }

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

  /// ECMA-262 `CanonicalNumericIndexString`.
  ///
  /// Returns `Some(numericIndex)` when `s` is a *canonical numeric string* (including the special
  /// `"-0"` case), and `None` otherwise.
  pub(crate) fn canonical_numeric_index_string(&self, s: GcString) -> Result<Option<f64>, VmError> {
    let units = self.get_string(s)?.as_code_units();
    if units.len() == 2 && units[0] == b'-' as u16 && units[1] == b'0' as u16 {
      return Ok(Some(-0.0));
    }

    let n = crate::ops::string_to_number(self, s)?;
    let mut buf = [0u8; 64];
    let s2 = number_to_string_ascii_bytes(n, &mut buf);

    let matches = units.len() == s2.len()
      && units
        .iter()
        .zip(s2.iter().copied())
        .all(|(u, b)| *u == b as u16);
    if matches {
      Ok(Some(n))
    } else {
      Ok(None)
    }
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

  fn get_weak_ref(&self, obj: GcObject) -> Result<&JsWeakRef, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::WeakRef(wr) => Ok(wr),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_finalization_registry(&self, obj: GcObject) -> Result<&JsFinalizationRegistry, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::FinalizationRegistry(fr) => Ok(fr),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_finalization_registry_mut(
    &mut self,
    obj: GcObject,
  ) -> Result<&mut JsFinalizationRegistry, VmError> {
    match self.get_heap_object_mut(obj.0)? {
      HeapObject::FinalizationRegistry(fr) => Ok(fr),
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
    // Convert via ToNumber; supports string/boolean/null/undefined, throws on Symbol.
    //
    // Spec: `TypedArraySetElement` performs value conversion before checking `IsValidIntegerIndex`.
    let n = self.to_number(value)?;
    self.typed_array_set_element_number(view_obj, index, n)
  }

  /// Writes a numeric value into a typed array element.
  ///
  /// This is a helper for cases where the caller has already performed `ToNumber` (or is copying
  /// between numeric sources) and wants to reuse the typed array integer wrapping / clamping logic.
  pub(crate) fn typed_array_set_element_number(
    &mut self,
    view_obj: GcObject,
    index: usize,
    n: f64,
  ) -> Result<bool, VmError> {
    // Extract view fields without holding a mutable borrow across ArrayBuffer access.
    let (buffer, byte_offset, length, kind) = {
      let view = self.get_typed_array(view_obj)?;
      (view.viewed_array_buffer, view.byte_offset, view.length, view.kind)
    };

    // If the backing buffer is detached or the view is out-of-bounds, writes are a silent no-op.
    //
    // Spec: `TypedArraySetElement` (no effect when `IsValidIntegerIndex` is false).
    {
      let buf = self.get_array_buffer(buffer)?;
      if buf.is_immutable() {
        return Err(VmError::TypeError("ArrayBuffer is immutable"));
      }
      let Some(data) = buf.data.as_deref() else {
        return Ok(false);
      };
      let buf_len = data.len();

      let byte_len = match length.checked_mul(kind.bytes_per_element()) {
        Some(n) => n,
        None => return Ok(false),
      };
      let end = match byte_offset.checked_add(byte_len) {
        Some(end) => end,
        None => return Ok(false),
      };
      if end > buf_len {
        return Ok(false);
      }
    }

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
    if abs_end > data.len() {
      return Ok(false);
    }

    match kind {
      TypedArrayKind::Int8 => {
        let v = if !n.is_finite() {
          0
        } else {
          n.trunc().rem_euclid(256.0) as u8 as i8
        };
        data[abs_start] = v as u8;
      }
      TypedArrayKind::Uint8 => {
        let v = if !n.is_finite() {
          0
        } else {
          n.trunc().rem_euclid(256.0) as u8
        };
        data[abs_start] = v;
      }
      TypedArrayKind::Uint8Clamped => {
        data[abs_start] = to_uint8_clamp(n);
      }
      TypedArrayKind::Int16 => {
        let v = if !n.is_finite() {
          0
        } else {
          n.trunc().rem_euclid(65_536.0) as u16 as i16
        };
        data[abs_start..abs_end].copy_from_slice(&v.to_le_bytes());
      }
      TypedArrayKind::Uint16 => {
        let v = if !n.is_finite() {
          0
        } else {
          n.trunc().rem_euclid(65_536.0) as u16
        };
        data[abs_start..abs_end].copy_from_slice(&v.to_le_bytes());
      }
      TypedArrayKind::Int32 => {
        let v = if !n.is_finite() {
          0
        } else {
          n.trunc().rem_euclid(4_294_967_296.0) as u32 as i32
        };
        data[abs_start..abs_end].copy_from_slice(&v.to_le_bytes());
      }
      TypedArrayKind::Uint32 => {
        let v = if !n.is_finite() {
          0
        } else {
          n.trunc().rem_euclid(4_294_967_296.0) as u32
        };
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

  fn get_generator(&self, gen: GcObject) -> Result<&JsGenerator, VmError> {
    match self.get_heap_object(gen.0)? {
      HeapObject::Generator(g) => Ok(g),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_generator_mut(&mut self, gen: GcObject) -> Result<&mut JsGenerator, VmError> {
    match self.get_heap_object_mut(gen.0)? {
      HeapObject::Generator(g) => Ok(g),
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn generator_state(&self, gen: GcObject) -> Result<GeneratorState, VmError> {
    Ok(self.get_generator(gen)?.state)
  }

  pub(crate) fn generator_set_state(
    &mut self,
    gen: GcObject,
    state: GeneratorState,
  ) -> Result<(), VmError> {
    self.get_generator_mut(gen)?.state = state;
    Ok(())
  }

  /// Takes the generator continuation out of the generator object.
  ///
  /// Note: this does **not** update heap accounting immediately. Callers should update the slot
  /// bytes after restoring or dropping the continuation.
  pub(crate) fn generator_take_continuation(
    &mut self,
    gen: GcObject,
  ) -> Result<Option<Box<GeneratorContinuation>>, VmError> {
    Ok(self.get_generator_mut(gen)?.continuation.take())
  }

  /// Sets the generator continuation and updates heap accounting.
  pub(crate) fn generator_set_continuation(
    &mut self,
    gen: GcObject,
    continuation: Option<Box<GeneratorContinuation>>,
  ) -> Result<(), VmError> {
    let idx = self
      .validate(gen.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let new_bytes = {
      let gen = match self.slots[idx].value.as_mut() {
        Some(HeapObject::Generator(g)) => g,
        _ => return Err(VmError::invalid_handle()),
      };

      gen.continuation = continuation;
      gen.heap_size_bytes()
    };

    self.update_slot_bytes(idx, new_bytes);
    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
    Ok(())
  }

  fn get_async_generator(&self, gen: GcObject) -> Result<&JsAsyncGenerator, VmError> {
    match self.get_heap_object(gen.0)? {
      HeapObject::AsyncGenerator(g) => Ok(g),
      _ => Err(VmError::invalid_handle()),
    }
  }

  fn get_async_generator_mut(&mut self, gen: GcObject) -> Result<&mut JsAsyncGenerator, VmError> {
    match self.get_heap_object_mut(gen.0)? {
      HeapObject::AsyncGenerator(g) => Ok(g),
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn async_generator_state(&self, gen: GcObject) -> Result<AsyncGeneratorState, VmError> {
    Ok(self.get_async_generator(gen)?.state)
  }

  pub(crate) fn async_generator_set_state(
    &mut self,
    gen: GcObject,
    state: AsyncGeneratorState,
  ) -> Result<(), VmError> {
    self.get_async_generator_mut(gen)?.state = state;
    Ok(())
  }

  /// Takes the async generator continuation out of the generator object.
  ///
  /// Note: this does **not** update heap accounting immediately. Callers should update the slot
  /// bytes after restoring or dropping the continuation.
  pub(crate) fn async_generator_take_continuation(
    &mut self,
    gen: GcObject,
  ) -> Result<Option<Box<AsyncGeneratorContinuation>>, VmError> {
    Ok(self.get_async_generator_mut(gen)?.continuation.take())
  }

  /// Sets the async generator continuation and updates heap accounting.
  pub(crate) fn async_generator_set_continuation(
    &mut self,
    gen: GcObject,
    continuation: Option<Box<AsyncGeneratorContinuation>>,
  ) -> Result<(), VmError> {
    let idx = self
      .validate(gen.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let new_bytes = {
      let gen = match self.slots[idx].value.as_mut() {
        Some(HeapObject::AsyncGenerator(g)) => g,
        _ => return Err(VmError::invalid_handle()),
      };

      gen.continuation = continuation;
      gen.heap_size_bytes()
    };

    self.update_slot_bytes(idx, new_bytes);
    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
    Ok(())
  }

  pub(crate) fn async_generator_request_queue_len(&self, gen: GcObject) -> Result<usize, VmError> {
    Ok(self.get_async_generator(gen)?.request_queue.len())
  }

  pub(crate) fn async_generator_request_queue_peek(
    &self,
    gen: GcObject,
  ) -> Result<Option<AsyncGeneratorRequest>, VmError> {
    Ok(self.get_async_generator(gen)?.request_queue.front().copied())
  }

  pub(crate) fn async_generator_request_queue_pop(
    &mut self,
    gen: GcObject,
  ) -> Result<Option<AsyncGeneratorRequest>, VmError> {
    let idx = self
      .validate(gen.0)
      .ok_or_else(|| VmError::invalid_handle())?;

    let (req, new_bytes) = {
      let gen = match self.slots[idx].value.as_mut() {
        Some(HeapObject::AsyncGenerator(g)) => g,
        _ => return Err(VmError::invalid_handle()),
      };
      let req = gen.request_queue.pop_front();
      let new_bytes = gen.heap_size_bytes();
      (req, new_bytes)
    };

    self.update_slot_bytes(idx, new_bytes);
    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
    Ok(req)
  }

  pub(crate) fn async_generator_request_queue_push(
    &mut self,
    gen: GcObject,
    req: AsyncGeneratorRequest,
  ) -> Result<(), VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(match req.kind {
      AsyncGeneratorRequestKind::Next(v)
      | AsyncGeneratorRequestKind::Return(v)
      | AsyncGeneratorRequestKind::Throw(v) => v,
    }));
    debug_assert!(self.debug_value_is_valid_or_primitive(req.capability.promise));
    debug_assert!(self.debug_value_is_valid_or_primitive(req.capability.resolve));
    debug_assert!(self.debug_value_is_valid_or_primitive(req.capability.reject));

    let (slot_idx, property_count, continuation_bytes, queue_len, queue_cap, old_bytes) = {
      let slot_idx = self
        .validate(gen.0)
        .ok_or_else(|| VmError::invalid_handle())?;
      let slot = &self.slots[slot_idx];
      let Some(HeapObject::AsyncGenerator(g)) = slot.value.as_ref() else {
        return Err(VmError::invalid_handle());
      };

      (
        slot_idx,
        g.object.base.properties.len(),
        g.continuation
          .as_ref()
          .map(|c| c.heap_size_bytes())
          .unwrap_or(0),
        g.request_queue.len(),
        g.request_queue.capacity(),
        slot.bytes,
      )
    };

    let required_len = queue_len.checked_add(1).ok_or(VmError::OutOfMemory)?;
    let desired_capacity = grown_capacity(queue_cap, required_len);
    if desired_capacity == usize::MAX {
      return Err(VmError::OutOfMemory);
    }
    let queue_bytes = desired_capacity
      .checked_mul(mem::size_of::<AsyncGeneratorRequest>())
      .unwrap_or(usize::MAX);
    let expected_new_bytes = ObjectBase::properties_heap_size_bytes_for_count(property_count)
      .saturating_add(continuation_bytes)
      .saturating_add(queue_bytes);
    let grow_by = expected_new_bytes.saturating_sub(old_bytes);

    if grow_by != 0 {
      let kind_value = match req.kind {
        AsyncGeneratorRequestKind::Next(v)
        | AsyncGeneratorRequestKind::Return(v)
        | AsyncGeneratorRequestKind::Throw(v) => v,
      };
      let extra_roots = [
        Value::Object(gen),
        kind_value,
        req.capability.promise,
        req.capability.resolve,
        req.capability.reject,
      ];
      self.ensure_can_allocate_with_extra_roots(|_| grow_by, &extra_roots, &[], &[], &[])?;

      let Some(HeapObject::AsyncGenerator(g)) = self.slots[slot_idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      reserve_vec_deque_to_len::<AsyncGeneratorRequest>(&mut g.request_queue, required_len)?;
    }

    let new_bytes = {
      let Some(HeapObject::AsyncGenerator(g)) = self.slots[slot_idx].value.as_mut() else {
        return Err(VmError::invalid_handle());
      };
      g.request_queue.push_back(req);
      g.heap_size_bytes()
    };

    self.update_slot_bytes(slot_idx, new_bytes);
    #[cfg(debug_assertions)]
    self.debug_assert_used_bytes_is_correct();
    Ok(())
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

  /// Returns the ECMAScript well-known symbols for this heap, allocating them lazily.
  ///
  /// Well-known symbols are specified to be **agent-wide**; while live, all realms created within
  /// this heap observe the same `Symbol.*` identities.
  pub(crate) fn ensure_well_known_symbols(&mut self) -> Result<WellKnownSymbols, VmError> {
    if let Some(wks) = self.well_known_symbols {
      // Fast path: cached and still live.
      let all_valid = self.is_valid_symbol(wks.async_iterator)
        && self.is_valid_symbol(wks.async_dispose)
        && self.is_valid_symbol(wks.dispose)
        && self.is_valid_symbol(wks.has_instance)
        && self.is_valid_symbol(wks.is_concat_spreadable)
        && self.is_valid_symbol(wks.iterator)
        && self.is_valid_symbol(wks.match_)
        && self.is_valid_symbol(wks.match_all)
        && self.is_valid_symbol(wks.replace)
        && self.is_valid_symbol(wks.search)
        && self.is_valid_symbol(wks.species)
        && self.is_valid_symbol(wks.split)
        && self.is_valid_symbol(wks.to_primitive)
        && self.is_valid_symbol(wks.to_string_tag)
        && self.is_valid_symbol(wks.unscopables);
      if all_valid {
        return Ok(wks);
      }

      // `well_known_symbols` is a weak-ish cache: entries may become invalid after GC if no realms
      // (or other roots) keep them alive. Validate each symbol and recreate only the missing ones.
      //
      // Root any live symbols while we allocate replacements to avoid GC races.
      let mut scope = self.scope();
      let mut ensure = |sym: GcSymbol, desc: &str| -> Result<GcSymbol, VmError> {
        if scope.heap.is_valid_symbol(sym) {
          scope.push_root(Value::Symbol(sym))?;
          Ok(sym)
        } else {
          let sym = scope.alloc_symbol(Some(desc))?;
          scope.push_root(Value::Symbol(sym))?;
          Ok(sym)
        }
      };

      let wks = WellKnownSymbols {
        async_iterator: ensure(wks.async_iterator, "Symbol.asyncIterator")?,
        async_dispose: ensure(wks.async_dispose, "Symbol.asyncDispose")?,
        dispose: ensure(wks.dispose, "Symbol.dispose")?,
        has_instance: ensure(wks.has_instance, "Symbol.hasInstance")?,
        is_concat_spreadable: ensure(wks.is_concat_spreadable, "Symbol.isConcatSpreadable")?,
        iterator: ensure(wks.iterator, "Symbol.iterator")?,
        match_: ensure(wks.match_, "Symbol.match")?,
        match_all: ensure(wks.match_all, "Symbol.matchAll")?,
        replace: ensure(wks.replace, "Symbol.replace")?,
        search: ensure(wks.search, "Symbol.search")?,
        species: ensure(wks.species, "Symbol.species")?,
        split: ensure(wks.split, "Symbol.split")?,
        to_primitive: ensure(wks.to_primitive, "Symbol.toPrimitive")?,
        to_string_tag: ensure(wks.to_string_tag, "Symbol.toStringTag")?,
        unscopables: ensure(wks.unscopables, "Symbol.unscopables")?,
      };

      scope.heap.well_known_symbols = Some(wks);
      return Ok(wks);
    }

    // Allocate well-known symbols in a temporary rooting scope, then cache them on the heap so
    // subsequent realms can reuse the same identities.
    //
    // These symbols are kept alive by realm roots (and by any other live objects that reference
    // them), not by the heap cache itself.
    let mut scope = self.scope();

    // Root each newly-created symbol while we allocate the rest of the set, in case allocations
    // trigger GC.
    let async_iterator = scope.alloc_symbol(Some("Symbol.asyncIterator"))?;
    scope.push_root(Value::Symbol(async_iterator))?;
    let async_dispose = scope.alloc_symbol(Some("Symbol.asyncDispose"))?;
    scope.push_root(Value::Symbol(async_dispose))?;
    let dispose = scope.alloc_symbol(Some("Symbol.dispose"))?;
    scope.push_root(Value::Symbol(dispose))?;
    let has_instance = scope.alloc_symbol(Some("Symbol.hasInstance"))?;
    scope.push_root(Value::Symbol(has_instance))?;
    let is_concat_spreadable = scope.alloc_symbol(Some("Symbol.isConcatSpreadable"))?;
    scope.push_root(Value::Symbol(is_concat_spreadable))?;
    let iterator = scope.alloc_symbol(Some("Symbol.iterator"))?;
    scope.push_root(Value::Symbol(iterator))?;
    let match_ = scope.alloc_symbol(Some("Symbol.match"))?;
    scope.push_root(Value::Symbol(match_))?;
    let match_all = scope.alloc_symbol(Some("Symbol.matchAll"))?;
    scope.push_root(Value::Symbol(match_all))?;
    let replace = scope.alloc_symbol(Some("Symbol.replace"))?;
    scope.push_root(Value::Symbol(replace))?;
    let search = scope.alloc_symbol(Some("Symbol.search"))?;
    scope.push_root(Value::Symbol(search))?;
    let species = scope.alloc_symbol(Some("Symbol.species"))?;
    scope.push_root(Value::Symbol(species))?;
    let split = scope.alloc_symbol(Some("Symbol.split"))?;
    scope.push_root(Value::Symbol(split))?;
    let to_primitive = scope.alloc_symbol(Some("Symbol.toPrimitive"))?;
    scope.push_root(Value::Symbol(to_primitive))?;
    let to_string_tag = scope.alloc_symbol(Some("Symbol.toStringTag"))?;
    scope.push_root(Value::Symbol(to_string_tag))?;
    let unscopables = scope.alloc_symbol(Some("Symbol.unscopables"))?;
    scope.push_root(Value::Symbol(unscopables))?;

    let wks = WellKnownSymbols {
      async_dispose,
      async_iterator,
      dispose,
      has_instance,
      is_concat_spreadable,
      iterator,
      match_,
      match_all,
      replace,
      search,
      species,
      split,
      to_primitive,
      to_string_tag,
      unscopables,
    };

    scope.heap.well_known_symbols = Some(wks);
    Ok(wks)
  }

  fn ensure_internal_symbol(
    &mut self,
    description: &'static str,
    get: impl FnOnce(&InternalSymbols) -> Option<GcSymbol>,
    set: impl FnOnce(&mut InternalSymbols, GcSymbol),
  ) -> Result<GcSymbol, VmError> {
    if let Some(sym) = get(&self.internal_symbols) {
      return Ok(sym);
    }
    // Allocate the symbol in a temporary rooting scope, then store it on the heap so it is traced
    // by GC and stable across realms.
    let mut scope = self.scope();
    let desc = scope.alloc_string(description)?;
    let sym = scope.new_internal_symbol(Some(desc))?;
    set(&mut scope.heap.internal_symbols, sym);
    Ok(sym)
  }

  pub(crate) fn internal_string_data_symbol(&self) -> Option<GcSymbol> {
    self.internal_symbols.string_data
  }

  pub(crate) fn ensure_internal_string_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.StringData",
      |s| s.string_data,
      |s, sym| s.string_data = Some(sym),
    )
  }

  pub(crate) fn internal_symbol_data_symbol(&self) -> Option<GcSymbol> {
    self.internal_symbols.symbol_data
  }

  pub(crate) fn ensure_internal_symbol_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.SymbolData",
      |s| s.symbol_data,
      |s, sym| s.symbol_data = Some(sym),
    )
  }

  pub(crate) fn internal_boolean_data_symbol(&self) -> Option<GcSymbol> {
    self.internal_symbols.boolean_data
  }

  pub(crate) fn ensure_internal_boolean_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.BooleanData",
      |s| s.boolean_data,
      |s, sym| s.boolean_data = Some(sym),
    )
  }

  pub(crate) fn internal_number_data_symbol(&self) -> Option<GcSymbol> {
    self.internal_symbols.number_data
  }

  pub(crate) fn ensure_internal_number_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.NumberData",
      |s| s.number_data,
      |s, sym| s.number_data = Some(sym),
    )
  }

  pub(crate) fn internal_bigint_data_symbol(&self) -> Option<GcSymbol> {
    self.internal_symbols.bigint_data
  }

  pub(crate) fn ensure_internal_bigint_data_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.BigIntData",
      |s| s.bigint_data,
      |s, sym| s.bigint_data = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_array_iterator_array_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.ArrayIteratorArray",
      |s| s.array_iterator_array,
      |s, sym| s.array_iterator_array = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_array_iterator_index_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.ArrayIteratorIndex",
      |s| s.array_iterator_index,
      |s, sym| s.array_iterator_index = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_array_iterator_kind_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.ArrayIteratorKind",
      |s| s.array_iterator_kind,
      |s, sym| s.array_iterator_kind = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_map_iterator_map_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.MapIteratorMap",
      |s| s.map_iterator_map,
      |s, sym| s.map_iterator_map = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_map_iterator_index_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.MapIteratorIndex",
      |s| s.map_iterator_index,
      |s, sym| s.map_iterator_index = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_map_iterator_kind_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.MapIteratorKind",
      |s| s.map_iterator_kind,
      |s, sym| s.map_iterator_kind = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_set_iterator_set_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.SetIteratorSet",
      |s| s.set_iterator_set,
      |s, sym| s.set_iterator_set = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_set_iterator_index_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.SetIteratorIndex",
      |s| s.set_iterator_index,
      |s, sym| s.set_iterator_index = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_set_iterator_kind_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.SetIteratorKind",
      |s| s.set_iterator_kind,
      |s, sym| s.set_iterator_kind = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_string_iterator_iterated_string_symbol(
    &mut self,
  ) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.StringIteratorIteratedString",
      |s| s.string_iterator_iterated_string,
      |s, sym| s.string_iterator_iterated_string = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_string_iterator_next_index_symbol(
    &mut self,
  ) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.StringIteratorNextIndex",
      |s| s.string_iterator_next_index,
      |s, sym| s.string_iterator_next_index = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_regexp_string_iterator_iterating_regexp_symbol(
    &mut self,
  ) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.RegExpStringIteratorIteratingRegExp",
      |s| s.regexp_string_iterator_iterating_regexp,
      |s, sym| s.regexp_string_iterator_iterating_regexp = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_regexp_string_iterator_iterated_string_symbol(
    &mut self,
  ) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.RegExpStringIteratorIteratedString",
      |s| s.regexp_string_iterator_iterated_string,
      |s, sym| s.regexp_string_iterator_iterated_string = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_regexp_string_iterator_global_symbol(
    &mut self,
  ) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.RegExpStringIteratorGlobal",
      |s| s.regexp_string_iterator_global,
      |s, sym| s.regexp_string_iterator_global = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_regexp_string_iterator_unicode_symbol(
    &mut self,
  ) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.RegExpStringIteratorUnicode",
      |s| s.regexp_string_iterator_unicode,
      |s, sym| s.regexp_string_iterator_unicode = Some(sym),
    )
  }

  pub(crate) fn ensure_internal_regexp_string_iterator_done_symbol(
    &mut self,
  ) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.RegExpStringIteratorDone",
      |s| s.regexp_string_iterator_done,
      |s, sym| s.regexp_string_iterator_done = Some(sym),
    )
  }

  pub(crate) fn internal_is_raw_json_symbol(&self) -> Option<GcSymbol> {
    self.internal_symbols.is_raw_json
  }

  pub(crate) fn ensure_internal_is_raw_json_symbol(&mut self) -> Result<GcSymbol, VmError> {
    self.ensure_internal_symbol(
      "vm-js.internal.IsRawJSON",
      |s| s.is_raw_json,
      |s, sym| s.is_raw_json = Some(sym),
    )
  }

  pub(crate) fn is_internal_symbol(&self, sym: GcSymbol) -> bool {
    match self.get_heap_object(sym.0) {
      Ok(HeapObject::Symbol(sym)) => sym.is_internal(),
      _ => false,
    }
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
      let PropertyKey::Symbol(sym) = prop.key else {
        continue;
      };
      // Internal slot marker symbols must not be observable from JS, otherwise scripts can obtain
      // the symbol via `Reflect.ownKeys` and access/modify internal slot state.
      if self.is_internal_symbol(sym) {
        continue;
      }
      out.push(prop.key);
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

  pub(crate) fn env_set_private_names(
    &mut self,
    env: GcEnv,
    private_names: Box<[PrivateNameEntry]>,
  ) -> Result<(), VmError> {
    let rec = self.get_declarative_env_mut(env)?;
    rec.private_names = Some(private_names);
    Ok(())
  }

  /// Resolves a private name within the innermost active private-name environment.
  ///
  /// Private names are lexically scoped to the nearest enclosing class body and are not inherited
  /// across class boundaries. Therefore this resolves against the first environment record in the
  /// chain that contains private-name metadata, and does **not** continue searching outer
  /// environments.
  pub(crate) fn resolve_private_name_symbol(
    &self,
    env: GcEnv,
    name: &str,
  ) -> Result<Option<GcSymbol>, VmError> {
    let mut current = Some(env);
    while let Some(e) = current {
      match self.get_env(e)? {
        EnvRecord::Declarative(rec) => {
          if let Some(private_names) = rec.private_names.as_deref() {
            for entry in private_names {
              if entry.name.as_ref() == name {
                return Ok(Some(entry.sym));
              }
            }
            // Stop at the first private-name environment boundary.
            return Ok(None);
          }
          current = rec.outer;
        }
        EnvRecord::Object(rec) => {
          current = rec.outer;
        }
      }
    }
    Ok(None)
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

    // Array exotic object fast path: store small array-index properties in the array's dense
    // elements table instead of the ordinary property table.
    if let Some(index) = key_array_index {
      if index <= MAX_FAST_ARRAY_INDEX {
        if let Some(HeapObject::Object(obj)) = self.slots[idx].value.as_mut() {
          if obj.array_length().is_some() {
            let ObjectKind::Array(arr) = &mut obj.base.kind else {
              return Err(VmError::InvariantViolation("expected array object kind"));
            };

            let needed_len = index as usize + 1;
            if arr.elements.len() < needed_len {
              // Avoid panicking on OOM: reserve fallibly.
              arr
                .elements
                .try_reserve(needed_len - arr.elements.len())
                .map_err(|_| VmError::OutOfMemory)?;
              arr.elements.resize(needed_len, None);
            }
            arr.elements[index as usize] = Some(desc);

            // Array exotic index semantics: writing an array index extends `length`.
            let new_len = index.wrapping_add(1);
            if let Some(current_len) = obj.array_length() {
              if new_len > current_len {
                obj.set_array_length(new_len);
              }
            }
            return Ok(());
          }
        }
      }
    }

    #[derive(Clone, Copy)]
    enum TargetKind {
      OrdinaryObject,
      RegExp { program_bytes: usize },
      ArrayBuffer,
      TypedArray,
      DataView,
      Map { entry_capacity: usize },
      Set { entry_capacity: usize },
      WeakMap {
        entry_capacity: usize,
      },
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
      WeakRef,
      FinalizationRegistry {
        cell_capacity: usize,
      },
      Generator {
        continuation_bytes: usize,
      },
      AsyncGenerator {
        continuation_bytes: usize,
        queue_bytes: usize,
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
        HeapObject::RegExp(obj) => {
          let existing_idx = obj
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::RegExp {
              program_bytes: obj.program.heap_size_bytes(),
            },
            obj.base.properties.len(),
            slot.bytes,
            existing_idx,
            None,
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
        HeapObject::Map(obj) => {
          let existing_idx = obj
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::Map {
              entry_capacity: obj.entries.capacity(),
            },
            obj.base.properties.len(),
            slot.bytes,
            existing_idx,
            None,
          )
        }
        HeapObject::Set(obj) => {
          let existing_idx = obj
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::Set {
              entry_capacity: obj.entries.capacity(),
            },
            obj.base.properties.len(),
            slot.bytes,
            existing_idx,
            None,
          )
        }
        HeapObject::WeakMap(obj) => {
          let existing_idx = obj
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::WeakMap {
              entry_capacity: obj.entries.capacity(),
            },
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
        HeapObject::WeakRef(obj) => {
          let existing_idx = obj
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::WeakRef,
            obj.base.properties.len(),
            slot.bytes,
            existing_idx,
            None,
          )
        }
        HeapObject::FinalizationRegistry(obj) => {
          let existing_idx = obj
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::FinalizationRegistry {
              cell_capacity: obj.cells.capacity(),
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
              continuation_bytes: g
                .continuation
                .as_ref()
                .map(|c| c.heap_size_bytes())
                .unwrap_or(0),
            },
            g.object.base.properties.len(),
            slot.bytes,
            existing_idx,
            None,
          )
        }
        HeapObject::AsyncGenerator(g) => {
          let existing_idx = g
            .object
            .base
            .properties
            .iter()
            .position(|entry| self.property_key_eq(&entry.key, &key));
          (
            TargetKind::AsyncGenerator {
              continuation_bytes: g
                .continuation
                .as_ref()
                .map(|c| c.heap_size_bytes())
                .unwrap_or(0),
              queue_bytes: g
                .request_queue
                .capacity()
                .checked_mul(mem::size_of::<AsyncGeneratorRequest>())
                .unwrap_or(usize::MAX),
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
          Some(HeapObject::RegExp(obj)) => obj.base.properties[existing_idx].desc = desc,
          Some(HeapObject::ArrayBuffer(obj)) => obj.base.properties[existing_idx].desc = desc,
          Some(HeapObject::TypedArray(obj)) => obj.base.properties[existing_idx].desc = desc,
          Some(HeapObject::DataView(obj)) => obj.base.properties[existing_idx].desc = desc,
          Some(HeapObject::Map(m)) => m.base.properties[existing_idx].desc = desc,
          Some(HeapObject::Set(s)) => s.base.properties[existing_idx].desc = desc,
          Some(HeapObject::WeakMap(wm)) => wm.base.properties[existing_idx].desc = desc,
          Some(HeapObject::Function(func)) => func.base.properties[existing_idx].desc = desc,
          Some(HeapObject::Promise(p)) => p.object.base.properties[existing_idx].desc = desc,
          Some(HeapObject::WeakRef(wr)) => wr.base.properties[existing_idx].desc = desc,
          Some(HeapObject::WeakSet(ws)) => ws.base.properties[existing_idx].desc = desc,
          Some(HeapObject::FinalizationRegistry(fr)) => fr.base.properties[existing_idx].desc = desc,
          Some(HeapObject::Generator(g)) => g.object.base.properties[existing_idx].desc = desc,
          Some(HeapObject::AsyncGenerator(g)) => g.object.base.properties[existing_idx].desc = desc,
          _ => return Err(VmError::invalid_handle()),
        }
      }
      None => {
        let new_property_count = property_count
          .checked_add(1)
          .ok_or(VmError::OutOfMemory)?;
        let new_bytes = match target_kind {
          TargetKind::OrdinaryObject => JsObject::heap_size_bytes_for_property_count(new_property_count),
          TargetKind::RegExp { program_bytes } => ObjectBase::properties_heap_size_bytes_for_count(new_property_count)
            .saturating_add(program_bytes),
          TargetKind::ArrayBuffer => JsArrayBuffer::heap_size_bytes_for_property_count(new_property_count),
          TargetKind::TypedArray => JsTypedArray::heap_size_bytes_for_property_count(new_property_count),
          TargetKind::DataView => JsDataView::heap_size_bytes_for_property_count(new_property_count),
          TargetKind::Map { entry_capacity } => JsMap::heap_size_bytes_for_counts(new_property_count, entry_capacity),
          TargetKind::Set { entry_capacity } => JsSet::heap_size_bytes_for_counts(new_property_count, entry_capacity),
          TargetKind::WeakMap { entry_capacity } => {
            JsWeakMap::heap_size_bytes_for_counts(new_property_count, entry_capacity)
          }
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
          TargetKind::WeakRef => JsWeakRef::heap_size_bytes_for_property_count(new_property_count),
          TargetKind::FinalizationRegistry { cell_capacity } => {
            JsFinalizationRegistry::heap_size_bytes_for_counts(new_property_count, cell_capacity)
          }
          TargetKind::Generator {
            continuation_bytes,
          } => ObjectBase::properties_heap_size_bytes_for_count(new_property_count)
            .saturating_add(continuation_bytes),
          TargetKind::AsyncGenerator {
            continuation_bytes,
            queue_bytes,
          } => ObjectBase::properties_heap_size_bytes_for_count(new_property_count)
            .saturating_add(continuation_bytes)
            .saturating_add(queue_bytes),
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
            Some(HeapObject::RegExp(obj)) => buf.extend_from_slice(&obj.base.properties),
            Some(HeapObject::ArrayBuffer(obj)) => buf.extend_from_slice(&obj.base.properties),
            Some(HeapObject::TypedArray(obj)) => buf.extend_from_slice(&obj.base.properties),
            Some(HeapObject::DataView(obj)) => buf.extend_from_slice(&obj.base.properties),
            Some(HeapObject::Map(m)) => buf.extend_from_slice(&m.base.properties),
            Some(HeapObject::Set(s)) => buf.extend_from_slice(&s.base.properties),
            Some(HeapObject::WeakMap(wm)) => buf.extend_from_slice(&wm.base.properties),
            Some(HeapObject::Function(func)) => buf.extend_from_slice(&func.base.properties),
            Some(HeapObject::Promise(p)) => buf.extend_from_slice(&p.object.base.properties),
            Some(HeapObject::WeakRef(wr)) => buf.extend_from_slice(&wr.base.properties),
            Some(HeapObject::WeakSet(ws)) => buf.extend_from_slice(&ws.base.properties),
            Some(HeapObject::FinalizationRegistry(fr)) => buf.extend_from_slice(&fr.base.properties),
            Some(HeapObject::Generator(g)) => buf.extend_from_slice(&g.object.base.properties),
            Some(HeapObject::AsyncGenerator(g)) => buf.extend_from_slice(&g.object.base.properties),
            _ => return Err(VmError::invalid_handle()),
          }
        }

        buf.push(PropertyEntry { key, desc });
        let properties = buf.into_boxed_slice();

        match self.slots[idx].value.as_mut() {
          Some(HeapObject::Object(obj)) => obj.base.properties = properties,
          Some(HeapObject::RegExp(obj)) => obj.base.properties = properties,
          Some(HeapObject::ArrayBuffer(obj)) => obj.base.properties = properties,
          Some(HeapObject::TypedArray(obj)) => obj.base.properties = properties,
          Some(HeapObject::DataView(obj)) => obj.base.properties = properties,
          Some(HeapObject::Map(m)) => m.base.properties = properties,
          Some(HeapObject::Set(s)) => s.base.properties = properties,
          Some(HeapObject::WeakMap(wm)) => wm.base.properties = properties,
          Some(HeapObject::Function(func)) => func.base.properties = properties,
          Some(HeapObject::Promise(p)) => p.object.base.properties = properties,
          Some(HeapObject::WeakRef(wr)) => wr.base.properties = properties,
          Some(HeapObject::WeakSet(ws)) => ws.base.properties = properties,
          Some(HeapObject::FinalizationRegistry(fr)) => fr.base.properties = properties,
          Some(HeapObject::Generator(g)) => g.object.base.properties = properties,
          Some(HeapObject::AsyncGenerator(g)) => g.object.base.properties = properties,
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

  #[track_caller]
  fn get_heap_object(&self, id: HeapId) -> Result<&HeapObject, VmError> {
    let invalid = VmError::InvalidHandle {
      location: std::panic::Location::caller(),
    };
    let idx = self.validate(id).ok_or(invalid.clone())?;
    self
      .slots[idx]
      .value
      .as_ref()
      .ok_or(invalid)
  }

  #[track_caller]
  fn get_heap_object_mut(&mut self, id: HeapId) -> Result<&mut HeapObject, VmError> {
    let invalid = VmError::InvalidHandle {
      location: std::panic::Location::caller(),
    };
    let idx = self.validate(id).ok_or(invalid.clone())?;
    self
      .slots[idx]
      .value
      .as_mut()
      .ok_or(invalid)
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
      // If we're over budget even after GC, try releasing reserved-but-unused vector capacity before
      // failing. This can matter for small heaps because `Vec` growth is exponential but capacity is
      // charged against `HeapLimits`.
      self.shrink_excess_capacity();

      let after = self
        .estimated_total_bytes()
        .saturating_add(additional_bytes(self));
      if after <= self.limits.max_bytes {
        return Ok(());
      }
      return Err(VmError::OutOfMemory);
    }
    Ok(())
  }

  fn shrink_excess_capacity(&mut self) {
    // Slot table + mark bits.
    self.slots.shrink_to_fit();
    self.marks.shrink_to_fit();

    let slot_len = self.slots.len();
    // Keep enough capacity so GC can never allocate while pushing onto these vectors.
    self.free_list.shrink_to(slot_len);
    self.gc_worklist.shrink_to(slot_len);

    // Root stacks.
    self.root_stack.shrink_to_fit();
    self.env_root_stack.shrink_to_fit();

    // Persistent roots. Keep enough capacity in the free lists so `remove_root` cannot allocate.
    self.persistent_roots.shrink_to_fit();
    self.persistent_roots_free.shrink_to(self.persistent_roots.len());
    self.persistent_env_roots.shrink_to_fit();
    self
      .persistent_env_roots_free
      .shrink_to(self.persistent_env_roots.len());

    // Global symbol registry.
    self.symbol_registry.shrink_to_fit();
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
      Value::Undefined | Value::Null | Value::Bool(_) | Value::Number(_) => true,
      Value::BigInt(b) => self.is_valid_bigint(b),
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

  /// Sets the value of a captured native slot on a function object.
  ///
  /// This is used to implement spec "abstract closure" semantics for built-ins such as
  /// `Proxy.revocable` where the associated revoker must clear its `[[RevocableProxy]]` slot after
  /// the first call so it does not keep the proxy alive.
  pub(crate) fn set_function_native_slot(
    &mut self,
    func: GcObject,
    slot_idx: usize,
    value: Value,
  ) -> Result<(), VmError> {
    debug_assert!(self.debug_value_is_valid_or_primitive(value));

    let HeapObject::Function(f) = self.get_heap_object_mut(func.0)? else {
      return Err(VmError::NotCallable);
    };
    let Some(slots) = f.native_slots.as_deref_mut() else {
      return Err(VmError::InvariantViolation(
        "attempted to set native slot on a function without native slots",
      ));
    };
    let Some(slot) = slots.get_mut(slot_idx) else {
      return Err(VmError::InvariantViolation(
        "native slot index out of bounds",
      ));
    };
    *slot = value;
    Ok(())
  }

  pub(crate) fn get_function_construct_handler(
    &self,
    func: GcObject,
  ) -> Result<Option<ConstructHandler>, VmError> {
    match self.get_heap_object(func.0)? {
      HeapObject::Function(f) => Ok(f.construct.clone()),
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

  pub(crate) fn get_derived_constructor_state(
    &self,
    obj: GcObject,
  ) -> Result<&DerivedConstructorState, VmError> {
    match self.get_heap_object(obj.0)? {
      HeapObject::DerivedConstructorState(s) => Ok(s),
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn get_derived_constructor_state_mut(
    &mut self,
    obj: GcObject,
  ) -> Result<&mut DerivedConstructorState, VmError> {
    match self.get_heap_object_mut(obj.0)? {
      HeapObject::DerivedConstructorState(s) => Ok(s),
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

  pub(crate) fn get_function_home_object(&self, func: GcObject) -> Result<Option<GcObject>, VmError> {
    // `[[HomeObject]]` is stored on the actual function object and is **not** proxy-aware.
    match self.get_heap_object(func.0)? {
      HeapObject::Function(f) => Ok(f.home_object),
      _ => Err(VmError::NotCallable),
    }
  }

  pub(crate) fn get_function_job_realm(&self, func: GcObject) -> Option<RealmId> {
    // Promise job callbacks may be Proxy objects; follow the proxy chain to the underlying target.
    let mut obj = func;
    for _ in 0..MAX_PROTOTYPE_CHAIN {
      match self.get_heap_object(obj.0) {
        Ok(HeapObject::Function(f)) => {
          return (f.job_realm != 0).then(|| RealmId::from_raw(f.job_realm - 1));
        }
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

  pub(crate) fn get_function_script_or_module_token(&self, func: GcObject) -> Option<NonZeroU32> {
    // Promise job callbacks may be Proxy objects; follow the proxy chain to the underlying target.
    let mut obj = func;
    for _ in 0..MAX_PROTOTYPE_CHAIN {
      match self.get_heap_object(obj.0) {
        Ok(HeapObject::Function(f)) => return f.script_or_module_token,
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

  pub(crate) fn set_function_home_object(
    &mut self,
    func: GcObject,
    home: Option<GcObject>,
  ) -> Result<(), VmError> {
    // `[[HomeObject]]` is stored on the actual function object and is **not** proxy-aware.
    match self.get_heap_object_mut(func.0)? {
      HeapObject::Function(f) => {
        f.home_object = home;
        Ok(())
      }
      _ => Err(VmError::NotCallable),
    }
  }

  /// Sets a function object's `[[Realm]]` metadata (represented as the realm's global object).
  ///
  /// This is used by built-ins like `%Function%`/`eval` to determine which realm to create new
  /// objects in.
  pub fn set_function_realm(&mut self, func: GcObject, realm: GcObject) -> Result<(), VmError> {
    match self.get_heap_object_mut(func.0)? {
      HeapObject::Function(f) => {
        f.realm = Some(realm);
        Ok(())
      }
      _ => Err(VmError::invalid_handle()),
    }
  }

  /// Sets a function object's `[[JobRealm]]` metadata.
  ///
  /// This is used by Promise jobs / host callbacks to recover a realm when no execution context is
  /// currently active.
  pub fn set_function_job_realm(&mut self, func: GcObject, realm: RealmId) -> Result<(), VmError> {
    match self.get_heap_object_mut(func.0)? {
      HeapObject::Function(f) => {
        f.job_realm = realm.to_raw().checked_add(1).ok_or(VmError::OutOfMemory)?;
        Ok(())
      }
      _ => Err(VmError::invalid_handle()),
    }
  }

  pub(crate) fn set_function_script_or_module_token(
    &mut self,
    func: GcObject,
    token: Option<NonZeroU32>,
  ) -> Result<(), VmError> {
    match self.get_heap_object_mut(func.0)? {
      HeapObject::Function(f) => {
        f.script_or_module_token = token;
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

    // Under small heap limits, temporary rooting scopes can cause the root stack capacity to grow
    // and permanently contribute a significant fraction of `estimated_total_bytes`, even after all
    // roots are popped.
    //
    // Reclaim root stack capacity opportunistically when exiting the outermost scope, but only when
    // the heap is close to its memory limit (avoid thrashing on large heaps).
    if self.root_stack_len_at_entry == 0
      && self.heap.root_stack.is_empty()
      && self.heap.root_stack.capacity() > 256
    {
      let max = self.heap.limits.max_bytes;
      if self.heap.estimated_total_bytes() > max.saturating_mul(3) / 4 {
        self.heap.root_stack = Vec::new();
      }
    }
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
    let js = JsString::from_u16_vec(units)?;
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

    let js = JsString::from_u16_vec(buf)?;
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

    let js = JsString::from_u16_vec(units)?;
    debug_assert_eq!(new_bytes, js.heap_size_bytes());
    let obj = HeapObject::String(js);
    Ok(GcString(self.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Convenience alias for [`Scope::alloc_string_from_utf8`].
  pub fn alloc_string(&mut self, s: &str) -> Result<GcString, VmError> {
    self.alloc_string_from_utf8(s)
  }

  pub(crate) fn common_key_name(&mut self) -> Result<GcString, VmError> {
    if let Some(s) = self.heap.common_key_name {
      if self.heap.is_valid_string(s) {
        return Ok(s);
      }
    }
    let s = self.alloc_string("name")?;
    self.heap.common_key_name = Some(s);
    Ok(s)
  }

  pub(crate) fn common_key_message(&mut self) -> Result<GcString, VmError> {
    if let Some(s) = self.heap.common_key_message {
      if self.heap.is_valid_string(s) {
        return Ok(s);
      }
    }
    let s = self.alloc_string("message")?;
    self.heap.common_key_message = Some(s);
    Ok(s)
  }

  pub(crate) fn common_key_length(&mut self) -> Result<GcString, VmError> {
    if let Some(s) = self.heap.common_key_length {
      if self.heap.is_valid_string(s) {
        return Ok(s);
      }
    }
    let s = self.alloc_string("length")?;
    self.heap.common_key_length = Some(s);
    Ok(s)
  }

  pub(crate) fn common_key_constructor(&mut self) -> Result<GcString, VmError> {
    if let Some(s) = self.heap.common_key_constructor {
      if self.heap.is_valid_string(s) {
        return Ok(s);
      }
    }
    let s = self.alloc_string("constructor")?;
    self.heap.common_key_constructor = Some(s);
    Ok(s)
  }

  pub(crate) fn common_key_prototype(&mut self) -> Result<GcString, VmError> {
    if let Some(s) = self.heap.common_key_prototype {
      if self.heap.is_valid_string(s) {
        return Ok(s);
      }
    }
    let s = self.alloc_string("prototype")?;
    self.heap.common_key_prototype = Some(s);
    Ok(s)
  }

  /// Allocates a canonical ECMAScript array-index string for `idx` (decimal digits, no leading
  /// zeros).
  ///
  /// This avoids an intermediate Rust `String` allocation (e.g. formatting via `ToString`), which is
  /// infallible and can abort the host process under allocator OOM.
  pub fn alloc_u32_index_string(&mut self, idx: u32) -> Result<GcString, VmError> {
    if (idx as usize) < SMALL_INT_STRING_CACHE_SIZE {
      if let Some(s) = self.heap.small_int_strings[idx as usize] {
        return Ok(s);
      }
    }

    // `u32::MAX` has 10 decimal digits.
    let mut buf = [0u8; 10];
    let mut n = idx;
    let mut pos = buf.len();

    if n == 0 {
      pos -= 1;
      buf[pos] = b'0';
    } else {
      while n != 0 {
        pos -= 1;
        buf[pos] = b'0' + (n % 10) as u8;
        n /= 10;
      }
    }

    let s = std::str::from_utf8(&buf[pos..]).map_err(|_| {
      VmError::InvariantViolation("invalid UTF-8 in array index formatting buffer")
    })?;
    let out = self.alloc_string_from_utf8(s)?;
    if (idx as usize) < SMALL_INT_STRING_CACHE_SIZE {
      self.heap.small_int_strings[idx as usize] = Some(out);
    }
    Ok(out)
  }

  /// Allocates a canonical array-index property key for `idx` (decimal digits).
  #[inline]
  pub fn alloc_array_index_key(&mut self, idx: u32) -> Result<PropertyKey, VmError> {
    Ok(PropertyKey::from_string(self.alloc_u32_index_string(idx)?))
  }

  /// Allocates a JavaScript BigInt on the heap.
  pub fn alloc_bigint(&mut self, bigint: JsBigInt) -> Result<GcBigInt, VmError> {
    let new_bytes = bigint.heap_size_bytes();
    self.heap.ensure_can_allocate(new_bytes)?;
    let obj = HeapObject::BigInt(bigint);
    Ok(GcBigInt(self.heap.alloc_unchecked(obj, new_bytes)?))
  }

  pub fn alloc_bigint_from_u128(&mut self, value: u128) -> Result<GcBigInt, VmError> {
    self.alloc_bigint(JsBigInt::from_u128(value)?)
  }

  pub fn alloc_bigint_from_i128(&mut self, value: i128) -> Result<GcBigInt, VmError> {
    self.alloc_bigint(JsBigInt::from_i128(value)?)
  }

  pub fn alloc_u32_string(&mut self, value: u32) -> Result<GcString, VmError> {
    let mut buf = itoa::Buffer::new();
    self.alloc_string(buf.format(value))
  }

  pub fn alloc_usize_string(&mut self, value: usize) -> Result<GcString, VmError> {
    let mut buf = itoa::Buffer::new();
    self.alloc_string(buf.format(value))
  }

  pub fn alloc_i32_string(&mut self, value: i32) -> Result<GcString, VmError> {
    let mut buf = itoa::Buffer::new();
    self.alloc_string(buf.format(value))
  }

  /// Allocates a JavaScript string for an ECMAScript `Number` value using the spec `ToString`
  /// formatting rules.
  pub fn alloc_number_string(&mut self, n: f64) -> Result<GcString, VmError> {
    // https://tc39.es/ecma262/multipage/ecmascript-data-types-and-values.html#sec-numeric-types-number-tostring
    if n.is_nan() {
      return self.alloc_string("NaN");
    }
    if n == 0.0 {
      // Covers both +0 and -0.
      return self.alloc_u32_index_string(0);
    }
    if n.is_infinite() {
      if n.is_sign_negative() {
        return self.alloc_string("-Infinity");
      }
      return self.alloc_string("Infinity");
    }

    // Common case: small non-negative integers (frequently used as array indices).
    if n.is_finite() && n.fract() == 0.0 && n >= 0.0 && n < SMALL_INT_STRING_CACHE_SIZE as f64 {
      return self.alloc_u32_index_string(n as u32);
    }

    // `ryu` is used only for digit/exponent decomposition; the final formatting rules match
    // ECMAScript `Number::toString()` (not Rust's float formatting).
    let sign_negative = n.is_sign_negative();
    let abs = n.abs();

    let mut ryu_buf = ryu::Buffer::new();
    let raw = ryu_buf.format_finite(abs);
    // `ryu` formats `1.0` as `"1.0"`, but ECMAScript `ToString(1)` is `"1"`.
    let raw = raw.strip_suffix(".0").unwrap_or(raw);

    let mut digits_buf = [0u8; 32];
    let (digits, exp) = parse_ryu_to_decimal(raw, &mut digits_buf);
    let k = exp + digits.len() as i32;

    // Output is ASCII and has a small fixed upper bound (< 32 bytes for f64).
    let mut out_buf = [0u8; 64];
    let mut out_len = 0usize;

    if sign_negative {
      out_buf[out_len] = b'-';
      out_len += 1;
    }

    if k > 0 && k <= 21 {
      let k_usize = k as usize;
      if k_usize >= digits.len() {
        push_bytes(&mut out_buf, &mut out_len, digits);
        for _ in 0..(k_usize - digits.len()) {
          push_byte(&mut out_buf, &mut out_len, b'0');
        }
      } else {
        push_bytes(&mut out_buf, &mut out_len, &digits[..k_usize]);
        push_byte(&mut out_buf, &mut out_len, b'.');
        push_bytes(&mut out_buf, &mut out_len, &digits[k_usize..]);
      }

      let s = unsafe { std::str::from_utf8_unchecked(&out_buf[..out_len]) };
      return self.alloc_string(s);
    }

    if k <= 0 && k > -6 {
      push_bytes(&mut out_buf, &mut out_len, b"0.");
      for _ in 0..((-k) as usize) {
        push_byte(&mut out_buf, &mut out_len, b'0');
      }
      push_bytes(&mut out_buf, &mut out_len, digits);

      let s = unsafe { std::str::from_utf8_unchecked(&out_buf[..out_len]) };
      return self.alloc_string(s);
    }

    // Exponential form.
    push_byte(&mut out_buf, &mut out_len, digits[0]);
    if digits.len() > 1 {
      push_byte(&mut out_buf, &mut out_len, b'.');
      push_bytes(&mut out_buf, &mut out_len, &digits[1..]);
    }
    push_byte(&mut out_buf, &mut out_len, b'e');

    let exp = k - 1;
    let mut exp_buf = itoa::Buffer::new();
    if exp >= 0 {
      push_byte(&mut out_buf, &mut out_len, b'+');
      push_bytes(&mut out_buf, &mut out_len, exp_buf.format(exp as u32).as_bytes());
    } else {
      push_byte(&mut out_buf, &mut out_len, b'-');
      push_bytes(&mut out_buf, &mut out_len, exp_buf.format((-exp) as u32).as_bytes());
    }

    let s = unsafe { std::str::from_utf8_unchecked(&out_buf[..out_len]) };
    self.alloc_string(s)
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

    let obj = HeapObject::Symbol(JsSymbol::new(id, description, /* internal */ false));
    Ok(GcSymbol(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates an **engine-internal** JavaScript symbol on the heap.
  ///
  /// Internal symbols are used to model spec internal slots and private names as symbol-keyed
  /// properties while ensuring they remain unreachable and unobservable from JavaScript.
  pub(crate) fn new_internal_symbol(&mut self, description: Option<GcString>) -> Result<GcSymbol, VmError> {
    // Root the description string during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    if let Some(desc) = description {
      scope.push_root(Value::String(desc))?;
    }
 
    let new_bytes = 0;
    scope.heap.ensure_can_allocate(new_bytes)?;
 
    let id = scope.heap.next_symbol_id;
    scope.heap.next_symbol_id = scope.heap.next_symbol_id.wrapping_add(1);
 
    let obj = HeapObject::Symbol(JsSymbol::new(id, description, /* internal */ true));
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

  pub(crate) fn alloc_derived_constructor_state(
    &mut self,
    class_constructor: GcObject,
  ) -> Result<GcObject, VmError> {
    // Root the class constructor across allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    scope.push_root(Value::Object(class_constructor))?;

    let new_bytes = 0;
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::DerivedConstructorState(DerivedConstructorState {
      class_constructor,
      this_value: None,
    });
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates an empty `Map` object on the heap.
  pub fn alloc_map(&mut self) -> Result<GcObject, VmError> {
    self.alloc_map_with_prototype(None)
  }

  /// Allocates an empty `Map` object on the heap with an explicit `[[Prototype]]`.
  pub fn alloc_map_with_prototype(
    &mut self,
    prototype: Option<GcObject>,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    if let Some(proto) = prototype {
      scope.push_root(Value::Object(proto))?;
    }

    let new_bytes = JsMap::heap_size_bytes_for_counts(0, 0);
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Map(JsMap::new(prototype));
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates an empty `Set` object on the heap.
  pub fn alloc_set(&mut self) -> Result<GcObject, VmError> {
    self.alloc_set_with_prototype(None)
  }

  /// Allocates an empty `Set` object on the heap with an explicit `[[Prototype]]`.
  pub fn alloc_set_with_prototype(
    &mut self,
    prototype: Option<GcObject>,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    if let Some(proto) = prototype {
      scope.push_root(Value::Object(proto))?;
    }

    let new_bytes = JsSet::heap_size_bytes_for_counts(0, 0);
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Set(JsSet::new(prototype));
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates an empty `WeakSet` object on the heap.
  pub fn alloc_weak_set(&mut self) -> Result<GcObject, VmError> {
    self.alloc_weak_set_with_prototype(None)
  }

  /// Allocates an empty `WeakSet` object on the heap with an explicit `[[Prototype]]`.
  pub fn alloc_weak_set_with_prototype(
    &mut self,
    prototype: Option<GcObject>,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    if let Some(proto) = prototype {
      scope.push_root(Value::Object(proto))?;
    }

    let new_bytes = JsWeakSet::heap_size_bytes_for_counts(0, 0);
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::WeakSet(JsWeakSet::new(prototype));
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates an empty `WeakMap` object on the heap.
  pub fn alloc_weak_map(&mut self) -> Result<GcObject, VmError> {
    self.alloc_weak_map_with_prototype(None)
  }

  /// Allocates an empty `WeakMap` object on the heap with an explicit `[[Prototype]]`.
  pub fn alloc_weak_map_with_prototype(
    &mut self,
    prototype: Option<GcObject>,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    if let Some(proto) = prototype {
      scope.push_root(Value::Object(proto))?;
    }

    let new_bytes = JsWeakMap::heap_size_bytes_for_counts(0, 0);
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::WeakMap(JsWeakMap::new(prototype));
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates a `WeakRef` object on the heap with an explicit `[[Prototype]]`.
  pub fn alloc_weak_ref_with_prototype(
    &mut self,
    prototype: Option<GcObject>,
    target: Value,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    let mut roots = [Value::Undefined; 2];
    let mut root_count = 0usize;
    if let Some(proto) = prototype {
      roots[root_count] = Value::Object(proto);
      root_count += 1;
    }
    roots[root_count] = target;
    root_count += 1;
    scope.push_roots(&roots[..root_count])?;

    let Some(target_weak) = WeakGcKey::from_value(target, &*scope.heap)? else {
      return Err(VmError::TypeError("WeakRef target cannot be held weakly"));
    };

    let new_bytes = JsWeakRef::heap_size_bytes_for_property_count(0);
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::WeakRef(JsWeakRef::new(prototype, target_weak));
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates a `FinalizationRegistry` object on the heap with an explicit `[[Prototype]]`.
  pub fn alloc_finalization_registry_with_prototype(
    &mut self,
    prototype: Option<GcObject>,
    cleanup_callback: Value,
    realm: Option<RealmId>,
  ) -> Result<GcObject, VmError> {
    debug_assert!(self.heap.debug_value_is_valid_or_primitive(cleanup_callback));

    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    let mut roots = [Value::Undefined; 2];
    let mut root_count = 0usize;
    if let Some(proto) = prototype {
      roots[root_count] = Value::Object(proto);
      root_count += 1;
    }
    roots[root_count] = cleanup_callback;
    root_count += 1;
    scope.push_roots(&roots[..root_count])?;

    let new_bytes = JsFinalizationRegistry::heap_size_bytes_for_counts(0, 0);
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::FinalizationRegistry(JsFinalizationRegistry::new(
      prototype,
      cleanup_callback,
      realm,
    ));
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  /// Allocates a JavaScript Error object on the heap.
  pub fn alloc_error(&mut self) -> Result<GcObject, VmError> {
    let new_bytes = JsObject::heap_size_bytes_for_property_count(0);
    self.heap.ensure_can_allocate(new_bytes)?;

    let obj = JsObject {
      base: ObjectBase {
        prototype: None,
        extensible: true,
        properties: Box::default(),
        kind: ObjectKind::Error,
      },
    };
    Ok(GcObject(self.heap.alloc_unchecked(HeapObject::Object(obj), new_bytes)?))
  }

  /// Allocates a JavaScript Arguments object on the heap.
  pub fn alloc_arguments_object(&mut self) -> Result<GcObject, VmError> {
    let new_bytes = JsObject::heap_size_bytes_for_property_count(0);
    self.heap.ensure_can_allocate(new_bytes)?;

    let obj = JsObject {
      base: ObjectBase {
        prototype: None,
        extensible: true,
        properties: Box::default(),
        kind: ObjectKind::Arguments,
      },
    };
    Ok(GcObject(self.heap.alloc_unchecked(HeapObject::Object(obj), new_bytes)?))
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

  /// Allocates a Module Namespace Exotic Object (ECMA-262 §9.4.6 / §26.3).
  pub(crate) fn alloc_module_namespace_object(
    &mut self,
    exports: Box<[ModuleNamespaceExport]>,
  ) -> Result<GcObject, VmError> {
    // Root all string/object handles stored in `exports` during allocation in case
    // `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    let mut roots: Vec<Value> = Vec::new();
    // Each export stores at least one string key. Some store an additional string or object.
    roots
      .try_reserve_exact(exports.len().saturating_mul(3))
      .map_err(|_| VmError::OutOfMemory)?;
    for export in exports.iter() {
      roots.push(Value::String(export.name));
      roots.push(Value::Object(export.getter));
      match export.value {
        ModuleNamespaceExportValue::Binding { name, .. } => {
          roots.push(Value::String(name));
        }
        ModuleNamespaceExportValue::Namespace { namespace } => {
          roots.push(Value::Object(namespace));
        }
      }
    }
    scope.push_roots(&roots)?;

    // Allocate the namespace object first (as an ordinary, non-extensible object with a null
    // prototype), then allocate and attach the exports table.
    //
    // This avoids storing the exports slice inline in `ObjectKind`, which would inflate the size of
    // every heap slot (important for unit tests that use small heap limits).
    let obj_bytes = JsObject::heap_size_bytes_for_property_count(0);
    scope.heap.ensure_can_allocate(obj_bytes)?;
    let obj = GcObject(scope.heap.alloc_unchecked(
      HeapObject::Object(JsObject {
        base: ObjectBase {
          prototype: None,
          extensible: false,
          properties: Box::default(),
          kind: ObjectKind::Ordinary,
        },
      }),
      obj_bytes,
    )?);

    // Root the object across exports allocation, since the exports heap allocation is not itself a
    // `Value` and cannot be rooted directly.
    scope.push_root(Value::Object(obj))?;

    let exports_bytes = exports
      .len()
      .checked_mul(mem::size_of::<ModuleNamespaceExport>())
      .unwrap_or(usize::MAX);
    scope.heap.ensure_can_allocate(exports_bytes)?;
    let exports = GcModuleNamespaceExports(scope.heap.alloc_unchecked(
      HeapObject::ModuleNamespaceExports(ModuleNamespaceExportsData { exports }),
      exports_bytes,
    )?);

    scope.heap.get_object_base_mut(obj)?.kind =
      ObjectKind::ModuleNamespace(ModuleNamespaceObject { exports });

    Ok(obj)
  }

  /// Replaces the `[[Exports]]` list for an existing Module Namespace Exotic Object.
  ///
  /// This is used by `ModuleGraph::get_module_namespace` to support cyclic namespace graphs, such
  /// as `export * as ns from './self.js'`, without infinite recursion: the namespace object can be
  /// allocated (and cached) before its exports are computed, then populated afterwards.
  pub(crate) fn set_module_namespace_exports(
    &mut self,
    obj: GcObject,
    exports: Box<[ModuleNamespaceExport]>,
  ) -> Result<(), VmError> {
    // Root all values stored in `exports` during allocation in case `ensure_can_allocate` triggers a
    // GC. Also root `obj` across the exports heap allocation since the `ModuleNamespaceExports`
    // object is not itself a `Value` root.
    let mut scope = self.reborrow();
    let mut roots: Vec<Value> = Vec::new();
    roots
      .try_reserve_exact(exports.len().saturating_mul(3).saturating_add(1))
      .map_err(|_| VmError::OutOfMemory)?;
    roots.push(Value::Object(obj));
    for export in exports.iter() {
      roots.push(Value::String(export.name));
      roots.push(Value::Object(export.getter));
      match export.value {
        ModuleNamespaceExportValue::Binding { name, .. } => {
          roots.push(Value::String(name));
        }
        ModuleNamespaceExportValue::Namespace { namespace } => {
          roots.push(Value::Object(namespace));
        }
      }
    }
    scope.push_roots(&roots)?;

    let exports_bytes = exports
      .len()
      .checked_mul(mem::size_of::<ModuleNamespaceExport>())
      .unwrap_or(usize::MAX);
    scope.heap.ensure_can_allocate(exports_bytes)?;
    let exports = GcModuleNamespaceExports(scope.heap.alloc_unchecked(
      HeapObject::ModuleNamespaceExports(ModuleNamespaceExportsData { exports }),
      exports_bytes,
    )?);

    let base = scope.heap.get_object_base_mut(obj)?;
    // Ensure the object keeps the required invariants for Module Namespace Exotic Objects.
    base.prototype = None;
    base.extensible = false;
    base.kind = ObjectKind::ModuleNamespace(ModuleNamespaceObject { exports });

    Ok(())
  }

  /// Allocates a JavaScript array exotic object on the heap.
  ///
  /// The array's `length` internal slot is initialised to `len`.
  ///
  /// Note: `[[Prototype]]` is initialised to `None` and should be set by the caller.
  pub fn alloc_array(&mut self, len: usize) -> Result<GcObject, VmError> {
    let len_u32 = u32::try_from(len).map_err(|_| VmError::RangeError("Invalid array length"))?;

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
        kind: ObjectKind::Array(ArrayObject {
          length: len_u32,
          elements: Vec::new(),
        }),
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
  /// If both `target` and `handler` are `None`, the Proxy is considered **revoked**.
  pub fn alloc_proxy(
    &mut self,
    target: impl Into<Option<GcObject>>,
    handler: impl Into<Option<GcObject>>,
  ) -> Result<GcObject, VmError> {
    let target = target.into();
    let handler = handler.into();
    if target.is_some() != handler.is_some() {
      return Err(VmError::InvariantViolation(
        "Proxy allocation requires both target and handler (or neither for a revoked proxy)",
      ));
    }

    // Validate handles up-front (this should not trigger GC).
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

    // Proxy callability/constructability is fixed at creation time and must be preserved even after
    // the Proxy is revoked.
    let (callable, constructable) = match target {
      Some(target) => (
        self.heap().is_callable(Value::Object(target))?,
        self.heap().is_constructor(Value::Object(target))?,
      ),
      None => (false, false),
    };

    // Root inputs during allocation in case root-stack growth, `ensure_can_allocate`, or slot-table
    // growth triggers GC.
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
    let obj = HeapObject::Proxy(JsProxy {
      target,
      handler,
      callable,
      constructable,
    });
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }
  /// Revokes a Proxy exotic object by clearing its target and handler.
  pub fn revoke_proxy(&mut self, proxy: GcObject) -> Result<(), VmError> {
    self.heap.proxy_revoke(proxy)
  }

  /// Allocates a new `RegExp` object.
  ///
  /// Note: `[[Prototype]]` is initialised to `None` and should be set by the caller (typically via
  /// `OrdinaryCreateFromConstructor`).
  pub fn alloc_regexp(
    &mut self,
    original_source: GcString,
    original_flags: GcString,
    flags: RegExpFlags,
    program: RegExpProgram,
  ) -> Result<GcObject, VmError> {
    // Root inputs during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    scope.push_root(Value::String(original_source))?;
    scope.push_root(Value::String(original_flags))?;

    let last_index_key_s = scope.alloc_string("lastIndex")?;
    scope.push_root(Value::String(last_index_key_s))?;
    let last_index_key = PropertyKey::from_string(last_index_key_s);

    let mut props: Vec<PropertyEntry> = Vec::new();
    props.try_reserve_exact(1).map_err(|_| VmError::OutOfMemory)?;
    props.push(PropertyEntry {
      key: last_index_key,
      desc: PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Number(0.0),
          writable: true,
        },
      },
    });
    let properties = props.into_boxed_slice();

    let new_bytes = JsRegExp::heap_size_bytes_for_program(properties.len(), &program);
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::RegExp(JsRegExp {
      base: ObjectBase {
        prototype: None,
        extensible: true,
        properties,
        kind: ObjectKind::Ordinary,
      },
      original_source,
      original_flags,
      flags,
      program,
    });
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
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

    let obj = HeapObject::ArrayBuffer(JsArrayBuffer::new(
      None,
      data,
      byte_length,
      false,
      false,
    ));
    let id = self.heap.alloc_unchecked_after_ensure(obj, new_bytes)?;
    self.heap.add_external_bytes(byte_length);
    Ok(GcObject(id))
  }

  /// Allocates a new resizable `ArrayBuffer` with a fixed initial length and a maximum byte length.
  ///
  /// Note: `[[Prototype]]` is initialised to `None` and should be set by the caller.
  pub fn alloc_resizable_array_buffer(
    &mut self,
    byte_length: usize,
    max_byte_length: usize,
  ) -> Result<GcObject, VmError> {
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

    let obj = HeapObject::ArrayBuffer(JsArrayBuffer::new(
      None,
      data,
      max_byte_length,
      true,
      false,
    ));
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

    let obj = HeapObject::ArrayBuffer(JsArrayBuffer::new(
      None,
      data,
      byte_length,
      false,
      false,
    ));
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

    let buf = scope.heap.get_array_buffer(viewed_array_buffer)?;
    if buf.data.is_none() {
      return Err(VmError::TypeError("DataView view over detached ArrayBuffer"));
    }
    let buf_len = buf.byte_length();
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

  pub(crate) fn alloc_generator_with_prototype(
    &mut self,
    prototype: Option<GcObject>,
    state: GeneratorState,
    continuation: Option<Box<GeneratorContinuation>>,
  ) -> Result<GcObject, VmError> {
    // Root the prototype during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    if let Some(proto) = prototype {
      scope.push_root(Value::Object(proto))?;
    }

    let gen = JsGenerator::new(prototype, state, continuation);
    let new_bytes = gen.heap_size_bytes();
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Generator(gen);
    Ok(GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?))
  }

  pub(crate) fn alloc_async_generator_with_prototype(
    &mut self,
    prototype: Option<GcObject>,
    state: AsyncGeneratorState,
    continuation: Option<Box<AsyncGeneratorContinuation>>,
    request_queue: VecDeque<AsyncGeneratorRequest>,
  ) -> Result<GcObject, VmError> {
    // Root the prototype during allocation in case `ensure_can_allocate` triggers a GC.
    let mut scope = self.reborrow();
    if let Some(proto) = prototype {
      scope.push_root(Value::Object(proto))?;
    }

    let gen = JsAsyncGenerator::new(prototype, state, continuation, request_queue);
    let new_bytes = gen.heap_size_bytes();
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::AsyncGenerator(gen);
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
    self.alloc_native_function_with_slots_and_env_impl(
      call,
      construct,
      name,
      length,
      slots,
      closure_env,
      true,
    )
  }

  /// Like [`Scope::alloc_native_function_with_slots_and_env`], but does **not** create a
  /// constructor `.prototype` property.
  ///
  /// This exists for spec-accurate intrinsics like `%Proxy%`, which is constructible but does not
  /// have an own `"prototype"` property (test262: `built-ins/Proxy/proxy-no-prototype.js`).
  pub(crate) fn alloc_native_function_with_slots_and_env_no_constructor_prototype(
    &mut self,
    call: NativeFunctionId,
    construct: Option<NativeConstructId>,
    name: GcString,
    length: u32,
    slots: &[Value],
    closure_env: Option<GcEnv>,
  ) -> Result<GcObject, VmError> {
    self.alloc_native_function_with_slots_and_env_impl(
      call,
      construct,
      name,
      length,
      slots,
      closure_env,
      false,
    )
  }

  fn alloc_native_function_with_slots_and_env_impl(
    &mut self,
    call: NativeFunctionId,
    construct: Option<NativeConstructId>,
    name: GcString,
    length: u32,
    slots: &[Value],
    closure_env: Option<GcEnv>,
    make_constructor_prototype: bool,
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
    // Root the newly-allocated function object while defining its standard properties.
    //
    // `set_function_name` / `set_function_length` / `make_constructor` can allocate (and therefore
    // trigger GC). Without rooting, the fresh function object is only held in a local Rust
    // variable and can be collected before this method returns, producing an invalid handle.
    scope.push_root(Value::Object(func))?;

    // Define standard function properties.
    //
    // Per ECMA-262 `CreateBuiltinFunction`, the order of property creation is observable via
    // `Object.getOwnPropertyNames` and must be: `"length"` then `"name"` (test262 relies on this).
    crate::function_properties::set_function_length(&mut scope, func, length)?;
    crate::function_properties::set_function_name(
      &mut scope,
      func,
      PropertyKey::String(name),
      None,
    )?;

    // Constructors get a `.prototype` object.
    if make_constructor_prototype && construct.is_some() {
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
      /* is_constructable */ true,
      name,
      length,
      ThisMode::Global,
      /* is_strict */ false,
      /* closure_env */ None,
    );
    let is_constructable = func.construct.is_some();
    let new_bytes = func.heap_size_bytes();
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Function(func);
    let func = GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?);

    // Root the newly-allocated function object while defining its standard properties.
    //
    // `set_function_name` / `set_function_length` / `make_constructor` can allocate (and therefore
    // trigger GC). Without rooting, the fresh function object is only held in a local Rust
    // variable and can be collected before this method returns, producing an invalid handle.
    scope.push_root(Value::Object(func))?;

    // Define standard function metadata properties (`name`, `length`).
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

  pub(crate) fn alloc_user_function_with_env(
    &mut self,
    func: CompiledFunctionRef,
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

    let func = JsFunction::new_user(
      func,
      is_constructable,
      name,
      length,
      this_mode,
      is_strict,
      closure_env,
    );
    let is_constructable = func.construct.is_some();
    let new_bytes = func.heap_size_bytes();
    scope.heap.ensure_can_allocate(new_bytes)?;

    let obj = HeapObject::Function(func);
    let func = GcObject(scope.heap.alloc_unchecked(obj, new_bytes)?);

    // Root the newly-allocated function object while defining its standard properties.
    //
    // `set_function_name` / `set_function_length` / `make_constructor` can allocate (and therefore
    // trigger GC). Without rooting, the fresh function object is only held in a local Rust
    // variable and can be collected before this method returns, producing an invalid handle.
    scope.push_root(Value::Object(func))?;

    // Define standard function metadata properties (`name`, `length`).
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
    //
    // `target` may be a callable/constructable Proxy, so unwrap Proxy chains until we reach the
    // underlying function object for handler metadata, but preserve the original `target` object as
    // `[[BoundTargetFunction]]`.
    let (target_call, target_construct) = {
      let mut obj = target;
      let mut remaining = MAX_PROTOTYPE_CHAIN;
      loop {
        if remaining == 0 {
          return Err(VmError::NotCallable);
        }
        remaining -= 1;
        match self.heap().get_heap_object(obj.0)? {
          HeapObject::Function(f) => break (f.call.clone(), f.construct.clone()),
          HeapObject::Proxy(p) => {
            let (Some(next), Some(_handler)) = (p.target, p.handler) else {
              return Err(VmError::TypeError(
                "Cannot create bound function for a proxy that has been revoked",
              ));
            };
            obj = next;
          }
          _ => return Err(VmError::NotCallable),
        }
      }
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

/// Shared state for a derived class constructor's `this` binding.
///
/// Derived class constructors have an uninitialized `this` binding until `super()` returns. That
/// initialization must be observable by nested arrow functions and direct eval code.
///
/// `vm-js` represents that shared state as a heap object so it can be:
/// - captured by value by arrow functions (`[[ThisMode]] = Lexical`), and
/// - passed into nested direct eval evaluators,
/// while still allowing `super()` to initialize the enclosing constructor's `this` exactly once.
#[derive(Debug)]
pub(crate) struct DerivedConstructorState {
  /// The containing class constructor wrapper function object (the native wrapper holding field
  /// metadata and the hidden `extends` slot).
  pub(crate) class_constructor: GcObject,
  /// The initialized `this` value produced by `super()`, if it has been called.
  pub(crate) this_value: Option<GcObject>,
}

#[derive(Debug)]
enum HeapObject {
  String(JsString),
  Symbol(JsSymbol),
  BigInt(JsBigInt),
  Object(JsObject),
  DerivedConstructorState(DerivedConstructorState),
  ModuleNamespaceExports(ModuleNamespaceExportsData),
  ArrayBuffer(JsArrayBuffer),
  TypedArray(JsTypedArray),
  DataView(JsDataView),
  Function(JsFunction),
  Proxy(JsProxy),
  RegExp(JsRegExp),
  Env(EnvRecord),
  Promise(JsPromise),
  Map(JsMap),
  Set(JsSet),
  WeakRef(JsWeakRef),
  WeakMap(JsWeakMap),
  WeakSet(JsWeakSet),
  FinalizationRegistry(JsFinalizationRegistry),
  Generator(JsGenerator),
  AsyncGenerator(JsAsyncGenerator),
}

impl Trace for HeapObject {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    match self {
      HeapObject::String(s) => s.trace(tracer),
      HeapObject::Symbol(s) => s.trace(tracer),
      HeapObject::BigInt(b) => b.trace(tracer),
      HeapObject::Object(o) => o.trace(tracer),
      HeapObject::DerivedConstructorState(s) => s.trace(tracer),
      HeapObject::ModuleNamespaceExports(ns) => ns.trace(tracer),
      HeapObject::ArrayBuffer(b) => b.trace(tracer),
      HeapObject::TypedArray(a) => a.trace(tracer),
      HeapObject::DataView(v) => v.trace(tracer),
      HeapObject::Function(f) => f.trace(tracer),
      HeapObject::Proxy(p) => p.trace(tracer),
      HeapObject::RegExp(r) => r.trace(tracer),
      HeapObject::Env(e) => e.trace(tracer),
      HeapObject::Promise(p) => p.trace(tracer),
      HeapObject::Map(m) => m.trace(tracer),
      HeapObject::Set(s) => s.trace(tracer),
      HeapObject::WeakRef(wr) => wr.trace(tracer),
      HeapObject::WeakMap(wm) => wm.trace(tracer),
      HeapObject::WeakSet(ws) => ws.trace(tracer),
      HeapObject::FinalizationRegistry(fr) => fr.trace(tracer),
      HeapObject::Generator(g) => g.trace(tracer),
      HeapObject::AsyncGenerator(g) => g.trace(tracer),
    }
  }
}

impl Trace for DerivedConstructorState {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    tracer.trace_value(Value::Object(self.class_constructor));
    if let Some(this_obj) = self.this_value {
      tracer.trace_value(Value::Object(this_obj));
    }
  }
}

impl HeapObject {
  #[cfg(any(debug_assertions, feature = "gc_validate"))]
  fn debug_kind(&self) -> &'static str {
    match self {
      HeapObject::String(_) => "String",
      HeapObject::Symbol(_) => "Symbol",
      HeapObject::BigInt(_) => "BigInt",
      HeapObject::Object(_) => "Object",
      HeapObject::DerivedConstructorState(_) => "DerivedConstructorState",
      HeapObject::ModuleNamespaceExports(_) => "ModuleNamespaceExports",
      HeapObject::ArrayBuffer(_) => "ArrayBuffer",
      HeapObject::TypedArray(_) => "TypedArray",
      HeapObject::DataView(_) => "DataView",
      HeapObject::Function(_) => "Function",
      HeapObject::Proxy(_) => "Proxy",
      HeapObject::RegExp(_) => "RegExp",
      HeapObject::Env(_) => "Env",
      HeapObject::Promise(_) => "Promise",
      HeapObject::Map(_) => "Map",
      HeapObject::Set(_) => "Set",
      HeapObject::WeakRef(_) => "WeakRef",
      HeapObject::WeakMap(_) => "WeakMap",
      HeapObject::WeakSet(_) => "WeakSet",
      HeapObject::FinalizationRegistry(_) => "FinalizationRegistry",
      HeapObject::Generator(_) => "Generator",
      HeapObject::AsyncGenerator(_) => "AsyncGenerator",
    }
  }

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

#[cfg(any(debug_assertions, feature = "gc_validate"))]
fn debug_expected_is_object(obj: &HeapObject) -> bool {
  matches!(
    obj,
    HeapObject::Object(_)
      | HeapObject::ArrayBuffer(_)
      | HeapObject::TypedArray(_)
      | HeapObject::DataView(_)
      | HeapObject::Function(_)
      | HeapObject::Proxy(_)
      | HeapObject::RegExp(_)
      | HeapObject::Promise(_)
      | HeapObject::Map(_)
      | HeapObject::Set(_)
      | HeapObject::WeakRef(_)
      | HeapObject::WeakMap(_)
      | HeapObject::WeakSet(_)
      | HeapObject::FinalizationRegistry(_)
      | HeapObject::Generator(_)
      | HeapObject::AsyncGenerator(_)
  )
}

#[cfg(any(debug_assertions, feature = "gc_validate"))]
fn debug_expected_is_array_buffer(obj: &HeapObject) -> bool {
  matches!(obj, HeapObject::ArrayBuffer(_))
}

#[cfg(any(debug_assertions, feature = "gc_validate"))]
fn debug_expected_is_string(obj: &HeapObject) -> bool {
  matches!(obj, HeapObject::String(_))
}

#[cfg(any(debug_assertions, feature = "gc_validate"))]
fn debug_expected_is_symbol(obj: &HeapObject) -> bool {
  matches!(obj, HeapObject::Symbol(_))
}

#[cfg(any(debug_assertions, feature = "gc_validate"))]
fn debug_expected_is_bigint(obj: &HeapObject) -> bool {
  matches!(obj, HeapObject::BigInt(_))
}

#[cfg(any(debug_assertions, feature = "gc_validate"))]
fn debug_expected_is_env(obj: &HeapObject) -> bool {
  matches!(obj, HeapObject::Env(_))
}

#[cfg(any(debug_assertions, feature = "gc_validate"))]
fn debug_expected_is_module_namespace_exports(obj: &HeapObject) -> bool {
  matches!(obj, HeapObject::ModuleNamespaceExports(_))
}

impl Trace for JsString {
  fn trace(&self, _tracer: &mut Tracer<'_>) {
    // Strings have no outgoing GC references.
  }
}

impl Trace for JsBigInt {
  fn trace(&self, _tracer: &mut Tracer<'_>) {
    // BigInts have no outgoing GC references.
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
  /// Whether this Proxy has an ECMAScript `[[Call]]` internal method.
  ///
  /// Per spec, callability is determined when the Proxy is created and is preserved even if the
  /// Proxy is later revoked (`[[ProxyTarget]]`/`[[ProxyHandler]]` set to `null`).
  callable: bool,
  /// Whether this Proxy has an ECMAScript `[[Construct]]` internal method.
  ///
  /// Like `callable`, this is fixed at creation time and preserved across revocation.
  constructable: bool,
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
      ObjectKind::Ordinary
      | ObjectKind::Date(_)
      | ObjectKind::Error
      | ObjectKind::Arguments
      | ObjectKind::ModuleNamespace(_) => None,
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
    match &self.kind {
      ObjectKind::Ordinary | ObjectKind::Date(_) | ObjectKind::Error | ObjectKind::Arguments => {}
      ObjectKind::Array(a) => {
        for elem in a.elements.iter().flatten() {
          elem.trace(tracer);
        }
      }
      ObjectKind::ModuleNamespace(ns) => ns.trace(tracer),
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
  // `None` represents a detached (neutered) ArrayBuffer.
  data: Option<Box<[u8]>>,
  /// Maximum byte length for resizable ArrayBuffers.
  ///
  /// Note: `ArrayBuffer.prototype.maxByteLength` returns `0` when the buffer is detached, so this
  /// value is not directly observable after detachment.
  max_byte_length: usize,
  /// Whether this buffer was constructed with a `maxByteLength` option.
  ///
  /// Per ECMA-262, this is observable even when the buffer is detached.
  resizable: bool,
  /// Whether this buffer is immutable (ImmutableArrayBuffer proposal).
  immutable: bool,
}

impl JsArrayBuffer {
  fn new(
    prototype: Option<GcObject>,
    data: Box<[u8]>,
    max_byte_length: usize,
    resizable: bool,
    immutable: bool,
  ) -> Self {
    Self {
      base: ObjectBase::new(prototype),
      data: Some(data),
      max_byte_length,
      resizable,
      immutable,
    }
  }

  fn new_detached(
    prototype: Option<GcObject>,
    max_byte_length: usize,
    resizable: bool,
    immutable: bool,
  ) -> Self {
    Self {
      base: ObjectBase::new(prototype),
      data: None,
      max_byte_length,
      resizable,
      immutable,
    }
  }

  fn byte_length(&self) -> usize {
    // Detached ArrayBuffers have a `byteLength` of 0.
    self.data.as_deref().map(|data| data.len()).unwrap_or(0)
  }

  fn max_byte_length(&self) -> usize {
    self.max_byte_length
  }

  fn is_resizable(&self) -> bool {
    self.resizable
  }

  fn is_immutable(&self) -> bool {
    self.immutable
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
pub enum TypedArrayKind {
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
  pub fn bytes_per_element(self) -> usize {
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
struct JsWeakRef {
  base: ObjectBase,
  target: WeakGcKey,
}

impl JsWeakRef {
  fn new(prototype: Option<GcObject>, target: WeakGcKey) -> Self {
    Self {
      base: ObjectBase::new(prototype),
      target,
    }
  }

  fn heap_size_bytes_for_property_count(property_count: usize) -> usize {
    ObjectBase::properties_heap_size_bytes_for_count(property_count)
  }
}

impl Trace for JsWeakRef {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    // WeakRef targets are weak: do not trace `target`.
    self.base.trace(tracer);
  }
}

#[derive(Debug, Clone, Copy)]
struct FinalizationRegistryCell {
  /// Weak reference to the target value (Object or Symbol). Cleared to `None` when the target is
  /// collected.
  target: Option<WeakGcKey>,
  /// Strongly-held value passed to the cleanup callback.
  held_value: Value,
  /// Optional weak unregister token.
  unregister_token: Option<WeakGcKey>,
}

#[derive(Debug)]
struct JsFinalizationRegistry {
  base: ObjectBase,
  cleanup_callback: Value,
  cells: Vec<FinalizationRegistryCell>,
  realm: Option<RealmId>,
  cleanup_pending: bool,
}

impl JsFinalizationRegistry {
  fn new(prototype: Option<GcObject>, cleanup_callback: Value, realm: Option<RealmId>) -> Self {
    Self {
      base: ObjectBase::new(prototype),
      cleanup_callback,
      cells: Vec::new(),
      realm,
      cleanup_pending: false,
    }
  }

  fn heap_size_bytes(&self) -> usize {
    Self::heap_size_bytes_for_counts(self.base.properties.len(), self.cells.capacity())
  }

  fn heap_size_bytes_for_counts(property_count: usize, cell_capacity: usize) -> usize {
    let props_bytes = ObjectBase::properties_heap_size_bytes_for_count(property_count);
    let cell_bytes = cell_capacity
      .checked_mul(mem::size_of::<FinalizationRegistryCell>())
      .unwrap_or(usize::MAX);
    props_bytes.saturating_add(cell_bytes)
  }
}

impl Trace for JsFinalizationRegistry {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.base.trace(tracer);
    tracer.trace_value(self.cleanup_callback);
    // Cells keep held values alive, but targets and unregister tokens are weak.
    for cell in self.cells.iter() {
      tracer.trace_value(cell.held_value);
    }
  }
}

#[derive(Debug, Clone, Copy)]
struct MapEntry {
  key: Option<Value>,
  value: Option<Value>,
}

#[derive(Debug)]
struct JsMap {
  base: ObjectBase,
  entries: Vec<MapEntry>,
  /// Count of live entries (where `key` is not empty).
  size: usize,
}

impl JsMap {
  fn new(prototype: Option<GcObject>) -> Self {
    Self {
      base: ObjectBase::new(prototype),
      entries: Vec::new(),
      size: 0,
    }
  }

  fn heap_size_bytes(&self) -> usize {
    Self::heap_size_bytes_for_counts(self.base.properties.len(), self.entries.capacity())
  }

  fn heap_size_bytes_for_counts(property_count: usize, entry_capacity: usize) -> usize {
    let props_bytes = ObjectBase::properties_heap_size_bytes_for_count(property_count);
    let entries_bytes = entry_capacity
      .checked_mul(mem::size_of::<MapEntry>())
      .unwrap_or(usize::MAX);
    props_bytes.saturating_add(entries_bytes)
  }
}

impl Trace for JsMap {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.base.trace(tracer);
    for entry in self.entries.iter() {
      let Some(k) = entry.key else {
        continue;
      };
      tracer.trace_value(k);
      if let Some(v) = entry.value {
        tracer.trace_value(v);
      }
    }
  }
}

#[derive(Debug)]
struct JsSet {
  base: ObjectBase,
  entries: Vec<Option<Value>>,
  /// Count of live entries (where entry is not empty).
  size: usize,
}

impl JsSet {
  fn new(prototype: Option<GcObject>) -> Self {
    Self {
      base: ObjectBase::new(prototype),
      entries: Vec::new(),
      size: 0,
    }
  }

  fn heap_size_bytes(&self) -> usize {
    Self::heap_size_bytes_for_counts(self.base.properties.len(), self.entries.capacity())
  }

  fn heap_size_bytes_for_counts(property_count: usize, entry_capacity: usize) -> usize {
    let props_bytes = ObjectBase::properties_heap_size_bytes_for_count(property_count);
    let entries_bytes = entry_capacity
      .checked_mul(mem::size_of::<Option<Value>>())
      .unwrap_or(usize::MAX);
    props_bytes.saturating_add(entries_bytes)
  }
}

impl Trace for JsSet {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.base.trace(tracer);
    for entry in self.entries.iter() {
      if let Some(v) = *entry {
        tracer.trace_value(v);
      }
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum WeakGcKey {
  Object(WeakGcObject),
  Symbol(WeakGcSymbol),
}

impl WeakGcKey {
  fn from_value(value: Value, heap: &Heap) -> Result<Option<Self>, VmError> {
    Ok(match value {
      Value::Object(obj) => heap.is_valid_object(obj).then_some(Self::Object(WeakGcObject::from(obj))),
      Value::Symbol(sym) => heap.is_valid_symbol(sym).then_some(Self::Symbol(WeakGcSymbol::from(sym))),
      _ => None,
    })
  }

  fn id(self) -> HeapId {
    match self {
      Self::Object(obj) => obj.id(),
      Self::Symbol(sym) => sym.id(),
    }
  }

  fn upgrade_value(self, heap: &Heap) -> Option<Value> {
    match self {
      Self::Object(obj) => obj.upgrade(heap).map(Value::Object),
      Self::Symbol(sym) => sym.upgrade(heap).map(Value::Symbol),
    }
  }
}

#[derive(Debug, Clone, Copy)]
struct WeakMapEntry {
  key: WeakGcKey,
  value: Value,
}

#[derive(Debug)]
struct JsWeakMap {
  base: ObjectBase,
  entries: Vec<WeakMapEntry>,
}

impl JsWeakMap {
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
      .checked_mul(mem::size_of::<WeakMapEntry>())
      .unwrap_or(usize::MAX);
    props_bytes.saturating_add(entries_bytes)
  }
}

impl Trace for JsWeakMap {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    // WeakMap keys are weak: do not trace `entries`.
    // WeakMap values are traced during GC via ephemeron processing when their keys are live.
    self.base.trace(tracer);
  }
}

#[derive(Debug)]
struct JsWeakSet {
  base: ObjectBase,
  entries: Vec<WeakGcKey>,
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
      .checked_mul(mem::size_of::<WeakGcKey>())
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
struct JsRegExp {
  base: ObjectBase,
  original_source: GcString,
  original_flags: GcString,
  flags: RegExpFlags,
  program: RegExpProgram,
}

impl JsRegExp {
  fn heap_size_bytes_for_program(property_count: usize, program: &RegExpProgram) -> usize {
    ObjectBase::properties_heap_size_bytes_for_count(property_count)
      .saturating_add(program.heap_size_bytes())
  }
}

impl Trace for JsRegExp {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.base.trace(tracer);
    tracer.trace_value(Value::String(self.original_source));
    tracer.trace_value(Value::String(self.original_flags));
  }
}

#[derive(Debug)]
pub(crate) struct GeneratorContinuation {
  pub(crate) env: RuntimeEnv,
  pub(crate) strict: bool,
  pub(crate) this: Value,
  pub(crate) new_target: Value,
  pub(crate) home_object: Option<GcObject>,
  pub(crate) func: Arc<Node<Func>>,
  pub(crate) args: Box<[Value]>,
  pub(crate) frames: VecDeque<GenFrame>,
}

impl GeneratorContinuation {
  fn heap_size_bytes(&self) -> usize {
    let args_bytes = self
      .args
      .len()
      .checked_mul(mem::size_of::<Value>())
      .unwrap_or(usize::MAX);
    let frames_bytes = self
      .frames
      .capacity()
      .checked_mul(mem::size_of::<GenFrame>())
      .unwrap_or(usize::MAX);

    mem::size_of::<Self>()
      .checked_add(args_bytes)
      .and_then(|b| b.checked_add(frames_bytes))
      .unwrap_or(usize::MAX)
  }
}

impl Trace for GeneratorContinuation {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.env.trace(tracer);
    tracer.trace_value(self.this);
    tracer.trace_value(self.new_target);
    if let Some(home_object) = self.home_object {
      tracer.trace_value(Value::Object(home_object));
    }
    for v in self.args.iter().copied() {
      tracer.trace_value(v);
    }
    for frame in self.frames.iter() {
      frame.trace(tracer);
    }
  }
}

#[derive(Debug)]
pub(crate) struct AsyncGeneratorContinuation {
  pub(crate) env: RuntimeEnv,
  pub(crate) strict: bool,
  pub(crate) this: Value,
  pub(crate) new_target: Value,
  pub(crate) home_object: Option<GcObject>,
  pub(crate) func: Arc<Node<Func>>,
  pub(crate) args: Box<[Value]>,
}

impl AsyncGeneratorContinuation {
  fn heap_size_bytes(&self) -> usize {
    let args_bytes = self
      .args
      .len()
      .checked_mul(mem::size_of::<Value>())
      .unwrap_or(usize::MAX);

    mem::size_of::<Self>()
      .checked_add(args_bytes)
      .unwrap_or(usize::MAX)
  }
}

impl Trace for AsyncGeneratorContinuation {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.env.trace(tracer);
    tracer.trace_value(self.this);
    tracer.trace_value(self.new_target);
    if let Some(home_object) = self.home_object {
      tracer.trace_value(Value::Object(home_object));
    }
    for v in self.args.iter().copied() {
      tracer.trace_value(v);
    }
  }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum AsyncGeneratorRequestKind {
  Next(Value),
  Return(Value),
  Throw(Value),
}

impl Trace for AsyncGeneratorRequestKind {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    match self {
      AsyncGeneratorRequestKind::Next(v)
      | AsyncGeneratorRequestKind::Return(v)
      | AsyncGeneratorRequestKind::Throw(v) => tracer.trace_value(*v),
    }
  }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AsyncGeneratorRequest {
  pub(crate) kind: AsyncGeneratorRequestKind,
  pub(crate) capability: PromiseCapability,
}

impl Trace for AsyncGeneratorRequest {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.kind.trace(tracer);
    self.capability.trace(tracer);
  }
}

#[derive(Debug)]
struct JsGenerator {
  object: JsObject,
  state: GeneratorState,
  continuation: Option<Box<GeneratorContinuation>>,
}

impl JsGenerator {
  fn new(
    prototype: Option<GcObject>,
    state: GeneratorState,
    continuation: Option<Box<GeneratorContinuation>>,
  ) -> Self {
    Self {
      object: JsObject::new(prototype),
      state,
      continuation,
    }
  }

  fn heap_size_bytes(&self) -> usize {
    let props_bytes = ObjectBase::properties_heap_size_bytes_for_count(self.object.base.property_count());
    let cont_bytes = self
      .continuation
      .as_ref()
      .map(|c| c.heap_size_bytes())
      .unwrap_or(0);
    props_bytes
      .checked_add(cont_bytes)
      .unwrap_or(usize::MAX)
  }
}

impl Trace for JsGenerator {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.object.trace(tracer);
    if let Some(cont) = &self.continuation {
      cont.trace(tracer);
    }
  }
}

#[derive(Debug)]
struct JsAsyncGenerator {
  object: JsObject,
  state: AsyncGeneratorState,
  continuation: Option<Box<AsyncGeneratorContinuation>>,
  request_queue: VecDeque<AsyncGeneratorRequest>,
}

impl JsAsyncGenerator {
  fn new(
    prototype: Option<GcObject>,
    state: AsyncGeneratorState,
    continuation: Option<Box<AsyncGeneratorContinuation>>,
    request_queue: VecDeque<AsyncGeneratorRequest>,
  ) -> Self {
    Self {
      object: JsObject::new(prototype),
      state,
      continuation,
      request_queue,
    }
  }

  fn heap_size_bytes(&self) -> usize {
    let props_bytes = ObjectBase::properties_heap_size_bytes_for_count(
      self.object.base.property_count(),
    );
    let cont_bytes = self
      .continuation
      .as_ref()
      .map(|c| c.heap_size_bytes())
      .unwrap_or(0);
    let queue_bytes = self
      .request_queue
      .capacity()
      .checked_mul(mem::size_of::<AsyncGeneratorRequest>())
      .unwrap_or(usize::MAX);

    props_bytes
      .checked_add(cont_bytes)
      .and_then(|b| b.checked_add(queue_bytes))
      .unwrap_or(usize::MAX)
  }
}

impl Trace for JsAsyncGenerator {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    self.object.trace(tracer);
    if let Some(cont) = &self.continuation {
      cont.trace(tracer);
    }
    for req in self.request_queue.iter() {
      req.trace(tracer);
    }
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
  Error,
  Arguments,
  ModuleNamespace(ModuleNamespaceObject),
}

#[derive(Debug)]
struct ArrayObject {
  length: u32,
  /// Fast storage for array-indexed own properties.
  ///
  /// Array index properties (`"0"`, `"1"`, ...) are extremely common and can appear in tight loops.
  /// Storing them separately avoids `O(N^2)` behaviour from repeatedly reallocating and scanning
  /// the ordinary property table.
  ///
  /// Indices greater than `MAX_FAST_ARRAY_INDEX` are stored in the ordinary property table.
  elements: Vec<Option<PropertyDescriptor>>,
}

#[derive(Debug)]
struct DateObject {
  value: f64,
}

/// A Module Namespace Exotic Object's internal slots (ECMA-262 §9.4.6 / §26.3).
#[derive(Debug)]
pub(crate) struct ModuleNamespaceObject {
  exports: GcModuleNamespaceExports,
}

/// Backing storage for a Module Namespace object's `[[Exports]]` list.
///
/// This is stored as a separate heap allocation (referenced by [`GcModuleNamespaceExports`]) so the
/// `JsObject` header stays small and does not inflate the size of every heap slot.
#[derive(Debug)]
struct ModuleNamespaceExportsData {
  exports: Box<[ModuleNamespaceExport]>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ModuleNamespaceExport {
  /// The exported name (property key).
  pub(crate) name: GcString,
  /// Getter function object used by `[[GetOwnProperty]]`.
  pub(crate) getter: GcObject,
  pub(crate) value: ModuleNamespaceExportValue,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ModuleNamespaceExportValue {
  /// A live binding backed by a module environment record.
  Binding { env: GcEnv, name: GcString },
  /// A re-exported namespace binding.
  Namespace { namespace: GcObject },
}

impl Trace for ModuleNamespaceObject {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    tracer.trace_heap_id(self.exports.0);
  }
}

impl Trace for ModuleNamespaceExportsData {
  fn trace(&self, tracer: &mut Tracer<'_>) {
    for export in self.exports.iter() {
      tracer.trace_value(Value::String(export.name));
      tracer.trace_value(Value::Object(export.getter));
      match export.value {
        ModuleNamespaceExportValue::Binding { env, name } => {
          tracer.trace_env(env);
          tracer.trace_value(Value::String(name));
        }
        ModuleNamespaceExportValue::Namespace { namespace } => {
          tracer.trace_value(Value::Object(namespace));
        }
      }
    }
  }
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
      Value::Undefined | Value::Null | Value::Bool(_) | Value::Number(_) => {}
      Value::BigInt(b) => self.trace_heap_id(b.0),
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
      // `Tracer::validate` is on the hot path; keep the default release behaviour (ignore invalid
      // edges) unless debug assertions are enabled or `gc_validate` is explicitly requested.
      if cfg!(any(debug_assertions, feature = "gc_validate")) {
        panic!("stale handle during GC: {id:?}");
      }
      return None;
    }
    if slot.value.is_none() {
      if cfg!(any(debug_assertions, feature = "gc_validate")) {
        panic!("handle points at a free slot during GC: {id:?}");
      }
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
  // Heap metadata vectors (slot table, root stacks, etc) can contain very large elements (e.g. the
  // slot table stores `HeapObject` inline), so "double on grow" can dramatically over-allocate and
  // cause premature `OutOfMemory` under small heap limits.
  //
  // Use a more conservative growth strategy: grow by ~1.5x (plus a minimum of 1 element), which
  // keeps amortized O(1) push behaviour while avoiding huge capacity cliffs.
  let mut cap = current_capacity.max(MIN_VEC_CAPACITY);
  while cap < required_len {
    let grow = (cap / 2).max(1);
    cap = match cap.checked_add(grow) {
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

fn reserve_vec_deque_to_len<T>(deque: &mut VecDeque<T>, required_len: usize) -> Result<(), VmError> {
  if required_len <= deque.capacity() {
    return Ok(());
  }
  let desired_capacity = grown_capacity(deque.capacity(), required_len);
  if desired_capacity == usize::MAX {
    return Err(VmError::OutOfMemory);
  }
  let additional = desired_capacity
    .checked_sub(deque.len())
    .ok_or(VmError::OutOfMemory)?;
  deque
    .try_reserve_exact(additional)
    .map_err(|_| VmError::OutOfMemory)?;
  Ok(())
}

fn push_byte(buf: &mut [u8; 64], len: &mut usize, b: u8) {
  debug_assert!(*len < buf.len());
  buf[*len] = b;
  *len += 1;
}

fn push_bytes(buf: &mut [u8; 64], len: &mut usize, bytes: &[u8]) {
  debug_assert!(buf.len().saturating_sub(*len) >= bytes.len());
  let end = *len + bytes.len();
  buf[*len..end].copy_from_slice(bytes);
  *len = end;
}

fn number_to_string_ascii_bytes<'a>(n: f64, out_buf: &'a mut [u8; 64]) -> &'a [u8] {
  // https://tc39.es/ecma262/multipage/ecmascript-data-types-and-values.html#sec-numeric-types-number-tostring
  if n.is_nan() {
    out_buf[..3].copy_from_slice(b"NaN");
    return &out_buf[..3];
  }
  if n == 0.0 {
    // Covers both +0 and -0.
    out_buf[0] = b'0';
    return &out_buf[..1];
  }
  if n.is_infinite() {
    if n.is_sign_negative() {
      out_buf[..9].copy_from_slice(b"-Infinity");
      return &out_buf[..9];
    }
    out_buf[..8].copy_from_slice(b"Infinity");
    return &out_buf[..8];
  }

  // `ryu` is used only for digit/exponent decomposition; the final formatting rules match
  // ECMAScript `Number::toString()` (not Rust's float formatting).
  let sign_negative = n.is_sign_negative();
  let abs = n.abs();

  let mut ryu_buf = ryu::Buffer::new();
  let raw = ryu_buf.format_finite(abs);
  // `ryu` formats `1.0` as `"1.0"`, but ECMAScript `ToString(1)` is `"1"`.
  let raw = raw.strip_suffix(".0").unwrap_or(raw);

  let mut digits_buf = [0u8; 32];
  let (digits, exp) = parse_ryu_to_decimal(raw, &mut digits_buf);
  let k = exp + digits.len() as i32;

  // Output is ASCII and has a small fixed upper bound (< 32 bytes for f64).
  let mut out_len = 0usize;

  if sign_negative {
    out_buf[out_len] = b'-';
    out_len += 1;
  }

  if k > 0 && k <= 21 {
    let k_usize = k as usize;
    if k_usize >= digits.len() {
      push_bytes(out_buf, &mut out_len, digits);
      for _ in 0..(k_usize - digits.len()) {
        push_byte(out_buf, &mut out_len, b'0');
      }
    } else {
      push_bytes(out_buf, &mut out_len, &digits[..k_usize]);
      push_byte(out_buf, &mut out_len, b'.');
      push_bytes(out_buf, &mut out_len, &digits[k_usize..]);
    }
    return &out_buf[..out_len];
  }

  if k <= 0 && k > -6 {
    push_bytes(out_buf, &mut out_len, b"0.");
    for _ in 0..((-k) as usize) {
      push_byte(out_buf, &mut out_len, b'0');
    }
    push_bytes(out_buf, &mut out_len, digits);
    return &out_buf[..out_len];
  }

  // Exponential form.
  push_byte(out_buf, &mut out_len, digits[0]);
  if digits.len() > 1 {
    push_byte(out_buf, &mut out_len, b'.');
    push_bytes(out_buf, &mut out_len, &digits[1..]);
  }
  push_byte(out_buf, &mut out_len, b'e');

  let exp = k - 1;
  let mut exp_buf = itoa::Buffer::new();
  if exp >= 0 {
    push_byte(out_buf, &mut out_len, b'+');
    push_bytes(out_buf, &mut out_len, exp_buf.format(exp as u32).as_bytes());
  } else {
    push_byte(out_buf, &mut out_len, b'-');
    push_bytes(out_buf, &mut out_len, exp_buf.format((-exp) as u32).as_bytes());
  }

  &out_buf[..out_len]
}

fn parse_ryu_to_decimal<'a>(raw: &str, digits_buf: &'a mut [u8; 32]) -> (&'a [u8], i32) {
  // `raw` is expected to be ASCII and contain either:
  // - digits with optional decimal point
  // - digits with optional decimal point and a trailing `e[+-]?\d+`
  //
  // Returns `(digits, exp)` such that `value = digits × 10^exp` and `digits`
  // contains no leading zeros.
  let (mantissa, exp_part) = match raw.split_once('e') {
    Some((mantissa, exp)) => (mantissa, Some(exp)),
    None => (raw, None),
  };

  let mut exp: i32 = exp_part.map_or(0, |e| e.parse().unwrap_or(0));

  let mut digits_len = 0usize;
  if let Some((int_part, frac_part)) = mantissa.split_once('.') {
    exp = exp.saturating_sub(frac_part.len() as i32);
    for b in int_part.bytes().chain(frac_part.bytes()) {
      debug_assert!(digits_len < digits_buf.len());
      digits_buf[digits_len] = b;
      digits_len += 1;
    }
  } else {
    for b in mantissa.bytes() {
      debug_assert!(digits_len < digits_buf.len());
      digits_buf[digits_len] = b;
      digits_len += 1;
    }
  }

  // Strip leading zeros introduced by `0.xxx` forms.
  let mut start = 0usize;
  while start < digits_len && digits_buf[start] == b'0' {
    start += 1;
  }
  debug_assert!(start < digits_len, "expected non-zero number to produce digits");
  if start > 0 {
    digits_buf.copy_within(start..digits_len, 0);
    digits_len -= start;
  }

  (&digits_buf[..digits_len], exp)
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

#[cfg(test)]
mod small_int_string_cache_stress_tests {
  use super::*;
  use crate::{Budget, JsRuntime, Vm, VmError, VmHostHooks, VmOptions};

  fn count_digit_only_strings(heap: &Heap) -> usize {
    let mut count = 0usize;
    for slot in &heap.slots {
      let Some(obj) = slot.value.as_ref() else {
        continue;
      };
      let HeapObject::String(s) = obj else {
        continue;
      };
      let units = s.as_code_units();
      if units.is_empty() {
        continue;
      }
      if units
        .iter()
        .all(|u| (b'0' as u16..=b'9' as u16).contains(u))
      {
        count += 1;
      }
    }
    count
  }

  #[test]
  fn array_concat_reuses_cached_index_key_strings() -> Result<(), VmError> {
    #[derive(Default)]
    struct NoopHooks;

    impl VmHostHooks for NoopHooks {
      fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {
      }
    }

    // Use generous heap limits and set `gc_threshold == max_bytes` so this test can observe
    // allocation churn without GC collecting unreachable temporary strings mid-operation.
    let max_bytes = 64 * 1024 * 1024;
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));
    let mut rt = JsRuntime::new(vm, heap)?;
    rt.vm.set_budget(Budget::unlimited(1));

    let mut host = ();
    let mut hooks = NoopHooks::default();
    let callee = rt.realm().intrinsics().array_prototype();

    // Run everything inside one scope so intermediate values stay rooted.
    let mut scope = rt.heap.scope();

    // Pre-fill the small-int string cache for the index range we'll use. The test below asserts
    // `Array.prototype.concat` does not allocate fresh index strings when these are already cached.
    for i in 0..4000u32 {
      let _ = scope.alloc_u32_index_string(i)?;
    }

    // Construct a dense array of length 4000 without invoking the JS evaluator.
    let a = scope.alloc_array(4000)?;
    scope.push_root(Value::Object(a))?;
    {
      let elems = scope.heap_mut().array_fast_elements_mut(a)?;
      let desc = PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Number(0.0),
          writable: true,
        },
      };
      elems.resize(4000, Some(desc));
    }

    let before = count_digit_only_strings(scope.heap());
    let out = crate::builtins::array_prototype_concat(
      &mut rt.vm,
      &mut scope,
      &mut host,
      &mut hooks,
      callee,
      Value::Object(a),
      &[],
    )?;
    // Root the result so its elements aren't collected if concat triggers allocation pressure.
    scope.push_root(out)?;
    let after = count_digit_only_strings(scope.heap());

    let delta = after.saturating_sub(before);
    assert!(
      delta < 100,
      "expected concat to reuse cached index key strings; allocated {delta} digit-only strings"
    );
    Ok(())
  }
}

#[cfg(test)]
mod detached_array_buffer_tests {
  use super::*;

  #[test]
  fn detached_array_buffer_backing_stores_are_not_invariant_violations() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let ab = scope.alloc_array_buffer(4)?;
    scope.push_root(Value::Object(ab))?;
    let view = scope.alloc_uint8_array(ab, 0, 4)?;
    scope.push_root(Value::Object(view))?;

    assert!(scope.heap_mut().detach_array_buffer_take_data(ab)?.is_some());

    match scope.heap().array_buffer_data(ab) {
      Err(VmError::TypeError(_)) => {}
      Err(other) => panic!("expected TypeError, got {other:?}"),
      Ok(_) => panic!("expected error for detached ArrayBuffer"),
    }

    match scope.heap().uint8_array_data(view) {
      Err(VmError::TypeError(_)) => {}
      Err(other) => panic!("expected TypeError, got {other:?}"),
      Ok(_) => panic!("expected error for Uint8Array backed by a detached ArrayBuffer"),
    }

    // Writes on detached buffers are a safe no-op.
    assert_eq!(scope.heap_mut().uint8_array_write(view, 0, &[1, 2, 3])?, 0);

    Ok(())
  }

  #[test]
  fn uint8_array_data_out_of_bounds_is_type_error_and_writes_are_noop() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let ab = scope.alloc_array_buffer(2)?;
    scope.push_root(Value::Object(ab))?;
    let view = scope.alloc_uint8_array(ab, 0, 2)?;
    scope.push_root(Value::Object(view))?;

    match scope.heap_mut().get_heap_object_mut(view.0)? {
      HeapObject::TypedArray(arr) => {
        assert_eq!(arr.kind, TypedArrayKind::Uint8);
        arr.length = 3;
      }
      _ => panic!("expected Uint8Array"),
    }

    match scope.heap().uint8_array_data(view) {
      Err(VmError::TypeError(_)) => {}
      Err(other) => panic!("expected TypeError, got {other:?}"),
      Ok(_) => panic!("expected error for out-of-bounds Uint8Array view"),
    }

    // Out-of-bounds views behave like empty typed arrays for host byte writes.
    let before = scope.heap().array_buffer_data(ab)?.to_vec();
    assert_eq!(scope.heap_mut().uint8_array_write(view, 0, &[1])?, 0);
    let after = scope.heap().array_buffer_data(ab)?.to_vec();
    assert_eq!(after, before);

    Ok(())
  }
}

#[cfg(test)]
mod generator_object_tests {
  use super::*;
  use parse_js::ast::stmt::Stmt;

  fn dummy_generator_func() -> Arc<Node<Func>> {
    let program = parse_js::parse("function* g() {}").expect("parse generator function");
    let mut stmts = program.stx.body.into_iter();
    let Some(stmt) = stmts.next() else {
      panic!("expected at least one statement");
    };
    match *stmt.stx {
      Stmt::FunctionDecl(func_decl) => {
        let parse_js::ast::stmt::decl::FuncDecl { function, .. } = *func_decl.stx;
        Arc::new(function)
      }
      _ => panic!("expected generator function declaration"),
    }
  }

  #[test]
  fn generator_objects_are_ordinary_objects_and_gc_traces_continuation() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let gen;
    let this_obj;
    let arg_obj;
    let global;
    let env;
    let prop_obj;
    let key;
    let continuation_bytes;

    {
      let mut scope = heap.scope();

      // Create a generator object without a continuation, then attach one so we can validate
      // heap-accounting deltas and GC tracing behaviour.
      gen = scope.alloc_generator_with_prototype(None, GeneratorState::SuspendedStart, None)?;
      scope.push_root(Value::Object(gen))?;

      this_obj = scope.alloc_object()?;
      arg_obj = scope.alloc_object()?;
      global = scope.alloc_object()?;
      env = scope.env_create(None)?;
      prop_obj = scope.alloc_object()?;
      key = scope.alloc_string("x")?;

      // Attach a continuation that references `this_obj`, `arg_obj`, and `env`, and ensure those
      // values stay alive only via the generator object's internal slots.
      {
        let mut init_scope = scope.reborrow();
        init_scope.push_roots(&[
          Value::Object(this_obj),
          Value::Object(arg_obj),
          Value::Object(global),
          Value::String(key),
        ])?;
        init_scope.push_env_root(env)?;

        let mut runtime_env = RuntimeEnv::new_with_lexical_env(init_scope.heap_mut(), global, env)?;
        runtime_env.teardown(init_scope.heap_mut());

        let cont = GeneratorContinuation {
          env: runtime_env,
          strict: false,
          this: Value::Object(this_obj),
          new_target: Value::Undefined,
          home_object: None,
          func: dummy_generator_func(),
          args: vec![Value::Object(arg_obj)].into_boxed_slice(),
          frames: VecDeque::new(),
        };
        continuation_bytes = cont.heap_size_bytes();

        let used_before_cont_set = init_scope.heap().used_bytes();
        init_scope
          .heap_mut()
          .generator_set_continuation(gen, Some(Box::new(cont)))?;
        let used_after_cont_set = init_scope.heap().used_bytes();
        assert_eq!(
          used_after_cont_set - used_before_cont_set,
          continuation_bytes
        );
      }

      // Generator objects are still ordinary objects for property operations.
      scope.define_property(
        gen,
        PropertyKey::from_string(key),
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(prop_obj),
            writable: true,
          },
        },
      )?;
      assert_eq!(
        scope.heap().get(gen, &PropertyKey::from_string(key))?,
        Value::Object(prop_obj)
      );

      // Exercise object property deletion plumbing for Generator objects.
      assert!(scope
        .heap_mut()
        .ordinary_delete(gen, PropertyKey::from_string(key))?);
      assert_eq!(
        scope.heap().get(gen, &PropertyKey::from_string(key))?,
        Value::Undefined
      );

      // `prop_obj` is now unreachable (the property was deleted), but continuation references should
      // keep `this_obj`/`arg_obj`/`env` live.
      scope.heap_mut().collect_garbage();
      assert!(scope.heap().is_valid_object(gen));
      assert!(scope.heap().is_valid_object(this_obj));
      assert!(scope.heap().is_valid_object(arg_obj));
      assert!(scope.heap().is_valid_env(env));
      assert!(!scope.heap().is_valid_object(prop_obj));

      // Clearing the continuation should release the last references to `this_obj`/`arg_obj`/`env`
      // and shrink heap accounting accordingly.
      let used_before_cont_unset = scope.heap().used_bytes();
      scope.heap_mut().generator_set_continuation(gen, None)?;
      let used_after_cont_unset = scope.heap().used_bytes();
      assert_eq!(
        used_before_cont_unset - used_after_cont_unset,
        continuation_bytes
      );
      assert_eq!(
        used_after_cont_unset,
        used_before_cont_unset.saturating_sub(continuation_bytes)
      );

      scope.heap_mut().collect_garbage();
      assert!(scope.heap().is_valid_object(gen));
      assert!(!scope.heap().is_valid_object(this_obj));
      assert!(!scope.heap().is_valid_object(arg_obj));
      assert!(!scope.heap().is_valid_env(env));
    }

    // Stack roots were removed when the scope was dropped.
    heap.collect_garbage();
    assert!(!heap.is_valid_object(gen));
    assert!(!heap.is_valid_object(this_obj));
    assert!(!heap.is_valid_object(arg_obj));
    assert!(!heap.is_valid_env(env));
    assert!(!heap.is_valid_object(prop_obj));
    assert!(matches!(
      heap.get(gen, &PropertyKey::from_string(key)),
      Err(VmError::InvalidHandle { .. })
    ));
    Ok(())
  }
}

#[cfg(test)]
mod async_generator_object_tests {
  use super::*;

  #[test]
  fn async_generator_request_queue_is_traced_by_gc() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let gen;
    let promise;
    let resolve;
    let reject;
    let value;

    {
      let mut scope = heap.scope();

      gen = scope.alloc_async_generator_with_prototype(
        None,
        AsyncGeneratorState::SuspendedStart,
        None,
        VecDeque::new(),
      )?;
      scope.push_root(Value::Object(gen))?;

      assert_eq!(
        scope.heap().async_generator_state(gen)?,
        AsyncGeneratorState::SuspendedStart
      );
      scope
        .heap_mut()
        .async_generator_set_state(gen, AsyncGeneratorState::Executing)?;
      assert_eq!(
        scope.heap().async_generator_state(gen)?,
        AsyncGeneratorState::Executing
      );
      // No continuation was installed.
      assert!(scope
        .heap_mut()
        .async_generator_take_continuation(gen)?
        .is_none());
      scope.heap_mut().async_generator_set_continuation(gen, None)?;

      {
        // Create request capability values, enqueue them, then drop stack roots so the only live
        // references are through the async generator's internal request queue.
        let mut init_scope = scope.reborrow();

        promise = init_scope.alloc_promise()?;
        init_scope.push_root(Value::Object(promise))?;
        resolve = init_scope.alloc_object()?;
        init_scope.push_root(Value::Object(resolve))?;
        reject = init_scope.alloc_object()?;
        init_scope.push_root(Value::Object(reject))?;
        value = init_scope.alloc_object()?;
        init_scope.push_root(Value::Object(value))?;

        let cap = PromiseCapability {
          promise: Value::Object(promise),
          resolve: Value::Object(resolve),
          reject: Value::Object(reject),
        };
        let req = AsyncGeneratorRequest {
          kind: AsyncGeneratorRequestKind::Next(Value::Object(value)),
          capability: cap,
        };

        let used_before_push = init_scope.heap().used_bytes();
        init_scope
          .heap_mut()
          .async_generator_request_queue_push(gen, req)?;
        let used_after_push = init_scope.heap().used_bytes();
        assert_eq!(
          used_after_push - used_before_push,
          mem::size_of::<AsyncGeneratorRequest>()
        );

        let peeked = init_scope.heap().async_generator_request_queue_peek(gen)?;
        assert!(matches!(
          peeked,
          Some(AsyncGeneratorRequest {
            kind: AsyncGeneratorRequestKind::Next(_),
            ..
          })
        ));
      }

      // The request capability values should stay alive via the request queue.
      scope.heap_mut().collect_garbage();
      assert!(scope.heap().is_valid_object(gen));
      assert!(scope.heap().is_valid_object(promise));
      assert!(scope.heap().is_valid_object(resolve));
      assert!(scope.heap().is_valid_object(reject));
      assert!(scope.heap().is_valid_object(value));

      // Dequeueing should drop the last references and allow collection.
      assert!(scope
        .heap_mut()
        .async_generator_request_queue_pop(gen)?
        .is_some());
      scope.heap_mut().collect_garbage();
      assert!(scope.heap().is_valid_object(gen));
      assert!(!scope.heap().is_valid_object(promise));
      assert!(!scope.heap().is_valid_object(resolve));
      assert!(!scope.heap().is_valid_object(reject));
      assert!(!scope.heap().is_valid_object(value));
    }

    heap.collect_garbage();
    assert!(!heap.is_valid_object(gen));
    Ok(())
  }

  #[test]
  fn async_generator_request_queue_len_updates_under_tight_heap_limits() -> Result<(), VmError> {
    // Use a small heap limit to exercise request-queue growth error paths.
    let max_bytes = 8 * 1024;
    let mut heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));
    let mut scope = heap.scope();

    let gen = scope.alloc_async_generator_with_prototype(
      None,
      AsyncGeneratorState::SuspendedStart,
      None,
      VecDeque::new(),
    )?;
    scope.push_root(Value::Object(gen))?;

    let cap = PromiseCapability {
      promise: Value::Undefined,
      resolve: Value::Undefined,
      reject: Value::Undefined,
    };
    let req = AsyncGeneratorRequest {
      kind: AsyncGeneratorRequestKind::Next(Value::Undefined),
      capability: cap,
    };

    let mut pushed = 0usize;
    loop {
      match scope.heap_mut().async_generator_request_queue_push(gen, req) {
        Ok(()) => {
          pushed += 1;
          assert_eq!(scope.heap().async_generator_request_queue_len(gen)?, pushed);
          // Avoid accidental infinite loops if the heap limit is larger than expected.
          if pushed > 10_000 {
            panic!("unexpectedly pushed 10k async generator requests without hitting OOM");
          }
        }
        Err(VmError::OutOfMemory) => break,
        Err(other) => return Err(other),
      }
    }

    while pushed > 0 {
      assert!(scope
        .heap_mut()
        .async_generator_request_queue_pop(gen)?
        .is_some());
      pushed -= 1;
      assert_eq!(scope.heap().async_generator_request_queue_len(gen)?, pushed);
    }
    assert!(scope
      .heap_mut()
      .async_generator_request_queue_pop(gen)?
      .is_none());
    Ok(())
  }
}

#[cfg(test)]
mod typed_array_helper_tests {
  use super::*;

  fn trigger_gc_via_allocations(scope: &mut Scope<'_>) -> Result<(), VmError> {
    let before = scope.heap().gc_runs();

    // Allocate some unrooted garbage so the upcoming GC has work to do.
    for i in 0..8 {
      let _ = scope.alloc_object()?;
      let s = format!("gc-garbage-{i}");
      let _ = scope.alloc_string(&s)?;
    }

    // Allocate an ArrayBuffer sized to exceed `gc_threshold` and therefore force an implicit GC in
    // `ensure_can_allocate`. Using an ArrayBuffer keeps slot-table growth small while still
    // exercising the external-bytes accounting path.
    let threshold = scope.heap().limits().gc_threshold;
    let current = scope.heap().estimated_total_bytes();
    let bytes_to_force_gc = threshold
      .saturating_sub(current)
      .saturating_add(1)
      .max(1);
    let _ = scope.alloc_array_buffer(bytes_to_force_gc)?;

    assert!(
      scope.heap().gc_runs() > before,
      "expected allocations to trigger GC (before={before}, after={})",
      scope.heap().gc_runs()
    );
    Ok(())
  }

  #[test]
  fn typed_array_is_integer_kind_classifies_int_vs_float() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let ab = scope.alloc_array_buffer(8)?;
    scope.push_root(Value::Object(ab))?;

    for kind in [
      TypedArrayKind::Int8,
      TypedArrayKind::Uint8,
      TypedArrayKind::Uint8Clamped,
      TypedArrayKind::Int16,
      TypedArrayKind::Uint16,
      TypedArrayKind::Int32,
      TypedArrayKind::Uint32,
    ] {
      let view = scope.alloc_typed_array(kind, ab, 0, 1)?;
      scope.push_root(Value::Object(view))?;
      assert!(
        scope.heap().typed_array_is_integer_kind(view)?,
        "expected {kind:?} to be an integer typed array"
      );
    }

    for kind in [TypedArrayKind::Float32, TypedArrayKind::Float64] {
      let view = scope.alloc_typed_array(kind, ab, 0, 1)?;
      scope.push_root(Value::Object(view))?;
      assert!(
        !scope.heap().typed_array_is_integer_kind(view)?,
        "expected {kind:?} to NOT be an integer typed array"
      );
    }

    Ok(())
  }

  #[test]
  fn typed_array_view_bytes_returns_internal_slots_and_rejects_detached() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let ab = scope.alloc_array_buffer(16)?;
    scope.push_root(Value::Object(ab))?;

    let view = scope.alloc_typed_array(TypedArrayKind::Uint32, ab, 4, 2)?;
    scope.push_root(Value::Object(view))?;

    let (buf, off, len) = scope.heap().typed_array_view_bytes(view)?;
    assert_eq!(buf, ab);
    assert_eq!(off, 4);
    assert_eq!(len, 8);

    assert!(scope.heap_mut().detach_array_buffer_take_data(ab)?.is_some());

    match scope.heap().typed_array_view_bytes(view) {
      Err(VmError::TypeError(msg)) => assert_eq!(msg, "ArrayBuffer is detached"),
      Err(other) => panic!("expected TypeError, got {other:?}"),
      Ok(_) => panic!("expected error for typed array backed by detached ArrayBuffer"),
    }

    Ok(())
  }

  #[test]
  fn typed_array_view_bytes_rejects_out_of_bounds() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let ab = scope.alloc_array_buffer(2)?;
    scope.push_root(Value::Object(ab))?;
    let view = scope.alloc_uint8_array(ab, 0, 2)?;
    scope.push_root(Value::Object(view))?;

    match scope.heap_mut().get_heap_object_mut(view.0)? {
      HeapObject::TypedArray(arr) => {
        assert_eq!(arr.kind, TypedArrayKind::Uint8);
        arr.length = 3;
      }
      _ => panic!("expected Uint8Array"),
    }

    match scope.heap().typed_array_view_bytes(view) {
      Err(VmError::TypeError(msg)) => assert_eq!(msg, "TypedArray view out of bounds"),
      Err(other) => panic!("expected TypeError, got {other:?}"),
      Ok(_) => panic!("expected error for out-of-bounds typed array view"),
    }

    Ok(())
  }

  #[test]
  fn typed_array_accessors_treat_out_of_bounds_views_as_empty() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let ab = scope.alloc_array_buffer(2)?;
    scope.push_root(Value::Object(ab))?;

    // Ensure byteOffset is non-zero so `byteOffset` returning 0 is meaningful when out-of-bounds.
    let view = scope.alloc_uint8_array(ab, 1, 1)?;
    scope.push_root(Value::Object(view))?;

    // Corrupt the view's element length so `byteOffset + byteLength` exceeds the backing buffer.
    match scope.heap_mut().get_heap_object_mut(view.0)? {
      HeapObject::TypedArray(arr) => {
        assert_eq!(arr.kind, TypedArrayKind::Uint8);
        arr.length = 3;
      }
      _ => panic!("expected Uint8Array"),
    }

    // Out-of-bounds views behave like empty typed arrays per ECMA-262.
    assert_eq!(scope.heap().typed_array_length(view)?, 0);
    assert_eq!(scope.heap().typed_array_byte_length(view)?, 0);
    assert_eq!(scope.heap().typed_array_byte_offset(view)?, 0);
    assert_eq!(scope.heap().typed_array_get_element_value(view, 0)?, None);

    Ok(())
  }

  #[test]
  fn typed_array_keeps_backing_array_buffer_alive_across_gc() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(128 * 1024, 64 * 1024));
    let mut scope = heap.scope();

    let buf = scope.alloc_array_buffer(16)?;
    let view = scope.alloc_uint8_array(buf, 0, 16)?;

    // Root only the TypedArray, not the backing buffer.
    scope.push_root(Value::Object(view))?;

    // Force at least one GC cycle via `ensure_can_allocate` and ensure we exercise allocation
    // paths that normally trigger GC.
    trigger_gc_via_allocations(&mut scope)?;

    // Also run an explicit GC cycle to ensure the typed array continues to keep the buffer alive.
    scope.heap_mut().collect_garbage();

    assert_eq!(scope.heap().typed_array_buffer(view)?, buf);
    assert_eq!(scope.heap().array_buffer_byte_length(buf)?, 16);
    assert_eq!(scope.heap().typed_array_byte_length(view)?, 16);

    Ok(())
  }

  #[test]
  fn data_view_keeps_backing_array_buffer_alive_across_gc() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(128 * 1024, 64 * 1024));
    let mut scope = heap.scope();

    let buf = scope.alloc_array_buffer(16)?;
    let view = scope.alloc_data_view(buf, 0, 16)?;

    // Root only the DataView, not the backing buffer.
    scope.push_root(Value::Object(view))?;

    trigger_gc_via_allocations(&mut scope)?;
    scope.heap_mut().collect_garbage();

    assert_eq!(scope.heap().data_view_buffer(view)?, buf);
    assert_eq!(scope.heap().array_buffer_byte_length(buf)?, 16);
    assert_eq!(scope.heap().data_view_byte_length(view)?, 16);

    Ok(())
  }
}

#[cfg(test)]
mod gc_invariant_arraybuffer_view_tests {
  use super::*;

  #[test]
  fn gc_invariant_accepts_valid_arraybuffer_views() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let ab = scope.alloc_array_buffer(16)?;
    let ta = scope.alloc_uint8_array(ab, 0, 16)?;
    let dv = scope.alloc_data_view(ab, 0, 16)?;

    scope.push_root(Value::Object(ab))?;
    scope.push_root(Value::Object(ta))?;
    scope.push_root(Value::Object(dv))?;

    // Should not panic in debug builds: typed array/DataView internal slots should point at a live
    // ArrayBuffer allocation.
    scope.heap_mut().collect_garbage();
    Ok(())
  }

  #[cfg(debug_assertions)]
  #[test]
  #[should_panic(expected = "TypedArray(kind=")]
  fn gc_invariant_panics_on_typed_array_with_non_arraybuffer_viewed_array_buffer() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let ab = scope.alloc_array_buffer(8).unwrap();
    scope.push_root(Value::Object(ab)).unwrap();

    let ta = scope.alloc_uint8_array(ab, 0, 8).unwrap();
    scope.push_root(Value::Object(ta)).unwrap();

    let not_ab = scope.alloc_object().unwrap();

    // Corrupt the internal `viewed_array_buffer` slot. This is intentionally invalid (points at a
    // live object that is not an ArrayBuffer) and should be detected by debug GC invariant checks.
    match scope.heap_mut().get_heap_object_mut(ta.0).unwrap() {
      HeapObject::TypedArray(arr) => arr.viewed_array_buffer = not_ab,
      _ => panic!("expected TypedArray allocation"),
    }

    scope.heap_mut().collect_garbage();
  }

  #[cfg(debug_assertions)]
  #[test]
  #[should_panic(expected = "DataView")]
  fn gc_invariant_panics_on_dataview_with_non_arraybuffer_viewed_array_buffer() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let ab = scope.alloc_array_buffer(8).unwrap();
    scope.push_root(Value::Object(ab)).unwrap();

    let dv = scope.alloc_data_view(ab, 0, 8).unwrap();
    scope.push_root(Value::Object(dv)).unwrap();

    let not_ab = scope.alloc_object().unwrap();

    // Corrupt the internal `viewed_array_buffer` slot. This is intentionally invalid (points at a
    // live object that is not an ArrayBuffer) and should be detected by debug GC invariant checks.
    match scope.heap_mut().get_heap_object_mut(dv.0).unwrap() {
      HeapObject::DataView(view) => view.viewed_array_buffer = not_ab,
      _ => panic!("expected DataView allocation"),
    }

    scope.heap_mut().collect_garbage();
  }

  #[cfg(debug_assertions)]
  #[test]
  #[should_panic(expected = "kind=Free")]
  fn gc_invariant_panics_on_typed_array_with_stale_viewed_array_buffer_handle() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let ab_live = scope.alloc_array_buffer(8).unwrap();
    let ta = scope.alloc_uint8_array(ab_live, 0, 8).unwrap();
    scope.push_root(Value::Object(ta)).unwrap();

    // Allocate an ArrayBuffer that will be freed by the next collection.
    let ab_dead = scope.alloc_array_buffer(4).unwrap();
    let stale_id = ab_dead.0;

    // Only the typed array is rooted; the `ab_dead` handle becomes stale after GC.
    scope.heap_mut().collect_garbage();

    // Corrupt the internal `viewed_array_buffer` slot to the stale heap id.
    match scope.heap_mut().get_heap_object_mut(ta.0).unwrap() {
      HeapObject::TypedArray(arr) => arr.viewed_array_buffer = GcObject(stale_id),
      _ => panic!("expected TypedArray allocation"),
    }

    // Avoid `Tracer::validate` panics by running the invariant check directly.
    scope.heap().debug_validate_no_stale_internal_handles();
  }

  #[cfg(debug_assertions)]
  #[test]
  #[should_panic(expected = "kind=Free")]
  fn gc_invariant_panics_on_dataview_with_stale_viewed_array_buffer_handle() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let ab_live = scope.alloc_array_buffer(8).unwrap();
    let dv = scope.alloc_data_view(ab_live, 0, 8).unwrap();
    scope.push_root(Value::Object(dv)).unwrap();

    // Allocate an ArrayBuffer that will be freed by the next collection.
    let ab_dead = scope.alloc_array_buffer(4).unwrap();
    let stale_id = ab_dead.0;

    scope.heap_mut().collect_garbage();

    match scope.heap_mut().get_heap_object_mut(dv.0).unwrap() {
      HeapObject::DataView(view) => view.viewed_array_buffer = GcObject(stale_id),
      _ => panic!("expected DataView allocation"),
    }

    // Avoid `Tracer::validate` panics by running the invariant check directly.
    scope.heap().debug_validate_no_stale_internal_handles();
  }
}

#[cfg(test)]
mod gc_invariant_other_internal_handle_tests {
  use super::*;
  use crate::function::NativeFunctionId;

  #[test]
  fn gc_invariant_accepts_valid_proxy_and_bound_function() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    // Proxy invariants.
    let target = scope.alloc_object()?;
    let handler = scope.alloc_object()?;
    let proxy = scope.alloc_proxy(target, handler)?;
    scope.push_root(Value::Object(proxy))?;

    // Bound function invariants.
    let target_name = scope.alloc_string("target")?;
    let bound_name = scope.alloc_string("bound")?;
    let target_fn = scope.alloc_native_function(NativeFunctionId(0), None, target_name, 0)?;
    let bound_fn = scope.alloc_bound_function(target_fn, Value::Undefined, &[], bound_name, 0)?;
    scope.push_root(Value::Object(bound_fn))?;

    scope.heap_mut().collect_garbage();
    Ok(())
  }

  #[cfg(debug_assertions)]
  #[test]
  #[should_panic(expected = "Proxy")]
  fn gc_invariant_panics_on_proxy_with_non_object_target() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let target = scope.alloc_object().unwrap();
    let handler = scope.alloc_object().unwrap();
    let proxy = scope.alloc_proxy(target, handler).unwrap();
    scope.push_root(Value::Object(proxy)).unwrap();

    let s = scope.alloc_string("not an object").unwrap();
    let fake_obj = GcObject(s.id());

    match scope.heap_mut().get_heap_object_mut(proxy.0).unwrap() {
      HeapObject::Proxy(p) => p.target = Some(fake_obj),
      _ => panic!("expected Proxy allocation"),
    }

    scope.heap_mut().collect_garbage();
  }

  #[cfg(debug_assertions)]
  #[test]
  #[should_panic(expected = "non-callable bound_target")]
  fn gc_invariant_panics_on_bound_function_with_non_callable_target() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let target_name = scope.alloc_string("target").unwrap();
    let bound_name = scope.alloc_string("bound").unwrap();
    let target_fn = scope
      .alloc_native_function(NativeFunctionId(0), None, target_name, 0)
      .unwrap();
    let bound_fn = scope
      .alloc_bound_function(target_fn, Value::Undefined, &[], bound_name, 0)
      .unwrap();
    scope.push_root(Value::Object(bound_fn)).unwrap();

    let non_callable = scope.alloc_object().unwrap();

    match scope.heap_mut().get_heap_object_mut(bound_fn.0).unwrap() {
      HeapObject::Function(f) => f.bound_target = Some(non_callable),
      _ => panic!("expected Function allocation"),
    }

    scope.heap_mut().collect_garbage();
  }

  #[cfg(debug_assertions)]
  #[test]
  #[should_panic(expected = "Promise")]
  fn gc_invariant_panics_on_promise_with_wrong_kind_result() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let promise = scope.alloc_promise().unwrap();
    scope.push_root(Value::Object(promise)).unwrap();

    let obj = scope.alloc_object().unwrap();
    // Build an invalid `Value::String` whose handle points at a non-string allocation.
    let fake_string = GcString(obj.id());

    match scope.heap_mut().get_heap_object_mut(promise.0).unwrap() {
      HeapObject::Promise(p) => p.result = Some(Value::String(fake_string)),
      _ => panic!("expected Promise allocation"),
    }

    scope.heap_mut().collect_garbage();
  }

  #[test]
  fn gc_invariant_accepts_valid_map_set_weakmap_finalization_registry_module_namespace_and_env(
  ) -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    // Map + Set.
    let map = scope.alloc_map()?;
    let set = scope.alloc_set()?;
    scope.push_root(Value::Object(map))?;
    scope.push_root(Value::Object(set))?;
    scope
      .heap_mut()
      .map_set_with_tick(map, Value::Number(1.0), Value::Number(2.0), || Ok(()))?;
    scope
      .heap_mut()
      .set_add_with_tick(set, Value::Number(3.0), || Ok(()))?;

    // WeakMap: keep the key alive externally so the entry survives GC.
    let weak_map = scope.alloc_weak_map()?;
    let weak_key = scope.alloc_object()?;
    scope.push_root(Value::Object(weak_map))?;
    scope.push_root(Value::Object(weak_key))?;
    scope
      .heap_mut()
      .weak_map_set_with_tick(weak_map, Value::Object(weak_key), Value::Number(4.0), || Ok(()))?;

    // FinalizationRegistry: keep the target alive externally so the cell target is not cleared.
    let cleanup_name = scope.alloc_string("cleanup")?;
    let cleanup_fn = scope.alloc_native_function(NativeFunctionId(0), None, cleanup_name, 0)?;
    let registry = scope.alloc_finalization_registry_with_prototype(None, Value::Object(cleanup_fn), None)?;
    scope.push_root(Value::Object(registry))?;
    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;
    scope
      .heap_mut()
      .finalization_registry_register(registry, Value::Object(target), Value::Number(1.0), None)?;

    // EnvRecord.
    let env = scope.env_create(None)?;
    scope.push_env_root(env)?;

    // Module Namespace object + exports table.
    let export_name = scope.alloc_string("x")?;
    let getter_name = scope.alloc_string("get")?;
    let getter_fn = scope.alloc_native_function(NativeFunctionId(1), None, getter_name, 0)?;
    let ns_val = scope.alloc_object()?;
    let exports: Box<[ModuleNamespaceExport]> = vec![ModuleNamespaceExport {
      name: export_name,
      getter: getter_fn,
      value: ModuleNamespaceExportValue::Namespace { namespace: ns_val },
    }]
    .into_boxed_slice();
    let ns = scope.alloc_module_namespace_object(exports)?;
    scope.push_root(Value::Object(ns))?;

    scope.heap_mut().collect_garbage();
    Ok(())
  }

  #[cfg(debug_assertions)]
  #[test]
  #[should_panic(expected = "Map")]
  fn gc_invariant_panics_on_map_entry_with_wrong_kind_value() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let map = scope.alloc_map().unwrap();
    scope.push_root(Value::Object(map)).unwrap();
    scope
      .heap_mut()
      .map_set_with_tick(map, Value::Number(1.0), Value::Number(2.0), || Ok(()))
      .unwrap();

    let obj = scope.alloc_object().unwrap();
    let fake_string = GcString(obj.id());

    match scope.heap_mut().get_heap_object_mut(map.0).unwrap() {
      HeapObject::Map(m) => {
        m.entries[0].value = Some(Value::String(fake_string));
      }
      _ => panic!("expected Map allocation"),
    }

    scope.heap_mut().collect_garbage();
  }

  #[cfg(debug_assertions)]
  #[test]
  #[should_panic(expected = "ModuleNamespace")]
  fn gc_invariant_panics_on_module_namespace_with_wrong_kind_exports_handle() {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let export_name = scope.alloc_string("x").unwrap();
    let getter_name = scope.alloc_string("get").unwrap();
    let getter_fn = scope
      .alloc_native_function(NativeFunctionId(0), None, getter_name, 0)
      .unwrap();
    let ns_val = scope.alloc_object().unwrap();
    let exports: Box<[ModuleNamespaceExport]> = vec![ModuleNamespaceExport {
      name: export_name,
      getter: getter_fn,
      value: ModuleNamespaceExportValue::Namespace { namespace: ns_val },
    }]
    .into_boxed_slice();
    let ns = scope.alloc_module_namespace_object(exports).unwrap();
    scope.push_root(Value::Object(ns)).unwrap();

    let obj = scope.alloc_object().unwrap();
    let fake_exports = GcModuleNamespaceExports(obj.id());

    match scope.heap_mut().get_heap_object_mut(ns.0).unwrap() {
      HeapObject::Object(o) => match &mut o.base.kind {
        ObjectKind::ModuleNamespace(ns) => ns.exports = fake_exports,
        _ => panic!("expected ModuleNamespace object kind"),
      },
      _ => panic!("expected Object allocation"),
    }

    scope.heap_mut().collect_garbage();
  }
}

#[cfg(test)]
mod generator_object_gc_tests {
  use super::*;
  use parse_js::ast::func::FuncBody;
  use parse_js::loc::Loc;

  #[test]
  fn generator_objects_trace_continuation_and_update_heap_accounting() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let gen;
    let global_obj;
    let this_obj;
    let arg_obj;
    let lexical_env;
    let var_env;
    let prop_obj;
    let key;

    let cont_bytes;
    {
      let mut scope = heap.scope();

      global_obj = scope.alloc_object()?;
      this_obj = scope.alloc_object()?;
      arg_obj = scope.alloc_object()?;
      lexical_env = scope.env_create(None)?;
      var_env = scope.env_create(None)?;
      prop_obj = scope.alloc_object()?;
      key = scope.alloc_string("x")?;

      // Allocate a generator object without a continuation; keep it live across collection via a
      // stack root.
      gen = scope.alloc_generator_with_prototype(None, GeneratorState::SuspendedStart, None)?;
      scope.push_root(Value::Object(gen))?;

      // Attach a regular data property so we can exercise ordinary object semantics (and ensure
      // GC only keeps the value alive while the property exists).
      scope.define_property(
        gen,
        PropertyKey::from_string(key),
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(prop_obj),
            writable: true,
          },
        },
      )?;
      assert_eq!(
        scope.heap().get(gen, &PropertyKey::from_string(key))?,
        Value::Object(prop_obj)
      );
      assert!(scope
        .heap_mut()
        .ordinary_delete(gen, PropertyKey::from_string(key))?);

      // Build a minimal generator continuation that references `this_obj`/`arg_obj` and captures a
      // `RuntimeEnv` pointing at `global_obj`/`lexical_env`/`var_env`.
      let mut env =
        RuntimeEnv::new_with_var_env(scope.heap_mut(), global_obj, lexical_env, var_env)?;
      // Generator continuations are heap-owned; they must not retain persistent env roots.
      env.teardown(scope.heap_mut());

      let func = Arc::new(Node::new(
        Loc(0, 0),
        Func {
          arrow: false,
          async_: false,
          generator: true,
          type_parameters: None,
          parameters: Vec::new(),
          return_type: None,
          body: Some(FuncBody::Block(Vec::new())),
        },
      ));

      let cont = GeneratorContinuation {
        env,
        strict: false,
        this: Value::Object(this_obj),
        new_target: Value::Undefined,
        home_object: None,
        func,
        args: vec![Value::Object(arg_obj)].into_boxed_slice(),
        frames: VecDeque::new(),
      };
      cont_bytes = cont.heap_size_bytes();

      let used_before_cont_set = scope.heap().used_bytes();
      scope
        .heap_mut()
        .generator_set_continuation(gen, Some(Box::new(cont)))?;
      let used_after_cont_set = scope.heap().used_bytes();
      assert_eq!(used_after_cont_set - used_before_cont_set, cont_bytes);

      // `prop_obj` was only referenced via the deleted property, so it should be collected. Values
      // referenced by the continuation should stay live.
      scope.heap_mut().collect_garbage();
      assert!(scope.heap().is_valid_object(gen));
      assert!(scope.heap().is_valid_object(global_obj));
      assert!(scope.heap().is_valid_object(this_obj));
      assert!(scope.heap().is_valid_object(arg_obj));
      assert!(scope.heap().is_valid_env(lexical_env));
      assert!(scope.heap().is_valid_env(var_env));
      assert!(!scope.heap().is_valid_object(prop_obj));

      // Clearing the continuation should release both heap accounting bytes and the GC references
      // held by the continuation.
      let used_before_cont_clear = scope.heap().used_bytes();
      scope.heap_mut().generator_set_continuation(gen, None)?;
      let used_after_cont_clear = scope.heap().used_bytes();
      assert_eq!(used_before_cont_clear - used_after_cont_clear, cont_bytes);

      scope.heap_mut().collect_garbage();
      assert!(scope.heap().is_valid_object(gen));
      assert!(!scope.heap().is_valid_object(global_obj));
      assert!(!scope.heap().is_valid_object(this_obj));
      assert!(!scope.heap().is_valid_object(arg_obj));
      assert!(!scope.heap().is_valid_env(lexical_env));
      assert!(!scope.heap().is_valid_env(var_env));
    }

    // Stack roots were removed when the scope was dropped.
    heap.collect_garbage();
    assert!(!heap.is_valid_object(gen));
    Ok(())
  }
}

#[cfg(test)]
mod runtime_env_rooting_tests {
  use super::*;

  #[test]
  fn runtime_env_roots_fresh_env_across_root_stack_growth_gc() -> Result<(), VmError> {
    let max_bytes = 8 * 1024 * 1024;
    let mut heap = Heap::new(HeapLimits::new(max_bytes, max_bytes));

    let global_obj;
    let env;
    let global_root;
    {
      // Allocate a global object and a fresh environment record. Once this scope is dropped the
      // `env` allocation becomes unreachable (and therefore eligible for collection).
      let mut scope = heap.scope();
      global_obj = scope.alloc_object()?;
      scope.push_root(Value::Object(global_obj))?;
      env = scope.env_create(None)?;
      scope.push_env_root(env)?;

      // Keep the global object alive via a persistent root so GC inside `RuntimeEnv` setup cannot
      // collect it.
      global_root = scope.heap_mut().add_root(Value::Object(global_obj))?;
    }

    // Force the next root-stack growth to trigger a GC cycle, and ensure the root stacks have
    // minimal capacity so growth is required.
    //
    // Historically, `RuntimeEnv::new_with_var_env` pushed the global-object root *before* rooting
    // the `lexical_env` argument. If growing `root_stack` triggered a GC, the fresh env record could
    // be collected and a stale `GcEnv` handle stored in `persistent_env_roots`, eventually causing
    // `VmError::InvalidHandle` during identifier resolution.
    heap.root_stack = Vec::new();
    heap.env_root_stack = Vec::new();
    heap.limits.gc_threshold = 0;

    let gc_before = heap.gc_runs();
    let mut runtime_env = RuntimeEnv::new_with_var_env(&mut heap, global_obj, env, env)?;
    assert!(
      heap.gc_runs() > gc_before,
      "expected GC to run during RuntimeEnv root registration"
    );

    // The newly-created environment record must still be valid and accessible.
    assert!(heap.is_valid_env(env));
    let _ = heap.get_env_record(env)?;

    // And it must remain live across explicit collections while the runtime env root is installed.
    heap.collect_garbage();
    assert!(heap.is_valid_env(env));

    runtime_env.teardown(&mut heap);
    heap.remove_root(global_root);

    Ok(())
  }
}
