use fastrender::{FastRender, RenderOptions};

#[test]
fn container_query_prepass_avoids_extra_fixpoint_iteration() {
  // This test exercises the render pipeline's "pre-layout" container query pass:
  //
  // - Without a pre-layout pass, we first lay out with container queries disabled (no container
  //   sizes), then re-cascade + relayout once container sizes are known.
  // - The relayout can change the container's block size (height), which is part of the container
  //   query context fingerprint, forcing a second "no-op" iteration even when queries only depend
  //   on inline-size.
  //
  // By approximating container inline sizes before the initial layout pass, we can apply container
  // query rules up-front and converge in a single fixpoint iteration.
  let html = r#"<!doctype html>
    <style>
      #container { container-type: inline-size; width: 300px; }
      #target { display: block; height: 10px; }
      @container (min-width: 200px) {
        #target { display: none; }
      }
    </style>
    <div id="container"><div id="target"></div></div>
  "#;

  let mut renderer = FastRender::new().expect("create renderer");
  let result = renderer
    .render_html_with_diagnostics(html, RenderOptions::new().with_viewport(400, 200))
    .expect("render");

  let diagnostics = result
    .diagnostics
    .container_queries
    .expect("container query diagnostics");

  assert!(
    diagnostics.converged,
    "expected container query fixpoint to converge: {diagnostics:?}"
  );
  assert_eq!(
    diagnostics.iterations, 1,
    "expected pre-layout container query pass to avoid extra iterations: {diagnostics:?}"
  );
}

