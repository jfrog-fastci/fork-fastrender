// META: script=/resources/testharness.js

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  const body = document.body;
  clear_children(body);

  const host = document.createElement("div");
  body.appendChild(host);

  const shadow = host.attachShadow({ mode: "open", slotAssignment: "manual" });
  assert_equals(shadow.slotAssignment, "manual");

  const slot1 = document.createElement("slot");
  const slot2 = document.createElement("slot");
  shadow.appendChild(slot1);
  shadow.appendChild(slot2);

  const a = document.createElement("span");
  const b = document.createElement("span");
  host.appendChild(a);
  host.appendChild(b);

  slot1.assign(a);
  slot2.assign(b);

  assert_equals(a.assignedSlot, slot1);
  assert_equals(b.assignedSlot, slot2);

  assert_array_equals(slot1.assignedNodes(), [a]);
  assert_array_equals(slot2.assignedNodes(), [b]);

  // Reassigning a node to a different slot should update both slots.
  slot2.assign(a, b);
  assert_equals(a.assignedSlot, slot2);
  assert_equals(b.assignedSlot, slot2);
  assert_array_equals(slot1.assignedNodes(), []);
  assert_array_equals(slot2.assignedNodes(), [a, b]);
}, "Manual slot assignment distributes nodes via HTMLSlotElement.assign()");

