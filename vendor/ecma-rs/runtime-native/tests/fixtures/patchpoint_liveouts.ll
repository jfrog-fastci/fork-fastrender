; LLVM 18 patchpoint fixture with non-empty live-out list.
;
; Generated with:
;   llvm-as-18 patchpoint_liveouts.ll -o patchpoint_liveouts.bc
;   llc-18 patchpoint_liveouts.bc -filetype=obj -o patchpoint_liveouts.o
;   llvm-objcopy-18 --dump-section .llvm_stackmaps=patchpoint_liveouts.bin patchpoint_liveouts.o

; ModuleID = 'patchpoint_liveouts_fixture'
target triple = "x86_64-unknown-linux-gnu"

declare void @llvm.experimental.patchpoint.void(i64, i32, ptr, i32, ...)

declare void @callee(i64)

define void @test(i64 %a) {
entry:
  call void (i64, i32, ptr, i32, ...) @llvm.experimental.patchpoint.void(i64 1, i32 8, ptr @callee, i32 1, i64 %a)
  ret void
}
