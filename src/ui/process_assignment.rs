use crate::multiprocess::RendererProcessId;
use crate::site_isolation::{site_key_for_navigation, SiteKey};
use crate::ui::TabId;
use std::collections::HashMap;
use url::Url;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessAssignmentEvent {
  SpawnProcess { process: RendererProcessId, site: SiteKey },
  ShutdownProcess { process: RendererProcessId },
}

/// Browser-side bookkeeping for process-assignment policy.
///
/// This state is a policy primitive; higher-level code is responsible for reacting to violations
/// (e.g. terminating the renderer or showing a crash page).
#[derive(Debug, Clone)]
pub struct ProcessAssignmentState {
  process_model: ProcessModel,
  next_process_id: u64,
  tab_to_process: HashMap<TabId, RendererProcessId>,
  process_to_site: HashMap<RendererProcessId, SiteKey>,
  process_refcount: HashMap<RendererProcessId, usize>,
  /// Only used in `PerSiteKey` mode.
  site_to_process: HashMap<SiteKey, RendererProcessId>,
}

impl Default for ProcessAssignmentState {
  fn default() -> Self {
    Self::new(ProcessModel::default())
  }
}

impl ProcessAssignmentState {
  pub fn new(process_model: ProcessModel) -> Self {
    Self {
      process_model,
      next_process_id: 1,
      tab_to_process: HashMap::new(),
      process_to_site: HashMap::new(),
      process_refcount: HashMap::new(),
      site_to_process: HashMap::new(),
    }
  }

