// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Minimal IntersectionObserver coverage for the renderer-backed WPT DOM runner backend.
//
// Note: FastRender does not yet implement the full IntersectionObserver spec; the rendered backend
// installs a tiny polyfill (only when the platform API is missing) that uses real layout geometry.

async_test(t => {
  assert_true(
    typeof IntersectionObserver === "function",
    "IntersectionObserver must exist (native or polyfilled)"
  );

  document.body.innerHTML = `
    <style>
      html, body { margin: 0; padding: 0; }
      #target {
        position: absolute;
        left: 4px;
        top: 6px;
        width: 80px;
        height: 40px;
        background: rgb(0, 0, 255);
      }
    </style>
    <div id="target"></div>
  `;

  const target = document.getElementById("target");
  let called = false;

  const io = new IntersectionObserver(
    t.step_func((entries, observer) => {
      if (called) return;
      called = true;

      assert_greater_than_equal(entries.length, 1, "entries length");
      const entry = entries[0];
      assert_equals(entry.target, target, "entry.target");

      assert_true(entry.boundingClientRect.width > 0, "boundingClientRect.width > 0");
      assert_true(entry.boundingClientRect.height > 0, "boundingClientRect.height > 0");
      assert_approx_equals(entry.boundingClientRect.width, 80, 0.5, "boundingClientRect.width");
      assert_approx_equals(entry.boundingClientRect.height, 40, 0.5, "boundingClientRect.height");

      assert_true(entry.isIntersecting, "isIntersecting");
      assert_true(entry.intersectionRect.width > 0, "intersectionRect.width > 0");
      assert_true(entry.intersectionRect.height > 0, "intersectionRect.height > 0");
      assert_true(entry.intersectionRatio > 0, "intersectionRatio > 0");

      observer.disconnect();
      t.done();
    })
  );

  io.observe(target);
}, "IntersectionObserver callback receives non-zero intersectionRect");

