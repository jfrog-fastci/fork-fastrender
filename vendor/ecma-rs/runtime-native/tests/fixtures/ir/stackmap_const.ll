; This fixture exists solely to generate a stable `.llvm_stackmaps` section for tests.
; Regenerate `tests/fixtures/bin/stackmap_const_x86_64.bin` with:
;   bash tests/fixtures/gen.sh
;
; The stackmap varargs include:
;   - i64 0x1122334455667788 (doesn't fit in i32 => encoded as ConstIndex + constant pool entry)
;   - i64 7                  (fits in i32 => encoded inline as Constant)
;
; Note: LLVM IR doesn't support integer hex literals directly; use the decimal form.
source_filename = "stackmap_const"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define void @stackmap_const() {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 1, i32 0, i64 1234605616436508552, i64 7)
  ret void
}

