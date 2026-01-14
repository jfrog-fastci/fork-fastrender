// META: script=/resources/testharness.js

test(() => {
  const a = document.createElement("div");
  a.setAttribute("id", "x");
  a.appendChild(document.createTextNode("hello"));

  const b = document.createElement("div");
  b.setAttribute("id", "x");
  b.appendChild(document.createTextNode("hello"));

  assert_true(a.isEqualNode(b), "equivalent trees should be equal");
  assert_false(a.isEqualNode({}), "non-Node arguments should compare unequal");
  assert_false(a.isEqualNode(null), "null should compare unequal");

  b.setAttribute("id", "y");
  assert_false(a.isEqualNode(b), "attribute mismatch should make nodes unequal");

  b.setAttribute("id", "x");
  b.firstChild.textContent = "world";
  assert_false(a.isEqualNode(b), "text data mismatch should make nodes unequal");
}, "Node.isEqualNode performs deep structural equality");

test(() => {
  const frag1 = document.createDocumentFragment();
  frag1.appendChild(document.createElement("a"));
  frag1.appendChild(document.createElement("b"));

  const frag2 = document.createDocumentFragment();
  // Same children, different order.
  frag2.appendChild(document.createElement("b"));
  frag2.appendChild(document.createElement("a"));

  assert_false(frag1.isEqualNode(frag2), "different child order should compare unequal");
}, "Node.isEqualNode detects different child order");

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

  assert_true(
    frag1.isEqualNode(frag2),
    "fragments should compare structurally equal across documents",
  );
  assert_true(div1.isEqualNode(div2), "elements should compare structurally equal across documents");

  div2.firstChild.textContent = "world";
  assert_false(
    frag1.isEqualNode(frag2),
    "changing a descendant should make fragments compare unequal",
  );
}, "Node.isEqualNode works across documents and ignores attribute order");

test(() => {
  const a = document.createComment("hello");
  const b = document.createComment("hello");
  assert_true(a.isEqualNode(b), "equivalent comments should be equal");
  b.data = "world";
  assert_false(a.isEqualNode(b), "comment data mismatch should compare unequal");
}, "Node.isEqualNode compares Comment.data");

test(() => {
  const SVG_NS = "http://www.w3.org/2000/svg";
  const a = document.createElementNS(SVG_NS, "svg:svg");
  const b = document.createElementNS(SVG_NS, "svg:svg");
  assert_true(a.isEqualNode(b), "equivalent namespaced elements should be equal");
  const c = document.createElementNS(SVG_NS, "svg");
  assert_false(a.isEqualNode(c), "prefix mismatch should compare unequal");
}, "Node.isEqualNode compares element namespaces and prefixes");
