use super::*;

use std::time::Duration;

impl ProgramState {
  pub(in super::super) fn cache_body_result(&mut self, body: BodyId, result: Arc<BodyCheckResult>) {
    self.body_results.insert(body, Arc::clone(&result));
    if !self.snapshot_loaded {
      self.typecheck_db.lock().set_body_result(body, result);
    }
  }

  #[allow(dead_code)]
  fn cached_body_result(&mut self, body: BodyId) -> Option<Arc<BodyCheckResult>> {
    let cached = self.body_results.get(&body).cloned()?;
    if !self.snapshot_loaded {
      self
        .typecheck_db
        .lock()
        .set_body_result(body, Arc::clone(&cached));
    }
    self
      .query_stats
      .record(QueryKind::CheckBody, true, Duration::ZERO);
    Some(cached)
  }

  #[allow(dead_code)]
  pub(in super::super) fn check_body(
    &mut self,
    body_id: BodyId,
  ) -> Result<Arc<BodyCheckResult>, FatalError> {
    self.check_cancelled()?;
    if let Some(cached) = self.cached_body_result(body_id) {
      return Ok(cached);
    }

    if self.snapshot_loaded {
      let res = BodyCheckResult::empty(body_id);
      self.body_results.insert(body_id, Arc::clone(&res));
      self
        .query_stats
        .record(QueryKind::CheckBody, false, Duration::ZERO);
      return Ok(res);
    }

    let context = self.body_check_context();
    let db = BodyCheckDb::from_shared_context(context);
    let res = db::queries::body_check::check_body(&db, body_id);
    self.cache_body_result(body_id, Arc::clone(&res));
    Ok(res)
  }

  pub(in super::super) fn check_body_for_inference(
    &mut self,
    body_id: BodyId,
  ) -> Result<Arc<BodyCheckResult>, FatalError> {
    self.check_cancelled()?;
    // Inference checks are often triggered while building the shared body-check
    // context (e.g. to infer function return types). Reuse any cached
    // `BodyCheckResult` when available so incremental edits do not force
    // unaffected bodies to be re-checked purely for type inference.
    if let Some(cached) = self.cached_body_result(body_id) {
      return Ok(cached);
    }
    if self.snapshot_loaded {
      return Ok(BodyCheckResult::empty(body_id));
    }
    let context = Arc::new(self.build_body_check_context_snapshot());
    let db = BodyCheckDb::from_shared_context(context);
    let res = db::queries::body_check::check_body(&db, body_id);
    Ok(res)
  }
}
