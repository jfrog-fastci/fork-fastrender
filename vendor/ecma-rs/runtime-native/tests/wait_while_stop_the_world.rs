use runtime_native::test_util::TestRuntimeGuard;
use runtime_native::threading;
use runtime_native::threading::ThreadKind;
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

#[derive(Debug)]
enum Cmd {
  DropGuard,
}

#[derive(Debug)]
enum Event {
  EnteredGcSafe,
  DroppingGuard,
  DroppedGuard,
}

struct ThreadRegistrationGuard;

impl Drop for ThreadRegistrationGuard {
  fn drop(&mut self) {
    threading::unregister_current_thread();
  }
}

struct ResumeWorldOnDrop;

impl Drop for ResumeWorldOnDrop {
  fn drop(&mut self) {
    runtime_native::rt_gc_resume_world();
  }
}

#[test]
fn wait_while_stop_the_world_blocks_until_resume() {
  let _rt = TestRuntimeGuard::new();

  threading::register_current_thread(ThreadKind::Main);
  let _main_thread = ThreadRegistrationGuard;

  // Keep the global epoch even even if this test panics midway through.
  let _resume_guard = ResumeWorldOnDrop;

  let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
  let (evt_tx, evt_rx) = mpsc::channel::<Event>();

  let handle = std::thread::spawn(move || {
    threading::register_current_thread(ThreadKind::Worker);
    let _worker_thread = ThreadRegistrationGuard;

    let guard = threading::enter_gc_safe_region();
    evt_tx.send(Event::EnteredGcSafe).unwrap();

    match cmd_rx.recv() {
      Ok(Cmd::DropGuard) => {
        evt_tx.send(Event::DroppingGuard).unwrap();
        // This drop path calls `safepoint::wait_while_stop_the_world()` when an STW
        // request is active.
        drop(guard);
        evt_tx.send(Event::DroppedGuard).unwrap();
      }
      Err(_) => {
        // Test failed early; exit without blocking teardown.
      }
    }
  });

  let mut failure: Option<String> = None;

  match evt_rx.recv_timeout(Duration::from_secs(1)) {
    Ok(Event::EnteredGcSafe) => {}
    Ok(evt) => failure = Some(format!("unexpected worker event: {evt:?} (expected EnteredGcSafe)")),
    Err(err) => failure = Some(format!("timed out waiting for worker to enter GC-safe region: {err}")),
  }

  if failure.is_none() {
    runtime_native::rt_gc_request_stop_the_world();
    if !runtime_native::rt_gc_wait_for_world_stopped_timeout(Duration::from_secs(1)) {
      failure = Some("world did not reach safepoint in time".to_string());
    }
  }

  if failure.is_none() && cmd_tx.send(Cmd::DropGuard).is_err() {
    failure = Some("worker command channel disconnected".to_string());
  }

  if failure.is_none() {
    match evt_rx.recv_timeout(Duration::from_secs(1)) {
      Ok(Event::DroppingGuard) => {}
      Ok(evt) => failure = Some(format!("unexpected worker event: {evt:?} (expected DroppingGuard)")),
      Err(err) => failure = Some(format!("timed out waiting for worker to start dropping guard: {err}")),
    }
  }

  if failure.is_none() {
    // The worker must be blocked in `wait_while_stop_the_world()` while the STW
    // epoch is odd. If we see `DroppedGuard` before resuming the world, the
    // implementation is broken (e.g. polling a stale epoch source).
    match evt_rx.recv_timeout(Duration::from_millis(100)) {
      Ok(Event::DroppedGuard) => {
        failure = Some(
          "GC-safe region exit completed while stop-the-world was active (expected it to block)".to_string(),
        );
      }
      Ok(evt) => failure = Some(format!("unexpected worker event while STW is active: {evt:?}")),
      Err(RecvTimeoutError::Timeout) => {
        // Expected.
      }
      Err(RecvTimeoutError::Disconnected) => {
        failure = Some("worker event channel disconnected while expecting it to block".to_string());
      }
    }
  }

  runtime_native::rt_gc_resume_world();

  if failure.is_none() {
    match evt_rx.recv_timeout(Duration::from_secs(1)) {
      Ok(Event::DroppedGuard) => {}
      Ok(evt) => failure = Some(format!("unexpected worker event: {evt:?} (expected DroppedGuard)")),
      Err(err) => failure = Some(format!("timed out waiting for worker to exit GC-safe region: {err}")),
    }
  }

  drop(cmd_tx);
  if let Err(panic) = handle.join() {
    if failure.is_none() {
      failure = Some(format!("worker thread panicked: {panic:?}"));
    }
  }

  if let Some(msg) = failure {
    panic!("{msg}");
  }
}

