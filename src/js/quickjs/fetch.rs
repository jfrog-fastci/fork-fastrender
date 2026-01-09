//! QuickJS (rquickjs) bindings for a minimal Fetch API surface.
//!
//! This module is intended for **offline Rust tests** only. The production JS engine for
//! FastRender is `ecma-rs`; QuickJS is used here as a lightweight harness for deterministic
//! API-surface tests while the WebIDL-driven bindings pipeline is still under construction.
//!
//! Exposed globals:
//! - `fetch`
//! - `Headers`
//! - `Request`
//! - `Response`
//!
//! This is intentionally a small, spec-shaped subset:
//! - `fetch()` performs the Rust-side fetch synchronously (blocking) and wraps it in a resolved /
//!   rejected Promise.
//! - `Body` is in-memory only (no streaming).
//! - `Headers` iteration is deterministic and uses `Headers::sort_and_combine()`.

use std::sync::Arc;

use rquickjs::class::{Trace, Tracer};
use rquickjs::function::{Opt, This};
use rquickjs::{Class, Ctx, FromJs, Function, IntoJs, JsLifetime, Object, Result as JsResult, Value};

use fastrender::resource::{
  DocumentOrigin, FetchDestination, PolicyError, ReferrerPolicy, ResourceAccessPolicy, ResourceFetcher,
};
use fastrender::resource::web_fetch::{
  execute_web_fetch, Body, Headers, HeadersGuard, RequestCredentials, RequestMode, RequestRedirect,
  Response as CoreResponse, ResponseType, WebFetchError, WebFetchExecutionContext,
};

#[derive(Clone)]
pub struct QuickjsFetchEnv {
  pub fetcher: Arc<dyn ResourceFetcher>,
  pub document_url: Option<String>,
  pub document_origin: Option<DocumentOrigin>,
  pub referrer_policy: ReferrerPolicy,
  pub access_policy: Option<ResourceAccessPolicy>,
}

fn resolve_promise<'js, V>(ctx: Ctx<'js>, value: V) -> JsResult<Value<'js>>
where
  V: IntoJs<'js>,
{
  let globals = ctx.globals();
  let promise: Object<'js> = globals.get("Promise")?;
  let resolve: Function<'js> = promise.get("resolve")?;
  let value = value.into_js(&ctx)?;
  resolve.call((This(promise), value))
}

fn reject_promise_type_error<'js>(ctx: Ctx<'js>, message: &str) -> JsResult<Value<'js>> {
  let globals = ctx.globals();
  let promise: Object<'js> = globals.get("Promise")?;
  let reject: Function<'js> = promise.get("reject")?;
  let type_error: Function<'js> = globals.get("TypeError")?;
  let err: Value<'js> = type_error.call((message,))?;
  reject.call((This(promise), err))
}

fn throw_type_error<'js, T>(ctx: Ctx<'js>, message: &str) -> JsResult<T> {
  // rquickjs surfaces thrown values via `Error::Exception`. The easiest way to throw a TypeError
  // without relying on internal error constructors is to `eval` a `throw`.
  //
  // This is only used in test-harness bindings, so the overhead is acceptable.
  let msg = serde_json::to_string(message).unwrap_or_else(|_| "\"TypeError\"".to_string());
  match ctx.eval::<(), _>(format!("throw new TypeError({msg});")) {
    Ok(()) => Err(rquickjs::Error::Exception),
    Err(err) => Err(err),
  }
}

fn throw_web_fetch_type_error<'js, T>(ctx: Ctx<'js>, err: WebFetchError) -> JsResult<T> {
  throw_type_error(ctx, &err.to_string())
}

fn map_web_fetch_result<'js, T>(
  ctx: Ctx<'js>,
  result: std::result::Result<T, WebFetchError>,
) -> JsResult<T> {
  match result {
    Ok(value) => Ok(value),
    Err(err) => throw_web_fetch_type_error(ctx, err),
  }
}

