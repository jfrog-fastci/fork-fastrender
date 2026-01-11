; This fixture exists solely to generate a `.llvm_stackmaps` section that exercises
; LLVM 18's "unknown stack size" sentinel (`u64::MAX`) for dynamically-sized stack
; frames.
;
; Regenerate `tests/fixtures/bin/statepoint_dynamic_alloca_x86_64.bin` with:
;   bash tests/fixtures/gen.sh
;
; The key property: a variable-sized `alloca` forces the per-function stackmap
; `stack_size` field to become "unknown". LLVM still emits usable GC root
; locations, typically switching to FP-based addressing (`Indirect [RBP + off]`).
;
; This allows the runtime to scan roots even without knowing the fixed frame size.
source_filename = "statepoint_dynamic_alloca"
target triple = "x86_64-unknown-linux-gnu"

declare void @llvm.gcroot(ptr, ptr)
declare void @safepoint()

define i64 @statepoint_dynamic_alloca(ptr addrspace(1) %p, i64 %n) gc "coreclr" {
entry:
  %root = alloca ptr addrspace(1), align 8
  call void @llvm.gcroot(ptr %root, ptr null)
  store ptr addrspace(1) %p, ptr %root, align 8

  ; Dynamic alloca: forces stackmap stack_size to be reported as unknown (u64::MAX).
  %buf = alloca i8, i64 %n, align 16
  ; Use the buffer so it is not trivially dead even at low optimization levels.
  store i8 0, ptr %buf, align 1

  ; Load the GC pointer before the safepoint so it is live across it.
  %v = load ptr addrspace(1), ptr %root, align 8
  call void @safepoint()

  ; Use the pointer after the safepoint so rewrite-statepoints-for-gc emits relocations.
  %i = ptrtoint ptr addrspace(1) %v to i64
  ret i64 %i
}

