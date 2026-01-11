; Example of the recommended LLVM-18-compatible strategy:
; insert an explicit fast poll at a loop backedge and have the slow-path call a
; runtime function that will be rewritten to a statepoint by
; `rewrite-statepoints-for-gc`.
;
; Usage (from repo root):
;   opt-18 -S -passes=rewrite-statepoints-for-gc vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_manual_poll.ll -o /tmp/out.ll

source_filename = "llvm_place_safepoints_llvm18_manual_poll"

; Global safepoint epoch (runtime-native):
;   - even: no stop-the-world requested
;   - odd:  stop-the-world requested
@RT_GC_EPOCH = external global i64

; Slow-path entrypoint (runtime-native). Must be called with the *observed odd*
; epoch value so the runtime can safely coordinate a stop-the-world.
declare void @rt_gc_safepoint_slow(i64)

define void @foo(i1 %cond) gc "coreclr" {
entry:
  br label %loop

loop:
  ; Fast path is just a load+branch.
  %epoch = load atomic i64, ptr @RT_GC_EPOCH acquire, align 8
  %lowbit = and i64 %epoch, 1
  %flag = icmp ne i64 %lowbit, 0
  br i1 %flag, label %poll_slow, label %poll_fast

poll_slow:
  ; Slow path call becomes a `llvm.experimental.gc.statepoint.*`.
  call void @rt_gc_safepoint_slow(i64 %epoch)
  br label %poll_fast

poll_fast:
  br i1 %cond, label %loop, label %exit

exit:
  ret void
}
