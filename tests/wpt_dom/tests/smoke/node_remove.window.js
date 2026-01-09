test(() => {
  const parent = document.createElement("div");
  const child = document.createElement("span");
  parent.appendChild(child);

  assert_equals(parent.childNodes.length, 1);
  assert_equals(parent.childNodes[0], child);
  assert_equals(child.parentNode, parent);

  const ret = child.remove();
  assert_equals(ret, undefined, "remove() should return undefined");

  assert_equals(parent.childNodes.length, 0);
  assert_equals(child.parentNode, null);
}, "Node.remove detaches the node from its parent");

test(() => {
  const detached = document.createElement("p");
  detached.remove();
  assert_equals(detached.parentNode, null, "removing a detached node is a no-op");
}, "Node.remove is a no-op for detached nodes");

test(() => {
  const frag = document.createDocumentFragment();
  const child = document.createElement("span");
  frag.appendChild(child);
  assert_equals(frag.childNodes.length, 1);
  child.remove();
  assert_equals(frag.childNodes.length, 0);
  assert_equals(child.parentNode, null);
}, "Node.remove works for nodes inside a DocumentFragment");

