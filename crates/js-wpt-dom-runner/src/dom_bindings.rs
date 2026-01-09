use rquickjs::{Ctx, Object};

/// Install a minimal DOM + exception surface onto the provided `document` object.
///
/// This is intentionally tiny and only covers the APIs needed by the curated DOM WPT subset:
/// - `document.createElement`, `document.createTextNode`
/// - `node.appendChild`, `node.removeChild`
/// - `document.querySelector`
/// - `DOMException` + deterministic exception mapping
pub fn install_dom_bindings<'js>(
  ctx: Ctx<'js>,
  _globals: &Object<'js>,
) -> rquickjs::Result<()> {
  ctx.eval::<(), _>(DOM_BINDINGS_SHIM)?;
  Ok(())
}

// Note: use `r##"..."##` (double-hash) so the shim can contain `"#` sequences (e.g. CSS selectors
// like `"#id"`), which would otherwise terminate a `r#"... "#` raw string literal.
const DOM_BINDINGS_SHIM: &str = r##"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;

  // --- DOMException (minimal but spec-shaped enough for WPT + real scripts) ---
  if (typeof g.DOMException !== "function") {
    g.DOMException = class DOMException extends Error {
      constructor(message, name) {
        super(message === undefined ? "" : String(message));
        this.name = name === undefined ? "Error" : String(name);
      }
    };
  }

  // Helpers used by Rust host functions to throw the desired error type.
  g.__fastrender_throw_dom_exception = function (name, message) {
    throw new g.DOMException(message, name);
  };
  g.__fastrender_throw_syntax_error = function (message) {
    throw new SyntaxError(message === undefined ? "" : String(message));
  };

  var doc = g.document;
  if (!doc) return;

  function define(obj, key, value) {
    try {
      Object.defineProperty(obj, key, { value: value, writable: true, configurable: true });
    } catch (_e) {
      obj[key] = value;
    }
  }

  function domException(name, message) {
    return new g.DOMException(message === undefined ? "" : String(message), name);
  }

  function ensureNodeKind(obj, kind) {
    if (!obj) return obj;
    if (obj.__node_kind == null) {
      define(obj, "__node_kind", kind);
    }
    if (!("parentNode" in obj)) {
      define(obj, "parentNode", null);
    }
    if (!("childNodes" in obj)) {
      define(obj, "childNodes", []);
    }
    return obj;
  }

  function ensureRemoveChild(obj) {
    if (!obj) return;
    obj.removeChild = function (child) {
      if (!child || (typeof child !== "object" && typeof child !== "function")) {
        throw domException("InvalidNodeType", "InvalidNodeType");
      }
      if (child.parentNode !== this) {
        throw domException("NotFoundError", "NotFoundError");
      }
      if (Array.isArray(this.childNodes)) {
        var idx = this.childNodes.indexOf(child);
        if (idx >= 0) this.childNodes.splice(idx, 1);
      }
      child.parentNode = null;
      return child;
    };
  }

  function ensureAppendChild(obj) {
    if (!obj) return;
    obj.appendChild = function (child) {
      if (!child || (typeof child !== "object" && typeof child !== "function")) {
        throw domException("InvalidNodeType", "InvalidNodeType");
      }
      if (this.__node_kind === "text") {
        throw domException("HierarchyRequestError", "HierarchyRequestError");
      }
      if (child.parentNode && child.parentNode !== this) {
        try {
          child.parentNode.removeChild(child);
        } catch (_e) {
          // Ignore.
        }
      }
      child.parentNode = this;
      if (Array.isArray(this.childNodes)) {
        this.childNodes.push(child);
      }
      return child;
    };
  }

  function ensureNodeApis(obj) {
    if (!obj) return;
    ensureAppendChild(obj);
    ensureRemoveChild(obj);
  }

  function wrapAppendChild(proto) {
    if (!proto || typeof proto.appendChild !== "function") return;
    var orig = proto.appendChild;
    proto.appendChild = function (child) {
      if (!child || (typeof child !== "object" && typeof child !== "function")) {
        throw domException("InvalidNodeType", "InvalidNodeType");
      }
      // HTML/DomError mapping: appending into Text must throw HierarchyRequestError.
      if (this.__node_kind === "text") {
        throw domException("HierarchyRequestError", "HierarchyRequestError");
      }
      return orig.call(this, child);
    };
  }

  ensureNodeKind(doc, "document");
  ensureNodeApis(doc);

  // Wrap `createElement` (from EventTarget shim if present) so returned nodes carry a node kind.
  var origCreateElement = doc.createElement;
  doc.createElement = function (tagName) {
    var name = String(tagName);
    var el =
      typeof origCreateElement === "function"
        ? origCreateElement.call(this, name)
        : { tagName: name };
    ensureNodeKind(el, "element");
    ensureNodeApis(el);
    return el;
  };

  // Text nodes aren't modeled by the EventTarget shim; provide a minimal shape with mutation APIs.
  doc.createTextNode = function (data) {
    var text = { data: String(data), parentNode: null };
    ensureNodeKind(text, "text");
    text.appendChild = function (_child) {
      throw domException("HierarchyRequestError", "HierarchyRequestError");
    };
    ensureRemoveChild(text);
    return text;
  };

  // Patch `removeChild` onto EventTarget's Document/Element prototypes when available.
  try {
    if (typeof g.Document === "function" && g.Document.prototype) {
      wrapAppendChild(g.Document.prototype);
      ensureRemoveChild(g.Document.prototype);
      ensureAppendChild(g.Document.prototype);
    }
    if (typeof g.Element === "function" && g.Element.prototype) {
      wrapAppendChild(g.Element.prototype);
      ensureRemoveChild(g.Element.prototype);
      ensureAppendChild(g.Element.prototype);
    }
  } catch (_e) {
    // Ignore; fallback nodes still have per-instance methods via `ensureNodeApis`.
  }

  // Host shims may have created structural nodes (`document.documentElement`, `document.head`,
  // `document.body`) before this DOM shim installs its node-kind markers. Ensure they are treated
  // as elements so selector APIs like `matches()` / `closest()` can traverse them.
  ensureNodeKind(doc.documentElement, "element");
  ensureNodeKind(doc.head, "element");
  ensureNodeKind(doc.body, "element");

  function validateSelectors(sel) {
    sel = String(sel);
    if (sel === "") throw new SyntaxError("SyntaxError");
    // Extremely small validity check: detect unbalanced []/() pairs. This is enough to ensure the
    // curated WPT subset sees deterministic `SyntaxError` for obviously-invalid selectors like "[".
    var brackets = 0;
    var parens = 0;
    for (var i = 0; i < sel.length; i++) {
      var ch = sel[i];
      if (ch === "[") brackets++;
      else if (ch === "]") brackets--;
      if (brackets < 0) throw new SyntaxError("SyntaxError");
      if (ch === "(") parens++;
      else if (ch === ")") parens--;
      if (parens < 0) throw new SyntaxError("SyntaxError");
    }
    if (brackets !== 0 || parens !== 0) throw new SyntaxError("SyntaxError");
    return sel;
  }

  function splitSelectorGroups(sel) {
    sel = String(sel);
    var groups = [];
    var start = 0;
    var brackets = 0;
    var parens = 0;
    for (var i = 0; i < sel.length; i++) {
      var ch = sel[i];
      if (ch === "[") brackets++;
      else if (ch === "]") brackets--;
      else if (ch === "(") parens++;
      else if (ch === ")") parens--;
      if (ch === "," && brackets === 0 && parens === 0) {
        groups.push(sel.slice(start, i).trim());
        start = i + 1;
      }
    }
    groups.push(sel.slice(start).trim());
    var out = [];
    for (var j = 0; j < groups.length; j++) {
      var g = groups[j];
      if (g === "") throw new SyntaxError("SyntaxError");
      out.push(g);
    }
    return out;
  }

  function parseChain(sel) {
    sel = String(sel).trim();
    if (sel === "") throw new SyntaxError("SyntaxError");
    var chain = [];
    var i = 0;
    var pending = null; // combinator from previous token to the next one
    while (true) {
      // Skip whitespace before the next token.
      while (i < sel.length && /\s/.test(sel[i])) i++;
      if (i >= sel.length) break;
      if (sel[i] === ">") {
        // Leading combinator.
        throw new SyntaxError("SyntaxError");
      }

      var start = i;
      while (i < sel.length && !/\s/.test(sel[i]) && sel[i] !== ">") i++;
      var token = sel.slice(start, i);
      if (token === "") throw new SyntaxError("SyntaxError");
      chain.push({ token: token, combinator: pending });

      var sawWs = false;
      while (i < sel.length && /\s/.test(sel[i])) {
        sawWs = true;
        i++;
      }
      if (i >= sel.length) break;

      if (sel[i] === ">") {
        pending = ">";
        i++;
        while (i < sel.length && /\s/.test(sel[i])) i++;
        if (i >= sel.length) throw new SyntaxError("SyntaxError");
      } else if (sawWs) {
        pending = " ";
      } else {
        // Shouldn't happen (token parsing stops only on whitespace or '>'), but fall back to the
        // descendant combinator.
        pending = " ";
      }
    }
    return chain;
  }

  function parseSimpleToken(token) {
    token = String(token);
    if (token === ":scope") return { scope: true };

    // Only attempt to parse the subset we support (tag / #id / .class / *).
    for (var k = 0; k < token.length; k++) {
      var ch = token[k];
      if (ch === "." || ch === "#" || ch === "*") continue;
      if (
        (ch >= "0" && ch <= "9") ||
        (ch >= "A" && ch <= "Z") ||
        (ch >= "a" && ch <= "z") ||
        ch === "_" ||
        ch === "-"
      ) {
        continue;
      }
      // Unsupported selector syntax (attribute selectors, pseudo-classes, etc).
      return null;
    }

    var tag = null;
    var id = null;
    var classes = [];
    var i = 0;
    if (token[i] === "*") {
      tag = "*";
      i++;
    } else if (token[i] !== "." && token[i] !== "#") {
      var start = i;
      while (i < token.length && token[i] !== "." && token[i] !== "#") i++;
      tag = token.slice(start, i);
    }

    while (i < token.length) {
      var ch = token[i];
      if (ch === "#") {
        i++;
        var start = i;
        while (i < token.length && token[i] !== "." && token[i] !== "#") i++;
        var value = token.slice(start, i);
        if (value === "") throw new SyntaxError("SyntaxError");
        if (id != null) throw new SyntaxError("SyntaxError");
        id = value;
        continue;
      }
      if (ch === ".") {
        i++;
        var start = i;
        while (i < token.length && token[i] !== "." && token[i] !== "#") i++;
        var value = token.slice(start, i);
        if (value === "") throw new SyntaxError("SyntaxError");
        classes.push(value);
        continue;
      }
      return null;
    }

    return { tag: tag, id: id, classes: classes };
  }

  function nodeHasClass(node, cls) {
    var raw = String(node.className || "");
    if (raw === "") return false;
    var parts = raw.split(/\s+/);
    for (var i = 0; i < parts.length; i++) {
      if (parts[i] === cls) return true;
    }
    return false;
  }

  function matchesSimple(node, token, scopeRoot) {
    token = String(token);
    if (token === ":scope") return node === scopeRoot;
    if (!node || node.__node_kind !== "element") return false;
    var parsed = parseSimpleToken(token);
    if (!parsed) return false;

    if (parsed.tag && parsed.tag !== "*") {
      if (String(node.tagName || "").toLowerCase() !== String(parsed.tag).toLowerCase()) {
        return false;
      }
    }
    if (parsed.id != null) {
      if (String(node.id || "") !== parsed.id) return false;
    }
    if (parsed.classes && parsed.classes.length) {
      for (var i = 0; i < parsed.classes.length; i++) {
        if (!nodeHasClass(node, parsed.classes[i])) return false;
      }
    }
    return true;
  }

  function matchesChain(node, chain, scopeRoot) {
    if (!chain || chain.length === 0) return false;
    var cur = node;
    var i = chain.length - 1;
    if (!matchesSimple(cur, chain[i].token, scopeRoot)) return false;
    while (i > 0) {
      if (scopeRoot && cur === scopeRoot) return false;
      var combinator = chain[i].combinator || " ";
      var want = chain[i - 1].token;
      if (combinator === ">") {
        cur = cur.parentNode;
        if (!cur) return false;
        if (!matchesSimple(cur, want, scopeRoot)) return false;
      } else {
        cur = cur.parentNode;
        while (cur && cur !== scopeRoot && !matchesSimple(cur, want, scopeRoot)) {
          cur = cur.parentNode;
        }
        if (!cur) return false;
        if (cur === scopeRoot && !matchesSimple(cur, want, scopeRoot)) return false;
      }
      i--;
    }
    return true;
  }

  // Like `matchesChain`, but does not treat `scopeRoot` as a hard boundary. This is used to
  // implement `Element.matches()` / `Element.closest()`, where `:scope` refers to the element being
  // tested but ancestor traversal should not be restricted to within it.
  function matchesChainUnbounded(node, chain, scopeRoot) {
    if (!chain || chain.length === 0) return false;
    var cur = node;
    var i = chain.length - 1;
    if (!matchesSimple(cur, chain[i].token, scopeRoot)) return false;
    while (i > 0) {
      var combinator = chain[i].combinator || " ";
      var want = chain[i - 1].token;
      if (combinator === ">") {
        cur = cur.parentNode;
        if (!cur) return false;
        if (!matchesSimple(cur, want, scopeRoot)) return false;
      } else {
        cur = cur.parentNode;
        while (cur && !matchesSimple(cur, want, scopeRoot)) {
          cur = cur.parentNode;
        }
        if (!cur) return false;
      }
      i--;
    }
    return true;
  }

  function collectDescendants(root, out) {
    if (!root || !Array.isArray(root.childNodes)) return;
    for (var i = 0; i < root.childNodes.length; i++) {
      var child = root.childNodes[i];
      if (!child) continue;
      if (child.__node_kind === "element") {
        out.push(child);
        // Skip inert <template> subtrees (template contents).
        if (String(child.tagName || "").toLowerCase() === "template") {
          continue;
        }
      }
      collectDescendants(child, out);
    }
  }

  function queryAll(scope, selectors) {
    var sel = validateSelectors(selectors);
    var groups = splitSelectorGroups(sel);
    var chains = [];
    for (var i = 0; i < groups.length; i++) {
      chains.push(parseChain(groups[i]));
    }
    var matches = [];
    // `querySelector(All)` does not normally include the scope root itself, but `:scope` is special:
    // it matches the scope root. Add it explicitly so `el.querySelector(":scope")` works.
    if (scope && scope.__node_kind === "element") {
      for (var c = 0; c < chains.length; c++) {
        var chain = chains[c];
        if (chain.length > 0 && String(chain[chain.length - 1].token) === ":scope") {
          if (matchesChain(scope, chain, scope)) {
            matches.push(scope);
            break;
          }
        }
      }
    }
    var nodes = [];
    collectDescendants(scope, nodes);
    for (var n = 0; n < nodes.length; n++) {
      var node = nodes[n];
      for (var c = 0; c < chains.length; c++) {
        if (matchesChain(node, chains[c], scope)) {
          matches.push(node);
          break;
        }
      }
    }
    return matches;
  }

  function queryFirst(scope, selectors) {
    var matches = queryAll(scope, selectors);
    return matches.length > 0 ? matches[0] : null;
  }

  // Provide a default `document.body` so real-world patterns work in the test harness.
  if (!("body" in doc) || doc.body == null) {
    try {
      var body = doc.createElement("body");
      define(doc, "body", body);
      doc.appendChild(body);
    } catch (_e) {
      // Ignore.
    }
  }

  doc.querySelector = function (selectors) {
    return queryFirst(this, selectors);
  };
  doc.querySelectorAll = function (selectors) {
    return queryAll(this, selectors);
  };

  try {
    if (typeof g.Element === "function" && g.Element.prototype) {
      g.Element.prototype.querySelector = function (selectors) {
        return queryFirst(this, selectors);
      };
      g.Element.prototype.querySelectorAll = function (selectors) {
        return queryAll(this, selectors);
      };
      g.Element.prototype.matches = function (selectors) {
        var sel = validateSelectors(selectors);
        var groups = splitSelectorGroups(sel);
        for (var i = 0; i < groups.length; i++) {
          var chain = parseChain(groups[i]);
          if (matchesChainUnbounded(this, chain, this)) return true;
        }
        return false;
      };
      g.Element.prototype.closest = function (selectors) {
        var sel = validateSelectors(selectors);
        var groups = splitSelectorGroups(sel);
        var chains = [];
        for (var i = 0; i < groups.length; i++) {
          chains.push(parseChain(groups[i]));
        }
        var cur = this;
        while (cur) {
          if (cur.__node_kind === "element") {
            for (var c = 0; c < chains.length; c++) {
              if (matchesChainUnbounded(cur, chains[c], cur)) return cur;
            }
          }
          cur = cur.parentNode;
        }
        return null;
      };
    }
  } catch (_e) {
    // Ignore.
  }
  })();
  "##;

