; Minimal LLVM IR that triggers StackMap v3 LocationKind=5 (ConstantIndex) via
; `rewrite-statepoints-for-gc` on x86_64.
;
; Repro (LLVM 18):
;   llvm-as-18 statepoint_deopt_constantindex.ll -o statepoint_deopt_constantindex.bc
;   opt-18 -passes=rewrite-statepoints-for-gc statepoint_deopt_constantindex.bc -o statepoint_deopt_constantindex.sp.bc
;   llc-18 -O2 -filetype=obj statepoint_deopt_constantindex.sp.bc -o statepoint_deopt_constantindex.o
;   llvm-readobj-18 --stackmap statepoint_deopt_constantindex.o
;
target triple = "x86_64-pc-linux-gnu"

declare void @callee()

define ptr addrspace(1) @foo(ptr addrspace(1) %obj) gc "statepoint-example" {
entry:
  ; Force a deopt constant that doesn't fit in the 32-bit "small constant" field.
  ; 0x123456789abcdef0
  call void @callee() ["deopt"(i64 1311768467463790320)]
  ret ptr addrspace(1) %obj
}

