//! JavaScript runtime/VM scaffolding for `ecma-rs`.
//!
//! This crate is the foundation for browser-grade JavaScript execution. It provides:
//! - A non-moving mark/sweep GC heap ([`Heap`])
//! - Stable, generation-checked handles ([`GcObject`], [`GcString`], [`GcSymbol`])
//! - Stack rooting via RAII scopes ([`Scope`]) + persistent roots ([`RootId`])
//! - Cooperative interruption primitives ([`Vm`], [`Budget`], [`InterruptToken`])
//! - Source/stack-trace types ([`SourceText`], [`StackFrame`])
//!
//! # Rooting and handle validity
//!
//! Heap-allocated objects (strings, symbols, objects) are referenced using stable handles.
//! A handle contains `{ index, generation }`; the `index` points into the heap slot vector and the
//! `generation` is incremented every time that slot is freed.
//!
//! This means:
//! - Handles are stable across `Vec` reallocations because objects are stored in index-addressed
//!   slots (the object itself never moves to a different index).
//! - A handle becomes invalid once the object is collected; future allocations may reuse the same
//!   slot index with a newer generation.
//! - Public APIs that dereference handles validate `{index,generation}` and return
//!   [`VmError::InvalidHandle`] for stale handles.
//! - During GC, encountering a stale handle in a root set indicates a bug; this crate will
//!   `debug_assert!` and ignore it.
//!
//! The GC traces from two root sets:
//! - **Stack roots**: stored in `Heap::root_stack` and managed by [`Scope`]. When a `Scope` is
//!   dropped, all stack roots created within it are popped.
//! - **Persistent roots**: managed by [`Heap::add_root`] / [`Heap::remove_root`], intended for host
//!   embeddings.
//!
//! # Embedding guide: budgets + interrupts
//!
//! `vm-js` supports **cooperative** interruption: the host configures a budget/cancellation token,
//! and evaluators periodically call [`Vm::tick`].
//!
//! ## Host cancellation (`interrupt_flag`)
//!
//! Create a shared cancellation flag:
//!
//! ```rust
//! use std::sync::Arc;
//! use std::sync::atomic::AtomicBool;
//! let cancel = Arc::new(AtomicBool::new(false));
//! # drop(cancel);
//! ```
//!
//! Pass it to [`VmOptions::interrupt_flag`]. The VM will observe the same flag (via its internal
//! [`InterruptToken`]), so the host can cooperatively cancel execution by setting the flag to
//! `true`.
//!
//! This flag is considered **internal** to the VM: it is also used by [`InterruptHandle`], and it
//! is cleared by [`Vm::reset_interrupt`].
//!
//! ## External host cancellation (`external_interrupt_flag`)
//!
//! Some embeddings have a long-lived cancellation flag (e.g. render-wide abort) that should be
//! observed by the VM but must **not** be cleared by [`Vm::reset_interrupt`] (which is often called
//! between tasks/callbacks). Use [`VmOptions::external_interrupt_flag`] for this.
//!
//! The VM treats itself as interrupted when either `interrupt_flag` (internal) or
//! `external_interrupt_flag` (external) is set.
//!
//! ## Per-task budgets
//!
//! - Use [`Vm::set_budget`] to apply a per-task [`Budget`] (fuel and/or deadline).
//! - Use [`Vm::reset_budget_to_default`] to restore the defaults configured in [`VmOptions`].
//! - Use [`Vm::push_budget`] to temporarily override the budget with an RAII guard.
//!
//! **Important:** budgets/interrupts are only enforced where the evaluator calls [`Vm::tick`]. The
//! provided [`JsRuntime`] calls `tick()` once per statement and once per loop iteration; custom
//! evaluators must do the same for budgets to have any effect.
//!
//! ## Minimal example: fuel budget + interrupt flag
//!
//! ```rust
//! use std::sync::Arc;
//! use std::sync::atomic::{AtomicBool, Ordering};
//! use vm_js::{Budget, Heap, HeapLimits, JsRuntime, TerminationReason, Vm, VmError, VmOptions};
//!
//! # fn main() -> Result<(), VmError> {
//! // Shared host cancellation flag.
//! let interrupt_flag = Arc::new(AtomicBool::new(false));
//!
//! let vm = Vm::new(VmOptions {
//!   interrupt_flag: Some(interrupt_flag.clone()),
//!   ..VmOptions::default()
//! });
//! let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
//! let mut rt = JsRuntime::new(vm, heap)?;
//!
//! // Give this run a tiny fuel budget so an infinite loop terminates quickly.
//! rt.vm.set_budget(Budget {
//!   fuel: Some(5),
//!   deadline: None,
//!   check_time_every: 1,
//! });
//!
//! let result = rt.exec_script("while (true) {}");
//! // Typically you'd reset budgets in a `drop` guard / `defer` after each host task.
//! rt.vm.reset_budget_to_default();
//!
//! let err = result.unwrap_err();
//! match err {
//!   VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
//!   other => panic!("expected termination, got {other:?}"),
//! }
//!
//! // The host can also request cooperative cancellation via the shared interrupt flag:
//! interrupt_flag.store(true, Ordering::Relaxed);
//! let err = rt.vm.tick().unwrap_err();
//! match err {
//!   VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::Interrupted),
//!   other => panic!("expected termination, got {other:?}"),
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # WebIDL / host objects
//!
//! If you are embedding `vm-js` in a browser-style host and need to expose Web APIs (constructors,
//! `prototype` objects, native methods/attributes, wrapper identity caches), see
//! [`docs::webidl_host_objects`](crate::docs::webidl_host_objects).
//!
//! # ECMAScript modules
//!
//! For an embedder-facing guide to ES modules (module loading hooks, [`ModuleGraph`], dynamic
//! `import()`, top-level `await`), see [`docs::modules`](crate::docs::modules).
//!
//! # RegExp: Unicode property escapes (`\p{…}` / `\P{…}`)
//!
//! RegExp Unicode property escapes are extremely data-driven. For the supported properties,
//! strict matching rules, Unicode version policy (test262 alignment), and regeneration procedure,
//! see [`docs::regexp_unicode_properties`](crate::docs::regexp_unicode_properties).

