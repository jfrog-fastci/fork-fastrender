// META: script=/resources/testharness.js

test(() => {
  const parent = document.createElement("div");
  parent.appendChild(document.createElement("span"));

  const nodeList = parent.childNodes;
  const htmlCollection = parent.children;

  assert_equals(typeof NodeList, "function", "NodeList constructor should exist");
  assert_equals(typeof HTMLCollection, "function", "HTMLCollection constructor should exist");

  assert_true(nodeList instanceof NodeList, "childNodes should be a NodeList");
  assert_true(htmlCollection instanceof HTMLCollection, "children should be an HTMLCollection");

  assert_equals(
    Object.getPrototypeOf(nodeList),
    NodeList.prototype,
    "NodeList prototype should be NodeList.prototype"
  );
  assert_equals(
    Object.getPrototypeOf(htmlCollection),
    HTMLCollection.prototype,
    "HTMLCollection prototype should be HTMLCollection.prototype"
  );

  assert_false(Array.isArray(nodeList), "NodeList should not be an Array");
  assert_false(Array.isArray(htmlCollection), "HTMLCollection should not be an Array");

  assert_equals(typeof nodeList.item, "function", "NodeList.item should be a function");
  assert_equals(typeof htmlCollection.item, "function", "HTMLCollection.item should be a function");

  assert_equals(typeof nodeList.length, "number", "NodeList.length should be a number");
  assert_equals(typeof htmlCollection.length, "number", "HTMLCollection.length should be a number");
}, "NodeList/HTMLCollection basic prototype shape");
