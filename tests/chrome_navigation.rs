use fastrender::js::{WindowRealm, WindowRealmConfig};
use vm_js::Value;

#[test]
fn chrome_navigation_navigate_rejects_javascript_scheme_and_does_not_dispatch() {
  let mut realm =
    WindowRealm::new(WindowRealmConfig::new("https://example.com/")).expect("WindowRealm::new");

  let ok = realm
    .exec_script(
      r#"(() => {
        try { chrome.navigation.navigate('javascript:alert(1)'); return false; }
        catch (e) {
          return e instanceof TypeError && String(e).toLowerCase().includes('javascript');
        }
      })()"#,
    )
    .expect("script should catch the TypeError and return a boolean");

  assert_eq!(ok, Value::Bool(true));
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "rejected scheme should not dispatch navigation"
  );
}

#[test]
fn chrome_navigation_navigate_rejects_overlong_url_and_does_not_dispatch() {
  let mut realm =
    WindowRealm::new(WindowRealmConfig::new("https://example.com/")).expect("WindowRealm::new");

  let ok = realm
    .exec_script(
      r#"(() => {
        const url = 'https://example.com/' + 'a'.repeat(9000);
        try { chrome.navigation.navigate(url); return false; }
        catch (e) {
          return e instanceof TypeError && String(e).toLowerCase().includes('too long');
        }
      })()"#,
    )
    .expect("script should catch the TypeError and return a boolean");

  assert_eq!(ok, Value::Bool(true));
  assert!(
    realm.take_pending_navigation_request().is_none(),
    "overlong URL should not dispatch navigation"
  );
}