fn object_entries<'js>(ctx: &Ctx<'js>, obj: Object<'js>) -> JsResult<Vec<Vec<String>>> {
  let object_ctor: Object<'js> = ctx.globals().get("Object")?;
  let entries_fn: Function<'js> = object_ctor.get("entries")?;
  entries_fn.call((obj,))
}

/// Parse a `HeadersInit` value and append its entries into `headers`.
fn fill_headers_from_init<'js>(ctx: Ctx<'js>, headers: &mut Headers, init: Value<'js>) -> JsResult<()> {
  if init.is_undefined() {
    return Ok(());
  }

  // `HeadersInit` includes `Headers`, sequence-of-pairs, and record.
  if let Ok(existing) = Class::<JsHeaders>::from_value(&init) {
    let existing = existing.borrow();
    map_web_fetch_result(ctx.clone(), headers.fill_from_pairs(existing.inner.raw_pairs()))?;
    return Ok(());
  }

  // Prefer sequence-of-pairs if it converts cleanly.
  if let Ok(sequence) = <Vec<Vec<String>> as FromJs>::from_js(&ctx, init.clone()) {
    map_web_fetch_result(ctx.clone(), headers.fill_from_sequence(sequence))?;
    return Ok(());
  }

  // Treat remaining objects as records via `Object.entries`.
  let obj: Object<'js> = match Object::from_js(&ctx, init) {
    Ok(obj) => obj,
    Err(_) => return throw_type_error(ctx, "HeadersInit must be an object or a sequence"),
  };
  let entries = object_entries(&ctx, obj)?;
  map_web_fetch_result(ctx, headers.fill_from_sequence(entries))?;
  Ok(())
}

fn headers_guard_for_mode(mode: RequestMode) -> HeadersGuard {
  match mode {
    RequestMode::NoCors => HeadersGuard::RequestNoCors,
    _ => HeadersGuard::Request,
  }
}

fn response_type_string(r#type: ResponseType) -> &'static str {
  match r#type {
    ResponseType::Basic => "basic",
    ResponseType::Cors => "cors",
    ResponseType::Default => "default",
    ResponseType::Error => "error",
    ResponseType::Opaque => "opaque",
    ResponseType::OpaqueRedirect => "opaqueredirect",
  }
}

#[rquickjs::class(rename = "Headers")]
pub struct JsHeaders {
  inner: Headers,
}

impl JsHeaders {
  fn from_core(headers: Headers) -> Self {
    Self { inner: headers }
  }
}

unsafe impl<'js> JsLifetime<'js> for JsHeaders {
  type Changed<'to> = JsHeaders;
}

impl<'js> Trace<'js> for JsHeaders {
  fn trace<'a>(&self, _tracer: Tracer<'a, 'js>) {}
}

#[rquickjs::methods]
impl<'js> JsHeaders {
  #[qjs(constructor)]
  pub fn constructor(ctx: Ctx<'js>, init: Opt<Value<'js>>) -> JsResult<Self> {
    let mut headers = Headers::new();
    if let Some(init) = init.0.filter(|v| !v.is_undefined()) {
      fill_headers_from_init(ctx.clone(), &mut headers, init)?;
    }
    Ok(Self { inner: headers })
  }

  pub fn append(&mut self, ctx: Ctx<'js>, name: String, value: String) -> JsResult<()> {
    map_web_fetch_result(ctx, self.inner.append(&name, &value))
  }

  pub fn delete(&mut self, ctx: Ctx<'js>, name: String) -> JsResult<()> {
    map_web_fetch_result(ctx, self.inner.delete(&name))
  }

  pub fn get(&self, ctx: Ctx<'js>, name: String) -> JsResult<Value<'js>> {
    match self.inner.get(&name) {
      Ok(Some(value)) => value.into_js(&ctx),
      Ok(None) => Ok(Value::new_null(ctx)),
      Err(err) => throw_web_fetch_type_error(ctx, err),
    }
  }

  #[qjs(rename = "getSetCookie")]
  pub fn get_set_cookie(&self) -> Vec<String> {
    self.inner.get_set_cookie()
  }

  pub fn has(&self, ctx: Ctx<'js>, name: String) -> JsResult<bool> {
    map_web_fetch_result(ctx, self.inner.has(&name))
  }

  pub fn set(&mut self, ctx: Ctx<'js>, name: String, value: String) -> JsResult<()> {
    map_web_fetch_result(ctx, self.inner.set(&name, &value))
  }

  #[qjs(rename = "__fastrenderPairs")]
  pub fn fastrender_pairs(&self) -> Vec<Vec<String>> {
    self
      .inner
      .sort_and_combine()
      .into_iter()
      .map(|(k, v)| vec![k, v])
      .collect()
  }
}

