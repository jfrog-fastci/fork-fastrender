; Generated with LLVM 18 (llc-18) and extracted via:
;   llvm-objcopy-18 --dump-section .llvm_stackmaps=stackmaps_v3.bin <obj>
;
; The IR is intentionally tiny but exercises all StackMap v3 location kinds.

; ModuleID = 'stackmaps_v3_fixture'
target triple = "x86_64-unknown-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)
declare void @dummy(i64)

define void @test(i64 %a1, i64 %a2, i64 %a3, i64 %a4, i64 %a5, i64 %a6, i64 %a7, i64 %a8) {
entry:
  %slot = alloca i8, i32 8, align 8
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 99, i32 0,
    i64 123,
    i64 1234605616436508552,
    i64 %a1,
    i64 %a7,
    ptr %slot)

  call void @dummy(i64 %a1)

  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 100, i32 0,
    i64 %a8)

  ret void
}
