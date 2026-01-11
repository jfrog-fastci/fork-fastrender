; Minimal LLVM IR that triggers StackMap v3 LocationKind=1 (Register) in a
; vector/float register (x86_64 XMM0).
;
; Repro (LLVM 18):
;   llvm-as-18 stackmap_register_xmm0.ll -o stackmap_register_xmm0.bc
;   llc-18 -O2 -filetype=obj stackmap_register_xmm0.bc -o stackmap_register_xmm0.o
;   llvm-readobj-18 --stackmap stackmap_register_xmm0.o
;
; Expected:
;   #1: Register R#17, size: 16
; where DWARF register 17 is `xmm0` on x86_64.
;
target triple = "x86_64-pc-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define <2 x i64> @foo(<2 x i64> %x) {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 43, i32 0, <2 x i64> %x)
  ret <2 x i64> %x
}