#[rquickjs::class(rename = "Request")]
pub struct JsRequest<'js> {
  method: String,
  url: String,
  headers: Class<'js, JsHeaders>,
  mode: RequestMode,
  credentials: RequestCredentials,
  redirect: RequestRedirect,
  referrer: String,
  referrer_policy: ReferrerPolicy,
  body: Option<Vec<u8>>,
}

unsafe impl<'js> JsLifetime<'js> for JsRequest<'js> {
  type Changed<'to> = JsRequest<'to>;
}

impl<'js> Trace<'js> for JsRequest<'js> {
  fn trace<'a>(&self, tracer: Tracer<'a, 'js>) {
    self.headers.trace(tracer);
  }
}

impl<'js> JsRequest<'js> {
  fn new_empty(ctx: Ctx<'js>, url: String) -> JsResult<Self> {
    let mode = RequestMode::Cors;
    let guard = headers_guard_for_mode(mode);
    let headers = Class::instance(ctx.clone(), JsHeaders::from_core(Headers::new_with_guard(guard)))?;
    Ok(Self {
      method: "GET".to_string(),
      url,
      headers,
      mode,
      credentials: RequestCredentials::SameOrigin,
      redirect: RequestRedirect::Follow,
      referrer: String::new(),
      referrer_policy: ReferrerPolicy::EmptyString,
      body: None,
    })
  }

  fn clone_with_new_headers(&self, ctx: Ctx<'js>) -> JsResult<Self> {
    let headers = self.headers.borrow();
    let headers = Class::instance(ctx.clone(), JsHeaders::from_core(headers.inner.clone()))?;
    Ok(Self {
      method: self.method.clone(),
      url: self.url.clone(),
      headers,
      mode: self.mode,
      credentials: self.credentials,
      redirect: self.redirect,
      referrer: self.referrer.clone(),
      referrer_policy: self.referrer_policy,
      body: self.body.clone(),
    })
  }

  fn set_mode_and_guard(&mut self, mode: RequestMode) {
    self.mode = mode;
    let guard = headers_guard_for_mode(mode);
    self.headers.borrow_mut().inner.set_guard(guard);
  }

  fn to_core_request(&self, ctx: Ctx<'js>) -> JsResult<fastrender::resource::web_fetch::Request> {
    let mut req = fastrender::resource::web_fetch::Request::new(self.method.clone(), self.url.clone());
    req.mode = self.mode;
    req.credentials = self.credentials;
    req.redirect = self.redirect;
    req.referrer = self.referrer.clone();
    req.referrer_policy = self.referrer_policy;
    req.headers = self.headers.borrow().inner.clone();
    if let Some(body) = self.body.as_ref() {
      req.body = Some(map_web_fetch_result(ctx, Body::new(body.clone()))?);
    }
    Ok(req)
  }
}

#[rquickjs::methods]
impl<'js> JsRequest<'js> {
  #[qjs(constructor)]
  pub fn constructor(ctx: Ctx<'js>, input: Value<'js>, init: Opt<Value<'js>>) -> JsResult<Self> {
    let mut out = if let Ok(existing) = Class::<JsRequest<'js>>::from_value(&input) {
      let existing = existing.borrow();
      existing.clone_with_new_headers(ctx.clone())?
    } else {
      let url = match <String as FromJs>::from_js(&ctx, input) {
        Ok(url) => url,
        Err(_) => return throw_type_error(ctx, "Request input must be a string or a Request"),
      };
      Self::new_empty(ctx.clone(), url)?
    };

