// META: script=/resources/testharness.js
//
// Modern DOM convenience mutation APIs (ParentNode.append).

function clear_children(node) {
  while (node.firstChild) {
    node.removeChild(node.firstChild);
  }
}

test(() => {
  const body = document.body;
  clear_children(body);

  const parent = document.createElement("div");
  body.appendChild(parent);

  const middle = document.createElement("span");
  middle.id = "middle";

  const ret = parent.append("a", middle, "c");
  assert_equals(ret, undefined, "append() should return undefined");

  assert_equals(parent.childNodes.length, 3, "expected three childNodes after append()");
  assert_equals(parent.childNodes[0].nodeType, Node.TEXT_NODE, "expected first child to be Text");
  assert_equals(parent.childNodes[0].data, "a", "expected first Text node data");
  assert_equals(parent.childNodes[1], middle, "expected middle element to be inserted as-is");
  assert_equals(parent.childNodes[2].nodeType, Node.TEXT_NODE, "expected last child to be Text");
  assert_equals(parent.childNodes[2].data, "c", "expected last Text node data");
}, "ParentNode.append inserts nodes in order and converts non-Nodes to Text");

test(() => {
  const body = document.body;
  clear_children(body);

  const parent = document.createElement("div");
  body.appendChild(parent);

  const fragment = document.createDocumentFragment();
  const a = document.createElement("span");
  a.id = "a";
  const b = document.createElement("span");
  b.id = "b";
  fragment.appendChild(a);
  fragment.appendChild(b);

  const ret = parent.append(fragment);
  assert_equals(ret, undefined, "append() should return undefined");

  assert_equals(parent.childNodes.length, 2, "expected fragment children to be inserted");
  assert_equals(parent.childNodes[0], a, "expected first inserted child to be a");
  assert_equals(parent.childNodes[1], b, "expected second inserted child to be b");

  assert_equals(fragment.childNodes.length, 0, "expected fragment to be empty after insertion");
  assert_equals(fragment.parentNode, null, "DocumentFragment must not be inserted into the tree");
}, "ParentNode.append inserts DocumentFragment children and empties the fragment");

test(() => {
  const body = document.body;
  clear_children(body);

  const parent = document.createElement("div");
  body.appendChild(parent);

  const doc2 = new DOMParser().parseFromString("<!doctype html><p>hi</p>", "text/html");
  const foreign = doc2.createElement("p");
  foreign.appendChild(doc2.createTextNode("hello"));

  assert_equals(foreign.ownerDocument, doc2, "expected foreign node to start in doc2");

  parent.append(foreign);
  assert_equals(foreign.parentNode, parent, "expected foreign node to be inserted into parent");
  assert_equals(foreign.ownerDocument, document, "expected foreign node to be adopted into document");
  assert_equals(
    foreign.firstChild.ownerDocument,
    document,
    "expected descendants to also be adopted into document"
  );
}, "ParentNode.append adopts nodes created in another document");
