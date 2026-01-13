// META: script=/resources/testharness.js

test(() => {
  assert_equals(typeof window.getSelection, "function");
  assert_equals(typeof document.getSelection, "function");

  assert_true(window.getSelection() === window.getSelection());
  assert_true(document.getSelection() === document.getSelection());
  assert_true(window.getSelection() === document.getSelection());

  const sel = window.getSelection();
  assert_equals(sel.rangeCount, 0);
  assert_equals(sel.toString(), "");
  // Calling these should be stable/no-throw, even though Selection is currently a lightweight stub.
  sel.removeAllRanges();
  sel.addRange(null);
}, "window.getSelection()/document.getSelection() minimal stub exists and is stable");
