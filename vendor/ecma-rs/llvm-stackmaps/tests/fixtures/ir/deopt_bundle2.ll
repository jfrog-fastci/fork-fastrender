; ModuleID = 'deopt_bundle2'
; Regenerate the corresponding `.llvm_stackmaps` fixture with:
;   bash vendor/ecma-rs/llvm-stackmaps/tests/fixtures/gen.sh
source_filename = "deopt_bundle2"

declare void @llvm.gcroot(ptr, ptr)
declare void @safepoint()

define i64 @foo(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  %root = alloca ptr addrspace(1), align 8
  call void @llvm.gcroot(ptr %root, ptr null)
  store ptr addrspace(1) %obj, ptr %root, align 8
  %v = load ptr addrspace(1), ptr %root, align 8

  ; After `rewrite-statepoints-for-gc`, LLVM18 encodes this as:
  ;   locations[2] = deopt_count (=2)
  ;   locations[3..5) = deopt operand locations (constants here)
  call void @safepoint() [ "deopt"(i64 1, i64 2) ]

  %i = ptrtoint ptr addrspace(1) %v to i64
  ret i64 %i
}

