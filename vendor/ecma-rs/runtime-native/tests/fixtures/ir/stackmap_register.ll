; This fixture exists solely to generate a stable `.llvm_stackmaps` section for tests.
; Regenerate `tests/fixtures/bin/stackmap_register_x86_64.bin` with:
;   bash tests/fixtures/gen.sh
;
; The stackmap varargs include:
;   - `i64 %x` where `%x` is an argument passed in a register. LLVM encodes this as a `Register`
;     location.
source_filename = "stackmap_register"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define void @stackmap_register(i64 %x) {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 1, i32 0, i64 %x)
  ret void
}

