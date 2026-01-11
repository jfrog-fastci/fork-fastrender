; This fixture is compiled by `scripts/test_statepoint_flags_patchbytes.sh`.
; It exercises the `gc.statepoint` lowering where:
; - `patch_bytes > 0` reserves a patchable region (NOP sled on x86_64).
; - `flags = 3` (both currently-valid bits) must remain visible in the generated stackmap record.

target triple = "x86_64-pc-linux-gnu"

declare void @callee()

declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)

define void @test(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 0, i32 16,
    ptr elementtype(void ()) @callee,
    i32 0, i32 3,
    i32 0, i32 0) [ "gc-live"(ptr addrspace(1) %obj) ]
  ret void
}
