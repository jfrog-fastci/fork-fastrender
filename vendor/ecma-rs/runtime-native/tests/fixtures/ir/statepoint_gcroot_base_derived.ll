; This fixture exists solely to generate a stable `.llvm_stackmaps` section for tests.
; Regenerate `tests/fixtures/bin/statepoint_{base_derived_x86_64,base_derived_aarch64}.bin` with:
;   bash tests/fixtures/gen.sh
;
; This file encodes the LLVM 18 "known-good" `gc.statepoint` + `gc.relocate` pattern directly
; (no `rewrite-statepoints-for-gc` pass required).
;
; We intentionally exercise **two** `gc.relocate` uses:
; - base==derived: relocating the base pointer itself
; - base!=derived: relocating an interior/derived pointer (%derived = %base + 16)
;
; Empirically on LLVM 18, this produces a stackmap record with:
;   3 + 2*2 = 7 locations
; (3 internal statepoint header constants + 2 relocation pairs).
source_filename = "statepoint_base_derived"

declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32, i32)

define void @statepoint_base_derived_callee() {
entry:
  ret void
}

define i64 @statepoint_base_derived(ptr addrspace(1) %base) gc "coreclr" {
entry:
  %derived = getelementptr i8, ptr addrspace(1) %base, i64 16
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
      i64 2882400000, i32 0,
      ptr elementtype(void ()) @statepoint_base_derived_callee,
      i32 0, i32 0,
      i32 0, i32 0
    ) [ "gc-live"(ptr addrspace(1) %base, ptr addrspace(1) %derived) ]
  %base.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 0)
  %derived.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 1)

  %i_base = ptrtoint ptr addrspace(1) %base.relocated to i64
  %i_derived = ptrtoint ptr addrspace(1) %derived.relocated to i64
  %sum = add i64 %i_base, %i_derived
  ret i64 %sum
}
