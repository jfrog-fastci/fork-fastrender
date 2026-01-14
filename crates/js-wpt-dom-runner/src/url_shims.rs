use rquickjs::{Ctx, Function, Object, Result as JsResult};
use thiserror::Error;
use url::Url;

#[derive(Debug, Error)]
enum ParseUrlError {
  #[error("relative URL has no base URL")]
  RelativeUrlWithoutBase,
  #[error(transparent)]
  Url(#[from] url::ParseError),
}

fn parse_url(input: &str, base_url: Option<&str>) -> Result<Url, ParseUrlError> {
  if base_url.is_none() {
    match Url::parse(input) {
      Ok(url) => return Ok(url),
      Err(url::ParseError::RelativeUrlWithoutBase) => {
        return Err(ParseUrlError::RelativeUrlWithoutBase)
      }
      Err(err) => return Err(ParseUrlError::Url(err)),
    }
  }

  let base = Url::parse(base_url.expect("base_url is Some"))?;
  let url = base.join(input)?;
  Ok(url)
}

pub fn install_url_shims<'js>(ctx: Ctx<'js>, globals: &Object<'js>) -> JsResult<()> {
  // Host hook used by the JS shim to parse/resolve URLs using Rust's `url` crate.
  let parse_inner = Function::new(
    ctx.clone(),
    |ctx: Ctx<'js>, input: String, base: Option<String>| -> JsResult<Object<'js>> {
      let url = parse_url(&input, base.as_deref()).map_err(|err| {
        // The caller wraps this in a JS shim that rethrows `TypeError`. Here we surface a plain
        // `Error` so the message is preserved.
        rquickjs::Error::new_from_js_message("Error", "Error", err.to_string())
      })?;

      let href = url.as_str().to_string();
      let origin = url.origin().unicode_serialization();
      let pathname = url.path().to_string();
      let search = url.query().map(|q| format!("?{q}")).unwrap_or_default();

      let out = Object::new(ctx.clone())?;
      out.set("href", href)?;
      out.set("origin", origin)?;
      out.set("pathname", pathname)?;
      out.set("search", search)?;
      Ok(out)
    },
  )?;

  globals.set("__fastrender_parse_url_inner", parse_inner)?;

  // WebIDL dictates that URL parsing failures surface as TypeError. rquickjs's error construction
  // helpers don't map 1:1 onto the browser's TypeError surface, so we install a tiny JS wrapper
  // that always throws a real `TypeError` instance.
  ctx.eval::<(), _>(PARSE_URL_WRAPPER_SHIM)?;
  ctx.eval::<(), _>(URL_SHIM)?;

  Ok(())
}

