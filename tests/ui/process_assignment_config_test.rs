use fastrender::ui::process_assignment_config::{
  parse_process_model, process_model_from_env_value,
};
use fastrender::ui::process_assignment::ProcessModel;

#[test]
fn parse_defaults_to_tab() {
  assert_eq!(parse_process_model(None).unwrap(), ProcessModel::PerTab);
  assert_eq!(
    parse_process_model(Some("")).unwrap(),
    ProcessModel::PerTab
  );
  assert_eq!(
    parse_process_model(Some(" \n\t ")).unwrap(),
    ProcessModel::PerTab
  );
}

#[test]
fn parse_tab_values() {
  assert_eq!(parse_process_model(Some("tab")).unwrap(), ProcessModel::PerTab);
  assert_eq!(parse_process_model(Some("TAB")).unwrap(), ProcessModel::PerTab);
}

#[test]
fn parse_site_values() {
  assert_eq!(parse_process_model(Some("site")).unwrap(), ProcessModel::PerSiteKey);
  assert_eq!(
    parse_process_model(Some("origin")).unwrap(),
    ProcessModel::PerSiteKey
  );
}

#[test]
fn parse_rejects_invalid_values() {
  assert!(parse_process_model(Some("invalid")).is_err());
}

#[test]
fn env_value_helper_falls_back_to_default() {
  assert_eq!(
    process_model_from_env_value(Some("invalid")),
    ProcessModel::PerTab
  );
}
