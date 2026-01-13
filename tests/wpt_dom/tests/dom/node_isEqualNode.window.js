// META: script=/resources/testharness.js

test(() => {
  const frag1 = document.createDocumentFragment();
  const div1 = document.createElement("div");
  div1.setAttribute("a", "1");
  div1.setAttribute("b", "2");
  div1.appendChild(document.createTextNode("hello"));
  frag1.appendChild(div1);

  const doc2 = document.implementation.createHTMLDocument("");
  const frag2 = doc2.createDocumentFragment();
  const div2 = doc2.createElement("div");
  // Reverse attribute order: isEqualNode compares attribute sets (order-insensitive).
  div2.setAttribute("b", "2");
  div2.setAttribute("a", "1");
  div2.appendChild(doc2.createTextNode("hello"));
  frag2.appendChild(div2);

  assert_true(frag1.isEqualNode(frag2), "fragments should compare structurally equal across documents");
  assert_true(div1.isEqualNode(div2), "elements should compare structurally equal across documents");
}, "Node.isEqualNode compares document fragments structurally and ignores attribute order");

test(() => {
  const frag1 = document.createDocumentFragment();
  frag1.appendChild(document.createTextNode("hello"));

  const frag2 = document.createDocumentFragment();
  frag2.appendChild(document.createTextNode("world"));

  assert_false(frag1.isEqualNode(frag2), "different text contents should not compare equal");
}, "Node.isEqualNode detects different text contents");
