// META: script=/resources/testharness.js

test(() => {
  const node = document.createElement("div");
  const a = node.childNodes;
  const b = node.childNodes;
  assert_equals(a, b);
}, "Node.childNodes is [SameObject]");

test(() => {
  const node = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createTextNode("hello");
  const c = document.createComment("c");
  node.appendChild(a);
  node.appendChild(b);
  node.appendChild(c);

  const list = node.childNodes;
  assert_equals(typeof list.item, "function", "NodeList.item should be a function");
  assert_equals(list.length, 3);
  assert_equals(list.item(0), a, "NodeList.item(0)");
  assert_equals(list.item(1), b, "NodeList.item(1)");
  assert_equals(list.item(2), c, "NodeList.item(2)");
  assert_equals(list.item(3), null, "NodeList.item(out of range) is null");

  // NodeList has indexed property access via its WebIDL getter.
  assert_equals(list[0], a, "NodeList[0]");
  assert_equals(list[1], b, "NodeList[1]");
  assert_equals(list[2], c, "NodeList[2]");
}, "Node.childNodes returns a NodeList of all child nodes");

test(() => {
  const node = document.createElement("div");
  const list = node.childNodes;

  assert_equals(list.length, 0);

  const a = document.createElement("span");
  node.appendChild(a);
  assert_equals(list.length, 1);
  assert_equals(typeof list.item, "function", "NodeList.item should be a function");
  assert_equals(list.item(0), a, "NodeList.item(0) after appendChild");

  const b = document.createElement("span");
  node.appendChild(b);
  assert_equals(list.length, 2);
  assert_equals(list.item(1), b, "NodeList.item(1) after second appendChild");

  node.removeChild(a);
  assert_equals(list.length, 1);
  assert_equals(list.item(0), b, "NodeList.item(0) after removeChild");
}, "Node.childNodes NodeList updates live on DOM mutation");

test(() => {
  const node = document.createElement("div");
  const list = node.childNodes;
  assert_equals(list.length, 0);

  node.innerHTML = "<span></span>";
  assert_equals(node.childNodes, list, "Node.childNodes is [SameObject] after innerHTML");
  assert_equals(list.length, 1, "NodeList.length after innerHTML insertion");
  assert_equals(list[0], node.firstChild, "NodeList[0] tracks firstChild after innerHTML insertion");

  node.innerHTML = "";
  assert_equals(list.length, 0, "NodeList.length after innerHTML clears children");
}, "Node.childNodes updates live on Element.innerHTML mutations");

test(() => {
  const parent = document.createElement("div");
  parent.innerHTML = "<p></p>";
  const ref = parent.firstChild;

  const parent_list = parent.childNodes;
  const ref_list = ref.childNodes;

  assert_equals(parent_list.length, 1);
  assert_equals(ref_list.length, 0);

  ref.insertAdjacentHTML("beforebegin", "<span></span>");
  assert_equals(parent_list.length, 2, "parent.childNodes length after beforebegin");
  assert_equals(parent_list[0], parent.firstChild, "parent.childNodes[0] after beforebegin");
  assert_equals(parent_list[1], ref, "parent.childNodes[1] after beforebegin");
  assert_equals(ref_list.length, 0, "ref.childNodes unchanged by beforebegin");

  ref.insertAdjacentHTML("afterend", "<span></span>");
  assert_equals(parent_list.length, 3, "parent.childNodes length after afterend");
  assert_equals(parent_list[2], parent.lastChild, "parent.childNodes[2] after afterend");

  ref.insertAdjacentHTML("afterbegin", "<em></em>");
  assert_equals(ref_list.length, 1, "ref.childNodes length after afterbegin");
  assert_equals(ref_list[0], ref.firstChild, "ref.childNodes[0] after afterbegin");

  ref.insertAdjacentHTML("beforeend", "<strong></strong>");
  assert_equals(ref_list.length, 2, "ref.childNodes length after beforeend");
  assert_equals(ref_list[1], ref.lastChild, "ref.childNodes[1] after beforeend");
}, "Node.childNodes updates live on Element.insertAdjacentHTML mutations");

test(() => {
  const parent = document.createElement("div");
  parent.innerHTML = "<a></a><b></b>";

  const a = parent.firstChild;
  const list = parent.childNodes;

  assert_equals(list.length, 2);
  a.outerHTML = "<span></span><span></span>";

  assert_equals(list.length, 3, "parent.childNodes length after outerHTML replaces node with fragment");
  assert_equals(list[0], parent.firstChild, "parent.childNodes[0] after outerHTML");
  assert_equals(list[2], parent.lastChild, "parent.childNodes[2] after outerHTML");
}, "Node.childNodes updates live on Element.outerHTML mutations");

test(() => {
  const node = document.createElement("div");
  const list = node.childNodes;

  // Do not touch list.length or list.item() before mutating the DOM; indexed access should still be
  // live and reflect the current tree.
  node.innerHTML = "<span></span><span></span>";
  assert_equals(list[0], node.firstChild, "childNodes[0] after insertion");
  assert_equals(list[1], node.lastChild, "childNodes[1] after insertion");

  node.innerHTML = "<p></p>";
  assert_equals(list[0], node.firstChild, "childNodes[0] after shrink");
  assert_equals(list[1], undefined, "childNodes[1] should be undefined after shrink");
}, "Node.childNodes indexed property access stays live without calling length/item");
