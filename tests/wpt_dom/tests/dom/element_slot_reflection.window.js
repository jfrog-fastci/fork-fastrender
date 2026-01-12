// META: script=/resources/testharness.js
//
// Element.slot is a reflected IDL attribute for the `slot` content attribute.
// It is used by the (named) slotting algorithm to assign light DOM nodes to
// <slot name="..."> elements in a shadow tree.

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  const host = document.createElement("div");
  document.body.appendChild(host);

  const shadow = host.attachShadow({ mode: "open" });

  const slot = document.createElement("slot");
  slot.setAttribute("name", "a");
  shadow.appendChild(slot);

  const slottable = document.createElement("span");
  host.appendChild(slottable);

  assert_equals(
    slottable.slot,
    "",
    "Element.slot getter should return \"\" when the slot attribute is missing"
  );
  assert_equals(
    slottable.getAttribute("slot"),
    null,
    "The slot content attribute should initially be missing"
  );

  assert_equals(
    slottable.assignedSlot,
    null,
    "Unslooted element should have null assignedSlot when no matching <slot> exists"
  );
  assert_array_equals(
    slot.assignedNodes(),
    [],
    "Named slot should have no assigned nodes before the slot attribute is set"
  );

  slottable.slot = "a";
  assert_equals(
    slottable.getAttribute("slot"),
    "a",
    "Element.slot setter should reflect to the slot content attribute"
  );
  assert_equals(slottable.slot, "a", "Element.slot getter should reflect attribute value");

  assert_equals(
    slottable.assignedSlot,
    slot,
    "Updating Element.slot should update the slotting assignment"
  );
  assert_array_equals(
    slot.assignedNodes(),
    [slottable],
    "Updating Element.slot should update HTMLSlotElement.assignedNodes()"
  );

  slottable.slot = "";
  assert_equals(
    slottable.assignedSlot,
    null,
    "Clearing Element.slot should remove the node from the named slot"
  );
  assert_array_equals(
    slot.assignedNodes(),
    [],
    "Clearing Element.slot should update assignedNodes()"
  );
}, "Element.slot reflects the slot content attribute and affects named slot assignment");

