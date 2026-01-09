#![cfg(feature = "browser_ui")]

use fastrender::ui::cancel::CancelGens;
use fastrender::ui::messages::{TabId, UiToWorker, WorkerToUi};
use fastrender::ui::test_worker::spawn_ui_worker;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use super::support;

struct Harness {
  tab_id: TabId,
  cancel_gens: CancelGens,
  tx: std::sync::mpsc::Sender<UiToWorker>,
  rx: Receiver<WorkerToUi>,
  join: std::thread::JoinHandle<()>,
}

impl Harness {
  fn new(tab_id: TabId) -> Self {
    let worker =
      spawn_ui_worker("fastr-browser-ui-worker-test").expect("spawn ui worker protocol harness");
    let cancel_gens = worker.cancel_gens();
    let (tx, rx, join) = worker.split();
    Self {
      tab_id,
      cancel_gens,
      tx,
      rx,
      join,
    }
  }

  fn send(&self, msg: UiToWorker) {
    self.tx.send(msg).expect("send UiToWorker");
  }

  fn recv_until(
    &self,
    timeout: Duration,
    mut f: impl FnMut(WorkerToUi) -> Option<WorkerToUi>,
  ) -> WorkerToUi {
    let deadline = Instant::now() + timeout;
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::ZERO);
      if remaining.is_zero() {
        panic!("timed out waiting for WorkerToUi message");
      }
      match self.rx.recv_timeout(remaining) {
        Ok(msg) => {
          if let Some(found) = f(msg) {
            return found;
          }
        }
        Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for WorkerToUi message"),
        Err(RecvTimeoutError::Disconnected) => panic!("worker disconnected"),
      }
    }
  }

  fn recv_next_frame(&self, timeout: Duration) -> fastrender::ui::messages::RenderedFrame {
    match self.recv_until(timeout, |msg| match msg {
      WorkerToUi::FrameReady { tab_id, frame } if tab_id == self.tab_id => {
        Some(WorkerToUi::FrameReady { tab_id, frame })
      }
      _ => None,
    }) {
      WorkerToUi::FrameReady { frame, .. } => frame,
      _ => unreachable!(),
    }
  }

  fn collect_until(
    &self,
    timeout: Duration,
    mut done: impl FnMut(&[WorkerToUi]) -> bool,
  ) -> Vec<WorkerToUi> {
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    loop {
      let remaining = deadline
        .checked_duration_since(Instant::now())
        .unwrap_or(Duration::ZERO);
      if remaining.is_zero() {
        return out;
      }
      // Use a short per-iteration timeout so we can re-check `done` frequently without sleeping for
      // the full remaining duration.
      let step = remaining.min(Duration::from_millis(50));
      match self.rx.recv_timeout(step) {
        Ok(msg) => out.push(msg),
        Err(RecvTimeoutError::Timeout) => {
          if done(&out) {
            return out;
          }
        }
        Err(RecvTimeoutError::Disconnected) => return out,
      }
      if done(&out) {
        return out;
      }
    }
  }

  fn shutdown(self) {
    let Self { tx, rx, join, .. } = self;
    drop(tx);
    drop(rx);
    join.join().expect("join ui worker thread");
  }
}

