// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Minimal ResizeObserver coverage for the renderer-backed WPT DOM runner backend.
//
// Note: FastRender does not yet implement the full ResizeObserver spec; the rendered backend
// installs a tiny polyfill (only when the platform API is missing) that uses real layout geometry.

async_test(t => {
  assert_true(typeof ResizeObserver === "function", "ResizeObserver must exist (native or polyfilled)");

  document.body.innerHTML = `
    <style>
      html, body { margin: 0; padding: 0; }
      #box { width: 120px; height: 34px; background: rgb(0, 255, 0); }
    </style>
    <div id="box"></div>
  `;

  const target = document.getElementById("box");
  let called = false;

  const ro = new ResizeObserver(
    t.step_func((entries, observer) => {
      if (called) return;
      called = true;

      assert_greater_than_equal(entries.length, 1, "entries length");
      const entry = entries[0];
      assert_equals(entry.target, target, "entry.target");

      assert_true(entry.contentRect.width > 0, "contentRect.width > 0");
      assert_true(entry.contentRect.height > 0, "contentRect.height > 0");
      assert_approx_equals(entry.contentRect.width, 120, 0.5, "contentRect.width");
      assert_approx_equals(entry.contentRect.height, 34, 0.5, "contentRect.height");

      observer.disconnect();
      t.done();
    })
  );

  ro.observe(target);
}, "ResizeObserver callback receives non-zero contentRect");