const PARSE_URL_WRAPPER_SHIM: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (typeof g.__fastrender_parse_url === "function") return;

  g.__fastrender_parse_url = function (input, base) {
    var url_input = String(input);
    var base_input = base === undefined || base === null ? null : String(base);
    try {
      return g.__fastrender_parse_url_inner(url_input, base_input);
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

// Minimal WHATWG URL + URLSearchParams shims for the QuickJS backend.
//
// The vm-js backend ships real WebIDL bindings for these; QuickJS only needs enough behavior to
// exercise the curated WPT DOM `url/**` subset.
const URL_SHIM: &str = r#"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (typeof g.URL === "function" && typeof g.URLSearchParams === "function") return;

  function encodeFormComponent(str) {
    return encodeURIComponent(str)
      .replace(/[!'()~]/g, function (c) {
        return "%" + c.charCodeAt(0).toString(16).toUpperCase();
      })
      .replace(/%20/g, "+");
  }

  function decodeFormComponent(str) {
    return decodeURIComponent(str.replace(/\+/g, " "));
  }

  function parsePairs(init) {
    var out = [];
    if (init === undefined || init === null) return out;
    var s = String(init);
    if (s.startsWith("?")) s = s.substring(1);
    if (s === "") return out;
    var parts = s.split("&");
    for (var i = 0; i < parts.length; i++) {
      var part = parts[i];
      if (part === "") continue;
      var eq = part.indexOf("=");
      var name = eq === -1 ? part : part.substring(0, eq);
      var value = eq === -1 ? "" : part.substring(eq + 1);
      out.push([decodeFormComponent(name), decodeFormComponent(value)]);
    }
    return out;
  }

  function serializePairs(pairs) {
    var out = [];
    for (var i = 0; i < pairs.length; i++) {
      var pair = pairs[i];
      out.push(encodeFormComponent(pair[0]) + "=" + encodeFormComponent(pair[1]));
    }
    return out.join("&");
  }

  function URLSearchParams(init) {
    this._pairs = [];
    this._url = null;
    this._updating = false;

    if (init === undefined || init === null) return;

    // WebIDL union conversion: `init` includes a USVString branch. Boxed strings (`new String()`)
    // must be treated as strings, not as iterables.
    if (typeof init === "object" && init !== null && init instanceof String) {
      this._pairs = parsePairs(init);
      return;
    }

    // WebIDL: `init` is (sequence<sequence<USVString>> or record<USVString, USVString> or
    // USVString). Objects are treated as either a sequence (if iterable) or a record.
    if (typeof init === "object" || typeof init === "function") {
      var iteratorMethod = init[Symbol.iterator];
      if (typeof iteratorMethod === "function") {
        var iterator = iteratorMethod.call(init);
        if (!iterator || typeof iterator.next !== "function") {
          throw new TypeError("URLSearchParams init is not iterable");
        }
        while (true) {
          var step = iterator.next();
          if (step.done) break;
          var pair = step.value;
          if (!pair || typeof pair !== "object" || pair.length !== 2) {
            throw new TypeError(
              "URLSearchParams init sequence must contain [name, value] tuples"
            );
          }
          this._pairs.push([String(pair[0]), String(pair[1])]);
        }
        return;
      }

      // WebIDL record conversion: enumerable symbol keys are not allowed.
      var symbols = Object.getOwnPropertySymbols(init);
      for (var i = 0; i < symbols.length; i++) {
        var sym = symbols[i];
        if (Object.prototype.propertyIsEnumerable.call(init, sym)) {
          throw new TypeError("URLSearchParams record init must not contain enumerable Symbol keys");
        }
      }

      var keys = Object.keys(init);
      for (var i = 0; i < keys.length; i++) {
        var key = keys[i];
        this._pairs.push([String(key), String(init[key])]);
      }
      return;
    }

    this._pairs = parsePairs(init);
  }

  URLSearchParams.prototype._setUrl = function (url) {
    this._url = url;
  };

  URLSearchParams.prototype._reset = function (init) {
    this._pairs = parsePairs(init);
  };

  URLSearchParams.prototype._notify = function () {
    if (!this._url) return;
    if (this._updating) return;
    this._updating = true;
    try {
      this._url._setSearchFromParams(this.toString());
    } finally {
      this._updating = false;
    }
  };

  URLSearchParams.prototype.append = function (name, value) {
    this._pairs.push([String(name), String(value)]);
    this._notify();
  };

  URLSearchParams.prototype.get = function (name) {
    var n = String(name);
    for (var i = 0; i < this._pairs.length; i++) {
      if (this._pairs[i][0] === n) return this._pairs[i][1];
    }
    return null;
  };

  URLSearchParams.prototype.getAll = function (name) {
    var n = String(name);
    var out = [];
    for (var i = 0; i < this._pairs.length; i++) {
      if (this._pairs[i][0] === n) out.push(this._pairs[i][1]);
    }
    return out;
  };

  URLSearchParams.prototype.set = function (name, value) {
    var n = String(name);
    var v = String(value);
    var first = -1;
    for (var i = 0; i < this._pairs.length; i++) {
      if (this._pairs[i][0] !== n) continue;
      if (first === -1) {
        first = i;
        this._pairs[i][1] = v;
      } else {
        this._pairs.splice(i, 1);
        i -= 1;
      }
    }
    if (first === -1) {
      this._pairs.push([n, v]);
    }
    this._notify();
  };

  // https://url.spec.whatwg.org/#dom-urlsearchparams-delete
  URLSearchParams.prototype.delete = function (name, value) {
    var n = String(name);
    if (arguments.length < 2) {
      for (var i = 0; i < this._pairs.length; i++) {
        if (this._pairs[i][0] !== n) continue;
        this._pairs.splice(i, 1);
        i -= 1;
      }
    } else {
      var v = String(value);
      for (var i = 0; i < this._pairs.length; i++) {
        var pair = this._pairs[i];
        if (pair[0] === n && pair[1] === v) {
          this._pairs.splice(i, 1);
          i -= 1;
        }
      }
    }
    this._notify();
  };

  // https://url.spec.whatwg.org/#dom-urlsearchparams-has
  URLSearchParams.prototype.has = function (name, value) {
    var n = String(name);
    if (arguments.length < 2) {
      for (var i = 0; i < this._pairs.length; i++) {
        if (this._pairs[i][0] === n) return true;
      }
      return false;
    }
    var v = String(value);
    for (var i = 0; i < this._pairs.length; i++) {
      var pair = this._pairs[i];
      if (pair[0] === n && pair[1] === v) return true;
    }
    return false;
  };

  function createIterator(params, kind) {
    var index = 0;
    var iterator = {};
    iterator.next = function () {
      if (index >= params._pairs.length) {
        return { value: undefined, done: true };
      }
      var pair = params._pairs[index++];
      if (kind === "keys") return { value: pair[0], done: false };
      if (kind === "values") return { value: pair[1], done: false };
      return { value: [pair[0], pair[1]], done: false };
    };
    iterator[Symbol.iterator] = function () {
      return iterator;
    };
    return iterator;
  }

  URLSearchParams.prototype.entries = function () {
    return createIterator(this, "entries");
  };

  URLSearchParams.prototype.keys = function () {
    return createIterator(this, "keys");
  };

  URLSearchParams.prototype.values = function () {
    return createIterator(this, "values");
  };

  URLSearchParams.prototype.forEach = function (callback, thisArg) {
    if (typeof callback !== "function") {
      throw new TypeError("URLSearchParams.forEach callback is not a function");
    }
    for (var i = 0; i < this._pairs.length; i++) {
      var pair = this._pairs[i];
      callback.call(thisArg, pair[1], pair[0], this);
    }
  };

  Object.defineProperty(URLSearchParams.prototype, "size", {
    configurable: true,
    enumerable: true,
    get: function () {
      return this._pairs.length;
    },
  });

  // https://url.spec.whatwg.org/#dom-urlsearchparams-symbol.iterator
  URLSearchParams.prototype[Symbol.iterator] = URLSearchParams.prototype.entries;

  URLSearchParams.prototype.sort = function () {
    var decorated = [];
    for (var i = 0; i < this._pairs.length; i++) {
      decorated.push({ name: this._pairs[i][0], index: i, pair: this._pairs[i] });
    }
    decorated.sort(function (a, b) {
      if (a.name < b.name) return -1;
      if (a.name > b.name) return 1;
      return a.index - b.index;
    });
    var next = [];
    for (var i = 0; i < decorated.length; i++) {
      next.push(decorated[i].pair);
    }
    this._pairs = next;
    this._notify();
  };

  URLSearchParams.prototype.toString = function () {
    return serializePairs(this._pairs);
  };

  function URL(input, base) {
    var baseStr = null;
    if (base !== undefined && base !== null) {
      if (typeof base === "object" && base !== null && typeof base.href === "string") {
        baseStr = base.href;
      } else {
        baseStr = String(base);
      }
    }

    var parsed = g.__fastrender_parse_url(String(input), baseStr);
    this.origin = parsed.origin;
    this.pathname = parsed.pathname;
    this._search = parsed.search || "";
    this._hrefNoSearch = parsed.href;
    if (this._search) {
      this._hrefNoSearch = parsed.href.substring(0, parsed.href.length - this._search.length);
    }
    this._href = parsed.href;

    var params = new URLSearchParams(this._search);
    params._setUrl(this);
    this._searchParams = params;
  }

  URL.prototype._setSearchFromParams = function (serialized) {
    this._search = serialized && serialized.length ? "?" + serialized : "";
    this._href = this._hrefNoSearch + this._search;
  };

  Object.defineProperty(URL.prototype, "href", {
    get: function () {
      return this._href;
    },
  });

  Object.defineProperty(URL.prototype, "search", {
    get: function () {
      return this._search;
    },
    set: function (value) {
      var s = String(value);
      if (s === "" || s === "?") {
        this._search = "";
      } else if (s[0] !== "?") {
        this._search = "?" + s;
      } else {
        this._search = s;
      }
      this._href = this._hrefNoSearch + this._search;
      // Update the associated URLSearchParams view without triggering a feedback loop.
      this._searchParams._reset(this._search);
    },
  });

  Object.defineProperty(URL.prototype, "searchParams", {
    get: function () {
      return this._searchParams;
    },
  });

  // https://url.spec.whatwg.org/#dom-url-canparse
  URL.canParse = function (url, base) {
    try {
      // `new URL` throws TypeError on parse failure.
      new URL(url, base);
      return true;
    } catch (_e) {
      return false;
    }
  };

  // https://url.spec.whatwg.org/#dom-url-parse
  URL.parse = function (url, base) {
    try {
      return new URL(url, base);
    } catch (_e) {
      return null;
    }
  };

  g.URLSearchParams = URLSearchParams;
  g.URL = URL;
})();
"#;
