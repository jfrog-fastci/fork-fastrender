pub(crate) fn rt_trap_invalid_arg(msg: &str) -> ! {
  eprintln!("runtime-native: invalid argument: {msg}");
  std::process::abort();
}

pub(crate) fn rt_trap_oom(bytes: usize, context: &str) -> ! {
  eprintln!("runtime-native: out of memory allocating {bytes} bytes in {context}");
  std::process::abort();
}
