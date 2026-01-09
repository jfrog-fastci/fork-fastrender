//! QuickJS harness bindings for WHATWG `URL` and `URLSearchParams`.
//!
//! This module is **test-only scaffolding** used to validate the Rust URL primitives in
//! `src/js/url.rs` against JavaScript-facing expectations.
//!
//! It intentionally keeps the JS binding surface small and self-contained so it can be replaced by
//! IDL-generated bindings later.
#![cfg(test)]

use fastrender::js::{Url, UrlLimits, UrlSearchParams};
use rquickjs::class::{Trace, Tracer};
// rquickjs uses `function::Opt<T>` for optional JS parameters (missing argument => `None`).
use rquickjs::function::Opt;
use rquickjs::prelude::Func;
use rquickjs::{Array, Class, Ctx, Error, Exception, Object, Value};

#[derive(Clone)]
#[rquickjs::class(rename = "URL")]
struct JsUrl {
  inner: Url,
}

impl<'js> Trace<'js> for JsUrl {
  fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

// These class wrappers don't hold any JS values, so it's always sound to treat them as `'js`
// lifetime carriers.
unsafe impl<'js> rquickjs::JsLifetime<'js> for JsUrl {
  type Changed<'to> = JsUrl;
}

#[rquickjs::methods]
impl JsUrl {
  #[qjs(constructor)]
  pub fn new<'js>(ctx: Ctx<'js>, url: String, base: Opt<Value<'js>>) -> Result<Self, Error> {
    let base = match base.0 {
      None => None,
      Some(v) if v.is_undefined() => None,
      Some(v) => Some(v.get::<String>()?),
    };
    let limits = UrlLimits::default();
    let inner = Url::parse(&url, base.as_deref(), &limits)
      .map_err(|_| Exception::throw_type(&ctx, "Invalid URL"))?;
    Ok(Self { inner })
  }

  #[qjs(get, enumerable, configurable)]
  pub fn href(&self) -> String {
    self.inner.href().expect("href")
  }

  #[qjs(get)]
  pub fn origin(&self) -> String {
    self.inner.origin()
  }

  #[qjs(get)]
  pub fn protocol(&self) -> String {
    self.inner.protocol().expect("protocol")
  }

  #[qjs(set, rename = "protocol")]
  pub fn set_protocol(&self, value: String) {
    let _ = self.inner.set_protocol(&value);
  }

  #[qjs(get)]
  pub fn host(&self) -> String {
    self.inner.host().expect("host")
  }

  #[qjs(set, rename = "host")]
  pub fn set_host(&self, value: String) {
    let _ = self.inner.set_host(&value);
  }

  #[qjs(get)]
  pub fn hostname(&self) -> String {
    self.inner.hostname().expect("hostname")
  }

  #[qjs(set, rename = "hostname")]
  pub fn set_hostname(&self, value: String) {
    let _ = self.inner.set_hostname(&value);
  }

  #[qjs(get)]
  pub fn port(&self) -> String {
    self.inner.port().expect("port")
  }

  #[qjs(set, rename = "port")]
  pub fn set_port(&self, value: String) {
    let _ = self.inner.set_port(&value);
  }

  #[qjs(get)]
  pub fn pathname(&self) -> String {
    self.inner.pathname().expect("pathname")
  }

  #[qjs(set, rename = "pathname")]
  pub fn set_pathname(&self, value: String) {
    let _ = self.inner.set_pathname(&value);
  }

  #[qjs(get)]
  pub fn search(&self) -> String {
    self.inner.search().expect("search")
  }

  #[qjs(set, rename = "search")]
  pub fn set_search(&self, value: String) {
    let _ = self.inner.set_search(&value);
  }

  #[qjs(get)]
  pub fn hash(&self) -> String {
    self.inner.hash().expect("hash")
  }

  #[qjs(set, rename = "hash")]
  pub fn set_hash(&self, value: String) {
    let _ = self.inner.set_hash(&value);
  }

  #[qjs(rename = "toJSON")]
  pub fn to_json(&self) -> String {
    self.inner.to_json().expect("to_json")
  }

  #[qjs(rename = "toString")]
  pub fn to_string(&self) -> String {
    self.inner.href().expect("href")
  }
}

#[derive(Clone)]
#[rquickjs::class(rename = "URLSearchParams")]
struct JsUrlSearchParams {
  inner: UrlSearchParams,
}

impl<'js> Trace<'js> for JsUrlSearchParams {
  fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

unsafe impl<'js> rquickjs::JsLifetime<'js> for JsUrlSearchParams {
  type Changed<'to> = JsUrlSearchParams;
}

#[rquickjs::methods]
impl JsUrlSearchParams {
  #[qjs(constructor)]
  pub fn new<'js>(ctx: Ctx<'js>, init: Opt<Value<'js>>) -> Result<Self, Error> {
    let limits = UrlLimits::default();
    let params = UrlSearchParams::new(&limits);

    let Some(init) = init.0 else {
      return Ok(Self { inner: params });
    };
    if init.is_undefined() {
      return Ok(Self { inner: params });
    }

    // sequence-of-pairs (array)
    if init.is_array() {
      let array = Array::from_value(init)?;
      let len = array.len();
      for idx in 0..len {
        let pair_val: Value = array.get(idx)?;
        let pair = Array::from_value(pair_val)
          .map_err(|_| Exception::throw_type(&ctx, "Invalid URLSearchParams init"))?;
        let name: String = pair.get(0)?;
        let value: String = pair.get(1)?;
        params
          .append(&name, &value)
          .map_err(|_| Exception::throw_type(&ctx, "Invalid URLSearchParams init"))?;
      }
      return Ok(Self { inner: params });
    }

