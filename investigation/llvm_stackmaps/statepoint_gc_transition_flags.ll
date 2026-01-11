; Minimal LLVM IR demonstrating that statepoints can have non-zero "flags" in
; meta location #2.
;
; Repro (LLVM 18):
;   llvm-as-18 statepoint_gc_transition_flags.ll -o statepoint_gc_transition_flags.bc
;   opt-18 -passes=rewrite-statepoints-for-gc statepoint_gc_transition_flags.bc -o statepoint_gc_transition_flags.sp.bc
;   llc-18 -O2 -filetype=obj statepoint_gc_transition_flags.sp.bc -o statepoint_gc_transition_flags.o
;   llvm-readobj-18 --stackmap statepoint_gc_transition_flags.o
;
; Expected (meta prefix):
;   #1: Constant 0  (callconv=ccc)
;   #2: Constant 1  (flags set because of "gc-transition")
;   #3: Constant 0  (no deopt operands)
;
target triple = "x86_64-pc-linux-gnu"

declare void @callee()

define ptr addrspace(1) @foo(ptr addrspace(1) %obj) gc "statepoint-example" {
entry:
  call void @callee() ["gc-transition"(i64 99)]
  ret ptr addrspace(1) %obj
}

