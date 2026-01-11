; Minimal LLVM IR that triggers StackMap v3 LocationKind=1 (Register) via
; `llvm.experimental.stackmap` (not a statepoint).
;
; Repro (LLVM 18):
;   llvm-as-18 stackmap_register.ll -o stackmap_register.bc
;   llc-18 -O2 -filetype=obj stackmap_register.bc -o stackmap_register.o
;   llvm-readobj-18 --stackmap stackmap_register.o
;
target triple = "x86_64-pc-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define i64 @foo(i64 %x) {
entry:
  ; Record %x at this point. At -O2, it is typically held in a register
  ; (e.g. RAX), yielding LocationKind=1 (Register).
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 42, i32 0, i64 %x)
  ret i64 %x
}

