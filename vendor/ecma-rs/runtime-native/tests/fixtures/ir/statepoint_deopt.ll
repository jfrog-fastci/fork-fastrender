; This fixture exists solely to generate a stable `.llvm_stackmaps` section for tests.
; Regenerate `tests/fixtures/bin/statepoint_deopt_x86_64.bin` with:
;   bash tests/fixtures/gen.sh
;
; This file encodes a `gc.statepoint` call that exercises the full LLVM 18 stackmap
; record layout:
; - non-zero callconv header (call fastcc token @llvm.experimental.gc.statepoint... => 8)
; - non-zero flags header (flags immarg = 1)
; - non-zero deopt operand count via a `"deopt"` operand bundle
; - two `"gc-live"` pointers, yielding two (base, derived) relocation pairs
;
; Empirically on LLVM 18, this produces a stackmap record with:
;   3 + deopt_count + 2*2 locations
; (3 internal statepoint header constants + deopt operands + 2 relocation pairs).
source_filename = "statepoint_deopt"
target triple = "x86_64-unknown-linux-gnu"

declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32, i32)

define void @statepoint_deopt_callee() {
entry:
  ret void
}

define i64 @statepoint_deopt(ptr addrspace(1) %base, i64 %x) gc "coreclr" {
entry:
  %derived = getelementptr i8, ptr addrspace(1) %base, i64 16
  %tok = call fastcc token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
      i64 2882400000, i32 0,
      ptr elementtype(void ()) @statepoint_deopt_callee,
      i32 0, i32 1,
      i32 0, i32 0
    ) [ "deopt"(i64 %x, i64 123), "gc-live"(ptr addrspace(1) %base, ptr addrspace(1) %derived) ]
  %base.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 0)
  %derived.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 1)

  %i_base = ptrtoint ptr addrspace(1) %base.relocated to i64
  %i_derived = ptrtoint ptr addrspace(1) %derived.relocated to i64
  %sum = add i64 %i_base, %i_derived
  %sum2 = add i64 %sum, %x
  ret i64 %sum2
}
