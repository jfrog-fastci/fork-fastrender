; Demonstrates that `rewrite-statepoints-for-gc` can turn callsites into statepoints
; without needing `place-safepoints`.
;
; Usage (from repo root):
;   opt-18 -S -passes=rewrite-statepoints-for-gc vendor/ecma-rs/docs/llvm_place_safepoints_llvm18_repro_call.ll -o /tmp/out.ll
;
; Note: this file is *not* expected to crash.

source_filename = "llvm_place_safepoints_llvm18_repro_call"

declare void @bar()

define void @foo() gc "coreclr" {
entry:
  call void @bar()
  ret void
}
