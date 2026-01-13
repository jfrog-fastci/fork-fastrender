use crate::source::StackFrame;
use crate::value::Value;
use diagnostics::Diagnostic;
use std::fmt::Display;

/// Errors produced by the VM and runtime.
///
/// ## Error taxonomy
///
/// `vm-js` must be robust against hostile JavaScript input. This enum is structured so that all
/// failures are representable as a `VmError` value (i.e. **no JS-triggerable panics**):
///
/// - **JavaScript exceptions (catchable):** [`VmError::Throw`] and [`VmError::ThrowWithStack`].
/// - **Early errors (user error, not catchable by JS):** [`VmError::Syntax`].
/// - **Hard termination (not catchable by JS):** [`VmError::Termination`] for budgets/interrupts
///   (and similar "stop now" conditions).
///   - Exceeding [`crate::VmOptions::max_stack_depth`] is surfaced as a *catchable* JavaScript
///     `RangeError` (see `Vm::push_frame`), rather than a termination.
/// - **Engine/embedding bugs:** [`VmError::InvariantViolation`] (and related variants like
///   [`VmError::InvalidHandle`]). These indicate internal corruption or a broken host contract.
///
/// Variants such as [`VmError::TypeError`] and [`VmError::NotCallable`] are *internal helper
/// errors*: evaluator entry points typically coerce them into JS exceptions when intrinsics are
/// available (see `vm.rs:coerce_error_to_throw`).
#[derive(Debug, Clone, thiserror::Error)]
pub enum VmError {
  /// The heap has exceeded its configured memory limit.
  #[error("out of memory")]
  OutOfMemory,

  /// An internal invariant was violated (bug in the VM or the embedding).
  #[error("invariant violation: {0}")]
  InvariantViolation(&'static str),

  /// A hard VM limit was exceeded (e.g. an embedding attempted to register too many handlers).
  #[error("limit exceeded: {0}")]
  LimitExceeded(&'static str),

  /// A GC handle was used after the underlying allocation was freed (or the handle is otherwise
  /// malformed).
  #[error("invalid handle ({location})")]
  InvalidHandle {
    location: &'static std::panic::Location<'static>,
  },

  /// An attempted prototype mutation would introduce a cycle in the `[[Prototype]]` chain.
  #[error("prototype cycle")]
  PrototypeCycle,

  /// A prototype chain traversal exceeded a hard upper bound.
  #[error("prototype chain too deep")]
  PrototypeChainTooDeep,

  /// A stubbed/unfinished codepath.
  #[error("unimplemented: {0}")]
  Unimplemented(&'static str),

  /// The provided property descriptor patch is invalid.
  #[error("invalid property descriptor patch: cannot mix data and accessor fields")]
  InvalidPropertyDescriptorPatch,

  /// Object property lookup failed.
  #[error("property not found")]
  PropertyNotFound,

  /// An operation expected a data property, but an accessor property was encountered instead.
  #[error("property is not a data property")]
  PropertyNotData,

  #[error("type error: {0}")]
  TypeError(&'static str),

  #[error("range error: {0}")]
  RangeError(&'static str),

  /// Attempted to call a non-callable value.
  #[error("value is not callable")]
  NotCallable,

  /// Attempted to construct a non-constructable value.
  #[error("value is not a constructor")]
  NotConstructable,

  /// A JavaScript `throw` value. This is catchable from JS.
  #[error("uncaught exception")]
  Throw(Value),

  /// A JavaScript `throw` value with a captured stack trace.
  ///
  /// This is catchable from JS and is surfaced when an exception escapes to the host.
  #[error("uncaught exception")]
  ThrowWithStack { value: Value, stack: Vec<StackFrame> },

  /// A non-catchable termination condition (fuel exhausted, deadline exceeded, host interrupt,
  /// etc).
  #[error("{0}")]
  Termination(Termination),

  /// Early (syntax/binding) errors produced before execution begins.
  #[error("syntax error")]
  Syntax(Vec<Diagnostic>),
}

impl VmError {
  /// Constructs [`VmError::InvalidHandle`] with caller location metadata.
  ///
  /// `VmError::InvalidHandle` is treated as an engine/embedding bug. Capturing the call site
  /// makes it much easier to debug which internal handle access produced the error, especially
  /// when the error is surfaced to the host as a string.
  #[track_caller]
  pub fn invalid_handle() -> Self {
    VmError::InvalidHandle {
      location: std::panic::Location::caller(),
    }
  }

  /// Returns the thrown JavaScript value if this error represents a JS exception.
  pub fn thrown_value(&self) -> Option<Value> {
    match self {
      VmError::Throw(value) => Some(*value),
      VmError::ThrowWithStack { value, .. } => Some(*value),
      _ => None,
    }
  }

  /// Returns the captured stack (if present) for thrown JavaScript exceptions.
  pub fn thrown_stack(&self) -> Option<&[StackFrame]> {
    match self {
      VmError::ThrowWithStack { stack, .. } => Some(stack.as_slice()),
      _ => None,
    }
  }

  /// Returns `true` if this error represents a JavaScript *throw completion* (i.e. catchable by
  /// `try/catch`) or an internal helper error that is coerced into a JS throw when intrinsics are
  /// available.
  ///
  /// This is useful for implementing spec algorithms like `IteratorClose` where iterator-closing
  /// errors (from `GetMethod`/`Call`) can override an incoming completion, but must never replace
  /// VM-internal fatal errors such as `Termination`/`OutOfMemory`.
  pub fn is_throw_completion(&self) -> bool {
    matches!(
      self,
      VmError::Throw(_)
        | VmError::ThrowWithStack { .. }
        | VmError::Unimplemented(_)
        | VmError::TypeError(_)
        | VmError::RangeError(_)
        | VmError::NotCallable
        | VmError::NotConstructable
        | VmError::PrototypeCycle
        | VmError::PrototypeChainTooDeep
        | VmError::InvalidPropertyDescriptorPatch
    )
  }
}

/// A non-catchable error that terminates execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Termination {
  pub reason: TerminationReason,
  pub stack: Vec<StackFrame>,
}

impl Termination {
  pub fn new(reason: TerminationReason, stack: Vec<StackFrame>) -> Self {
    Self { reason, stack }
  }
}

impl Display for Termination {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{reason}", reason = self.reason)
  }
}

/// The reason execution terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TerminationReason {
  OutOfFuel,
  DeadlineExceeded,
  Interrupted,
  OutOfMemory,
  StackOverflow,
}

impl Display for TerminationReason {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      TerminationReason::OutOfFuel => f.write_str("execution terminated: out of fuel"),
      TerminationReason::DeadlineExceeded => f.write_str("execution terminated: deadline exceeded"),
      TerminationReason::Interrupted => f.write_str("execution terminated: interrupted"),
      TerminationReason::OutOfMemory => f.write_str("execution terminated: out of memory"),
      TerminationReason::StackOverflow => f.write_str("execution terminated: stack overflow"),
    }
  }
}
