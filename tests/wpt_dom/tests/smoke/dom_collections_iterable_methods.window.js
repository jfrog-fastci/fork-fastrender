// META: script=/resources/testharness.js

test(() => {
  const parent = document.createElement("div");
  const a = document.createElement("span");
  const b = document.createElement("span");
  parent.appendChild(a);
  parent.appendChild(b);

  const nodeList = parent.childNodes;
  assert_true(nodeList instanceof NodeList, "parent.childNodes should be a NodeList");

  const htmlCollection = parent.children;
  assert_true(htmlCollection instanceof HTMLCollection, "parent.children should be an HTMLCollection");

  assert_equals(
    NodeList.prototype[Symbol.iterator],
    NodeList.prototype.values,
    "NodeList @@iterator should alias values()"
  );
  assert_equals(
    HTMLCollection.prototype[Symbol.iterator],
    HTMLCollection.prototype.values,
    "HTMLCollection @@iterator should alias values()"
  );

  for (const [name, obj] of [["NodeList", nodeList], ["HTMLCollection", htmlCollection]]) {
    assert_equals(typeof obj.values, "function", `${name}.values should exist`);
    assert_equals(typeof obj.keys, "function", `${name}.keys should exist`);
    assert_equals(typeof obj.entries, "function", `${name}.entries should exist`);
    assert_equals(typeof obj.forEach, "function", `${name}.forEach should exist`);
  }

  const nodeListValues = Array.from(nodeList.values());
  assert_equals(nodeListValues.length, 2, "NodeList.values length");
  assert_equals(nodeListValues[0], a, "NodeList.values[0]");
  assert_equals(nodeListValues[1], b, "NodeList.values[1]");
  assert_array_equals(Array.from(nodeList.keys()), [0, 1], "NodeList.keys iterates indices");

  const nodeListEntries = Array.from(nodeList.entries());
  assert_equals(nodeListEntries.length, 2, "NodeList.entries length");
  assert_equals(nodeListEntries[0][0], 0, "NodeList.entries[0][0]");
  assert_equals(nodeListEntries[0][1], a, "NodeList.entries[0][1]");
  assert_equals(nodeListEntries[1][0], 1, "NodeList.entries[1][0]");
  assert_equals(nodeListEntries[1][1], b, "NodeList.entries[1][1]");

  const htmlCollectionValues = Array.from(htmlCollection.values());
  assert_equals(htmlCollectionValues.length, 2, "HTMLCollection.values length");
  assert_equals(htmlCollectionValues[0], a, "HTMLCollection.values[0]");
  assert_equals(htmlCollectionValues[1], b, "HTMLCollection.values[1]");
  assert_array_equals(Array.from(htmlCollection.keys()), [0, 1], "HTMLCollection.keys iterates indices");

  const htmlCollectionEntries = Array.from(htmlCollection.entries());
  assert_equals(htmlCollectionEntries.length, 2, "HTMLCollection.entries length");
  assert_equals(htmlCollectionEntries[0][0], 0, "HTMLCollection.entries[0][0]");
  assert_equals(htmlCollectionEntries[0][1], a, "HTMLCollection.entries[0][1]");
  assert_equals(htmlCollectionEntries[1][0], 1, "HTMLCollection.entries[1][0]");
  assert_equals(htmlCollectionEntries[1][1], b, "HTMLCollection.entries[1][1]");

  const thisArg = { tag: "thisArg" };
  const seenNodeList = [];
  nodeList.forEach(function (value, index, list) {
    seenNodeList.push([value, index, list, this]);
  }, thisArg);
  assert_equals(seenNodeList.length, 2, "NodeList.forEach callback count");
  assert_equals(seenNodeList[0][0], a, "NodeList.forEach value[0]");
  assert_equals(seenNodeList[0][1], 0, "NodeList.forEach index[0]");
  assert_equals(seenNodeList[0][2], nodeList, "NodeList.forEach list arg");
  assert_equals(seenNodeList[0][3], thisArg, "NodeList.forEach thisArg");

  const seenHtmlCollection = [];
  htmlCollection.forEach(function (value, index, list) {
    seenHtmlCollection.push([value, index, list, this]);
  }, thisArg);
  assert_equals(seenHtmlCollection.length, 2, "HTMLCollection.forEach callback count");
  assert_equals(seenHtmlCollection[0][0], a, "HTMLCollection.forEach value[0]");
  assert_equals(seenHtmlCollection[0][1], 0, "HTMLCollection.forEach index[0]");
  assert_equals(seenHtmlCollection[0][2], htmlCollection, "HTMLCollection.forEach list arg");
  assert_equals(seenHtmlCollection[0][3], thisArg, "HTMLCollection.forEach thisArg");
}, "NodeList/HTMLCollection iterable methods smoke test");
