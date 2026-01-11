#[test]
fn runtime_native_c_header_declares_rt_thread_tls_symbol() {
  const HEADER: &str = include_str!("../include/runtime_native.h");
  assert!(
    HEADER.contains("RT_THREAD"),
    "`runtime_native.h` should declare the RT_THREAD TLS symbol for generated code"
  );
}

