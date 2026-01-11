; Minimal fixture for verifying LLVM 18 `gc.statepoint` stackmap ABI.
;
; This file is intentionally tiny and `llc`-only (no linker), so
; `vendor/ecma-rs/scripts/test_stackmap_abi.sh` can run in ~<1s.
source_filename = "statepoint.ll"

declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32, i32)

define void @stackmap_abi_callee(ptr addrspace(1) %p) {
entry:
  ret void
}

define ptr addrspace(1) @stackmap_abi_test(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  ; NOTE: the `elementtype(...)` annotation on the callee is required by LLVM 18
  ; with opaque pointers, otherwise the IR verifier rejects the statepoint.
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
      i64 0, i32 0,
      ptr elementtype(void (ptr addrspace(1))) @stackmap_abi_callee,
      i32 1, i32 0,
      ; call args:
      ptr addrspace(1) %obj,
      ; required trailing counts:
      i32 0, ; num_transition_args (must be 0; inline transition args are deprecated)
      i32 0  ; num_deopt_args
    ) [ "gc-live"(ptr addrspace(1) %obj) ]

  %rel = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 0)
  ret ptr addrspace(1) %rel
}
