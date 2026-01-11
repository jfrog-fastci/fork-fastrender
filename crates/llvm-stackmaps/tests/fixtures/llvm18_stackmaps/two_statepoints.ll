; ModuleID = 'two_statepoints'
target triple = "x86_64-pc-linux-gnu"

declare void @callee()

define ptr addrspace(1) @foo(ptr addrspace(1) %obj) gc "statepoint-example" {
entry:
  call void @callee()
  call void @callee()
  ret ptr addrspace(1) %obj
}
