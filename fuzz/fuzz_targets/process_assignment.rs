#![no_main]

use arbitrary::Arbitrary;
use fastrender::multiprocess::RendererProcessId;
use fastrender::site_isolation::{site_key_for_navigation, SiteKey};
use fastrender::ui::process_assignment::{ProcessAssignmentState, ProcessModel};
use libfuzzer_sys::fuzz_target;
use std::collections::HashMap;

const MAX_OPS: usize = 64;

#[derive(Arbitrary, Debug)]
enum Op {
  SetModel { per_site_key: bool },
  SetSiteLock { process: u8, use_example_org: bool },
  Commit { process: u8, use_example_org: bool },
}

#[derive(Arbitrary, Debug)]
struct Case {
  ops: Vec<Op>,
}

fn url_for_site(use_example_org: bool) -> &'static str {
  if use_example_org {
    "https://example.org/"
  } else {
    "https://example.com/"
  }
}

fuzz_target!(|case: Case| {
  let mut state = ProcessAssignmentState::new(ProcessModel::PerTab);
  let mut model = ProcessModel::PerTab;
  let mut expected: HashMap<RendererProcessId, SiteKey> = HashMap::new();

  for op in case.ops.iter().take(MAX_OPS) {
    match op {
      Op::SetModel { per_site_key } => {
        model = if *per_site_key {
          ProcessModel::PerSiteKey
        } else {
          ProcessModel::PerTab
        };
        state.set_process_model(model);
      }
      Op::SetSiteLock {
        process,
        use_example_org,
      } => {
        let pid = RendererProcessId::new(u64::from(*process) + 1);
        let url = url_for_site(*use_example_org);
        let site = site_key_for_navigation(url, None, false);
        state.set_site_lock(pid, site.clone());
        expected.insert(pid, site);
      }
      Op::Commit {
        process,
        use_example_org,
      } => {
        let pid = RendererProcessId::new(u64::from(*process) + 1);
        let url = url_for_site(*use_example_org);

        let Some(locked_site) = expected.get(&pid).cloned() else {
          // Unknown process should always error and should never mutate state.
          let res = state.validate_or_update_site_lock(pid, url);
          assert!(res.is_err());
          assert!(state.site_lock(pid).is_none());
          continue;
        };

        let committed_site = site_key_for_navigation(url, Some(&locked_site), false);
        let res = state.validate_or_update_site_lock(pid, url);

        match model {
          ProcessModel::PerSiteKey => {
            if committed_site != locked_site {
              assert!(res.is_err());
            } else {
              assert!(res.is_ok());
            }
            // PerSiteKey must never mutate the lock.
            assert_eq!(state.site_lock(pid), Some(&locked_site));
          }
          ProcessModel::PerTab => {
            assert!(res.is_ok());
            expected.insert(pid, committed_site.clone());
            assert_eq!(state.site_lock(pid), Some(&committed_site));
          }
        }
      }
    }
  }

  // Final consistency check: state must match our model of expected locks.
  for (pid, expected_site) in &expected {
    assert_eq!(state.site_lock(*pid), Some(expected_site));
  }
});
