// META: script=/resources/testharness.js

test(() => {
  const parent = document.createElement("div");
  const a = parent.children;
  const b = parent.children;
  assert_equals(a, b);
}, "Element.children is [SameObject]");

test(() => {
  const parent = document.createElement("div");
  const a = document.createElement("span");
  const text = document.createTextNode("t");
  const b = document.createElement("span");
  parent.appendChild(a);
  parent.appendChild(text);
  parent.appendChild(b);

  assert_true(parent.children !== null && parent.children !== undefined, "Element.children should exist");
  assert_equals(typeof parent.children.item, "function", "HTMLCollection.item should be a function");

  assert_equals(parent.childElementCount, 2, "childElementCount");
  assert_equals(parent.children.length, 2, "children.length");
  assert_equals(parent.children.item(0), a, "children.item(0)");
  assert_equals(parent.children.item(1), b, "children.item(1)");

  // HTMLCollection has indexed property access via its WebIDL getter.
  assert_equals(parent.children[0], a, "children[0]");
  assert_equals(parent.children[1], b, "children[1]");

  assert_equals(parent.firstElementChild, a, "firstElementChild");
  assert_equals(parent.lastElementChild, b, "lastElementChild");
  assert_equals(a.nextElementSibling, b, "nextElementSibling");
  assert_equals(b.previousElementSibling, a, "previousElementSibling");
}, "Element.children and element-sibling traversal ignore non-element nodes");

test(() => {
  const host = document.createElement("div");
  const shadow = host.attachShadow({ mode: "open" });
  const a = document.createElement("span");
  shadow.appendChild(a);

  // Light DOM element traversal must ignore the internal ShadowRoot node stored under its host.
  assert_equals(host.children.length, 0, "host.children should not expose ShadowRoot");
  assert_equals(host.childElementCount, 0, "host.childElementCount should not count ShadowRoot");
  assert_equals(host.firstElementChild, null, "host.firstElementChild should ignore ShadowRoot");
  assert_equals(host.lastElementChild, null, "host.lastElementChild should ignore ShadowRoot");

  // ShadowRoot implements ParentNode (via DocumentFragment); its element traversal APIs should
  // operate over the shadow tree.
  assert_equals(shadow.children.length, 1, "shadow.children.length");
  assert_equals(shadow.childElementCount, 1, "shadow.childElementCount");
  assert_equals(shadow.firstElementChild, a, "shadow.firstElementChild");
  assert_equals(shadow.lastElementChild, a, "shadow.lastElementChild");
  assert_equals(shadow.children.item(0), a, "shadow.children.item(0)");
  assert_equals(shadow.children[0], a, "shadow.children[0]");
}, "ShadowRoot supports ParentNode element traversal while host Element traversal ignores shadow root");

test(() => {
  const parent = document.createElement("div");
  const children = parent.children;

  assert_true(children !== null && children !== undefined, "Element.children should exist");
  assert_equals(typeof children.item, "function", "HTMLCollection.item should be a function");

  assert_equals(children.length, 0);
  assert_equals(parent.firstElementChild, null);
  assert_equals(parent.lastElementChild, null);

  const a = document.createElement("span");
  parent.appendChild(a);
  assert_equals(children.length, 1);
  assert_equals(children.item(0), a, "children.item(0) after appendChild");
  assert_equals(parent.firstElementChild, a);
  assert_equals(parent.lastElementChild, a);

  const b = document.createElement("span");
  parent.appendChild(b);
  assert_equals(children.length, 2);
  assert_equals(children.item(1), b, "children.item(1) after second appendChild");
  assert_equals(parent.firstElementChild, a);
  assert_equals(parent.lastElementChild, b);

  parent.removeChild(a);
  assert_equals(children.length, 1);
  assert_equals(children.item(0), b, "children.item(0) after removeChild");
  assert_equals(parent.firstElementChild, b);
  assert_equals(parent.lastElementChild, b);
}, "Element.children HTMLCollection updates live on DOM mutation");

test(() => {
  const parent = document.createElement("div");
  const children = parent.children;
  assert_equals(children.length, 0);

  parent.innerHTML = "<span></span><span></span>";
  assert_equals(parent.children, children, "Element.children is [SameObject] after innerHTML");
  assert_equals(children.length, 2, "HTMLCollection.length after innerHTML insertion");
  assert_equals(children[0], parent.firstElementChild, "children[0] tracks firstElementChild");
  assert_equals(children[1], parent.lastElementChild, "children[1] tracks lastElementChild");

  parent.innerHTML = "";
  assert_equals(children.length, 0, "HTMLCollection.length after innerHTML clears children");
}, "Element.children updates live on Element.innerHTML mutations");

test(() => {
  const parent = document.createElement("div");
  parent.innerHTML = "<p></p>";
  const ref = parent.firstElementChild;

  const parent_children = parent.children;
  const ref_children = ref.children;

  assert_equals(parent_children.length, 1);
  assert_equals(ref_children.length, 0);

  ref.insertAdjacentHTML("beforebegin", "<span></span>");
  assert_equals(parent_children.length, 2, "parent.children length after beforebegin");
  assert_equals(parent_children[0], parent.firstElementChild, "parent.children[0] after beforebegin");
  assert_equals(parent_children[1], ref, "parent.children[1] after beforebegin");
  assert_equals(ref_children.length, 0, "ref.children unchanged by beforebegin");

  ref.insertAdjacentHTML("afterend", "<span></span>");
  assert_equals(parent_children.length, 3, "parent.children length after afterend");
  assert_equals(parent_children[2], parent.lastElementChild, "parent.children[2] after afterend");

  ref.insertAdjacentHTML("afterbegin", "<em></em>");
  assert_equals(ref_children.length, 1, "ref.children length after afterbegin");
  assert_equals(ref_children[0], ref.firstElementChild, "ref.children[0] after afterbegin");

  ref.insertAdjacentHTML("beforeend", "<strong></strong>");
  assert_equals(ref_children.length, 2, "ref.children length after beforeend");
  assert_equals(ref_children[1], ref.lastElementChild, "ref.children[1] after beforeend");
}, "Element.children updates live on Element.insertAdjacentHTML mutations");

test(() => {
  const parent = document.createElement("div");
  parent.innerHTML = "<a></a><b></b>";

  const a = parent.firstElementChild;
  const children = parent.children;

  assert_equals(children.length, 2);
  a.outerHTML = "<span></span><span></span>";

  assert_equals(children.length, 3, "parent.children length after outerHTML replaces node with fragment");
  assert_equals(children[0], parent.firstElementChild, "parent.children[0] after outerHTML");
  assert_equals(children[2], parent.lastElementChild, "parent.children[2] after outerHTML");
}, "Element.children updates live on Element.outerHTML mutations");

test(() => {
  const parent = document.createElement("div");
  const children = parent.children;

  // Do not touch children.length or children.item() before mutating the DOM; indexed access should
  // still be live and reflect the current tree.
  parent.innerHTML = "<span></span><span></span>";
  assert_equals(children[0], parent.firstElementChild, "children[0] after insertion");
  assert_equals(children[1], parent.lastElementChild, "children[1] after insertion");

  parent.innerHTML = "<p></p>";
  assert_equals(children[0], parent.firstElementChild, "children[0] after shrink");
  assert_equals(children[1], undefined, "children[1] should be undefined after shrink");
}, "Element.children indexed property access stays live without calling length/item");
