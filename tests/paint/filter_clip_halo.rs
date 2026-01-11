use super::util::create_stacking_context_bounds_renderer;

fn rgba_at(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let p = pixmap.pixel(x, y).expect("pixel");
  [p.red(), p.green(), p.blue(), p.alpha()]
}

#[test]
fn filter_blur_halo_is_preserved_outside_overflow_clip() {
  let mut renderer = create_stacking_context_bounds_renderer();

  // Regression test for layer-bound computations inside stacking contexts.
  //
  // When a filtered element is painted under an `overflow:hidden` clip, we still need to allocate
  // enough transparent "halo" pixels outside the clip so blur kernels don't clamp to the clip edge.
  //
  // Without the halo, the blur sees the clip boundary as the pixmap edge and repeats the edge
  // pixels, producing an incorrect fully-opaque result at the clip boundary.
  let html = r#"
    <style>
      body { margin: 0; background: white; }
      .clip {
        position: absolute;
        left: 0px;
        top: 50px;
        width: 100px;
        height: 100px;
        overflow: hidden;
      }
      .blur {
        width: 100px;
        height: 100px;
        background: rgb(0, 0, 255);
        filter: blur(10px);
      }
    </style>
    <div class="clip"><div class="blur"></div></div>
  "#;

  let pixmap = renderer.render_html(html, 200, 200).expect("render");

  // Above the clipped area: just the page background.
  assert_eq!(rgba_at(&pixmap, 50, 49), [255, 255, 255, 255]);

  // At the top edge of the clip: the blurred content should fade to the backdrop rather than
  // remaining fully blue.
  let edge = rgba_at(&pixmap, 50, 50);
  assert!(
    edge[0] > 0 && edge[1] > 0,
    "expected the blur to fade to white at the clip edge, got {:?}",
    edge
  );

  // Center of the clipped area: fully covered by the original element, so should remain solid blue.
  assert_eq!(rgba_at(&pixmap, 50, 100), [0, 0, 255, 255]);
}

