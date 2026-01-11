; Verified minimal `gc.statepoint` / stackmap fixture for LLVM 18 (opaque pointers).
;
; This file is intentionally small but exercises the full verifier-accepted statepoint
; form (incl. the mandatory trailing i32 operands) and produces a `.llvm_stackmaps`
; record when compiled with `llc-18`.
;
; Build (x86_64):
;   llvm-as-18 tests/fixtures/llvm/statepoint_min.ll -o /tmp/statepoint_min.bc
;   llc-18 -filetype=obj /tmp/statepoint_min.bc -o /tmp/statepoint_min.o
;   llvm-readobj-18 --stackmap /tmp/statepoint_min.o

target triple = "x86_64-unknown-linux-gnu"

; A dummy callee. It is declared but not defined: `llc` still emits an object
; with a relocation for the call, and stackmaps still work.
declare ptr addrspace(1) @callee(ptr addrspace(1))

; Statepoint intrinsics.
declare token @llvm.experimental.gc.statepoint.p0(i64 immarg, i32 immarg, ptr, i32 immarg, i32 immarg, ...)
declare ptr addrspace(1) @llvm.experimental.gc.result.p1(token)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32 immarg, i32 immarg)

; IMPORTANT:
; - Function must be a GC function (`gc "coreclr"`), otherwise LLVM 18
;   will abort during verification ("unsupported GC").
; - GC pointers for this strategy live in addrspace(1).
define ptr addrspace(1) @test(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  ; A derived ("interior") pointer to demonstrate base+derived relocation.
  %derived = getelementptr i8, ptr addrspace(1) %obj, i64 8

  ; Statepoint call: represents `call @callee(%obj)` *and* a safepoint.
  ;
  ; In LLVM 18 the final two operands are REQUIRED and must be constant i32:
  ;   - numTransitionArgs (must be 0; inline transition args are rejected)
  ;   - numDeoptArgs      (must be 0; inline deopt args are rejected)
  %tok = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
      i64 0, i32 0,
      ptr elementtype(ptr addrspace(1) (ptr addrspace(1))) @callee,
      i32 1, i32 0,
      ptr addrspace(1) %obj,
      i32 0, i32 0)
    [ "gc-live"(ptr addrspace(1) %obj, ptr addrspace(1) %derived) ]

  ; Recover the callee return value (statepoint itself returns only a token).
  %call_res = call ptr addrspace(1) @llvm.experimental.gc.result.p1(token %tok)

  ; Relocate live GC pointers. Indices are 0-based into the `"gc-live"` list.
  %obj.reloc = call ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 0)
  %derived.reloc = call ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %tok, i32 0, i32 1)

  ; Keep the derived relocation live.
  store i8 0, ptr addrspace(1) %derived.reloc

  ret ptr addrspace(1) %call_res
}
