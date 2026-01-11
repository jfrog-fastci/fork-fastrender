; Minimal LLVM IR demonstrating StackMap v3 "meta location #1" for statepoints:
; it records the LLVM IR calling convention ID of the original call.
;
; Repro (LLVM 18):
;   llvm-as-18 statepoint_meta_callconv_fastcc.ll -o statepoint_meta_callconv_fastcc.bc
;   opt-18 -passes=rewrite-statepoints-for-gc statepoint_meta_callconv_fastcc.bc -o statepoint_meta_callconv_fastcc.sp.bc
;   llc-18 -O2 -filetype=obj statepoint_meta_callconv_fastcc.sp.bc -o statepoint_meta_callconv_fastcc.o
;   llvm-readobj-18 --stackmap statepoint_meta_callconv_fastcc.o
;
; Expected (meta location #1):
;   #1: Constant 8, size: 8   ; fastcc
;
target triple = "x86_64-pc-linux-gnu"

declare fastcc void @callee_fast()

define ptr addrspace(1) @foo(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  call fastcc void @callee_fast()
  ret ptr addrspace(1) %obj
}
