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

  // assignedNodes/assignedElements take a WebIDL dictionary; non-objects should throw.
  assert_throws_js(TypeError, () => slot2.assignedNodes(true));
  assert_throws_js(TypeError, () => slot2.assignedElements(true));
  // null is treated as an empty dictionary.
  assert_array_equals(slot2.assignedNodes(null), [a, b]);
  assert_array_equals(slot2.assignedElements(null), [a, b]);
}, "Manual slot assignment distributes nodes via HTMLSlotElement.assign()");

test(() => {
  const body = document.body;
  clear_children(body);

  const host = document.createElement("div");
  body.appendChild(host);

  const shadow = host.attachShadow({ mode: "open", slotAssignment: "manual" });

  // <slot id=outer><slot id=inner></slot></slot>
  const outer = document.createElement("slot");
  const inner = document.createElement("slot");
  outer.appendChild(inner);
  shadow.appendChild(outer);

  const a = document.createElement("span");
  host.appendChild(a);

  inner.assign(a);

  // Without flattening, `outer` has no assigned nodes so it returns its fallback children (the inner slot).
  assert_array_equals(outer.assignedNodes(), [inner]);

  // With flattening, the nested slot should be expanded to its assigned nodes.
  assert_array_equals(outer.assignedNodes({ flatten: true }), [a]);
  assert_array_equals(outer.assignedElements({ flatten: true }), [a]);
}, "assignedNodes({flatten:true}) flattens nested slots");

test(() => {
  const body = document.body;
  clear_children(body);

  const host = document.createElement("div");
  body.appendChild(host);

  const shadow = host.attachShadow({ mode: "closed", slotAssignment: "manual" });
  assert_equals(shadow.slotAssignment, "manual");

  const slot = document.createElement("slot");
  shadow.appendChild(slot);

  const child = document.createElement("span");
  host.appendChild(child);

  slot.assign(child);

  // `assignedSlot` uses the "open" find-a-slot variant, so closed shadow roots do not leak the slot.
  assert_equals(child.assignedSlot, null);
  assert_array_equals(slot.assignedNodes(), [child]);
}, "assignedSlot is null when assigned to a slot in a closed shadow root");
