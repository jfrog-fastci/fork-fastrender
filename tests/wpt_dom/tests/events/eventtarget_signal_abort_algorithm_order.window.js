// META: script=/resources/testharness.js

// Curated `{ signal }` abort-algorithm ordering + cleanup tests.
//
// This intentionally avoids newer JS syntax so it can run on the minimal vm-js backend.

function eventtarget_signal_abort_algorithm_order_test() {
  var controller = new AbortController();
  var signal = controller.signal;
  var target = document.createElement("div");

  var foo_called = false;
  function foo_listener(_e) {
    foo_called = true;
  }

  function abort_capture_listener(_e) {
    // If abort algorithms run before dispatching the `abort` event, this listener will have been
    // removed before any user abort listeners run.
    target.dispatchEvent(new Event("foo"));
    assert_false(
      foo_called,
      "foo listener should be removed before abort event listeners run"
    );
  }

  // Register a user abort capture listener first.
  signal.addEventListener("abort", abort_capture_listener, { capture: true });

  // Then register a listener with `{ signal }`.
  target.addEventListener("foo", foo_listener, { signal: signal });

  controller.abort();

  assert_false(foo_called, "foo listener should never be invoked");
}

test(
  eventtarget_signal_abort_algorithm_order_test,
  "AbortSignal abort algorithms for addEventListener({ signal }) run before abort event dispatch"
);

function eventtarget_signal_abort_algorithm_removed_on_remove_event_listener_test() {
  var controller = new AbortController();
  var signal = controller.signal;
  var target = document.createElement("div");

  function should_not_run(_e) {
    assert_unreached("removed listener should never be invoked");
  }

  target.addEventListener("foo", should_not_run, { signal: signal });
  target.removeEventListener("foo", should_not_run);

  // If the abort algorithm wasn't removed, aborting would still run internal cleanup. This should
  // still be a no-op from the JS point of view (no throw, no listener invocation).
  controller.abort();

  target.dispatchEvent(new Event("foo"));
}

test(
  eventtarget_signal_abort_algorithm_removed_on_remove_event_listener_test,
  "removeEventListener removes the associated abort algorithm for addEventListener({ signal })"
);