    let Some(init) = init.0.filter(|v| !v.is_undefined()) else {
      return Ok(out);
    };
    let init_obj: Object<'js> = match Object::from_js(&ctx, init) {
      Ok(obj) => obj,
      Err(_) => return throw_type_error(ctx, "Request init must be an object"),
    };

    if let Ok(Some(method)) = init_obj.get::<_, Option<String>>("method") {
      out.method = method;
    }

    if let Ok(Some(mode)) = init_obj.get::<_, Option<String>>("mode") {
      let mode = match mode.as_str() {
        "navigate" => RequestMode::Navigate,
        "same-origin" => RequestMode::SameOrigin,
        "no-cors" => RequestMode::NoCors,
        "cors" => RequestMode::Cors,
        _ => return throw_type_error(ctx, "Invalid request mode"),
      };
      out.set_mode_and_guard(mode);
    }

    if let Ok(Some(credentials)) = init_obj.get::<_, Option<String>>("credentials") {
      out.credentials = match credentials.as_str() {
        "omit" => RequestCredentials::Omit,
        "same-origin" => RequestCredentials::SameOrigin,
        "include" => RequestCredentials::Include,
        _ => return throw_type_error(ctx, "Invalid request credentials"),
      };
    }

    if let Ok(Some(redirect)) = init_obj.get::<_, Option<String>>("redirect") {
      out.redirect = match redirect.as_str() {
        "follow" => RequestRedirect::Follow,
        "error" => RequestRedirect::Error,
        "manual" => RequestRedirect::Manual,
        _ => return throw_type_error(ctx, "Invalid request redirect"),
      };
    }

    if let Ok(Some(referrer)) = init_obj.get::<_, Option<String>>("referrer") {
      out.referrer = referrer;
    }

    if let Ok(Some(policy)) = init_obj.get::<_, Option<String>>("referrerPolicy") {
      out.referrer_policy = ReferrerPolicy::parse(&policy).unwrap_or(ReferrerPolicy::EmptyString);
    }

    if let Ok(Some(body)) = init_obj.get::<_, Option<String>>("body") {
      out.body = Some(body.into_bytes());
    }

    if let Ok(Some(headers_init)) = init_obj.get::<_, Option<Value<'js>>>("headers") {
      let guard = headers_guard_for_mode(out.mode);
      let mut headers = Headers::new_with_guard(guard);
      fill_headers_from_init(ctx.clone(), &mut headers, headers_init)?;
      out.headers = Class::instance(ctx.clone(), JsHeaders::from_core(headers))?;
    }

    Ok(out)
  }

  #[qjs(get)]
  pub fn method(&self) -> String {
    self.method.clone()
  }

  #[qjs(get)]
  pub fn url(&self) -> String {
    self.url.clone()
  }

  #[qjs(get)]
  pub fn mode(&self) -> String {
    match self.mode {
      RequestMode::Navigate => "navigate",
      RequestMode::SameOrigin => "same-origin",
      RequestMode::NoCors => "no-cors",
      RequestMode::Cors => "cors",
    }
    .to_string()
  }

  #[qjs(get)]
  pub fn credentials(&self) -> String {
    match self.credentials {
      RequestCredentials::Omit => "omit",
      RequestCredentials::SameOrigin => "same-origin",
      RequestCredentials::Include => "include",
    }
    .to_string()
  }

  #[qjs(get)]
  pub fn redirect(&self) -> String {
    match self.redirect {
      RequestRedirect::Follow => "follow",
      RequestRedirect::Error => "error",
      RequestRedirect::Manual => "manual",
    }
    .to_string()
  }

  #[qjs(get)]
  pub fn referrer(&self) -> String {
    self.referrer.clone()
  }

  #[qjs(get, rename = "referrerPolicy")]
  pub fn referrer_policy(&self) -> String {
    self.referrer_policy.as_str().to_string()
  }

  #[qjs(get)]
  pub fn headers(&self) -> Object<'js> {
    self.headers.clone().into_inner()
  }

  pub fn clone(&self, ctx: Ctx<'js>) -> JsResult<Object<'js>> {
    let cloned = self.clone_with_new_headers(ctx.clone())?;
    Ok(Class::instance(ctx, cloned)?.into_inner())
  }
}

