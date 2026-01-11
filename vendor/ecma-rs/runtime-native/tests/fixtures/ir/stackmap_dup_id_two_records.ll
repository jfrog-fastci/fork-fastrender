; This fixture exists solely to generate a stable `.llvm_stackmaps` section for tests.
; Regenerate `tests/fixtures/bin/stackmap_dup_id_two_records_x86_64.bin` with:
;   bash tests/fixtures/gen.sh
;
; This module intentionally emits *two* `llvm.experimental.stackmap` callsites with the *same*
; `patchpoint_id`. LLVM permits this, and in practice LLVM 18 emits repeated IDs for GC statepoints
; too. The runtime stackmap parser must therefore **not** assume `patchpoint_id` uniqueness and must
; index records by callsite PC (function address + instruction offset).
source_filename = "stackmap_dup_id_two_records"
target triple = "x86_64-unknown-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)
declare void @dummy()

define void @two_records_same_id() {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 7, i32 0)
  call void @dummy()
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 7, i32 0)
  ret void
}
