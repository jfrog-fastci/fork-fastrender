// META: script=/resources/testharness.js
//
// NodeIterator / TreeWalker prototype plumbing for the handwritten vm-js DOM backend.

test(() => {
  const root = document.body || document.documentElement;

  // whatToShow = 1 == NodeFilter.SHOW_ELEMENT, but avoid depending on NodeFilter being present.
  const it = document.createNodeIterator(root, 1, null);
  const tw = document.createTreeWalker(root, 1, null);

  assert_true(it instanceof NodeIterator, "NodeIterator wrapper should inherit from NodeIterator.prototype");
  assert_true(tw instanceof TreeWalker, "TreeWalker wrapper should inherit from TreeWalker.prototype");

  assert_throws_js(TypeError, () => new NodeIterator(), "NodeIterator should be an illegal constructor");
  assert_throws_js(TypeError, () => new TreeWalker(), "TreeWalker should be an illegal constructor");

  // Prototype surface should be installed and non-enumerable (WebIDL-style).
  const nextDesc = Object.getOwnPropertyDescriptor(NodeIterator.prototype, "nextNode");
  assert_true(!!nextDesc, "NodeIterator.prototype.nextNode should exist");
  assert_false(nextDesc.enumerable, "NodeIterator.prototype.nextNode should be non-enumerable");

  const rootDesc = Object.getOwnPropertyDescriptor(TreeWalker.prototype, "root");
  assert_true(!!rootDesc, "TreeWalker.prototype.root should exist");
  assert_false(rootDesc.enumerable, "TreeWalker.prototype.root should be non-enumerable");
}, "NodeIterator/TreeWalker globals + prototype chains");

