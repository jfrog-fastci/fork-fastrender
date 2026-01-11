; Minimal LLVM IR exercising statepoint `"deopt"` bundle encoding for multiple
; value types/sizes.
;
; This is useful to validate:
; - meta location #3 = deopt operand count
; - `Size` field can vary (e.g. i32 = 4 bytes, vectors = 16 bytes)
; - `Indirect` offsets are `i32` and need not be 8-byte aligned (example uses +12)
;
; Repro (LLVM 18):
;   llvm-as-18 statepoint_deopt_mixed.ll -o statepoint_deopt_mixed.bc
;   opt-18 -passes=rewrite-statepoints-for-gc statepoint_deopt_mixed.bc -o statepoint_deopt_mixed.sp.bc
;   llc-18 -O2 -filetype=obj statepoint_deopt_mixed.sp.bc -o statepoint_deopt_mixed.o
;   llvm-readobj-18 --stackmap statepoint_deopt_mixed.o
;
target triple = "x86_64-pc-linux-gnu"

declare void @callee()

define ptr addrspace(1) @foo(ptr addrspace(1) %obj, i64 %x, i32 %y, <2 x i64> %v) gc "statepoint-example" {
entry:
  call void @callee() ["deopt"(i64 %x, i32 %y, <2 x i64> %v)]
  ret ptr addrspace(1) %obj
}

