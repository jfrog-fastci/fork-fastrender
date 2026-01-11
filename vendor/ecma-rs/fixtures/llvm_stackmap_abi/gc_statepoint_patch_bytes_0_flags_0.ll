; This fixture is compiled by `scripts/test_statepoint_flags_patchbytes.sh`.
; It exercises the `gc.statepoint` baseline lowering where `patch_bytes=0`
; produces a real call instruction and a corresponding stackmap record.

target triple = "x86_64-pc-linux-gnu"

declare void @callee()

declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)

define void @test(ptr %obj) gc "statepoint-example" {
entry:
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 0, i32 0,
    ptr elementtype(void ()) @callee,
    i32 0, i32 0,
    i32 0, i32 0) [ "gc-live"(ptr %obj) ]
  ret void
}
