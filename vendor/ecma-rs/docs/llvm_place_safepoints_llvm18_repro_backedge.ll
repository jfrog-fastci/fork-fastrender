; Crash reproducer for LLVM 18.1.3 `place-safepoints` when it needs to insert
; a backedge poll (loop with no calls).
;
; Usage (from repo root):
;   opt-18 -S -passes=place-safepoints -spp-no-entry vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_repro_backedge.ll -o /tmp/out.ll
;
; Expected: `place-safepoints` inserts a poll safepoint on the loop backedge.
; Actual (LLVM 18.1.3): opt segfaults inside `llvm::PlaceSafepointsPass::runImpl`.
;
; Workaround: predeclare `declare void @gc.safepoint_poll()` in the module before
; running `place-safepoints`.

source_filename = "llvm_place_safepoints_llvm18_repro_backedge"

define void @foo(i1 %cond) gc "statepoint-example" {
entry:
  br label %loop

loop:
  br i1 %cond, label %loop, label %exit

exit:
  ret void
}