  fn alloc_process_id(&mut self) -> RendererProcessId {
    loop {
      let id = self.next_process_id;
      self.next_process_id = self.next_process_id.wrapping_add(1);
      if id == 0 {
        continue;
      }
      let pid = RendererProcessId::new(id);
      if self.process_to_site.contains_key(&pid) {
        continue;
      }
      return pid;
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

  pub fn process_for_tab(&self, tab_id: TabId) -> Option<RendererProcessId> {
    self.tab_to_process.get(&tab_id).copied()
  }

  /// Validate that `process` owns (is attached to) `tab_id`.
  ///
  /// This should be called before applying any renderer→browser message that references a `tab_id`
  /// to prevent a compromised renderer process from spoofing/overwriting other tabs.
  pub fn validate_process_owns_tab(
    &self,
    process: RendererProcessId,
    tab_id: TabId,
  ) -> Result<(), String> {
    let owner = self
      .tab_to_process
      .get(&tab_id)
      .copied()
      .ok_or_else(|| format!("tab not attached: {:?}", tab_id))?;

    if owner != process {
      return Err(format!(
        "tab ownership violation: tab {:?} is owned by {:?}, not {:?}",
        tab_id, owner, process
      ));
    }

    Ok(())
  }

  /// Validate that `process` is allowed to commit `committed_url`, updating internal state when
  /// site isolation is disabled.
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

    // `site_key_for_navigation` treats unparseable URLs as opaque site keys. For a committed
    // navigation this is almost certainly a renderer bug or hostile input; reject it explicitly so
    // callers can treat it as a protocol violation.
    Url::parse(committed_url)
      .map_err(|err| format!("invalid committed URL {committed_url:?}: {err}"))?;

    // When deriving a SiteKey from a commit, treat the current process lock as the "parent" so
    // special URLs like `about:blank` inherit the existing site key.
    let committed_site = site_key_for_navigation(committed_url, Some(&locked_site));

    match self.process_model {
      ProcessModel::PerSiteKey => {
        if committed_site != locked_site {
          return Err(format!(
            "site lock violation: process {:?} locked to {} attempted to commit {} ({})",
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

  pub fn attach_tab(
    &mut self,
    tab_id: TabId,
    initial_url: &str,
  ) -> Result<(RendererProcessId, Vec<ProcessAssignmentEvent>), String> {
    if self.tab_to_process.contains_key(&tab_id) {
      return Err(format!("tab {tab_id:?} is already attached"));
    }

    Url::parse(initial_url).map_err(|err| format!("invalid URL {initial_url:?}: {err}"))?;
    let site = site_key_for_navigation(initial_url, None);

    match self.process_model {
      ProcessModel::PerTab => {
        let process = self.alloc_process_id();
        self.tab_to_process.insert(tab_id, process);
        self.process_refcount.insert(process, 1);
        self.set_site_lock(process, site.clone());
        Ok((process, vec![ProcessAssignmentEvent::SpawnProcess { process, site }]))
      }
      ProcessModel::PerSiteKey => match self.site_to_process.get(&site).copied() {
        Some(process) => {
          // Invariant: site_to_process must always point to a process whose site matches.
          match self.process_to_site.get(&process) {
            Some(actual_site) if actual_site == &site => {}
            Some(actual_site) => {
              return Err(format!(
                "process assignment invariant violated: site_to_process[{site:?}] points to {process:?}, but process_to_site has {actual_site:?}"
              ));
            }
            None => {
              return Err(format!(
                "process assignment invariant violated: site_to_process[{site:?}] points to {process:?}, but process_to_site has no entry"
              ));
            }
          }

          let new_refcount = self
            .process_refcount
            .get(&process)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
          self.process_refcount.insert(process, new_refcount);
          self.tab_to_process.insert(tab_id, process);
          Ok((process, Vec::new()))
        }
        None => {
          let process = self.alloc_process_id();
          self.tab_to_process.insert(tab_id, process);
          self.process_refcount.insert(process, 1);
          self.site_to_process.insert(site.clone(), process);
          self.set_site_lock(process, site.clone());
          Ok((process, vec![ProcessAssignmentEvent::SpawnProcess { process, site }]))
        }
      },
    }
  }

  /// Return the renderer process for a navigation, potentially reassigning the tab.
  ///
  /// In `PerSiteKey` mode, cross-site navigations reassign the tab to the process for the target
  /// site key (spawning it if needed).
  ///
  /// In `PerTab` mode, navigations never swap processes (MVP behaviour).
  pub fn process_for_navigation(
    &mut self,
    tab_id: TabId,
    target_url: &str,
  ) -> Result<(RendererProcessId, Vec<ProcessAssignmentEvent>), String> {
    let current_process = self
      .tab_to_process
      .get(&tab_id)
      .copied()
      .ok_or_else(|| format!("tab not attached: {:?}", tab_id))?;

    Url::parse(target_url).map_err(|err| format!("invalid URL {target_url:?}: {err}"))?;

    let current_site = self
      .process_to_site
      .get(&current_process)
      .ok_or_else(|| format!("unknown renderer process: {:?}", current_process))?
      .clone();
    let target_site = site_key_for_navigation(target_url, Some(&current_site));

    match self.process_model {
      ProcessModel::PerTab => Ok((current_process, Vec::new())),
      ProcessModel::PerSiteKey => {
        let mut events = Vec::new();

        let desired_process = match self.site_to_process.get(&target_site).copied() {
          Some(process) => {
            // Invariant: site_to_process must always point to a process whose site matches.
            match self.process_to_site.get(&process) {
              Some(actual_site) if actual_site == &target_site => {}
              Some(actual_site) => {
                return Err(format!(
                  "process assignment invariant violated: site_to_process[{target_site:?}] points to {process:?}, but process_to_site has {actual_site:?}"
                ));
              }
              None => {
                return Err(format!(
                  "process assignment invariant violated: site_to_process[{target_site:?}] points to {process:?}, but process_to_site has no entry"
                ));
              }
            }
            process
          }
          None => {
            let process = self.alloc_process_id();
            self.site_to_process.insert(target_site.clone(), process);
            self.set_site_lock(process, target_site.clone());
            events.push(ProcessAssignmentEvent::SpawnProcess {
              process,
              site: target_site.clone(),
            });
            process
          }
        };

        if desired_process == current_process {
          return Ok((desired_process, events));
        }

        self.tab_to_process.insert(tab_id, desired_process);

        // Update the refcount for the new process based on the authoritative tab→process mapping.
        let desired_tabs = self
          .tab_to_process
          .values()
          .filter(|&&p| p == desired_process)
          .count();
        self.process_refcount.insert(desired_process, desired_tabs);

        // Recompute old process refcount and shut it down if it becomes unused.
        let remaining_tabs = self
          .tab_to_process
          .values()
          .filter(|&&p| p == current_process)
          .count();

        if remaining_tabs == 0 {
          self.process_refcount.remove(&current_process);

          if let Some(site) = self.process_to_site.remove(&current_process) {
            if self.site_to_process.get(&site).copied() == Some(current_process) {
              self.site_to_process.remove(&site);
            }
          } else {
            self.site_to_process.retain(|_, &mut p| p != current_process);
          }

          events.push(ProcessAssignmentEvent::ShutdownProcess {
            process: current_process,
          });
        } else {
          self.process_refcount.insert(current_process, remaining_tabs);
        }

        Ok((desired_process, events))
      }
    }
  }

  pub fn detach_tab(&mut self, tab_id: TabId) -> Vec<ProcessAssignmentEvent> {
    let mut events = Vec::new();

    let Some(process) = self.tab_to_process.remove(&tab_id) else {
      return events;
    };

    // Recompute refcount from the authoritative tab→process mapping to avoid underflow and to keep
    // state robust if callers accidentally desynchronize the explicit refcount map.
    let remaining_tabs = self
      .tab_to_process
      .values()
      .filter(|&&p| p == process)
      .count();

    if remaining_tabs == 0 {
      self.process_refcount.remove(&process);

      if let Some(site) = self.process_to_site.remove(&process) {
        if self.process_model == ProcessModel::PerSiteKey {
          if self.site_to_process.get(&site).copied() == Some(process) {
            self.site_to_process.remove(&site);
          }
        }
      } else if self.process_model == ProcessModel::PerSiteKey {
        self.site_to_process.retain(|_, &mut p| p != process);
      }

      events.push(ProcessAssignmentEvent::ShutdownProcess { process });
      return events;
    }

    self.process_refcount.insert(process, remaining_tabs);
    events
  }

  /// Handle a renderer process exiting unexpectedly (crash/termination).
  ///
  /// This detaches all tabs currently mapped to `process`, clears process/site bookkeeping so future
  /// assignments can recreate a replacement process, and returns the affected tabs in sorted order.
  ///
  /// If `process` is unknown to the state machine, this is a no-op and returns an empty vector.
  pub fn handle_process_exit(&mut self, process: RendererProcessId) -> Vec<TabId> {
    let mut affected_tabs: Vec<TabId> = self
      .tab_to_process
      .iter()
      .filter_map(|(&tab_id, &pid)| (pid == process).then_some(tab_id))
      .collect();

    for tab_id in &affected_tabs {
      self.tab_to_process.remove(tab_id);
    }

    self.process_refcount.remove(&process);

    if let Some(site) = self.process_to_site.remove(&process) {
      if self.site_to_process.get(&site).copied() == Some(process) {
        self.site_to_process.remove(&site);
      }
    }

    // Defensive cleanup: `process_to_site` is the authoritative reverse map, but ensure we remove
    // any stray values even if invariants were already violated.
    let stale_sites: Vec<SiteKey> = self
      .site_to_process
      .iter()
      .filter_map(|(site, &pid)| (pid == process).then(|| site.clone()))
      .collect();
    for site in stale_sites {
      self.site_to_process.remove(&site);
    }

    affected_tabs.sort_by_key(|tab| tab.0);
    affected_tabs
  }

  pub fn site_for_process(&self, process: RendererProcessId) -> Option<&SiteKey> {
    self.site_lock(process)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn site(url: &str) -> SiteKey {
    site_key_for_navigation(url, None)
  }

  #[test]
  fn attached_tab_validates() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerTab);
    let tab_id = TabId(42);

    let (process, _events) = state.attach_tab(tab_id, "https://example.com").unwrap();

    assert_eq!(state.validate_process_owns_tab(process, tab_id), Ok(()));
  }

  #[test]
  fn wrong_process_returns_err() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerTab);
    let tab_id = TabId(42);

    let (owner, _events) = state.attach_tab(tab_id, "https://example.com").unwrap();
    let attacker = RendererProcessId::new(owner.raw().saturating_add(1));

    let err = state
      .validate_process_owns_tab(attacker, tab_id)
      .expect_err("expected ownership violation");
    assert!(err.contains("tab ownership violation"), "{err}");
    assert!(err.contains(&format!("{:?}", owner)), "{err}");
    assert!(err.contains(&format!("{:?}", attacker)), "{err}");
  }

  #[test]
  fn unattached_tab_returns_err() {
    let state = ProcessAssignmentState::new(ProcessModel::PerTab);
    let process = RendererProcessId::new(1);
    let tab_id = TabId(42);

    let err = state
      .validate_process_owns_tab(process, tab_id)
      .expect_err("expected unknown tab error");
    assert!(err.contains("tab not attached") || err.contains("unknown tab"), "{err}");
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

  #[test]
  fn unknown_process_is_an_error() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerTab);
    let err = state
      .validate_or_update_site_lock(RendererProcessId::new(42), "https://example.com")
      .expect_err("unknown process must error");
    assert!(err.contains("unknown renderer process"));
  }

  #[test]
  fn process_assignment_per_tab_spawns_unique_processes() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerTab);
    let tab_a = TabId(1);
    let tab_b = TabId(2);

    let (proc_a, events_a) = state.attach_tab(tab_a, "https://example.com").unwrap();
    let (proc_b, events_b) = state.attach_tab(tab_b, "https://example.com").unwrap();

    assert_ne!(proc_a, proc_b);
    assert_eq!(
      events_a,
      vec![ProcessAssignmentEvent::SpawnProcess {
        process: proc_a,
        site: site("https://example.com")
      }]
    );
    assert_eq!(
      events_b,
      vec![ProcessAssignmentEvent::SpawnProcess {
        process: proc_b,
        site: site("https://example.com")
      }]
    );
  }

  #[test]
  fn process_assignment_per_site_key_reuses_process_for_same_site() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerSiteKey);
    let tab_a = TabId(1);
    let tab_b = TabId(2);