mod agent;
mod async_generator;
mod bigint;
mod builtins;
mod class_fields;
mod code;
mod conversion_ops;
mod destructure;
mod env;
mod early_errors;
mod error;
mod error_object;
mod fallible_alloc;
mod fallible_format;
mod exec;
mod for_in;
mod execution_context;
mod function;
mod function_properties;
mod handle;
mod hir_exec;
mod heap;
mod import_meta;
mod interrupt;
mod intrinsics;
pub mod iterator;
pub mod job_queue;
mod jobs;
mod microtasks;
mod module_graph;
mod module_loading;
mod module_record;
mod module_request;
mod native;
mod object_ops;
mod ops;
mod promise;
mod promise_jobs;
mod promise_ops;
mod promise_rejection_tracker;
mod property;
mod property_descriptor_ops;
mod realm;
mod regexp;
mod regexp_case_folding;
mod regexp_unicode_property_strings;
mod regexp_unicode_property_tables;
mod regexp_unicode_resolver;
mod regexp_unicode_tables;
mod regexp_unicode_icu;
mod source;
mod spec_ops;
mod string;
mod symbol;
mod tick;
mod unicode_case_folding;
mod value;
mod vm;

#[cfg(test)]
mod test_alloc;

#[cfg(test)]
mod regexp_unicode_property_strings_tests;

// Some unit tests are authored as standalone files under `../tests/` so they can also run as
// integration tests. Alias the crate as `vm_js` so those files can `use vm_js::...` regardless of
// whether they are compiled as unit or integration tests.
#[cfg(test)]
extern crate self as vm_js;

// Unit tests that need access to crate-private internals live in `../tests/unit/` and are pulled
// into the library test target here so they can exercise non-public APIs.
#[cfg(test)]
#[path = "../tests/unit/regexp_unicode_resolver.rs"]
mod regexp_unicode_resolver_tests;

