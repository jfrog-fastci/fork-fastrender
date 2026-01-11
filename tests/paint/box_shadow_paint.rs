use super::util::create_stacking_context_bounds_renderer;
use tiny_skia::Pixmap;

fn render(html: &str, width: u32, height: u32) -> Pixmap {
  let mut renderer = create_stacking_context_bounds_renderer();
  renderer.render_html(html, width, height).expect("render")
}

fn rgba_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn box_shadow_spread_only_paints_ring() {
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }
      #target {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 20px;
        height: 20px;
        /* No background; interior should remain the page background. */
        box-shadow: 0 0 0 8px rgb(255, 0, 0);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 64, 64);

  // Inside the border box: no shadow fill.
  assert_eq!(rgba_at(&pixmap, 30, 30), (255, 255, 255, 255));
  // Inside the spread ring (well away from anti-aliased edges).
  assert_eq!(rgba_at(&pixmap, 15, 30), (255, 0, 0, 255));
  // Square-cornered boxes should not gain rounded outer corners just because spread is non-zero.
  assert_eq!(rgba_at(&pixmap, 13, 13), (255, 0, 0, 255));
  // Outside the ring.
  assert_eq!(rgba_at(&pixmap, 11, 30), (255, 255, 255, 255));
}

#[test]
fn box_shadow_blur_only_produces_soft_edge() {
  // Use a fractional blur radius to ensure subpixel values are respected.
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }
      #target {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 20px;
        height: 20px;
        box-shadow: 0 0 4.5px rgba(0, 0, 0, 0.8);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 64, 64);

  // Border box interior should not get a translucent fill from an outer shadow.
  assert_eq!(rgba_at(&pixmap, 30, 30), (255, 255, 255, 255));

  // Pixel just outside the left edge should be darkened, but not fully black.
  let (r, g, b, a) = rgba_at(&pixmap, 19, 30);
  assert_eq!(a, 255);
  assert!(r < 255 && g < 255 && b < 255, "expected shadow to darken pixel");
  assert!(r > 0 && g > 0 && b > 0, "expected blur to avoid a hard edge");
  assert_eq!((r, g, b), (r, r, r), "expected neutral shadow tint");
}

#[test]
fn box_shadow_blur_and_spread_respect_fractional_values() {
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }
      #target {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 20px;
        height: 20px;
        box-shadow: 0 0 4.5px 12.5px rgb(0, 0, 255);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 80, 80);

  // Just outside the border box should still be fully inside the spread area (opaque).
  assert_eq!(rgba_at(&pixmap, 17, 30), (0, 0, 255, 255));

  // A pixel whose center lies exactly on the outer edge (spread=12.5 => left edge at 7.5).
  // This should be partially covered due to AA and fractional geometry.
  let (r, g, b, a) = rgba_at(&pixmap, 7, 30);
  assert_eq!(b, 255, "expected blue channel to remain saturated over white");
  assert_eq!(r, g, "expected neutral falloff for red/green channels");
  assert!(r > 0 && r < 255, "expected a partially-covered outer edge pixel");
  assert_eq!(a, 255);
}

#[test]
fn box_shadow_inset_respects_border_radius_and_stays_inside() {
  let html = r#"
    <style>
      body { margin: 0; background: rgb(0, 0, 0); }
      #target {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 40px;
        height: 40px;
        background: rgb(255, 255, 255);
        border-radius: 20px;
        box-shadow: inset 10px 10px 0 0 rgb(255, 0, 0);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 96, 96);

  // Outside the rounded corner: should remain body background (no shadow leakage).
  assert_eq!(rgba_at(&pixmap, 21, 21), (0, 0, 0, 255));

  // Inside the box but within the inset-shadow band (top edge).
  assert_eq!(rgba_at(&pixmap, 40, 22), (255, 0, 0, 255));

  // Center of the box should remain the element background.
  assert_eq!(rgba_at(&pixmap, 40, 40), (255, 255, 255, 255));

  // Outside the element bounds: no shadow should paint outside.
  assert_eq!(rgba_at(&pixmap, 10, 40), (0, 0, 0, 255));
}

