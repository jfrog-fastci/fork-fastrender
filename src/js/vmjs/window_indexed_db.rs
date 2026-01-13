//! Minimal IndexedDB presence shim for `vm-js` Window realms.
//!
//! FastRender does not currently implement IndexedDB storage. However, many real-world scripts use
//! feature-detection to decide whether to use IndexedDB (e.g. localForage) and will fall back to
//! other storage mechanisms when `indexedDB` is missing or when `indexedDB.open(..)` fails.
//!
//! This shim installs a realistic `indexedDB` surface (and related constructor globals) while
//! ensuring **deterministic**, **asynchronous** failure for all operations. The goal is:
//! - `typeof indexedDB === "object"` and vendor aliases exist.
//! - `typeof IDBKeyRange === "function"` and other constructors exist for feature-detection.
//! - `indexedDB.open(..)` / `indexedDB.deleteDatabase(..)` return request-shaped objects
//!   immediately and then dispatch an `error` event asynchronously with
//!   `DOMException(name="NotSupportedError")`.
//!
//! Storage is intentionally not implemented in this task.

/// JS source that installs the IndexedDB presence shim in a realm.
///
/// The shim is guarded: it only installs when `globalThis.indexedDB` is missing so that a future
/// native implementation can replace it without churn.
pub(crate) const INDEXED_DB_SHIM_JS: &str = r#"
  (function () {
    var g = typeof globalThis !== "undefined" ? globalThis : this;
    if (typeof g.indexedDB !== "undefined") return;

    var NOT_SUPPORTED_MSG = "IndexedDB is not supported";

    function queueMicrotaskDeterministic(cb) {
      try {
        if (typeof g.__fastrender_queue_microtask === "function") {
          return g.__fastrender_queue_microtask(cb);
        }
      } catch (_e) {}

      try {
        if (typeof g.queueMicrotask === "function") {
          return g.queueMicrotask(cb);
        }
      } catch (_e2) {}

      Promise.resolve().then(cb);
    }

    function makeNotSupportedError(message) {
      var msg = message || NOT_SUPPORTED_MSG;
      try {
        if (typeof g.DOMException === "function") {
          return new g.DOMException(msg, "NotSupportedError");
        }
      } catch (_e) {}
      return { name: "NotSupportedError", message: msg };
    }

    function swallowCall(fn, self, arg) {
      try {
        fn.call(self, arg);
      } catch (_e) {}
    }

    function createEvent(type, target) {
      return { type: type, target: target, currentTarget: target };
    }

    function illegalConstructor(name) {
      function Ctor() {
        throw new TypeError("Illegal constructor");
      }
      return Ctor;
    }

    // ---------------------------------------------------------------------
    // Constructor globals (surface-only)
    // ---------------------------------------------------------------------
    var IDBFactory = illegalConstructor("IDBFactory");
    var IDBRequest = illegalConstructor("IDBRequest");
    var IDBOpenDBRequest = illegalConstructor("IDBOpenDBRequest");
    var IDBDatabase = illegalConstructor("IDBDatabase");
    var IDBTransaction = illegalConstructor("IDBTransaction");
    var IDBObjectStore = illegalConstructor("IDBObjectStore");
    var IDBKeyRange = illegalConstructor("IDBKeyRange");
    var IDBVersionChangeEvent = illegalConstructor("IDBVersionChangeEvent");

    // IDBOpenDBRequest extends IDBRequest (prototype chain only).
    try {
      IDBOpenDBRequest.prototype = Object.create(IDBRequest.prototype);
      IDBOpenDBRequest.prototype.constructor = IDBOpenDBRequest;
      Object.setPrototypeOf && Object.setPrototypeOf(IDBOpenDBRequest, IDBRequest);
    } catch (_e) {}

    function getListeners(obj) {
      var store = obj.__fastrender_idb_listeners;
      if (!store) {
        store = {};
        try {
          obj.__fastrender_idb_listeners = store;
        } catch (_e) {}
      }
      return store;
    }

    IDBRequest.prototype.addEventListener = function (type, cb) {
      if (typeof cb !== "function") return;
      var store = getListeners(this);
      var list = store[type];
      if (!list) list = store[type] = [];
      for (var i = 0; i < list.length; i++) {
        if (list[i] === cb) return;
      }
      list.push(cb);
    };

    IDBRequest.prototype.removeEventListener = function (type, cb) {
      var store = this.__fastrender_idb_listeners;
      if (!store) return;
      var list = store[type];
      if (!list) return;
      for (var i = 0; i < list.length; i++) {
        if (list[i] === cb) {
          list.splice(i, 1);
          return;
        }
      }
    };

    function dispatchRequestEvent(req, type) {
      var evt = createEvent(type, req);

      // Attribute handler first (deterministic ordering).
      var attr = req["on" + type];
      if (typeof attr === "function") swallowCall(attr, req, evt);

      // Then registered listeners.
      var store = req.__fastrender_idb_listeners;
      if (!store) return;
      var list = store[type];
      if (!list || list.length === 0) return;
      var snapshot = list.slice();
      for (var i = 0; i < snapshot.length; i++) {
        if (typeof snapshot[i] === "function") swallowCall(snapshot[i], req, evt);
      }
    }

    function createRequest(proto) {
      var req;
      try {
        req = Object.create(proto || null);
      } catch (_e) {
        req = {};
      }
      req.readyState = "pending";
      req.result = undefined;
      req.error = null;
      req.source = null;
      req.transaction = null;
      req.onsuccess = null;
      req.onerror = null;
      // IDBOpenDBRequest attributes.
      req.onblocked = null;
      req.onupgradeneeded = null;
      return req;
    }

    function failRequestAsync(req) {
      queueMicrotaskDeterministic(function () {
        req.readyState = "done";
        req.result = undefined;
        req.error = makeNotSupportedError(NOT_SUPPORTED_MSG);
        dispatchRequestEvent(req, "error");
      });
    }

    // ---------------------------------------------------------------------
    // indexedDB / IDBFactory surface
    // ---------------------------------------------------------------------
    var factory;
    try {
      factory = Object.create(IDBFactory.prototype);
    } catch (_e) {
      factory = {};
    }

    factory.open = function (_name, _version) {
      var req = createRequest(IDBOpenDBRequest.prototype || IDBRequest.prototype);
      failRequestAsync(req);
      return req;
    };

    factory.deleteDatabase = function (_name) {
      var req = createRequest(IDBOpenDBRequest.prototype || IDBRequest.prototype);
      failRequestAsync(req);
      return req;
    };

    // `cmp` exists on `IDBFactory` in browsers; optional for the shim.
    factory.cmp = function (_a, _b) {
      throw makeNotSupportedError(NOT_SUPPORTED_MSG);
    };

    // `IDBKeyRange` helpers exist in browsers; provide deterministic failure.
    IDBKeyRange.only = function () { throw makeNotSupportedError(NOT_SUPPORTED_MSG); };
    IDBKeyRange.lowerBound = function () { throw makeNotSupportedError(NOT_SUPPORTED_MSG); };
    IDBKeyRange.upperBound = function () { throw makeNotSupportedError(NOT_SUPPORTED_MSG); };
    IDBKeyRange.bound = function () { throw makeNotSupportedError(NOT_SUPPORTED_MSG); };

    g.IDBFactory = IDBFactory;
    g.IDBRequest = IDBRequest;
    g.IDBOpenDBRequest = IDBOpenDBRequest;
    g.IDBDatabase = IDBDatabase;
    g.IDBTransaction = IDBTransaction;
    g.IDBObjectStore = IDBObjectStore;
    g.IDBKeyRange = IDBKeyRange;
    // Vendor-prefixed constructor aliases (legacy IndexedDB shims).
    //
    // Many older libraries probe `webkitIDBKeyRange`/`mozIDBKeyRange`/`msIDBKeyRange` even when
    // `IDBKeyRange` exists.
    if (typeof g.webkitIDBKeyRange === "undefined") g.webkitIDBKeyRange = IDBKeyRange;
    if (typeof g.mozIDBKeyRange === "undefined") g.mozIDBKeyRange = IDBKeyRange;
    if (typeof g.msIDBKeyRange === "undefined") g.msIDBKeyRange = IDBKeyRange;
    g.IDBVersionChangeEvent = IDBVersionChangeEvent;

    g.indexedDB = factory;
    g.webkitIndexedDB = factory;
    g.mozIndexedDB = factory;
    g.msIndexedDB = factory;
    g.OIndexedDB = factory;
  })();
