; Pre-rewrite input for `rewrite-statepoints-for-gc` (LLVM 18).
;
; This file is intentionally tiny / human-edited. It is used to regenerate the
; post-rewrite fixture `complex_ptr_statepoint.ll` via:
;   vendor/ecma-rs/native-js/scripts/regenerate_complex_ptr_fixture.sh
;
; `native-js` standardizes on `gc "coreclr"` and uses `ptr addrspace(1)` for GC
; pointers (see `docs/llvm_gc_strategy.md`).

source_filename = "complex_ptr"
target datalayout = "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-n8:16:32:64-S128-ni:1"
target triple = "x86_64-unknown-linux-gnu"

declare void @leaf(ptr addrspace(1), ptr addrspace(1))
declare void @leaf2(ptr addrspace(1))
declare void @leaf6(
  ptr addrspace(1),
  ptr addrspace(1),
  ptr addrspace(1),
  ptr addrspace(1),
  ptr addrspace(1),
  ptr addrspace(1)
)

define void @inner(
  ptr addrspace(1) %a,
  ptr addrspace(1) %b,
  ptr addrspace(1) %c,
  ptr addrspace(1) %d,
  ptr addrspace(1) %e,
  ptr addrspace(1) %f
) gc "coreclr" {
entry:
  call void @leaf(ptr addrspace(1) %a, ptr addrspace(1) %b)
  call void @leaf6(
    ptr addrspace(1) %c,
    ptr addrspace(1) %d,
    ptr addrspace(1) %e,
    ptr addrspace(1) %f,
    ptr addrspace(1) %a,
    ptr addrspace(1) %b
  )
  call void @leaf2(ptr addrspace(1) %c)
  call void @leaf2(ptr addrspace(1) %d)
  call void @leaf2(ptr addrspace(1) %e)
  call void @leaf2(ptr addrspace(1) %f)
  ret void
}

define void @outer(
  ptr addrspace(1) %x,
  ptr addrspace(1) %y,
  ptr addrspace(1) %z,
  ptr addrspace(1) %w,
  ptr addrspace(1) %u,
  ptr addrspace(1) %v
) gc "coreclr" {
entry:
  call void @inner(
    ptr addrspace(1) %x,
    ptr addrspace(1) %y,
    ptr addrspace(1) %z,
    ptr addrspace(1) %w,
    ptr addrspace(1) %u,
    ptr addrspace(1) %v
  )
  call void @leaf(ptr addrspace(1) %x, ptr addrspace(1) %y)
  call void @leaf2(ptr addrspace(1) %z)
  call void @leaf2(ptr addrspace(1) %w)
  call void @leaf2(ptr addrspace(1) %u)
  call void @leaf2(ptr addrspace(1) %v)
  ret void
}

declare void @__tmp_use(...)
