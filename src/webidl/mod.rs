//! Minimal, spec-shaped WebIDL metadata.
//!
//! This module does **not** implement WebIDL semantics. Instead, it provides a deterministic,
//! queryable snapshot of upstream WHATWG IDL blocks, resolved for `partial interface` and
//! `includes` so downstream codegen/bindings can build on top.
//!
//! The data under [`generated`] is committed to the repository and updated via:
//! `cargo xtask webidl` (alias for `cargo xtask web-idl-codegen`).

pub mod generated;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebIdlExtendedAttribute {
  pub name: &'static str,
  pub value: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebIdlInterfaceMember {
  pub name: Option<&'static str>,
  pub ext_attrs: &'static [WebIdlExtendedAttribute],
  /// Member text without trailing `;`.
  pub raw: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebIdlInterface {
  pub name: &'static str,
  pub inherits: Option<&'static str>,
  pub callback: bool,
  pub ext_attrs: &'static [WebIdlExtendedAttribute],
  pub members: &'static [WebIdlInterfaceMember],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebIdlInterfaceMixin {
  pub name: &'static str,
  pub ext_attrs: &'static [WebIdlExtendedAttribute],
  pub members: &'static [WebIdlInterfaceMember],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebIdlDictionaryMember {
  pub name: Option<&'static str>,
  pub ext_attrs: &'static [WebIdlExtendedAttribute],
  /// Member text without trailing `;`.
  pub raw: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebIdlDictionary {
  pub name: &'static str,
  pub inherits: Option<&'static str>,
  pub ext_attrs: &'static [WebIdlExtendedAttribute],
  pub members: &'static [WebIdlDictionaryMember],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebIdlEnum {
  pub name: &'static str,
  pub ext_attrs: &'static [WebIdlExtendedAttribute],
  pub values: &'static [&'static str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebIdlTypedef {
  pub name: &'static str,
  pub ext_attrs: &'static [WebIdlExtendedAttribute],
  pub type_: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebIdlCallback {
  pub name: &'static str,
  pub ext_attrs: &'static [WebIdlExtendedAttribute],
  pub type_: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebIdlWorld {
  pub interfaces: &'static [WebIdlInterface],
  pub interface_mixins: &'static [WebIdlInterfaceMixin],
  pub dictionaries: &'static [WebIdlDictionary],
  pub enums: &'static [WebIdlEnum],
  pub typedefs: &'static [WebIdlTypedef],
  pub callbacks: &'static [WebIdlCallback],
}

impl WebIdlWorld {
  pub fn interface(&self, name: &str) -> Option<&WebIdlInterface> {
    self.interfaces.iter().find(|i| i.name == name)
  }

  pub fn interface_mixin(&self, name: &str) -> Option<&WebIdlInterfaceMixin> {
    self.interface_mixins.iter().find(|i| i.name == name)
  }

  pub fn dictionary(&self, name: &str) -> Option<&WebIdlDictionary> {
    self.dictionaries.iter().find(|d| d.name == name)
  }

  pub fn enum_(&self, name: &str) -> Option<&WebIdlEnum> {
    self.enums.iter().find(|e| e.name == name)
  }

  pub fn typedef_(&self, name: &str) -> Option<&WebIdlTypedef> {
    self.typedefs.iter().find(|t| t.name == name)
  }

  pub fn callback(&self, name: &str) -> Option<&WebIdlCallback> {
    self.callbacks.iter().find(|c| c.name == name)
  }
}

#[cfg(test)]
mod tests {
  use super::generated::WORLD;

  #[test]
  fn generated_world_includes_document_inheritance_and_html_body() {
    let doc = WORLD
      .interface("Document")
      .expect("generated world should include Document interface");
    assert_eq!(doc.inherits, Some("Node"));

    let member_names = doc.members.iter().filter_map(|m| m.name).collect::<Vec<_>>();
    assert!(
      member_names.contains(&"createElement"),
      "expected Document to contain createElement (from DOM spec): {member_names:?}"
    );
    assert!(
      member_names.contains(&"body"),
      "expected Document to contain body (from HTML spec partial): {member_names:?}"
    );
  }

  #[test]
  fn generated_world_includes_whatwg_url_interfaces() {
    let url = WORLD
      .interface("URL")
      .expect("generated world should include URL interface (WHATWG URL)");
    let url_member_names = url.members.iter().filter_map(|m| m.name).collect::<Vec<_>>();
    for member in ["href", "origin", "searchParams"] {
      assert!(
        url_member_names.contains(&member),
        "expected URL to contain {member}: {url_member_names:?}"
      );
    }

    let params = WORLD
      .interface("URLSearchParams")
      .expect("generated world should include URLSearchParams interface (WHATWG URL)");
    let params_member_names = params.members.iter().filter_map(|m| m.name).collect::<Vec<_>>();
    for member in ["append", "getAll", "sort", "size"] {
      assert!(
        params_member_names.contains(&member),
        "expected URLSearchParams to contain {member}: {params_member_names:?}"
      );
    }
    assert!(
      params.members.iter().any(|m| m.raw.starts_with("iterable")),
      "expected URLSearchParams to contain iterable member; found: {:?}",
      params
        .members
        .iter()
        .map(|m| m.raw)
        .collect::<Vec<_>>()
    );
  }

  #[test]
  fn generated_world_includes_whatwg_fetch_interfaces() {
    assert!(
      WORLD.typedef_("HeadersInit").is_some(),
      "expected WebIDL world to include Fetch typedef HeadersInit"
    );
    assert!(
      WORLD.typedef_("BodyInit").is_some(),
      "expected WebIDL world to include Fetch typedef BodyInit"
    );

    let headers = WORLD
      .interface("Headers")
      .expect("generated world should include Headers interface (WHATWG Fetch)");
    let headers_member_names = headers
      .members
      .iter()
      .filter_map(|m| m.name)
      .collect::<Vec<_>>();
    for member in ["append", "get"] {
      assert!(
        headers_member_names.contains(&member),
        "expected Headers to contain {member}: {headers_member_names:?}"
      );
    }
    assert!(
      headers.members.iter().any(|m| m.raw.starts_with("iterable")),
      "expected Headers to contain iterable member; found: {:?}",
      headers
        .members
        .iter()
        .map(|m| m.raw)
        .collect::<Vec<_>>()
    );

    let request = WORLD
      .interface("Request")
      .expect("generated world should include Request interface (WHATWG Fetch)");
    let request_member_names = request
      .members
      .iter()
      .filter_map(|m| m.name)
      .collect::<Vec<_>>();
    for member in ["headers", "clone"] {
      assert!(
        request_member_names.contains(&member),
        "expected Request to contain {member}: {request_member_names:?}"
      );
    }

    let response = WORLD
      .interface("Response")
      .expect("generated world should include Response interface (WHATWG Fetch)");
    let response_member_names = response
      .members
      .iter()
      .filter_map(|m| m.name)
      .collect::<Vec<_>>();
    assert!(
      response_member_names.contains(&"headers"),
      "expected Response to contain headers: {response_member_names:?}"
    );
    assert!(
      response
        .members
        .iter()
        .any(|m| m.name == Some("json") && m.raw.starts_with("static")),
      "expected Response to contain static json(); found: {:?}",
      response
        .members
        .iter()
        .filter(|m| m.name == Some("json"))
        .map(|m| m.raw)
        .collect::<Vec<_>>()
    );

    let global = WORLD
      .interface_mixin("WindowOrWorkerGlobalScope")
      .expect("generated world should include WindowOrWorkerGlobalScope interface mixin");
    let global_member_names = global
      .members
      .iter()
      .filter_map(|m| m.name)
      .collect::<Vec<_>>();
    assert!(
      global_member_names.contains(&"fetch"),
      "expected WindowOrWorkerGlobalScope to contain fetch: {global_member_names:?}"
    );
  }

  #[test]
  fn generated_world_preserves_callback_interface_flag() {
    for name in ["EventListener", "NodeFilter", "XPathNSResolver"] {
      assert!(
        WORLD
          .interface(name)
          .unwrap_or_else(|| panic!("generated world should include {name} interface"))
          .callback,
        "expected {name} to be marked as a callback interface"
      );
    }

    assert!(
      !WORLD
        .interface("EventTarget")
        .expect("generated world should include EventTarget interface")
        .callback,
      "expected EventTarget to NOT be marked as a callback interface"
    );
  }

  #[test]
  fn generated_world_includes_window_globals_and_timers() {
    let window = WORLD
      .interface("Window")
      .expect("generated world should include Window interface");
    assert_eq!(
      window.inherits,
      Some("EventTarget"),
      "expected Window to inherit EventTarget"
    );

    let member_names = window
      .members
      .iter()
      .filter_map(|m| m.name)
      .collect::<Vec<_>>();
    assert!(
      member_names.contains(&"document"),
      "expected Window to contain document: {member_names:?}"
    );
    assert!(
      member_names.contains(&"setTimeout"),
      "expected Window to contain setTimeout: {member_names:?}"
    );

    WORLD
      .interface_mixin("WindowOrWorkerGlobalScope")
      .expect("generated world should include WindowOrWorkerGlobalScope mixin");
    WORLD
      .typedef_("TimerHandler")
      .expect("generated world should include TimerHandler typedef");
  }
}
