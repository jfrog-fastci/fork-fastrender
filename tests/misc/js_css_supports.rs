use fastrender::js::{WindowRealm, WindowRealmConfig};
use vm_js::{Job, RealmId, Value, VmError, VmHostHooks};
 
#[derive(Default)]
struct NoopHostHooks;
 
impl VmHostHooks for NoopHostHooks {
  fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<RealmId>) {}
}
 
fn exec_script(realm: &mut WindowRealm, source: &str) -> std::result::Result<Value, VmError> {
  let mut host_ctx = ();
  let mut hooks = NoopHostHooks::default();
  realm.exec_script_with_host_and_hooks(&mut host_ctx, &mut hooks, source)
}
 
#[test]
fn window_realm_exec_script_css_supports() {
  let mut realm = WindowRealm::new(WindowRealmConfig::new("https://example.com/")).unwrap();
 
  let has_css = exec_script(&mut realm, r#"typeof CSS === "object" && CSS !== null"#).unwrap();
  assert_eq!(has_css, Value::Bool(true));
 
  let has_supports = exec_script(&mut realm, r#"typeof CSS.supports === "function""#).unwrap();
  assert_eq!(has_supports, Value::Bool(true));
 
  // supports(property, value)
  assert_eq!(
    exec_script(&mut realm, r#"CSS.supports(" width", "5px")"#).unwrap(),
    Value::Bool(false)
  );
  assert_eq!(
    exec_script(&mut realm, r#"CSS.supports("width", "5px !important")"#).unwrap(),
    Value::Bool(false)
  );
  assert_eq!(
    exec_script(&mut realm, r#"CSS.supports("display", "-ms-grid")"#).unwrap(),
    Value::Bool(false)
  );
  assert_eq!(
    exec_script(&mut realm, r#"CSS.supports("--fake-var", 0)"#).unwrap(),
    Value::Bool(true)
  );
  assert_eq!(
    exec_script(&mut realm, r#"CSS.supports("display", "grid")"#).unwrap(),
    Value::Bool(true)
  );
 
  // supports(conditionText)
  assert_eq!(
    exec_script(&mut realm, r#"CSS.supports("(display: grid)")"#).unwrap(),
    Value::Bool(true)
  );
  assert_eq!(
    exec_script(&mut realm, r#"CSS.supports("display: grid")"#).unwrap(),
    Value::Bool(true)
  );
 
  // selector() queries are heavily used by real sites (e.g. `:has()` feature checks).
  assert_eq!(
    exec_script(&mut realm, r#"CSS.supports("selector(:has(*))")"#).unwrap(),
    Value::Bool(true)
  );
 
  // Namespaces are treated as invalid in conditionText selector() arguments.
  assert_eq!(
    exec_script(&mut realm, r#"CSS.supports("selector(*|a)")"#).unwrap(),
    Value::Bool(false)
  );
}
