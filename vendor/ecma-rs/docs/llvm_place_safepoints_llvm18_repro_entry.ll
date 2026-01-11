; Minimal crash reproducer for LLVM 18.1.3 `place-safepoints`.
;
; Usage (from repo root):
;   opt-18 -S -passes=place-safepoints vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_repro_entry.ll -o /tmp/out.ll
;
; Expected: `place-safepoints` inserts a poll safepoint at function entry.
; Actual (LLVM 18.1.3): opt segfaults inside `llvm::PlaceSafepointsPass::runImpl`.

source_filename = "llvm_place_safepoints_llvm18_repro_entry"

define void @foo() gc "statepoint-example" {
entry:
  ret void
}

