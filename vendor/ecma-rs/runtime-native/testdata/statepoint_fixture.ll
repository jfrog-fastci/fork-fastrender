; LLVM IR fixture used by `runtime-native/tests/stackmap_fixture.rs`.
;
; This file is intentionally **post-`rewrite-statepoints-for-gc`**: it contains
; an explicit `llvm.experimental.gc.statepoint` call with a `"gc-live"` operand
; bundle containing 2 GC pointers.
;
; Regenerate the checked-in object file:
;   llc-18 -O0 -filetype=obj -o statepoint_fixture.o statepoint_fixture.ll
;
; Target: x86_64 Linux (SysV)
target triple = "x86_64-unknown-linux-gnu"
target datalayout = "e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-i128:128-f80:128-n8:16:32:64-S128"

declare void @callee()

define ptr addrspace(1) @test(ptr addrspace(1) %a, ptr addrspace(1) %b) gc "statepoint-example" {
entry:
  %statepoint_token = call token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(
    i64 2882400000,
    i32 0,
    ptr elementtype(void ()) @callee,
    i32 0,
    i32 0,
    i32 0,
    i32 0
  ) [ "gc-live"(ptr addrspace(1) %a, ptr addrspace(1) %b) ]

  %a.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(
    token %statepoint_token,
    i32 0,
    i32 0
  )
  %b.relocated = call coldcc ptr addrspace(1) @llvm.experimental.gc.relocate.p1(
    token %statepoint_token,
    i32 1,
    i32 1
  )

  %cmp = icmp eq ptr addrspace(1) %a.relocated, null
  %out = select i1 %cmp, ptr addrspace(1) %a.relocated, ptr addrspace(1) %b.relocated
  ret ptr addrspace(1) %out
}

declare token @llvm.experimental.gc.statepoint.p0(i64 immarg, i32 immarg, ptr, i32 immarg, i32 immarg, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32 immarg, i32 immarg)
