use crate::error::{RenderError, RenderStage};
use crate::paint::depth_sort;
use crate::paint::display_list::Transform3D;
use crate::render_control::{with_deadline, CancelCallback, RenderDeadline};
use crate::Rect;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[test]
fn preserve_3d_depth_sort_respects_deadline_timeout() {
  let plane_rect = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
  // Needs enough planes to ensure the nested pairwise sort loop runs long enough
  // to hit multiple periodic deadline checks.
  let planes = 80usize;
  let items: Vec<depth_sort::SceneItem> = (0..planes)
    .map(|paint_order| depth_sort::SceneItem {
      transform: Transform3D::translate(0.0, 0.0, paint_order as f32),
      plane_rect,
      paint_order,
    })
    .collect();

  let checks = Arc::new(AtomicUsize::new(0));
  let checks_for_cb = Arc::clone(&checks);
  // Require at least 3 checks (entry + two periodic loop checks) so this test
  // fails if deadline polling only happens at the start/end of sorting.
  let cancel: Arc<CancelCallback> = Arc::new(move || {
    let prev = checks_for_cb.fetch_add(1, Ordering::SeqCst);
    prev >= 2
  });
  let deadline = RenderDeadline::new(None, Some(cancel));

  let result = with_deadline(Some(&deadline), || depth_sort::depth_sort_checked(&items));

  assert!(
    checks.load(Ordering::SeqCst) >= 3,
    "expected >=3 deadline checks during depth sort loop, got {}",
    checks.load(Ordering::SeqCst)
  );
  assert!(
    matches!(
      result,
      Err(RenderError::Timeout {
        stage: RenderStage::Paint,
        ..
      })
    ),
    "expected paint-stage timeout, got {result:?}"
  );
}

