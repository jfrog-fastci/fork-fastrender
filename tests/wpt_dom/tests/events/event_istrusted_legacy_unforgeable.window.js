// META: script=/resources/testharness.js
//
// Event.isTrusted is `[LegacyUnforgeable]` per the WebIDL snapshot, meaning it must be defined as a
// non-configurable own property on each Event instance (and not on Event.prototype).

function event_is_trusted_legacy_unforgeable_test() {
  var e = new Event("x");
  assert_true(Object.prototype.hasOwnProperty.call(e, "isTrusted"));

  var desc = Object.getOwnPropertyDescriptor(e, "isTrusted");
  assert_false(desc.configurable);

  assert_equals(Object.getOwnPropertyDescriptor(Event.prototype, "isTrusted"), undefined);
}

test(
  event_is_trusted_legacy_unforgeable_test,
  "Event.isTrusted is a LegacyUnforgeable instance property"
);

