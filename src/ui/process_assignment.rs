use crate::multiprocess::RendererProcessId;
use crate::site_isolation::{site_key_for_navigation, SiteKey};
use std::collections::HashMap;

/// Controls how the browser assigns renderer processes to navigations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessModel {
  /// MVP: one renderer process per tab/frame, navigations may cross sites.
  PerTab,
  /// Site isolation: one renderer process per [`SiteKey`].
  ///
  /// In this mode, a renderer process is "site-locked": it must never commit a navigation whose
  /// derived `SiteKey` differs from the browser-assigned lock.
  PerSiteKey,
}

impl Default for ProcessModel {
  fn default() -> Self {
    Self::PerTab
  }
}

/// Browser-side bookkeeping for which renderer processes are locked to which sites.
#[derive(Debug, Default)]
pub struct ProcessAssignmentState {
  process_model: ProcessModel,
  process_to_site: HashMap<RendererProcessId, SiteKey>,
}

impl ProcessAssignmentState {
  pub fn new(process_model: ProcessModel) -> Self {
    Self {
      process_model,
      process_to_site: HashMap::new(),
    }
  }

  pub fn process_model(&self) -> ProcessModel {
    self.process_model
  }

  pub fn set_process_model(&mut self, model: ProcessModel) {
    self.process_model = model;
  }

  pub fn set_site_lock(&mut self, process: RendererProcessId, site: SiteKey) {
    self.process_to_site.insert(process, site);
  }

  pub fn site_lock(&self, process: RendererProcessId) -> Option<&SiteKey> {
    self.process_to_site.get(&process)
  }

  /// Validate that `process` is allowed to commit `committed_url`, updating internal state when
  /// site isolation is disabled.
  ///
  /// This is a policy primitive; higher-level code is responsible for reacting to violations (e.g.
  /// terminating the renderer or showing a crash page).
  pub fn validate_or_update_site_lock(
    &mut self,
    process: RendererProcessId,
    committed_url: &str,
  ) -> Result<(), String> {
    let locked_site = self
      .process_to_site
      .get(&process)
      .ok_or_else(|| format!("unknown renderer process: {:?}", process))?
      .clone();

    // When deriving a SiteKey from a commit, treat the current process lock as the "parent" so
    // special URLs like `about:blank` inherit the existing site key.
    let committed_site = site_key_for_navigation(committed_url, Some(&locked_site));

    match self.process_model {
      ProcessModel::PerSiteKey => {
        if committed_site != locked_site {
          return Err(format!(
            "site lock violation: process {:?} locked to {:?} attempted to commit {:?} ({})",
            process, locked_site, committed_site, committed_url
          ));
        }
        Ok(())
      }
      ProcessModel::PerTab => {
        self.process_to_site.insert(process, committed_site);
        Ok(())
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn site(url: &str) -> SiteKey {
    site_key_for_navigation(url, None)
  }

  #[test]
  fn per_tab_updates_site_lock_on_cross_site_commit() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerTab);
    let process = RendererProcessId::new(1);
    state.set_site_lock(process, site("https://example.com"));

    state
      .validate_or_update_site_lock(process, "https://evil.com")
      .expect("PerTab should allow cross-site commits");

    assert_eq!(state.site_lock(process), Some(&site("https://evil.com")));
  }

  #[test]
  fn per_site_key_rejects_cross_site_commit_without_mutation() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerSiteKey);
    let process = RendererProcessId::new(1);
    let initial = site("https://example.com");
    state.set_site_lock(process, initial.clone());

    let err = state
      .validate_or_update_site_lock(process, "https://evil.com")
      .expect_err("PerSiteKey must reject cross-site commits");
    assert!(err.contains("site lock violation"));

    assert_eq!(state.site_lock(process), Some(&initial));
  }

  #[test]
  fn per_site_key_allows_about_blank_commit_in_locked_process() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerSiteKey);
    let process = RendererProcessId::new(1);
    let initial = site("https://example.com");
    state.set_site_lock(process, initial.clone());

    state
      .validate_or_update_site_lock(process, "about:blank")
      .expect("about:blank should inherit the existing site lock");

    assert_eq!(state.site_lock(process), Some(&initial));
  }
}