#[test]
fn box_shadow_inset_blur_only_produces_inner_shadow() {
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }
      #target {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 40px;
        height: 40px;
        background: rgb(255, 255, 255);
        box-shadow: inset 0 0 8px rgba(0, 0, 0, 0.8);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 96, 96);

  // Outside the element should remain untouched.
  assert_eq!(rgba_at(&pixmap, 19, 40), (255, 255, 255, 255));

  // Inner shadow should darken pixels along the padding edge.
  let (er, eg, eb, ea) = rgba_at(&pixmap, 20, 40);
  assert_eq!(ea, 255);
  assert!(er < 255 && eg < 255 && eb < 255, "expected edge pixel to be darkened");

  // Center should be less affected than the edge.
  let (cr, cg, cb, ca) = rgba_at(&pixmap, 40, 40);
  assert_eq!(ca, 255);
  assert!(cr > er && cg > eg && cb > eb, "expected shadow to fade toward center");
}

#[test]
fn box_shadow_outset_respects_border_radius() {
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }
      #target {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 40px;
        height: 40px;
        border-radius: 20px;
        box-shadow: 0 0 0 10px rgb(0, 0, 255);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 96, 96);

  // A point inside the shadow's bounding box but outside the rounded shadow perimeter.
  assert_eq!(rgba_at(&pixmap, 10, 10), (255, 255, 255, 255));

  // Along the left edge of the ring.
  assert_eq!(rgba_at(&pixmap, 11, 40), (0, 0, 255, 255));

  // Inside the element: no outer shadow fill.
  assert_eq!(rgba_at(&pixmap, 40, 40), (255, 255, 255, 255));
}

#[test]
fn box_shadow_multiple_shadows_paint_front_to_back() {
  // CSS Backgrounds and Borders: box-shadow lists are ordered front-to-back (first is on top).
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }
      #target {
        position: absolute;
        left: 20px;
        top: 20px;
        width: 20px;
        height: 20px;
        box-shadow:
          0 0 0 6px rgb(255, 0, 0),
          0 0 0 6px rgb(0, 0, 255);
      }
    </style>
    <div id="target"></div>
  "#;

  let pixmap = render(html, 64, 64);

  // In the ring, the top-most (first) shadow should win.
  assert_eq!(rgba_at(&pixmap, 15, 30), (255, 0, 0, 255));
}

#[test]
fn box_shadow_figma_modal_smoke() {
  // Real-world regression inspired by the `figma.com` fixture: a modal with two outer shadows,
  // including fractional blur/spread radii and rounded corners.
  let html = r#"
    <style>
      body { margin: 0; background: rgb(255, 255, 255); }
      #container {
        position: absolute;
        left: 0;
        top: 0;
        width: 200px;
        height: 200px;
        display: flex;
        align-items: center;
        justify-content: center;
        background: rgba(0, 0, 0, 0.3);
      }
      #modal {
        width: 100px;
        height: 60px;
        background: rgb(248, 248, 248);
        border-radius: 3px;
        box-shadow:
          rgba(0, 0, 0, 0.2) 0px 0px 0.5px 0.5px,
          rgba(0, 0, 0, 0.15) 0px 2px 14px 0px;
      }
    </style>
    <div id="container"><div id="modal"></div></div>
  "#;

  let pixmap = render(html, 200, 200);

  // Overlay background is deterministic: white blended with rgba(0,0,0,0.3).
  assert_eq!(rgba_at(&pixmap, 10, 10), (178, 178, 178, 255));

  // Modal interior should remain unaffected by outer shadows.
  assert_eq!(rgba_at(&pixmap, 100, 100), (248, 248, 248, 255));

  // Just outside the modal's left edge (modal is centered at x=50..150) should be darkened.
  let (nr, ng, nb, na) = rgba_at(&pixmap, 49, 100);
  assert_eq!(na, 255);
  assert!(nr < 178 && ng < 178 && nb < 178, "expected shadow to darken pixel");

  // Further away from the box, the shadow should decay back toward the overlay background.
  let (fr, fg, fb, fa) = rgba_at(&pixmap, 30, 100);
  assert_eq!(fa, 255);
  assert!(
    fr > nr && fg > ng && fb > nb,
    "expected shadow to fade with distance"
  );
}
