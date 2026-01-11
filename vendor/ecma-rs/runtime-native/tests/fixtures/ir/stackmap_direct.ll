; This fixture exists solely to generate a stable `.llvm_stackmaps` section for tests.
; Regenerate `tests/fixtures/bin/stackmap_direct_x86_64.bin` with:
;   bash tests/fixtures/gen.sh
;
; The stackmap varargs include:
;   - `ptr %p` where `%p` is an `alloca` address. LLVM encodes this as a `Direct` location
;     (reg + offset), representing the *pointer value* itself (no memory indirection).
source_filename = "stackmap_direct"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define void @stackmap_direct() {
entry:
  %p = alloca i64
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 42, i32 0, ptr %p)
  ret void
}

