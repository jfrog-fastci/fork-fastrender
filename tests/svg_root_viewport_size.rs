use fastrender::image_loader::ImageCache;

#[test]
fn svg_root_viewport_resolves_percent_lengths_for_rasterization() {
  // Regression test for SVG-as-image rasterization: when the outermost <svg> omits width/height,
  // SVG defaults them to 100%. Percent-based sizes (like <image width="100%">) must then resolve
  // against the concrete render size supplied by the embedding context.
  //
  // `resvg/usvg` resolve percentage lengths during parse. If the outermost viewport is missing,
  // percent values can collapse to zero, producing fully transparent output. Ensure we inject a
  // definite viewport size before rasterization.
  let cache = ImageCache::new();

  // This is a minimized version of Next.js' blur placeholder SVG (as seen on theverge.com):
  // - outermost <svg> has no width/height (defaults to 100%),
  // - <image width="100%" height="100%"> is filtered via `style="filter: url(#b);"`
  // - referenced PNG is a 1×1 opaque light pixel (L=233, A=255).
  //
  // If percent lengths in SVG are resolved without the concrete raster size, the <image> can end
  // up with a 0×0 bounding box and the entire render becomes transparent.
  const PIXEL_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mN8+R8AAtcB6oaHtZcAAAAASUVORK5CYII=";
  let svg = format!(
    r#"<svg xmlns='http://www.w3.org/2000/svg' ><filter id='b' color-interpolation-filters='sRGB'><feGaussianBlur stdDeviation='20'/><feColorMatrix values='1 0 0 0 0 0 1 0 0 0 0 0 1 0 0 0 0 0 100 -1' result='s'/><feFlood x='0' y='0' width='100%' height='100%'/><feComposite operator='out' in='s'/><feComposite in2='SourceGraphic'/><feGaussianBlur stdDeviation='20'/></filter><image width='100%' height='100%' x='0' y='0' preserveAspectRatio='none' style='filter: url(#b);' href='data:image/png;base64,{PIXEL_PNG}'/></svg>"#
  );

  let svg_explicit = format!(
    r#"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='100'><filter id='b' color-interpolation-filters='sRGB'><feGaussianBlur stdDeviation='20'/><feColorMatrix values='1 0 0 0 0 0 1 0 0 0 0 0 1 0 0 0 0 0 100 -1' result='s'/><feFlood x='0' y='0' width='100%' height='100%'/><feComposite operator='out' in='s'/><feComposite in2='SourceGraphic'/><feGaussianBlur stdDeviation='20'/></filter><image width='100%' height='100%' x='0' y='0' preserveAspectRatio='none' style='filter: url(#b);' href='data:image/png;base64,{PIXEL_PNG}'/></svg>"#
  );

  let pixmap_explicit = cache
    .render_svg_pixmap_at_size(&svg_explicit, 100, 100, "test://svg", 1.0)
    .expect("render svg pixmap (explicit)");
  let explicit_center = pixmap_explicit.pixel(50, 50).expect("center pixel");
  assert!(
    explicit_center.alpha() > 0,
    "setup sanity check: expected explicit-viewport SVG to render; got rgba=({}, {}, {}, {})",
    explicit_center.red(),
    explicit_center.green(),
    explicit_center.blue(),
    explicit_center.alpha()
  );

  let pixmap = cache
    .render_svg_pixmap_at_size(&svg, 100, 100, "test://svg", 1.0)
    .expect("render svg pixmap");

  let center = pixmap.pixel(50, 50).expect("center pixel");
  assert!(
    center.alpha() > 0,
    "expected filtered percent-sized <image> to render when root viewport is implicit; got rgba=({}, {}, {}, {})",
    center.red(),
    center.green(),
    center.blue(),
    center.alpha()
  );
}
