; Minimal LLVM IR that triggers StackMap v3 LocationKind=1 (Register) for a
; *statepoint* `gc-live` root on x86_64.
;
; By default, LLVM tends to spill GC values to the stack and emit `Indirect`
; locations. To get `Register`, we need to allow keeping some GC values in
; callee-saved registers during the statepoint fixup phase.
;
; Repro (LLVM 18):
;   llvm-as-18 statepoint_gc_live_register.ll -o statepoint_gc_live_register.bc
;   opt-18 -passes=rewrite-statepoints-for-gc statepoint_gc_live_register.bc -o statepoint_gc_live_register.sp.bc
;   llc-18 -O2 -filetype=obj \
;     -fixup-allow-gcptr-in-csr \
;     -max-registers-for-gc-values=1 \
;     statepoint_gc_live_register.sp.bc -o statepoint_gc_live_register.o
;   llvm-readobj-18 --stackmap statepoint_gc_live_register.o
;
target triple = "x86_64-pc-linux-gnu"

declare void @callee()

define ptr addrspace(1) @foo(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  call void @callee()
  ret ptr addrspace(1) %obj
}
