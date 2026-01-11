use core::fmt;

pub(crate) fn rt_trap_invalid_arg(msg: &str) -> ! {
  rt_trap_invalid_arg_fmt(format_args!("{msg}"))
}

pub(crate) fn rt_trap_invalid_arg_fmt(msg: fmt::Arguments<'_>) -> ! {
  eprintln!("runtime-native: invalid argument: {msg}");
  std::process::abort();
}

pub(crate) fn rt_trap_oom(bytes: usize, context: &str) -> ! {
  eprintln!("runtime-native: out of memory allocating {bytes} bytes in {context}");
  std::process::abort();
}