#[test]
fn create_tab_with_initial_url_emits_navigation_and_frame() {
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId::new();
  let h = Harness::new(tab_id);

  h.send(support::create_tab_with_cancel(
    tab_id,
    Some("about:newtab"),
    h.cancel_gens.clone(),
  ));

  let messages = h.collect_until(Duration::from_secs(3), |msgs| {
    let mut saw_started = false;
    let mut saw_loading_true = false;
    let mut saw_committed = false;
    let mut saw_frame = false;
    let mut saw_loading_false = false;
    for msg in msgs {
      match msg {
        WorkerToUi::NavigationStarted { tab_id: t, .. } if *t == tab_id => saw_started = true,
        WorkerToUi::LoadingState {
          tab_id: t,
          loading: true,
        } if *t == tab_id => saw_loading_true = true,
        WorkerToUi::NavigationCommitted { tab_id: t, .. } if *t == tab_id => saw_committed = true,
        WorkerToUi::FrameReady { tab_id: t, .. } if *t == tab_id => saw_frame = true,
        WorkerToUi::LoadingState {
          tab_id: t,
          loading: false,
        } if *t == tab_id => saw_loading_false = true,
        _ => {}
      }
    }
    saw_started && saw_loading_true && saw_committed && saw_frame && saw_loading_false
  });

  let events: Vec<&WorkerToUi> = messages
    .iter()
    .filter(|m| match m {
      WorkerToUi::Stage { tab_id: t, .. } => *t == tab_id,
      WorkerToUi::FrameReady { tab_id: t, .. } => *t == tab_id,
      WorkerToUi::OpenSelectDropdown { tab_id: t, .. } => *t == tab_id,
      WorkerToUi::SelectDropdownOpened { tab_id: t, .. } => *t == tab_id,
      WorkerToUi::SelectDropdownClosed { tab_id: t } => *t == tab_id,
      WorkerToUi::NavigationStarted { tab_id: t, .. } => *t == tab_id,
      WorkerToUi::NavigationCommitted { tab_id: t, .. } => *t == tab_id,
      WorkerToUi::LoadingState { tab_id: t, .. } => *t == tab_id,
      WorkerToUi::NavigationFailed { tab_id: t, .. } => *t == tab_id,
      WorkerToUi::ScrollStateUpdated { tab_id: t, .. } => *t == tab_id,
      WorkerToUi::DebugLog { tab_id: t, .. } => *t == tab_id,
    })
    .collect();

  let nav_started = events.iter().position(|m| {
    matches!(
      m,
      WorkerToUi::NavigationStarted { tab_id: t, url } if *t == tab_id && url == "about:newtab"
    )
  });
  let loading_true = events.iter().position(|m| {
    matches!(
      m,
      WorkerToUi::LoadingState { tab_id: t, loading: true } if *t == tab_id
    )
  });
  let nav_committed = events.iter().position(|m| {
    matches!(
      m,
      WorkerToUi::NavigationCommitted { tab_id: t, url, .. } if *t == tab_id && url == "about:newtab"
    )
  });
  let frame_ready = events.iter().position(|m| {
    matches!(
      m,
      WorkerToUi::FrameReady { tab_id: t, .. } if *t == tab_id
    )
  });
  let loading_false = events.iter().position(|m| {
    matches!(
      m,
      WorkerToUi::LoadingState { tab_id: t, loading: false } if *t == tab_id
    )
  });

  assert!(nav_started.is_some(), "expected NavigationStarted, got {events:?}");
  assert!(loading_true.is_some(), "expected LoadingState(true), got {events:?}");
  assert!(
    nav_committed.is_some(),
    "expected NavigationCommitted, got {events:?}"
  );
  assert!(frame_ready.is_some(), "expected FrameReady, got {events:?}");
  assert!(
    loading_false.is_some(),
    "expected LoadingState(false), got {events:?}"
  );

  let nav_started = nav_started.unwrap();
  let loading_true = loading_true.unwrap();
  let nav_committed = nav_committed.unwrap();
  let frame_ready = frame_ready.unwrap();
  let loading_false = loading_false.unwrap();

  assert!(
    nav_started < loading_true,
    "expected NavigationStarted before LoadingState(true), got {events:?}"
  );
  assert!(
    loading_true < nav_committed,
    "expected LoadingState(true) before NavigationCommitted, got {events:?}"
  );
  assert!(
    nav_committed < frame_ready,
    "expected NavigationCommitted before FrameReady, got {events:?}"
  );
  assert!(
    loading_true < loading_false,
    "expected LoadingState(false) after LoadingState(true), got {events:?}"
  );

  // Title extraction is expected to be stable for about:newtab.
  let committed = messages.iter().find_map(|m| match m {
    WorkerToUi::NavigationCommitted {
      tab_id: t,
      url,
      title,
      can_go_back,
      can_go_forward,
    } if *t == tab_id && url == "about:newtab" => Some((
      title.clone(),
      *can_go_back,
      *can_go_forward,
    )),
    _ => None,
  });
  let (title, can_go_back, can_go_forward) =
    committed.expect("expected NavigationCommitted payload");
  assert_eq!(title.as_deref(), Some("New Tab"));
  assert!(!can_go_back);
  assert!(!can_go_forward);

  h.shutdown();
}

#[test]
fn viewport_changed_triggers_frame_ready() {
  let _lock = super::stage_listener_test_lock();

  let tab_id = TabId::new();
  let h = Harness::new(tab_id);

  h.send(support::create_tab_with_cancel(
    tab_id,
    Some("about:newtab"),
    h.cancel_gens.clone(),
  ));

  // Drain initial navigation.
  let _initial = h.recv_next_frame(Duration::from_secs(3));

  h.send(support::viewport_changed_msg(tab_id, (320, 240), 2.0));

  let frame = h.recv_next_frame(Duration::from_secs(3));
  assert_eq!(frame.viewport_css, (320, 240));
  assert_eq!(frame.dpr, 2.0);

  h.shutdown();
}
