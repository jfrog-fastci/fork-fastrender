// META: script=/resources/testharness.js
// META: script=/resources/fastrender_testharness_report.js
//
// Layout-sensitive smoke test for the renderer-backed WPT DOM runner backend.
//
// The legacy `vmjs` backend runs against `WindowHostState` which has no renderer/layout, so it
// cannot produce meaningful geometry. The `vmjs-rendered` backend exposes a small test-only hook
// (`__fastrender_get_rect_by_id`) backed by the real layout engine.

async_test(t => {
  assert_true(
    typeof __fastrender_get_rect_by_id === "function",
    "__fastrender_get_rect_by_id hook must be installed by the rendered backend"
  );

  document.body.innerHTML = `
    <style>
      html, body { margin: 0; padding: 0; }
      #target {
        position: absolute;
        left: 10px;
        top: 20px;
        width: 100px;
        height: 50px;
        background: rgb(255, 0, 0);
      }
    </style>
    <div id="target"></div>
  `;

  // Wait one task so the backend can complete a render/layout pass.
  setTimeout(
    t.step_func_done(() => {
      const rect = __fastrender_get_rect_by_id("target");
      assert_not_equals(rect, null, "rect should be available for rendered elements");
      assert_approx_equals(rect.width, 100, 0.5, "width");
      assert_approx_equals(rect.height, 50, 0.5, "height");
      assert_approx_equals(rect.x, 10, 0.5, "x");
      assert_approx_equals(rect.y, 20, 0.5, "y");
    }),
    0
  );
}, "renderer-backed backend produces non-zero element geometry");

