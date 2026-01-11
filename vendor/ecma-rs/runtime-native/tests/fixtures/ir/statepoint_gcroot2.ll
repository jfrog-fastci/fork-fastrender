; This fixture exists solely to generate stable `.llvm_stackmaps` sections for tests.
; Regenerate `tests/fixtures/bin/statepoint_{x86_64,aarch64}.bin` with:
;   bash tests/fixtures/gen.sh
;
; The IR uses `llvm.gcroot` + `ptr addrspace(1)` GC pointers. `opt-18` rewrites the
; call to `@safepoint` into `llvm.experimental.gc.statepoint` + `gc.relocate`.
;
; The function is structured so that exactly 2 GC pointers are live across each
; safepoint. LLVM 18 currently emits a stackmap record with:
;   3 + 2*2 = 7 locations
; (3 internal statepoint locations + (base, derived) pairs for 2 GC pointers).
source_filename = "statepoint_gcroot2"

declare void @llvm.gcroot(ptr, ptr)

declare void @safepoint()

define i64 @statepoint_gcroot2(ptr addrspace(1) %p1, ptr addrspace(1) %p2) gc "statepoint-example" {
entry:
  %root1 = alloca ptr addrspace(1), align 8
  %root2 = alloca ptr addrspace(1), align 8
  call void @llvm.gcroot(ptr %root1, ptr null)
  call void @llvm.gcroot(ptr %root2, ptr null)

  store ptr addrspace(1) %p1, ptr %root1, align 8
  store ptr addrspace(1) %p2, ptr %root2, align 8

  ; Load both pointers before the safepoint so they are live across it.
  %v1 = load ptr addrspace(1), ptr %root1, align 8
  %v2 = load ptr addrspace(1), ptr %root2, align 8

  ; Two safepoints so the resulting `.llvm_stackmaps` section has multiple records,
  ; which is useful for testing multi-frame stack walking.
  call void @safepoint()
  call void @safepoint()

  ; Use both pointers after the safepoint so rewrite-statepoints-for-gc emits relocations.
  %i1 = ptrtoint ptr addrspace(1) %v1 to i64
  %i2 = ptrtoint ptr addrspace(1) %v2 to i64
  %sum = add i64 %i1, %i2
  ret i64 %sum
}
