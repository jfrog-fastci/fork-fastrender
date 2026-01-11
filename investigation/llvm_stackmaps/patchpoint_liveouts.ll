; Minimal LLVM IR demonstrating StackMap v3 `LiveOuts[]` encoding via
; `llvm.experimental.patchpoint`.
;
; This is not a GC statepoint, but uses the same `.llvm_stackmaps` section
; format, including the per-record live-out register list.
;
; Repro (LLVM 18):
;   llvm-as-18 patchpoint_liveouts.ll -o patchpoint_liveouts.bc
;   llc-18 -O2 -filetype=obj patchpoint_liveouts.bc -o patchpoint_liveouts.o
;   llvm-readobj-18 --stackmap patchpoint_liveouts.o
;   llvm-readobj-18 -x .llvm_stackmaps patchpoint_liveouts.o
;
; Expected (example):
;   1 location (recorded operand) + 2 live-outs (rax + rsp)
;
target triple = "x86_64-pc-linux-gnu"

declare void @callee()

declare i64 @llvm.experimental.patchpoint.i64(i64, i32, ptr, i32, ...)

define i64 @foo(i64 %x) {
entry:
  ; numArgs=0, but record %x as an additional stackmap operand.
  %r = call i64 (i64, i32, ptr, i32, ...) @llvm.experimental.patchpoint.i64(i64 55, i32 16, ptr @callee, i32 0, i64 %x)
  ret i64 %r
}