#[rquickjs::class(rename = "Response")]
pub struct JsResponse<'js> {
  r#type: ResponseType,
  url: String,
  redirected: bool,
  status: u16,
  status_text: String,
  headers: Class<'js, JsHeaders>,
  body: Option<Body>,
}

unsafe impl<'js> JsLifetime<'js> for JsResponse<'js> {
  type Changed<'to> = JsResponse<'to>;
}

impl<'js> Trace<'js> for JsResponse<'js> {
  fn trace<'a>(&self, tracer: Tracer<'a, 'js>) {
    self.headers.trace(tracer);
  }
}

impl<'js> JsResponse<'js> {
  fn from_core(ctx: Ctx<'js>, mut res: CoreResponse) -> JsResult<Self> {
    // JS `Response.headers` is immutable in browsers for fetch() responses. Encode that by using
    // the `immutable` guard so mutation methods throw TypeError.
    res.headers.set_guard(HeadersGuard::Immutable);
    let headers = Class::instance(ctx.clone(), JsHeaders::from_core(res.headers))?;
    Ok(Self {
      r#type: res.r#type,
      url: res.url,
      redirected: res.redirected,
      status: res.status,
      status_text: res.status_text,
      headers,
      body: res.body,
    })
  }

  fn body_used(&self) -> bool {
    self.body.as_ref().map_or(false, Body::body_used)
  }
}

#[rquickjs::methods]
impl<'js> JsResponse<'js> {
  #[qjs(constructor)]
  pub fn constructor(ctx: Ctx<'js>, body: Opt<String>, init: Opt<Value<'js>>) -> JsResult<Self> {
    let mut status: u16 = 200;
    let mut status_text = String::new();
    let mut headers = Headers::new_with_guard(HeadersGuard::Response);

    if let Some(init) = init.0.filter(|v| !v.is_undefined()) {
      let init_obj: Object<'js> = match Object::from_js(&ctx, init) {
        Ok(obj) => obj,
        Err(_) => return throw_type_error(ctx, "Response init must be an object"),
      };
      if let Ok(Some(s)) = init_obj.get::<_, Option<u16>>("status") {
        status = s;
      }
      if let Ok(Some(st)) = init_obj.get::<_, Option<String>>("statusText") {
        status_text = st;
      }
      if let Ok(Some(headers_init)) = init_obj.get::<_, Option<Value<'js>>>("headers") {
        fill_headers_from_init(ctx.clone(), &mut headers, headers_init)?;
      }
    }

    let headers = Class::instance(ctx.clone(), JsHeaders::from_core(headers))?;
    let body = match body.0 {
      Some(body) => Some(map_web_fetch_result(ctx.clone(), Body::new(body.into_bytes()))?),
      None => None,
    };
    Ok(Self {
      r#type: ResponseType::Default,
      url: String::new(),
      redirected: false,
      status,
      status_text,
      headers,
      body,
    })
  }

  #[qjs(get)]
  pub fn r#type(&self) -> String {
    response_type_string(self.r#type).to_string()
  }

  #[qjs(get)]
  pub fn url(&self) -> String {
    self.url.clone()
  }

  #[qjs(get)]
  pub fn redirected(&self) -> bool {
    self.redirected
  }

  #[qjs(get)]
  pub fn status(&self) -> u16 {
    self.status
  }

  #[qjs(get)]
  pub fn ok(&self) -> bool {
    (200..300).contains(&self.status)
  }

  #[qjs(get, rename = "statusText")]
  pub fn status_text(&self) -> String {
    self.status_text.clone()
  }

  #[qjs(get)]
  pub fn headers(&self) -> Object<'js> {
    self.headers.clone().into_inner()
  }

  #[qjs(get, rename = "bodyUsed")]
  pub fn body_used_getter(&self) -> bool {
    self.body_used()
  }

  pub fn text(&mut self, ctx: Ctx<'js>) -> JsResult<Value<'js>> {
    let Some(body) = self.body.as_mut() else {
      return resolve_promise(ctx, "");
    };

    match body.text_utf8() {
      Ok(text) => resolve_promise(ctx, text),
      Err(WebFetchError::BodyUsed) => reject_promise_type_error(ctx, "Body is already used"),
      Err(err) => reject_promise_type_error(ctx, &err.to_string()),
    }
  }

  pub fn json(&mut self, ctx: Ctx<'js>) -> JsResult<Value<'js>> {
    let Some(body) = self.body.as_mut() else {
      return reject_promise_type_error(ctx, "Response has no body");
    };

    match body.json() {
      Ok(value) => {
        // `serde_json::Value` doesn't implement `IntoJs` by default; serialize and parse in JS.
        let json_str = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string());
        let json: Object<'js> = ctx.globals().get("JSON")?;
        let parse: Function<'js> = json.get("parse")?;
        let js_value: Value<'js> = parse.call((json_str,))?;
        resolve_promise(ctx, js_value)
      }
      Err(WebFetchError::BodyUsed) => reject_promise_type_error(ctx, "Body is already used"),
      Err(err) => reject_promise_type_error(ctx, &err.to_string()),
    }
  }
}

