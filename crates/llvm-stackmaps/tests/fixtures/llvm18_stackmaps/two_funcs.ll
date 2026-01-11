; ModuleID = 'two_funcs'
target triple = "x86_64-pc-linux-gnu"

declare void @callee()

define ptr addrspace(1) @foo(ptr addrspace(1) %obj) gc "statepoint-example" {
entry:
  call void @callee()
  ret ptr addrspace(1) %obj
}

define ptr addrspace(1) @bar(ptr addrspace(1) %obj) gc "statepoint-example" {
entry:
  call void @callee()
  ret ptr addrspace(1) %obj
}
