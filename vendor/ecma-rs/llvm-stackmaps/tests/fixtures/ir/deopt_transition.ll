; ModuleID = 'deopt_transition'
; Regenerate the corresponding `.llvm_stackmaps` fixture with:
;   bash vendor/ecma-rs/llvm-stackmaps/tests/fixtures/gen.sh
source_filename = "deopt_transition"

declare void @llvm.gcroot(ptr, ptr)
declare void @safepoint()

define i64 @foo(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  %root = alloca ptr addrspace(1), align 8
  call void @llvm.gcroot(ptr %root, ptr null)
  store ptr addrspace(1) %obj, ptr %root, align 8
  %v = load ptr addrspace(1), ptr %root, align 8

  call void @safepoint() [ "deopt"(i64 1, i64 2), "gc-transition"(i64 99) ]

  %i = ptrtoint ptr addrspace(1) %v to i64
  ret i64 %i
}

