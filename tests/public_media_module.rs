use fastrender::media::Timebase;

#[test]
fn media_module_is_exported() {
  // Compile-time regression test: `fastrender::media` must remain part of the public API.
  //
  // This is a minimal runtime assertion to ensure the type is usable without enabling any optional
  // codec/container features.
  let tb = Timebase::new(1, 90_000);
  assert_eq!(tb.den, 90_000);
}

