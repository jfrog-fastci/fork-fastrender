use crate::tree::box_tree::BoxNode;
use std::cell::RefCell;

thread_local! {
  // Stack of BoxNode identities currently being laid out as part of an overflow:auto scrollbar
  // reflow iteration. Layout for these nodes must bypass the dynamic scrollbar wrapper to avoid
  // re-entering the iteration logic, while still allowing descendants to run their own iterations.
  static AUTO_SCROLLBAR_BYPASS: RefCell<Vec<usize>> = RefCell::new(Vec::new());
}

#[inline]
fn node_key(node: &BoxNode) -> usize {
  node as *const BoxNode as usize
}

pub(crate) fn should_bypass(node: &BoxNode) -> bool {
  let key = node_key(node);
  AUTO_SCROLLBAR_BYPASS.with(|stack| stack.borrow().iter().rev().any(|id| *id == key))
}

pub(crate) fn with_bypass<R>(node: &BoxNode, f: impl FnOnce() -> R) -> R {
  let key = node_key(node);
  AUTO_SCROLLBAR_BYPASS.with(|stack| stack.borrow_mut().push(key));
  let result = f();
  AUTO_SCROLLBAR_BYPASS.with(|stack| {
    let popped = stack.borrow_mut().pop();
    debug_assert_eq!(popped, Some(key));
  });
  result
}

