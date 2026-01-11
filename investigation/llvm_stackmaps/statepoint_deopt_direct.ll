; Minimal LLVM IR that triggers StackMap v3 LocationKind=2 (Direct) via
; `rewrite-statepoints-for-gc` on x86_64.
;
; Repro (LLVM 18):
;   llvm-as-18 statepoint_deopt_direct.ll -o statepoint_deopt_direct.bc
;   opt-18 -passes=rewrite-statepoints-for-gc statepoint_deopt_direct.bc -o statepoint_deopt_direct.sp.bc
;   llc-18 -O2 -filetype=obj statepoint_deopt_direct.sp.bc -o statepoint_deopt_direct.o
;   llvm-readobj-18 --stackmap statepoint_deopt_direct.o
;
target triple = "x86_64-pc-linux-gnu"

declare void @callee()

define ptr addrspace(1) @foo(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  %slot = alloca i64, align 8
  ; Record the *address* of a stack slot in deopt info.
  ;
  ; In the emitted stackmap, this shows up as:
  ;   Direct R#7 + <offset>
  call void @callee() ["deopt"(ptr %slot)]
  ret ptr addrspace(1) %obj
}
