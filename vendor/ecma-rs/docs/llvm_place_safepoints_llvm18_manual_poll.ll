; Example of the recommended LLVM-18-compatible strategy:
; insert an explicit fast poll at a loop backedge and have the slow-path call a
; runtime function that will be rewritten to a statepoint by
; `rewrite-statepoints-for-gc`.
;
; Usage (from repo root):
;   opt-18 -S -passes=rewrite-statepoints-for-gc vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_manual_poll.ll -o /tmp/out.ll

source_filename = "llvm_place_safepoints_llvm18_manual_poll"

@gc_requested = external global i1

declare void @rt_gc_safepoint()

define void @foo(i1 %cond) gc "statepoint-example" {
entry:
  br label %loop

loop:
  ; Fast path is just a load+branch.
  %flag = load i1, ptr @gc_requested
  br i1 %flag, label %poll_slow, label %poll_fast

poll_slow:
  ; Slow path call becomes a `llvm.experimental.gc.statepoint.*`.
  call void @rt_gc_safepoint()
  br label %poll_fast

poll_fast:
  br i1 %cond, label %loop, label %exit

exit:
  ret void
}

