; This fixture exists solely to generate a stable `.llvm_stackmaps` section for tests.
; Regenerate `tests/fixtures/bin/stackmap_dup_id_two_funcs_x86_64.bin` with:
;   bash tests/fixtures/gen.sh
;
; This module defines two independent functions, each containing one stackmap callsite. LLVM emits
; a single StackMap v3 blob whose `NumFunctions` is the number of functions that contain stackmaps,
; and associates the record stream to functions *only* via `FunctionRecord.RecordCount`.
;
; Like real-world LLVM 18 statepoint output, both callsites use the same `patchpoint_id` to ensure
; parsers do not assume that the `patchpoint_id`/Record ID is unique.
source_filename = "stackmap_dup_id_two_funcs"
target triple = "x86_64-unknown-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define void @foo() {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 7, i32 0)
  ret void
}

define void @bar() {
entry:
  ; Make this function's frame layout different so we can detect record-to-function association.
  %buf = alloca i8, i32 64, align 16
  store i8 1, ptr %buf, align 1
  %v = load i8, ptr %buf, align 1
  %v2 = add i8 %v, 1
  store i8 %v2, ptr %buf, align 1
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 7, i32 0)
  ret void
}

