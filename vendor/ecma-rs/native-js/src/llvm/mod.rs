//! LLVM integration helpers for `native-js`.
//!
//! The native backend uses LLVM's *statepoint* infrastructure to support a
//! moving GC. On LLVM 18, the "manual" path (constructing `gc.statepoint`
//! intrinsics directly) is easy to get wrong:
//!
//! - Intrinsic signatures contain `immarg` parameters and require the callee
//!   argument to be annotated with `elementtype(<fn-ty>)`.
//! - Manually-built statepoints require extra trailing `i32 0, i32 0`
//!   transition/flags fields.
//!
//! Instead of computing liveness and constructing statepoints in Rust, we rely
//! on LLVM's `rewrite-statepoints-for-gc` pass to:
//! - rewrite plain calls into `llvm.experimental.gc.statepoint.*`
//! - attach the required `"gc-live"` operand bundle
//! - insert `llvm.experimental.gc.relocate.*` / `gc.result.*` and rewrite uses

pub mod gc;
pub mod passes;
pub mod statepoint_directives;