pub fn install_fetch_bindings<'js>(
  ctx: Ctx<'js>,
  globals: &Object<'js>,
  env: QuickjsFetchEnv,
) -> JsResult<()> {
  Class::<JsHeaders>::define(globals)?;
  Class::<JsRequest<'js>>::define(globals)?;
  Class::<JsResponse<'js>>::define(globals)?;

  // Install `fetch` as a host function that performs the Rust-side fetch synchronously and returns
  // a resolved/rejected Promise.
  let fetch = Function::new(
    ctx.clone(),
    move |ctx: Ctx<'js>, input: Value<'js>, init: Opt<Value<'js>>| -> JsResult<Value<'js>> {
      let request = JsRequest::constructor(ctx.clone(), input, init)?;
      let core = request.to_core_request(ctx.clone())?;

      if let Some(policy) = env.access_policy.as_ref() {
        if let Err(PolicyError { reason }) = policy.allows(&core.url) {
          return reject_promise_type_error(ctx, &reason);
        }
      }

      let referrer_policy = if core.referrer_policy == ReferrerPolicy::EmptyString {
        env.referrer_policy
      } else {
        core.referrer_policy
      };

      let exec = WebFetchExecutionContext {
        destination: FetchDestination::Fetch,
        referrer_url: env.document_url.as_deref(),
        client_origin: env.document_origin.as_ref(),
        referrer_policy,
        csp: None,
      };

      match execute_web_fetch(env.fetcher.as_ref(), &core, exec) {
        Ok(res) => {
          let js_res = JsResponse::from_core(ctx.clone(), res)?;
          let obj = Class::instance(ctx.clone(), js_res)?.into_inner();
          resolve_promise(ctx, obj)
        }
        Err(err) => reject_promise_type_error(ctx, &err.to_string()),
      }
    },
  )?;
  globals.set("fetch", fetch)?;

  // Deterministic iteration for `Headers`.
  //
  // We expose a non-standard `__fastrenderPairs()` method implemented in Rust that returns the
  // `sort_and_combine()` output, and build the JS iterator in a small shim so we don't have to
  // manually implement iterator objects in Rust.
  ctx.eval::<(), _>(
    r#"
    (function () {
      if (typeof Headers !== "function") return;
      if (Headers.prototype[Symbol.iterator]) return;
      Headers.prototype.entries = function () {
        return this.__fastrenderPairs()[Symbol.iterator]();
      };
      Headers.prototype[Symbol.iterator] = Headers.prototype.entries;
    })();
    "#,
  )?;

  Ok(())
}
