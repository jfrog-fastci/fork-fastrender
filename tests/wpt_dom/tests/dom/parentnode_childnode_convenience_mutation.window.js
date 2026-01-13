// META: script=/resources/testharness.js
//
// ParentNode / ChildNode convenience DOM mutation APIs.

test(() => {
  const parent = document.createElement("div");

  const span = document.createElement("span");
  span.id = "s";
  parent.append("a", span, "b");

  assert_equals(parent.childNodes.length, 3, "append() should insert 3 nodes");
  assert_equals(parent.childNodes[0].nodeType, Node.TEXT_NODE);
  assert_equals(parent.childNodes[0].data, "a");
  assert_equals(parent.childNodes[1], span, "append() should insert the passed node");
  assert_equals(parent.childNodes[2].nodeType, Node.TEXT_NODE);
  assert_equals(parent.childNodes[2].data, "b");

  const em = document.createElement("em");
  em.id = "e";
  parent.prepend(em, "z");

  assert_equals(parent.childNodes.length, 5, "prepend() should insert 2 more nodes");
  assert_equals(parent.childNodes[0], em);
  assert_equals(parent.childNodes[1].nodeType, Node.TEXT_NODE);
  assert_equals(parent.childNodes[1].data, "z");
  assert_equals(parent.childNodes[2].nodeType, Node.TEXT_NODE);
  assert_equals(parent.childNodes[2].data, "a");
  assert_equals(parent.childNodes[3], span);
  assert_equals(parent.childNodes[4].nodeType, Node.TEXT_NODE);
  assert_equals(parent.childNodes[4].data, "b");
}, "ParentNode.append/prepend insert nodes and convert strings to Text");

test(() => {
  const node = document.createElement("div");
  assert_equals(node.parentNode, null, "sanity: detached node has no parent");
  node.remove();
  assert_equals(node.parentNode, null, "remove() on a detached node should be a no-op");
}, "ChildNode.remove() is a no-op when the node is detached");

test(() => {
  const parent = document.createElement("div");
  const a = document.createElement("span");
  a.id = "a";
  const b = document.createElement("span");
  b.id = "b";
  const c = document.createElement("span");
  c.id = "c";
  parent.append(a, b, c);

  const em = document.createElement("em");
  em.id = "x";
  b.replaceWith("t", em);

  assert_equals(parent.childNodes.length, 4, "replaceWith() should replace the node with 2 nodes");
  assert_equals(parent.childNodes[0], a);
  assert_equals(parent.childNodes[1].nodeType, Node.TEXT_NODE);
  assert_equals(parent.childNodes[1].data, "t");
  assert_equals(parent.childNodes[2], em);
  assert_equals(parent.childNodes[3], c);
  assert_equals(b.parentNode, null, "replaced node should be detached");
}, "ChildNode.replaceWith() replaces a node with multiple nodes including strings");