    let (proc_a, events_a) = state.attach_tab(tab_a, "https://example.com").unwrap();
    let (proc_b, events_b) = state
      .attach_tab(tab_b, "https://example.com/some/path")
      .unwrap();

    assert_eq!(proc_a, proc_b);
    assert_eq!(events_a.len(), 1);
    assert!(events_b.is_empty());
    assert_eq!(state.process_refcount.get(&proc_a).copied(), Some(2));
    assert_eq!(state.site_for_process(proc_a), Some(&site("https://example.com")));
  }

  #[test]
  fn process_assignment_per_site_key_separates_different_sites() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerSiteKey);

    let (proc_a, events_a) = state.attach_tab(TabId(1), "https://example.com").unwrap();
    let (proc_b, events_b) = state.attach_tab(TabId(2), "https://example.org").unwrap();

    assert_ne!(proc_a, proc_b);
    assert_eq!(events_a.len(), 1);
    assert_eq!(events_b.len(), 1);
  }

  #[test]
  fn process_assignment_detach_emits_shutdown_when_last_tab_closes() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerSiteKey);
    let tab_a = TabId(1);
    let tab_b = TabId(2);

    let (proc, _events_a) = state.attach_tab(tab_a, "https://example.com").unwrap();
    state.attach_tab(tab_b, "https://example.com").unwrap();

    let events_a = state.detach_tab(tab_a);
    assert!(events_a.is_empty());
    assert_eq!(state.process_refcount.get(&proc).copied(), Some(1));

    let events_b = state.detach_tab(tab_b);
    assert_eq!(
      events_b,
      vec![ProcessAssignmentEvent::ShutdownProcess { process: proc }]
    );
    assert!(state.process_refcount.get(&proc).is_none());
    assert!(state.process_to_site.get(&proc).is_none());
    assert!(state.site_to_process.is_empty());
  }

  #[test]
  fn per_tab_process_exit_detaches_only_affected_tab() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerTab);
    let tab_a = TabId(1);
    let tab_b = TabId(2);

    let (proc_a, _events_a) = state.attach_tab(tab_a, "https://a.example/").unwrap();
    let (proc_b, _events_b) = state.attach_tab(tab_b, "https://b.example/").unwrap();

    assert_ne!(proc_a, proc_b);
    assert_eq!(state.process_for_tab(tab_a), Some(proc_a));
    assert_eq!(state.process_for_tab(tab_b), Some(proc_b));

    let affected = state.handle_process_exit(proc_a);
    assert_eq!(affected, vec![tab_a]);

    assert_eq!(state.process_for_tab(tab_a), None);
    assert_eq!(state.process_for_tab(tab_b), Some(proc_b));
    assert!(state.process_refcount.get(&proc_a).is_none());
    assert!(state.process_to_site.get(&proc_a).is_none());

    // Closing a crashed tab should be safe/no-op.
    assert!(state.detach_tab(tab_a).is_empty());
  }

  #[test]
  fn per_site_key_process_exit_detaches_all_tabs_and_clears_site_mapping() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerSiteKey);
    let tab_a = TabId(1);
    let tab_b = TabId(2);

    let (proc_a, _events_a) = state.attach_tab(tab_a, "https://example.com/").unwrap();
    let (proc_b, _events_b) = state
      .attach_tab(tab_b, "https://example.com/some/path")
      .unwrap();

    assert_eq!(proc_a, proc_b);
    assert_eq!(state.site_to_process.get(&site("https://example.com/")).copied(), Some(proc_a));

    let affected = state.handle_process_exit(proc_a);
    assert_eq!(affected, vec![tab_a, tab_b]);

    assert_eq!(state.process_for_tab(tab_a), None);
    assert_eq!(state.process_for_tab(tab_b), None);
    assert!(state.process_refcount.get(&proc_a).is_none());
    assert!(state.process_to_site.get(&proc_a).is_none());
    assert!(state.site_to_process.is_empty());

    // Detaching after a crash should be safe/no-op.
    assert!(state.detach_tab(tab_a).is_empty());
    assert!(state.detach_tab(tab_b).is_empty());
  }

  #[test]
  fn per_site_key_cross_site_navigation_swaps_process_and_shuts_down_old() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerSiteKey);

    let tab = TabId(1);
    let (p1, _events) = state.attach_tab(tab, "https://example.com/").unwrap();

    let (p2, events) = state
      .process_for_navigation(tab, "https://evil.com/")
      .unwrap();
    assert_ne!(p1, p2);
    assert_eq!(
      events,
      vec![
        ProcessAssignmentEvent::SpawnProcess {
          process: p2,
          site: site("https://evil.com/"),
        },
        ProcessAssignmentEvent::ShutdownProcess { process: p1 }
      ]
    );
    assert_eq!(state.process_for_tab(tab), Some(p2));
  }

  #[test]
  fn per_site_key_cross_site_navigation_preserves_old_process_when_shared() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerSiteKey);

    let tab1 = TabId(1);
    let tab2 = TabId(2);
    let (p1, _events) = state.attach_tab(tab1, "https://example.com/").unwrap();
    let (p1_again, events2) = state.attach_tab(tab2, "https://example.com/").unwrap();
    assert_eq!(p1_again, p1);
    assert!(events2.is_empty());

    let (p2, events) = state
      .process_for_navigation(tab1, "https://evil.com/path")
      .unwrap();
    assert_ne!(p2, p1);
    assert_eq!(
      events,
      vec![ProcessAssignmentEvent::SpawnProcess {
        process: p2,
        site: site("https://evil.com/path"),
      }]
    );

    assert_eq!(state.process_for_tab(tab1), Some(p2));
    assert_eq!(state.process_for_tab(tab2), Some(p1));
  }

  #[test]
  fn per_site_key_same_site_navigation_keeps_process_and_emits_no_events() {
    let mut state = ProcessAssignmentState::new(ProcessModel::PerSiteKey);

    let tab = TabId(1);
    let (p1, _events) = state.attach_tab(tab, "https://example.com/").unwrap();

    let (p2, events) = state
      .process_for_navigation(tab, "https://example.com/other")
      .unwrap();
    assert_eq!(p2, p1);
    assert!(events.is_empty());
  }
}
