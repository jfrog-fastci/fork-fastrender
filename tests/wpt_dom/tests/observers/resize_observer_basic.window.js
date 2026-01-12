// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Curated observer API test: ResizeObserver basic shape + delivery semantics.
//
// This is intentionally geometry-independent so it can run under FastRender's offline DOM runner
// (WindowHostState has no real layout engine). We only assert that the observer machinery exists
// and delivers at least one entry asynchronously after `observe()`.

test(() => {
  assert_equals(typeof ResizeObserver, "function");
}, "ResizeObserver constructor exists");

test(() => {
  let threw = false;
  try {
    // WebIDL constructors must throw when invoked without `new`.
    ResizeObserver(function () {});
  } catch (_e) {
    threw = true;
  }
  assert_true(threw, "constructor call without new should throw");
}, "ResizeObserver constructor requires 'new'");

promise_test(() => {
  const target = document.createElement("div");
  document.body.appendChild(target);

  let called = false;
  let resolveCb;
  let rejectCb;
  const p = new Promise((resolve, reject) => {
    resolveCb = resolve;
    rejectCb = reject;
  });

  const observer = new ResizeObserver(function (entries, obs) {
    called = true;
    try {
      assert_true(Array.isArray(entries), "entries should be an array");
      assert_true(entries.length > 0, "entries should contain at least one entry");
      assert_equals(obs, observer, "observer arg should be the observer instance");
      assert_equals(this, observer, "`this` should be the observer instance");

      // Geometry-independent surface: target identity is stable.
      assert_equals(entries[0].target, target, "entry.target should be the observed element");

      // `takeRecords()` should exist and return an array.
      const r1 = observer.takeRecords();
      assert_true(Array.isArray(r1), "takeRecords should return an array");
    } catch (e) {
      rejectCb(e);
      return;
    }

    // `disconnect()` should prevent delivery of callbacks that were queued before disconnect.
    let disconnectedCalled = false;
    const observer2 = new ResizeObserver(function () {
      disconnectedCalled = true;
    });
    observer2.observe(target);
    observer2.disconnect();

    setTimeout(() => {
      try {
        assert_false(disconnectedCalled, "disconnect() should prevent queued callback delivery");
        resolveCb();
      } catch (e) {
        rejectCb(e);
      }
    }, 0);
  });

  observer.observe(target);
  assert_false(called, "callback should be asynchronous");

  return p;
}, "ResizeObserver observe delivers entries asynchronously, takeRecords exists, and disconnect prevents callbacks");