"#;

#[cfg(test)]
mod tests {
  use super::*;
  use crate::dom2;
  use crate::error::{Error, Result};
  use crate::js::window::WindowHost;
  use crate::resource::{FetchedResource, ResourceFetcher};
  use selectors::context::QuirksMode;
  use std::sync::Arc;
  use vm_js::Value;

  #[derive(Clone, Copy)]
  struct NoFetchResourceFetcher;

  impl ResourceFetcher for NoFetchResourceFetcher {
    fn fetch(&self, _url: &str) -> Result<FetchedResource> {
      Err(Error::Other("NoFetchResourceFetcher".to_string()))
    }
  }

  #[test]
  fn indexed_db_vendor_prefixed_globals_are_exposed() -> Result<()> {
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new_with_fetcher(
      dom,
      "https://example.invalid/",
      Arc::new(NoFetchResourceFetcher),
    )?;

    let ok = host.exec_script("typeof webkitIndexedDB === 'object' && webkitIndexedDB === indexedDB")?;
    assert_eq!(ok, Value::Bool(true));

    let ok = host.exec_script(
      "typeof webkitIDBKeyRange === 'function' && webkitIDBKeyRange === IDBKeyRange",
    )?;
    assert_eq!(ok, Value::Bool(true));

    Ok(())
  }

  #[test]
  fn indexed_db_shim_is_guarded() -> Result<()> {
    // Ensure running the shim twice is a no-op and does not throw.
    let dom = dom2::Document::new(QuirksMode::NoQuirks);
    let mut host = WindowHost::new_with_fetcher(
      dom,
      "https://example.invalid/",
      Arc::new(NoFetchResourceFetcher),
    )?;
    let ok = host.exec_script(INDEXED_DB_SHIM_JS)?;
    // Script returns undefined; we only care that it runs without throwing.
    assert_eq!(ok, Value::Undefined);
    Ok(())
  }
}
