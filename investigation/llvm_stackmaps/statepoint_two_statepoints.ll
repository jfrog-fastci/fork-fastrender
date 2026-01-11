; Minimal LLVM IR with multiple statepoints in one function.
;
; Repro (LLVM 18):
;   llvm-as-18 statepoint_two_statepoints.ll -o statepoint_two_statepoints.bc
;   opt-18 -passes=rewrite-statepoints-for-gc statepoint_two_statepoints.bc -o statepoint_two_statepoints.sp.bc
;   llc-18 -O2 -filetype=obj statepoint_two_statepoints.sp.bc -o statepoint_two_statepoints.o
;   llvm-readobj-18 --stackmap statepoint_two_statepoints.o
;
; Notes:
; - By default, roots are typically spilled (`Indirect`).
; - With CSR/register fixup enabled, you may get `Register` roots, but
;   `-fixup-max-csr-statepoints` can affect whether LLVM keeps roots in
;   callee-saved registers when there are multiple statepoints.
;
target triple = "x86_64-pc-linux-gnu"

declare void @callee()

define ptr addrspace(1) @foo(ptr addrspace(1) %obj) gc "statepoint-example" {
entry:
  call void @callee()
  call void @callee()
  ret ptr addrspace(1) %obj
}