#[cfg(test)]
#[path = "../tests/unit/scf.rs"]
mod scf_tests;

#[cfg(test)]
#[path = "../tests/unit/class_static_block_hir_exec.rs"]
mod class_static_block_hir_exec_tests;

#[cfg(test)]
#[path = "../tests/unit/typed_array_dataview_rooting_gc.rs"]
mod typed_array_dataview_rooting_gc_tests;

#[cfg(test)]
#[path = "../tests/unit/private_in.rs"]
mod private_in_tests;

#[cfg(test)]
#[path = "../tests/unit/home_object.rs"]
mod home_object_tests;

#[cfg(test)]
#[path = "../tests/compound_assignment_bitwise_shift.rs"]
mod compound_assignment_bitwise_shift_tests;

#[cfg(test)]
#[path = "../tests/compiled_module_graph.rs"]
mod compiled_module_graph_tests;

#[cfg(test)]
#[path = "../tests/compound_assignment_arithmetic.rs"]
mod compound_assignment_arithmetic_tests;

#[cfg(test)]
#[path = "../tests/generators_compound_assignment_property_capture.rs"]
mod generators_compound_assignment_property_capture_tests;

#[cfg(test)]
#[path = "../tests/unit/error_coercion.rs"]
mod error_coercion_tests;

#[cfg(test)]
#[path = "../tests/logical_assignment.rs"]
mod logical_assignment_tests;

#[cfg(test)]
#[path = "../tests/unit/private_brand_check_in_operator.rs"]
mod private_brand_check_in_operator_tests;

#[cfg(test)]
#[path = "../tests/generators_short_circuit_and_comma.rs"]
mod generators_short_circuit_and_comma_tests;

#[cfg(test)]
#[path = "../tests/class_inheritance_and_super.rs"]
mod class_inheritance_and_super_tests;

#[cfg(test)]
#[path = "../tests/unit/object_literal_super.rs"]
mod object_literal_super_tests;

pub use crate::handle::EnvRootId;