#[cfg(test)]
mod tests {
  use super::*;
  use rquickjs::{Context, Runtime};

  fn eval_str(ctx: &Ctx<'_>, src: &str) -> String {
    ctx.eval::<String, _>(src).expect("eval")
  }

  #[test]
  fn maps_dom_mutation_errors_to_domexception() {
    let rt = Runtime::new().expect("runtime");
    let ctx = Context::full(&rt).expect("context");
    ctx.with(|ctx| {
      let globals = ctx.globals();
      let document = Object::new(ctx.clone()).expect("document");
      globals.set("document", document.clone()).expect("set document");
      install_dom_bindings(ctx.clone(), &globals).expect("install bindings");

      let out = eval_str(
        &ctx,
        r#"(() => {
          try {
            const t = document.createTextNode("x");
            const el = document.createElement("div");
            t.appendChild(el);
            return "no throw";
          } catch (e) {
            return String(e.name) + "|" + String(e instanceof DOMException);
          }
        })()"#,
      );
      assert_eq!(out, "HierarchyRequestError|true");

      let out = eval_str(
        &ctx,
        r#"(() => {
          try {
            const parent = document.createElement("div");
            const child = document.createElement("span");
            parent.removeChild(child);
            return "no throw";
          } catch (e) {
            return String(e.name);
          }
        })()"#,
      );
      assert_eq!(out, "NotFoundError");
    });
  }

  #[test]
  fn maps_invalid_selectors_to_syntaxerror() {
    let rt = Runtime::new().expect("runtime");
    let ctx = Context::full(&rt).expect("context");
    ctx.with(|ctx| {
      let globals = ctx.globals();
      let document = Object::new(ctx.clone()).expect("document");
      globals.set("document", document.clone()).expect("set document");
      install_dom_bindings(ctx.clone(), &globals).expect("install bindings");

      let out = eval_str(
        &ctx,
        r#"(() => {
          try {
            document.querySelector("[");
            return "no throw";
          } catch (e) {
            return String(e.name) + "|" + String(e instanceof SyntaxError);
          }
        })()"#,
      );
      assert_eq!(out, "SyntaxError|true");
    });
  }

  #[test]
  fn maps_invalid_node_types_to_invalidnodetype_domexception() {
    let rt = Runtime::new().expect("runtime");
    let ctx = Context::full(&rt).expect("context");
    ctx.with(|ctx| {
      let globals = ctx.globals();
      let document = Object::new(ctx.clone()).expect("document");
      globals.set("document", document.clone()).expect("set document");
      install_dom_bindings(ctx.clone(), &globals).expect("install bindings");

      let out = eval_str(
        &ctx,
        r#"(() => {
          try {
            const el = document.createElement("div");
            el.appendChild(123);
            return "no throw";
          } catch (e) {
            return String(e.name) + "|" + String(e instanceof DOMException);
          }
        })()"#,
      );
      assert_eq!(out, "InvalidNodeType|true");
    });
  }
}
