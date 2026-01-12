// META: script=/resources/testharness.js
//
// Curated WebIDL argument-conversion tests for EventTarget.addEventListener/removeEventListener.
// These focus on:
// - DOMString conversion for the `type` argument (ToString, including objects)
// - EventListener? callback-interface conversion (callable OR object with callable handleEvent)
// - (AddEventListenerOptions or boolean) / (EventListenerOptions or boolean) union option parsing

test(function () {
  var called = 0;
  var type_obj = {
    toString: function () {
      called++;
      return "eventtarget-webidl-type";
    },
  };

  var ran = false;
  var t = new EventTarget();
  t.addEventListener(type_obj, function () {
    ran = true;
  });
  t.dispatchEvent(new Event("eventtarget-webidl-type"));

  assert_equals(called, 1, "DOMString conversion must invoke ToString on objects exactly once");
  assert_true(ran, "listener registered with object type should fire for the converted type name");
}, "addEventListener: `type` DOMString conversion uses ToString (objects invoke toString)");

test(function () {
  var t = new EventTarget();
  assert_throws_js(TypeError, function () {
    t.addEventListener("x", {});
  });
}, "addEventListener: callback-interface conversion rejects objects without a callable handleEvent");

test(function () {
  var t = new EventTarget();
  var ran = false;
  t.addEventListener("x", {
    handleEvent: function () {
      ran = true;
    },
  });
  t.dispatchEvent(new Event("x"));
  assert_true(ran, "object EventListener should be invoked via handleEvent");
}, "addEventListener: callback-interface conversion accepts objects with callable handleEvent");

test(function () {
  var t = new EventTarget();
  assert_throws_js(TypeError, function () {
    t.addEventListener("x", 1);
  });
  assert_throws_js(TypeError, function () {
    t.removeEventListener("x", 1);
  });
}, "addEventListener/removeEventListener: callback-interface conversion rejects primitive values");

test(function () {
  var log = [];
  var t = new EventTarget();
  t.addEventListener(
    "listener-options-get-capture-inherited",
    function () {
      log.push("bubble");
    }
  );
  t.addEventListener(
    "listener-options-get-capture-inherited",
    function () {
      log.push("capture");
    },
    Object.create({ capture: true })
  );

  t.dispatchEvent(new Event("listener-options-get-capture-inherited", { bubbles: true }));
  assert_array_equals(
    log,
    ["capture", "bubble"],
    "capture option must be read via Get semantics (including the prototype chain)"
  );
}, "addEventListener: options dictionary members use Get semantics (prototype chain)");