    // record/object
    if init.is_object() {
      let obj = Object::from_value(init)?;
      for key in obj.keys::<String>() {
        let key = key?;
        let value: String = obj.get(key.as_str())?;
        params
          .append(&key, &value)
          .map_err(|_| Exception::throw_type(&ctx, "Invalid URLSearchParams init"))?;
      }
      return Ok(Self { inner: params });
    }

    // string (and any non-object)
    let s: String = init.get()?;
    Ok(Self {
      inner: UrlSearchParams::parse(&s, &limits)
        .map_err(|_| Exception::throw_type(&ctx, "Invalid URLSearchParams init"))?,
    })
  }

  #[qjs(get)]
  pub fn size(&self) -> usize {
    self.inner.size().expect("size")
  }

  pub fn append(&self, name: String, value: String) {
    self.inner.append(&name, &value).expect("append");
  }

  pub fn delete(&self, name: String, value: Opt<String>) {
    self
      .inner
      .delete(&name, value.0.as_deref())
      .expect("delete");
  }

  pub fn get(&self, name: String) -> Option<String> {
    self.inner.get(&name).expect("get")
  }

  #[qjs(rename = "getAll")]
  pub fn get_all(&self, name: String) -> Vec<String> {
    self.inner.get_all(&name).expect("get_all")
  }

  pub fn has(&self, name: String, value: Opt<String>) -> bool {
    self.inner.has(&name, value.0.as_deref()).expect("has")
  }

  pub fn set(&self, name: String, value: String) {
    self.inner.set(&name, &value).expect("set");
  }

  pub fn sort(&self) {
    self.inner.sort().expect("sort");
  }

  #[qjs(rename = "toString")]
  pub fn to_string(&self) -> String {
    self.inner.serialize().expect("serialize")
  }

  /// Internal helper used by the JS-level iterator shim.
  pub fn __pairs(&self) -> Vec<Vec<String>> {
    self
      .inner
      .pairs()
      .expect("pairs")
      .into_iter()
      .map(|(name, value)| vec![name, value])
      .collect()
  }
}

fn url_create_search_params<'js>(
  ctx: Ctx<'js>,
  url: Class<'js, JsUrl>,
) -> Result<Class<'js, JsUrlSearchParams>, Error> {
  let inner = { url.borrow().inner.search_params() };
  Class::instance(ctx, JsUrlSearchParams { inner })
}

fn url_set_href<'js>(
  ctx: Ctx<'js>,
  url: Class<'js, JsUrl>,
  value: String,
) -> Result<(), Error> {
  url
    .borrow()
    .inner
    .set_href(&value)
    .map_err(|_| Exception::throw_type(&ctx, "Invalid URL"))?;
  Ok(())
}

pub fn install_url_bindings<'js>(
  ctx: Ctx<'js>,
  globals: &Object<'js>,
) -> Result<(), Error> {
  Class::<JsUrl>::define(globals)?;
  Class::<JsUrlSearchParams>::define(globals)?;
  globals.set("__url_create_search_params", Func::from(url_create_search_params))?;
  globals.set("__url_set_href", Func::from(url_set_href))?;

  // Small JS shim layer to model WebIDL behaviors that aren't ergonomic to express in the Rust
  // class macro (e.g. `[SameObject]` caching and `Symbol.iterator`).
  ctx.eval::<(), _>(
    r#"
    (() => {
      const kSearchParams = Symbol("URL.searchParams");
      const hrefDesc = Object.getOwnPropertyDescriptor(URL.prototype, "href");
      const hrefGetter = hrefDesc && hrefDesc.get;
      if (typeof hrefGetter === "function") {
        Object.defineProperty(URL.prototype, "href", {
          get() {
            return hrefGetter.call(this);
          },
          set(v) {
            globalThis.__url_set_href(this, v);
          },
          enumerable: true,
          configurable: true,
        });
      }

      Object.defineProperty(URL.prototype, "searchParams", {
        get() {
          if (!this[kSearchParams]) {
            this[kSearchParams] = globalThis.__url_create_search_params(this);
          }
          return this[kSearchParams];
        },
        enumerable: true,
        configurable: true,
      });

      // Spec default iterator for URLSearchParams is `entries()`.
      URLSearchParams.prototype.entries = function () {
        return this.__pairs()[Symbol.iterator]();
      };
      URLSearchParams.prototype[Symbol.iterator] = URLSearchParams.prototype.entries;

      URLSearchParams.prototype.keys = function* () {
        for (const [k] of this.__pairs()) yield k;
      };

      URLSearchParams.prototype.values = function* () {
        for (const [, v] of this.__pairs()) yield v;
      };

      URLSearchParams.prototype.forEach = function (callback, thisArg) {
        if (typeof callback !== "function") {
          throw new TypeError("URLSearchParams.forEach callback is not a function");
        }
        for (const [k, v] of this.__pairs()) {
          callback.call(thisArg, v, k, this);
        }
      };

      // WHATWG `URLSearchParams.get()` returns `null` when the entry is missing. The Rust binding
      // uses `Option<String>` which maps to `undefined` in rquickjs, so wrap it here.
      const getImpl = URLSearchParams.prototype.get;
      if (typeof getImpl === "function") {
        URLSearchParams.prototype.get = function (name) {
          const v = getImpl.call(this, name);
          return v === undefined ? null : v;
        };
      }

      // WHATWG URL defines `URL.parse(url, base?)` which returns null on failure, and
      // `URL.canParse(url, base?)` which returns a boolean.
      URL.parse = function (url, base) {
        try {
          return new URL(url, base);
        } catch (_e) {
          return null;
        }
      };
      URL.canParse = function (url, base) {
        return URL.parse(url, base) !== null;
      };
    })();
    "#,
  )?;

  Ok(())
}
