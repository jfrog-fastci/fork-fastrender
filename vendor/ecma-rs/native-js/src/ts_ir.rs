//! LLVM IR helpers for TS-generated code.
//!
//! We currently treat "TS-generated" code as any IR we emit via this module.
//!
//! Critical invariant: **all TS-generated functions must have tail calls disabled**.
//! - Function attribute: `"disable-tail-calls"="true"`
//! - Calls in (potential) tail position should be emitted as `notail call ...`

pub const TAILCALL_TEST_CALLER: &str = "ts_tailcall_caller";
pub const TAILCALL_TEST_CALLEE: &str = "ts_tailcall_callee";

/// Minimal LLVM IR module that would normally be optimized into a tail call:
/// the caller ends with `call @callee; ret`.
///
/// The emitted IR enforces:
/// - every TS function uses attribute group `#0` which includes `"disable-tail-calls"="true"`.
/// - the tail-position call is marked `notail`.
/// - the callee is `noinline` so the call site survives `-O3`.
/// - the callee emits a StackMap record via `llvm.experimental.stackmap`, so the output object
///   contains a `.llvm_stackmaps` section (mirrors our planned statepoint/stackmap-based GC).
pub fn tailcall_regression_module_ir(target_triple: &str) -> String {
  format!(
    r#"; ModuleID = 'native-js-tailcall-regression'
target triple = "{target_triple}"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define i64 @{callee}(i64 %x) noinline #0 {{
entry:
  ; Force stackmap emission so we can assert stackmaps survive optimized codegen.
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 1, i32 0)
  %y = add i64 %x, 1
  ret i64 %y
}}

define i64 @{caller}(i64 %x) #0 {{
entry:
  %y = notail call i64 @{callee}(i64 %x)
  ret i64 %y
}}

attributes #0 = {{ "disable-tail-calls"="true" }}
"#,
    caller = TAILCALL_TEST_CALLER,
    callee = TAILCALL_TEST_CALLEE,
    target_triple = target_triple,
  )
}

