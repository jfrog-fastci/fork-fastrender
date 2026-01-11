#[cfg(target_os = "linux")]
pub mod linux_epoll;

#[cfg(not(target_os = "linux"))]
compile_error!("runtime-native currently only supports Linux (epoll/eventfd)");
