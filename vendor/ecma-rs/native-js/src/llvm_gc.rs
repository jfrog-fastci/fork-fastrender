//! LLVM GC integration constants.
//!
//! `native-js` relies on LLVM's **statepoint** infrastructure for precise GC.
//! LLVM selects the statepoint lowering rules via the function-level `gc
//! "<strategy>"` attribute.
//!
//! We standardize on a single strategy name across all generated code to avoid
//! drift between modules and to make it easy to change globally later.

/// The LLVM GC strategy name to use for all generated GC-aware functions.
///
/// Rationale and tradeoffs are documented in `native-js/docs/llvm_gc_strategy.md`.
pub(crate) const LLVM_GC_STRATEGY: &str = "coreclr";

