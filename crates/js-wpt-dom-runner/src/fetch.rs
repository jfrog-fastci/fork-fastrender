use rquickjs::{Ctx, Function, Object, Result as JsResult};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
enum UrlResolveError {
  #[error("relative URL has no base URL")]
  RelativeUrlWithoutBase,
  #[error(transparent)]
  Url(#[from] url::ParseError),
}

fn resolve_url(input: &str, base_url: Option<&str>) -> Result<String, UrlResolveError> {
  if base_url.is_none() {
    match Url::parse(input) {
      Ok(url) => return Ok(url.to_string()),
      Err(url::ParseError::RelativeUrlWithoutBase) => {
        return Err(UrlResolveError::RelativeUrlWithoutBase)
      }
      Err(err) => return Err(UrlResolveError::Url(err)),
    }
  }

  let base = Url::parse(base_url.expect("base_url is Some"))?;
  let url = base.join(input)?;
  Ok(url.to_string())
}

pub fn install_fetch_shims<'js>(ctx: Ctx<'js>, globals: &Object<'js>) -> JsResult<()> {
  // Host hook used by the JS shims to perform WHATWG URL resolution using Rust's `url` crate.
  let resolve_inner = Function::new(
    ctx.clone(),
    |input: String, base: Option<String>| -> JsResult<String> {
      resolve_url(&input, base.as_deref()).map_err(|err| {
        // The caller wraps this in a JS shim that rethrows `TypeError`. Here we surface a plain
        // `Error` so the message is preserved.
        rquickjs::Error::new_from_js_message("Error", "Error", err.to_string())
      })
    },
  )?;

  globals.set("__fastrender_resolve_url_inner", resolve_inner)?;

  // WebIDL dictates that URL parsing failures surface as TypeError. rquickjs's error construction
  // helpers don't map 1:1 onto the browser's TypeError surface, so we install a tiny JS wrapper
  // that always throws a real `TypeError` instance.
  ctx.eval::<(), _>(RESOLVE_URL_WRAPPER_SHIM)?;

  // Install minimal `Request`/`Response`/`fetch` shims. This is intentionally tiny (enough for
  // harness-level tests) and should be replaced by real WebIDL bindings as they land.
  ctx.eval::<(), _>(FETCH_SHIM)?;
  Ok(())
}

const RESOLVE_URL_WRAPPER_SHIM: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (typeof g.__fastrender_resolve_url === "function") return;

  g.__fastrender_resolve_url = function (input, base) {
    var url_input = String(input);
    var base_input =
      base === undefined || base === null ? null : String(base);
    try {
      return g.__fastrender_resolve_url_inner(url_input, base_input);
    } catch (e) {
      var msg =
        e && e.message !== undefined && e.message !== null
          ? String(e.message)
          : String(e);
      throw new TypeError(msg);
    }
  };
})();
"#;

const FETCH_SHIM: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (typeof g.fetch === "function") return;

  function baseUrl() {
    if (g.document && typeof g.document.URL === "string") return g.document.URL;
    if (g.location && typeof g.location.href === "string") return g.location.href;
    return null;
  }

  class Request {
    constructor(input) {
      if (input instanceof Request) {
        this.url = input.url;
        return;
      }
      var s = String(input);
      this.url = g.__fastrender_resolve_url(s, baseUrl());
    }
  }

  class Response {
    constructor(url) {
      this.url = url;
    }
  }

  g.Request = Request;
  g.Response = Response;

  g.fetch = async function (input) {
    var req = input instanceof Request ? input : new Request(input);
    return new Response(req.url);
  };
})();
"#;