pub use crate::agent::format_termination;
pub use crate::agent::Agent;
pub use crate::agent::HostHooks;
pub use crate::agent::VmErrorReport;
pub use crate::code::CompiledFunctionRef;
pub use crate::code::CompiledScript;
pub use crate::env::EnvBinding;
pub use crate::error::Termination;
pub use crate::error::TerminationReason;
pub use crate::error::VmError;
pub use crate::error_object::new_error;
pub use crate::error_object::new_range_error;
pub use crate::error_object::new_reference_error;
pub use crate::error_object::new_syntax_error_object;
pub use crate::error_object::new_type_error;
pub use crate::error_object::new_type_error_object;
pub use crate::error_object::throw_range_error;
pub use crate::error_object::throw_type_error;
pub use crate::exec::Completion;
pub use crate::exec::eval_script_with_host_and_hooks;
pub use crate::exec::JsRuntime;
pub use crate::exec::Thrown;
pub use crate::execution_context::ExecutionContext;
pub use crate::execution_context::ModuleId;
pub use crate::execution_context::ScriptId;
pub use crate::execution_context::ScriptOrModule;
pub use crate::function::EcmaFunctionId;
pub use crate::function::NativeConstructId;
pub use crate::function::NativeFunctionId;
pub use crate::function::ThisMode;
pub use crate::function_properties::make_constructor;
pub use crate::function_properties::make_async_generator_function_instance_prototype;
pub use crate::function_properties::make_generator_function_instance_prototype;
pub use crate::function_properties::set_function_length;
pub use crate::function_properties::set_function_name;
pub use crate::handle::GcEnv;
pub use crate::handle::GcBigInt;
pub use crate::handle::GcObject;
pub use crate::handle::GcString;
pub use crate::handle::GcSymbol;
pub use crate::handle::HeapId;
pub use crate::handle::RootId;
pub use crate::handle::WeakGcObject;
pub use crate::handle::WeakGcSymbol;
pub use crate::env::EnvBindingValue;
pub use crate::heap::Heap;
pub use crate::heap::ExternalMemoryToken;
pub use crate::heap::HeapLimits;
pub use crate::heap::HostSlots;
pub use crate::heap::PersistentRoot;
pub use crate::heap::Scope;
pub use crate::heap::TypedArrayKind;
pub use crate::conversion_ops::ToPrimitiveHint;
pub use crate::heap::MAX_PROTOTYPE_CHAIN;
pub use crate::import_meta::create_import_meta_object;
pub use crate::import_meta::ImportMetaProperty;
pub use crate::import_meta::VmImportMetaHostHooks;
pub use crate::interrupt::InterruptHandle;
pub use crate::interrupt::InterruptToken;
pub use crate::intrinsics::Intrinsics;
pub use crate::intrinsics::WellKnownSymbols;
pub use crate::jobs::Job;
#[deprecated(note = "Use Job instead (MicrotaskJob was renamed for spec alignment).")]
pub use crate::jobs::Job as MicrotaskJob;
pub use crate::jobs::JobCallback;
pub use crate::jobs::JobKind;
pub use crate::jobs::JobResult;
pub use crate::jobs::PromiseHandle;
pub use crate::jobs::PromiseRejectionOperation;
pub use crate::jobs::RealmId;
pub use crate::jobs::VmHost;
pub use crate::jobs::VmHostHooks;
#[deprecated(note = "Use VmHostHooks instead (JobQueue was renamed for spec alignment).")]
pub use crate::jobs::VmHostHooks as JobQueue;
pub use crate::jobs::VmJobContext;
pub use crate::microtasks::MicrotaskQueue;
pub use crate::module_graph::ModuleGraph;
pub use crate::module_loading::all_import_attributes_supported;
pub use crate::module_loading::continue_dynamic_import;
pub use crate::module_loading::continue_dynamic_import_with_host_and_hooks;
pub use crate::module_loading::continue_module_loading;
pub use crate::module_loading::continue_module_loading_with_host_and_hooks;
pub use crate::module_loading::finish_loading_imported_module;
pub use crate::module_loading::finish_loading_imported_module_with_host_and_hooks;
pub use crate::module_loading::import_attributes_from_options;
pub use crate::module_loading::import_attributes_from_options_with_host_and_hooks;
pub use crate::module_loading::load_requested_modules;
pub use crate::module_loading::load_requested_modules_with_host_and_hooks;
pub use crate::module_loading::start_dynamic_import;
pub use crate::module_loading::start_dynamic_import_with_host_and_hooks;
pub use crate::module_loading::GraphLoadingState;
pub use crate::module_loading::HostDefined;
pub use crate::module_loading::ImportCallError;
pub use crate::module_loading::ImportCallTypeError;
pub use crate::module_loading::LoadedModulesOwner;
pub use crate::module_loading::ModuleCompletion;
pub use crate::module_loading::ModuleLoadPayload;
pub use crate::module_loading::ModuleReferrer;
pub use crate::module_record::BindingName;
pub use crate::module_record::ModuleStatus;
pub use crate::module_record::ResolveExportResult;
pub use crate::module_record::ResolvedBinding;
pub use crate::module_record::SourceTextModuleRecord;
pub use crate::module_request::cmp_utf16;
pub use crate::module_request::module_requests_equal;
pub use crate::module_request::ImportAttribute;
pub use crate::module_request::LoadedModuleRequest;
pub use crate::module_request::ModuleRequest;
pub use crate::module_request::ModuleRequestLike;
pub use crate::native::alloc_native_function_name;
pub use crate::native::dispatch_native_call;
pub use crate::native::dispatch_native_construct;
pub use crate::native::native_construct_id;
pub use crate::native::native_function_meta;
pub use crate::native::NativeCallFn;
pub use crate::native::NativeConstructFn;
pub use crate::native::NativeFunctionMeta;
pub use crate::promise::await_value;
pub use crate::promise::create_promise_resolve_thenable_job;
pub use crate::promise::normalize_promise_then_handlers;
pub use crate::promise::Awaitable;
pub use crate::promise::Promise;
pub use crate::promise::PromiseCapability as ImportPromiseCapability;
pub use crate::promise::PromiseCapability;
pub use crate::promise::PromiseReaction;
pub use crate::promise::PromiseReactionRecord;
pub use crate::promise::PromiseReactionType;
pub use crate::promise::PromiseState;
pub use crate::promise_jobs::new_promise_reaction_job;
pub use crate::promise_jobs::new_promise_resolve_thenable_job;
pub use crate::promise_ops::new_promise_capability;
pub use crate::promise_ops::new_promise_capability_with_host_and_hooks;
pub use crate::promise_ops::perform_promise_then;
pub use crate::promise_ops::perform_promise_then_with_host_and_hooks;
pub use crate::promise_ops::perform_promise_then_with_result_capability;
pub use crate::promise_ops::perform_promise_then_with_result_capability_with_host_and_hooks;
pub use crate::promise_ops::promise_resolve;
pub use crate::promise_ops::promise_resolve_with_host_and_hooks;
pub use crate::promise_ops::promise_resolve_thenable_immediate;
pub use crate::promise_ops::promise_resolve_thenable_immediate_with_host_and_hooks;
pub use crate::promise_rejection_tracker::AboutToBeNotifiedBatch;
pub use crate::promise_rejection_tracker::PromiseRejectionHandleAction;
pub use crate::promise_rejection_tracker::PromiseRejectionTracker;
pub use crate::property::PropertyDescriptor;
pub use crate::property::PropertyDescriptorPatch;
pub use crate::property::PropertyKey;
pub use crate::property::PropertyKind;
pub use crate::property_descriptor_ops::complete_property_descriptor;
pub use crate::property_descriptor_ops::from_property_descriptor;
pub use crate::property_descriptor_ops::from_property_descriptor_patch;
pub use crate::property_descriptor_ops::is_compatible_property_descriptor;
pub use crate::property_descriptor_ops::to_property_descriptor_with_host_and_hooks;
pub use crate::realm::Realm;
pub use crate::regexp::RegExpFlags;
pub use crate::regexp::RegExpProgram;
pub use crate::regexp_case_folding::regexp_case_fold;
pub use crate::source::format_stack_trace;
pub use crate::source::SourceText;
pub use crate::source::SourceTextInput;
pub use crate::source::StackFrame;
pub use crate::spec_ops::create_data_property;
pub use crate::spec_ops::create_data_property_or_throw;
pub use crate::spec_ops::define_property_or_throw;
pub use crate::spec_ops::delete_property_or_throw;
pub use crate::spec_ops::get_method;
pub use crate::spec_ops::get_method_with_host_and_hooks;
pub use crate::spec_ops::get_prototype_from_constructor;
pub use crate::spec_ops::ordinary_create_from_constructor;
pub use crate::spec_ops::species_constructor;
pub use crate::spec_ops::species_constructor_with_host_and_hooks;
pub use crate::string::JsString;
pub use crate::symbol::JsSymbol;
pub use crate::bigint::JsBigInt;
pub use crate::value::Value;
pub use crate::vm::Budget;
pub use crate::vm::BudgetGuard;
pub use crate::vm::ExecutionContextGuard;
pub use crate::vm::NativeCall;
pub use crate::vm::NativeConstruct;
pub use crate::vm::Vm;
pub use crate::vm::VmFrameGuard;
pub use crate::vm::VmOptions;

/// Long-form guides and embedding documentation.
pub mod docs {
  /// WebIDL binding initialization patterns (constructors, prototypes, host objects).
  #[doc = include_str!("../docs/webidl_host_objects.md")]
  pub mod webidl_host_objects {}

  /// ECMAScript modules embedding guide (module loading, dynamic import, top-level await).
  #[doc = include_str!("../docs/modules.md")]
  pub mod modules {}

  /// RegExp Unicode property escapes (`\p{…}` / `\P{…}`) and Unicode data update procedure.
  #[doc = include_str!("../docs/regexp_unicode_properties.md")]
  pub mod regexp_unicode_properties {}
}
