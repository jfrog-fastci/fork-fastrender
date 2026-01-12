use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{parse_fragment, ParseOpts};
use markup5ever::{LocalName, Namespace, QualName};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use std::cell::RefCell;
use std::rc::Rc;

const HTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

const DOM_SHIM: &str = r##"
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (g.__fastrender_dom_installed) return;
  g.__fastrender_dom_installed = true;

  var NODE_ID = Symbol("fastrender_node_id");
  var NODE_CACHE = new Map(); // node id -> JS wrapper
  var STYLE_OWNER = Symbol("fastrender_style_owner");
  var STYLE_CACHE = new WeakMap(); // HTMLElement -> CSSStyleDeclaration

  function illegal() {
    throw new TypeError("Illegal constructor");
  }

  // --- Events (minimal EventTarget / Event) ---
  var EVENT_LISTENERS = new WeakMap(); // target -> Map(type -> [{callback,capture,once,passive}])
  var EVENT_PARENTS = new WeakMap(); // EventTarget instance -> parent EventTarget

  function normalizeListenerOptions(options) {
    var capture = false;
    var once = false;
    var passive = false;
    if (options === true) {
      capture = true;
    } else if (options && typeof options === "object") {
      if (options.capture) capture = true;
      if (options.once) once = true;
      if (options.passive) passive = true;
    }
    return { capture: capture, once: once, passive: passive };
  }

  function getListenerList(target, type, create) {
    var map = EVENT_LISTENERS.get(target);
    if (!map) {
      if (!create) return null;
      map = new Map();
      EVENT_LISTENERS.set(target, map);
    }
    var list = map.get(type);
    if (!list) {
      if (!create) return null;
      list = [];
      map.set(type, list);
    }
    return list;
  }

  function isNodeWrapper(o) {
    return typeof o === "object" && o !== null && typeof o[NODE_ID] === "number";
  }

  function eventParent(target) {
    if (target === g) return null;
    if (target === g.document) return g;
    if (isNodeWrapper(target)) return target.parentNode || null;
    return EVENT_PARENTS.get(target) || null;
  }

  function EventTarget(parent) {
    if (parent !== null && parent !== undefined) {
      if (typeof parent !== "object" || parent === null) {
        throw new TypeError("Failed to construct 'EventTarget': parameter 1 is not of type 'EventTarget'.");
      }
      EVENT_PARENTS.set(this, parent);
    }
  }

  EventTarget.prototype.addEventListener = function (type, callback, options) {
    if (callback === null || callback === undefined) return;
    if (typeof callback !== "function") return;
    var t = String(type);
    var opts = normalizeListenerOptions(options);
    var list = getListenerList(this, t, true);
    for (var i = 0; i < list.length; i++) {
      var l = list[i];
      if (l.callback === callback && l.capture === opts.capture) return;
    }
    list.push({
      callback: callback,
      capture: opts.capture,
      once: opts.once,
      passive: opts.passive,
    });
  };

  EventTarget.prototype.removeEventListener = function (type, callback, options) {
    if (callback === null || callback === undefined) return;
    if (typeof callback !== "function") return;
    var t = String(type);
    var opts = normalizeListenerOptions(options);
    var list = getListenerList(this, t, false);
    if (!list) return;
    for (var i = 0; i < list.length; i++) {
      var l = list[i];
      if (l.callback === callback && l.capture === opts.capture) {
        list.splice(i, 1);
        return;
      }
    }
  };

  function Event(type, init) {
    if (arguments.length < 1) {
      throw new TypeError("Failed to construct 'Event': 1 argument required, but only 0 present.");
    }
    this.type = String(type);
    this.bubbles = !!(init && init.bubbles);
    this.cancelable = !!(init && init.cancelable);
    this.defaultPrevented = false;
    this.target = null;
    this.currentTarget = null;
    this.eventPhase = 0;
    this._propagationStopped = false;
    this._immediateStopped = false;
    this._inPassiveListener = false;
  }

  Event.NONE = 0;
  Event.CAPTURING_PHASE = 1;
  Event.AT_TARGET = 2;
  Event.BUBBLING_PHASE = 3;

  Event.prototype.preventDefault = function () {
    if (this.cancelable && !this._inPassiveListener) {
      this.defaultPrevented = true;
    }
  };

  Event.prototype.stopPropagation = function () {
    this._propagationStopped = true;
  };

  Event.prototype.stopImmediatePropagation = function () {
    this._propagationStopped = true;
    this._immediateStopped = true;
  };

  EventTarget.prototype.dispatchEvent = function (event) {
    if (typeof event !== "object" || event === null) {
      throw new TypeError("Failed to execute 'dispatchEvent' on 'EventTarget': parameter 1 is not of type 'Event'.");
    }
    if (typeof event.type !== "string") {
      throw new TypeError("Failed to execute 'dispatchEvent' on 'EventTarget': event.type must be a string.");
    }
    event.target = this;
    event.currentTarget = null;
    event.eventPhase = Event.NONE;
    event._propagationStopped = false;
    event._immediateStopped = false;
    event._inPassiveListener = false;
    // Preserve defaultPrevented across dispatches, matching browser behavior for re-dispatch.

    var path = [];
    var seen = new Set();
    var cur = this;
    while (cur && !seen.has(cur) && path.length < 10000) {
      path.push(cur);
      seen.add(cur);
      cur = eventParent(cur);
    }

    function invoke(target, phase, capture) {
      var list = getListenerList(target, event.type, false);
      if (!list) return;
      for (var i = 0; i < list.length; i++) {
        var l = list[i];
        if (l.capture !== capture) continue;
        if (event._immediateStopped) break;
        event.currentTarget = target;
        event.eventPhase = phase;
        event._inPassiveListener = !!l.passive;
        try {
          l.callback.call(target, event);
        } finally {
          event._inPassiveListener = false;
        }
        if (l.once) {
          list.splice(i, 1);
          i -= 1;
        }
      }
    }

    // Capturing phase: root -> parent (exclude target).
    for (var i = path.length - 1; i >= 1; i--) {
      if (event._propagationStopped) break;
      invoke(path[i], Event.CAPTURING_PHASE, true);
    }

    // At target.
    if (!event._propagationStopped) {
      event._immediateStopped = false;
      invoke(path[0], Event.AT_TARGET, true);
      event._immediateStopped = false;
      invoke(path[0], Event.AT_TARGET, false);
    }

    // Bubbling phase: parent -> root (exclude target).
    if (event.bubbles) {
      for (var i = 1; i < path.length; i++) {
        if (event._propagationStopped) break;
        event._immediateStopped = false;
        invoke(path[i], Event.BUBBLING_PHASE, false);
      }
    }

    event.currentTarget = null;
    event.eventPhase = Event.NONE;
    return event.defaultPrevented ? false : true;
  };

  function Node() { illegal(); }
  function Document() { illegal(); }
  function DocumentFragment() { illegal(); }
  function Element() { illegal(); }
  function HTMLElement() { illegal(); }
  function HTMLInputElement() { illegal(); }
  function HTMLTextAreaElement() { illegal(); }
  function HTMLSelectElement() { illegal(); }
  function HTMLFormElement() { illegal(); }
  function HTMLOptionElement() { illegal(); }
  function Text() { illegal(); }
  function HTMLCollection() { illegal(); }
  function CSSStyleDeclaration() { illegal(); }
  function HTMLOptionsCollection() { illegal(); }
  function HTMLFormControlsCollection() { illegal(); }

  Object.setPrototypeOf(Node.prototype, EventTarget.prototype);
  Object.setPrototypeOf(Document.prototype, Node.prototype);
  Object.setPrototypeOf(DocumentFragment.prototype, Node.prototype);
  Object.setPrototypeOf(Element.prototype, Node.prototype);
  Object.setPrototypeOf(HTMLElement.prototype, Element.prototype);
  Object.setPrototypeOf(HTMLInputElement.prototype, HTMLElement.prototype);
  Object.setPrototypeOf(HTMLTextAreaElement.prototype, HTMLElement.prototype);
  Object.setPrototypeOf(HTMLSelectElement.prototype, HTMLElement.prototype);
  Object.setPrototypeOf(HTMLFormElement.prototype, HTMLElement.prototype);
  Object.setPrototypeOf(HTMLOptionElement.prototype, HTMLElement.prototype);
  Object.setPrototypeOf(Text.prototype, Node.prototype);
  Object.setPrototypeOf(HTMLOptionsCollection.prototype, HTMLCollection.prototype);
  Object.setPrototypeOf(HTMLFormControlsCollection.prototype, HTMLCollection.prototype);

  // Node type constants.
  Node.ELEMENT_NODE = 1;
  Node.TEXT_NODE = 3;
  Node.COMMENT_NODE = 8;
  Node.DOCUMENT_NODE = 9;
  Node.DOCUMENT_TYPE_NODE = 10;
  Node.DOCUMENT_FRAGMENT_NODE = 11;

  // Attach the existing `document` object (created by Rust) to `Document.prototype`.
  if (typeof g.document !== "object" || g.document === null) {
    g.document = Object.create(Document.prototype);
  } else {
    Object.setPrototypeOf(g.document, Document.prototype);
  }

  // Minimal `document.cookie` backing store (host-provided).
  if (
    typeof g.__fastrender_get_cookie === "function" &&
    typeof g.__fastrender_set_cookie === "function" &&
    !Object.getOwnPropertyDescriptor(Document.prototype, "cookie")
  ) {
    Object.defineProperty(Document.prototype, "cookie", {
      configurable: true,
      enumerable: true,
      get: function () {
        return String(g.__fastrender_get_cookie());
      },
      set: function (v) {
        g.__fastrender_set_cookie(String(v));
      },
    });
  }

  // Document node id is always 0.
  g.document[NODE_ID] = 0;
  g.document.parentNode = null;
  g.document.childNodes = [];
  NODE_CACHE.set(0, g.document);

  function ensureArray(o, key) {
    if (!o[key]) o[key] = [];
    return o[key];
  }

  function detachFromParent(child) {
    var parent = child.parentNode;
    if (!parent) return;
    var arr = ensureArray(parent, "childNodes");
    var idx = arr.indexOf(child);
    if (idx >= 0) arr.splice(idx, 1);
    child.parentNode = null;
  }

  function nodeIdFromThis(self) {
    if (typeof self !== "object" || self === null) {
      throw new TypeError("Illegal invocation");
    }
    var id = self[NODE_ID];
    if (typeof id !== "number") {
      throw new TypeError("Illegal invocation");
    }
    return id;
  }

  function makeNode(proto, id, tagName) {
    var cached = NODE_CACHE.get(id);
    if (cached) {
      if (Object.getPrototypeOf(cached) !== proto) {
        Object.setPrototypeOf(cached, proto);
      }
      if (tagName !== undefined) {
        cached.tagName = String(tagName);
      }
      return cached;
    }
    var o = Object.create(proto);
    NODE_CACHE.set(id, o);
    o[NODE_ID] = id;
    o.parentNode = null;
    o.childNodes = [];
    if (tagName !== undefined) {
      o.tagName = String(tagName);
    }
    return o;
  }

  function elementPrototypeForTag(tagNameLower) {
    // The shim only needs a small subset of element interfaces for WPT and common scripts.
    // Default to `HTMLElement` for all HTML tags.
    switch (String(tagNameLower).toLowerCase()) {
      case "input":
        return HTMLInputElement.prototype;
      case "textarea":
        return HTMLTextAreaElement.prototype;
      case "select":
        return HTMLSelectElement.prototype;
      case "form":
        return HTMLFormElement.prototype;
      case "option":
        return HTMLOptionElement.prototype;
      default:
        return HTMLElement.prototype;
    }
  }

  function elementFromId(id) {
    id = Number(id);
    var tag = g.__fastrender_dom_get_tag_name(id);
    var lower = String(tag).toLowerCase();
    return makeNode(elementPrototypeForTag(lower), id, String(tag).toUpperCase());
  }

  function nodeFromId(id) {
    id = Number(id);
    if (id === 0) return g.document;
    var t = g.__fastrender_dom_get_node_type(id);
    if (t === Node.ELEMENT_NODE) return elementFromId(id);
    if (t === Node.TEXT_NODE) return makeNode(Text.prototype, id);
    if (t === Node.DOCUMENT_FRAGMENT_NODE) return makeNode(DocumentFragment.prototype, id);
    if (t === Node.DOCUMENT_NODE) return g.document;
    return makeNode(Node.prototype, id);
  }

  function syncTree(root) {
    var seen = new Set();
    var stack = [root];

    while (stack.length) {
      var node = stack.pop();
      var nodeId = nodeIdFromThis(node);
      if (seen.has(nodeId)) continue;
      seen.add(nodeId);

      var childIds = g.__fastrender_dom_get_child_nodes(nodeId);
      var nodes = ensureArray(node, "childNodes");
      for (var i = 0; i < nodes.length; i++) {
        var old = nodes[i];
        if (old && typeof old === "object") old.parentNode = null;
      }
      nodes.length = 0;

      for (var j = 0; j < childIds.length; j++) {
        var child = nodeFromId(childIds[j]);
        nodes.push(child);
        child.parentNode = node;
      }

      for (var k = nodes.length - 1; k >= 0; k--) {
        var c = nodes[k];
        if (c instanceof Element || c instanceof DocumentFragment || c === g.document) {
          stack.push(c);
        }
      }
    }
  }

  function syntaxError(msg) {
    // Match browser behavior closely enough for WPT smoke tests: `e.name` must be "SyntaxError".
    throw new SyntaxError(msg || "Invalid selector");
  }

  function isIdentChar(ch) {
    // Keep this conservative; it only needs to support ids/class names in the offline corpus.
    return (
      (ch >= "a" && ch <= "z") ||
      (ch >= "A" && ch <= "Z") ||
      (ch >= "0" && ch <= "9") ||
      ch === "_" ||
      ch === "-" ||
      ch === "\\u00B7"
    );
  }

  function parseIdent(input, i) {
    var start = i;
    while (i < input.length && isIdentChar(input[i])) i++;
    if (i === start) syntaxError("Expected identifier");
    return { value: input.slice(start, i), next: i };
  }

  function skipWhitespace(input, i) {
    var start = i;
    while (i < input.length && /\s/.test(input[i])) i++;
    return { next: i, had: i !== start };
  }

  function parseCompound(input, i) {
    var tag = null;
    var id = null;
    var classes = [];
    var isScope = false;

    if (input[i] === "*") {
      tag = "*";
      i++;
    } else if (i < input.length && isIdentChar(input[i])) {
      var ident = parseIdent(input, i);
      tag = ident.value;
      i = ident.next;
    }

    while (i < input.length) {
      var ch = input[i];
      if (ch === "#") {
        i++;
        var ident = parseIdent(input, i);
        id = ident.value;
        i = ident.next;
      } else if (ch === ".") {
        i++;
        var ident = parseIdent(input, i);
        classes.push(ident.value);
        i = ident.next;
      } else if (ch === ":") {
        i++;
        var ident = parseIdent(input, i);
        i = ident.next;
        if (ident.value.toLowerCase() === "scope") {
          isScope = true;
        } else {
          syntaxError("Unsupported pseudo-class :" + ident.value);
        }
      } else {
        break;
      }
    }

    if (!tag && !id && classes.length === 0 && !isScope) {
      syntaxError("Expected selector");
    }

    return {
      compound: { tag: tag, id: id, classes: classes, isScope: isScope },
      next: i,
    };
  }

  function parseSelector(input) {
    var i = 0;
    var compounds = [];
    var combinators = [];
    var hasScope = false;

    var ws = skipWhitespace(input, i);
    i = ws.next;

    while (i < input.length) {
      var parsed = parseCompound(input, i);
      i = parsed.next;
      compounds.push(parsed.compound);
      if (parsed.compound.isScope) hasScope = true;

      ws = skipWhitespace(input, i);
      i = ws.next;
      if (i >= input.length) break;

      var combinator = null;
      if (input[i] === ">") {
        i++;
        ws = skipWhitespace(input, i);
        i = ws.next;
        combinator = "child";
      } else if (ws.had) {
        combinator = "descendant";
      } else {
        syntaxError("Unexpected character " + input[i]);
      }
      combinators.push(combinator);
    }

    if (compounds.length === 0) {
      syntaxError("Expected selector");
    }

    return { compounds: compounds, combinators: combinators, hasScope: hasScope };
  }

  function parseSelectorList(selectors) {
    var raw = String(selectors);
    var parts = raw.split(",").map(function (s) { return s.trim(); }).filter(Boolean);
    if (parts.length === 0) syntaxError("Empty selector");
    return parts.map(parseSelector);
  }

  function splitClassTokens(className) {
    var raw = String(className || "").trim();
    if (!raw) return [];
    return raw.split(/\s+/).filter(Boolean);
  }

  function compoundMatches(compound, el, scopeEl) {
    if (!(el instanceof Element)) return false;
    if (compound.isScope && el !== scopeEl) return false;
    if (compound.tag && compound.tag !== "*") {
      if (String(el.tagName).toLowerCase() !== compound.tag.toLowerCase()) return false;
    }
    if (compound.id) {
      if (String(el.id) !== compound.id) return false;
    }
    if (compound.classes && compound.classes.length) {
      var tokens = splitClassTokens(el.className);
      for (var i = 0; i < compound.classes.length; i++) {
        if (tokens.indexOf(compound.classes[i]) < 0) return false;
      }
    }
    return true;
  }

  function matchesSelectorChain(el, selector, scopeEl, limitRoot) {
    var idx = selector.compounds.length - 1;
    if (!compoundMatches(selector.compounds[idx], el, scopeEl)) return false;

    var current = el;
    for (var i = idx - 1; i >= 0; i--) {
      var combinator = selector.combinators[i];
      var need = selector.compounds[i];

      if (combinator === "child") {
        current = current.parentNode;
        if (!current) return false;
        if (limitRoot && current === limitRoot.parentNode) return false;
        if (!compoundMatches(need, current, scopeEl)) return false;
      } else if (combinator === "descendant") {
        var ancestor = current.parentNode;
        var stop = limitRoot ? limitRoot.parentNode : null;
        var found = false;
        while (ancestor && ancestor !== stop) {
          if (compoundMatches(need, ancestor, scopeEl)) {
            found = true;
            current = ancestor;
            break;
          }
          ancestor = ancestor.parentNode;
        }
        if (!found) return false;
      } else {
        return false;
      }
    }

    return true;
  }

  function traverseElementSubtree(root, visit) {
    var stack = [];
    var kids = root.childNodes || [];
    for (var i = kids.length - 1; i >= 0; i--) stack.push(kids[i]);

    while (stack.length) {
      var node = stack.pop();
      if (!(node instanceof Element)) continue;
      visit(node);
      // Treat `<template>` contents as inert.
      if (String(node.tagName).toUpperCase() === "TEMPLATE") continue;
      var children = node.childNodes || [];
      for (var j = children.length - 1; j >= 0; j--) stack.push(children[j]);
    }
  }

  function isArrayIndex(prop) {
    if (typeof prop !== "string") return false;
    if (prop === "") return false;
    var n = Number(prop);
    if (!Number.isInteger(n)) return false;
    if (n < 0) return false;
    // Ensure canonical decimal representation ("01" is not an index property).
    if (String(n) !== prop) return false;
    // Max array index per spec.
    return n < 4294967295;
  }

  function makeLiveElementCollection(getIds, proto) {
    var target = Object.create(proto || HTMLCollection.prototype);

    Object.defineProperty(target, "length", {
      get: function () { return getIds().length; },
      configurable: true,
    });

    target.item = function (index) {
      var i = Number(index);
      if (!isFinite(i) || isNaN(i)) i = 0;
      i = Math.trunc(i);
      var ids = getIds();
      if (i < 0 || i >= ids.length) return null;
      return elementFromId(ids[i]);
    };

    target[Symbol.iterator] = function () {
      var ids = getIds();
      var i = 0;
      return {
        next: function () {
          if (i >= ids.length) return { done: true, value: undefined };
          return { done: false, value: elementFromId(ids[i++]) };
        },
        [Symbol.iterator]: function () { return this; }
      };
    };

    return new Proxy(target, {
      get: function (t, prop, recv) {
        if (isArrayIndex(prop)) {
          var ids = getIds();
          var idx = Number(prop);
          if (idx < ids.length) return elementFromId(ids[idx]);
          return undefined;
        }
        return Reflect.get(t, prop, recv);
      }
    });
  }

  Document.prototype.createElement = function (tagName) {
    var raw = String(tagName);
    var id = g.__fastrender_dom_create_element(raw);
    return makeNode(elementPrototypeForTag(raw.toLowerCase()), id, raw.toUpperCase());
  };

  Document.prototype.createDocumentFragment = function () {
    var id = g.__fastrender_dom_create_document_fragment();
    return makeNode(DocumentFragment.prototype, id);
  };

  Document.prototype.createTextNode = function (data) {
    var id = g.__fastrender_dom_create_text_node(String(data));
    return makeNode(Text.prototype, id);
  };

  Document.prototype.getElementById = function (elementId) {
    if (this !== g.document) {
      throw new TypeError("Illegal invocation");
    }
    var needle = String(elementId);
    var root = g.document.documentElement;
    if (!root) return null;
    if (root.id === needle) return root;

    var found = null;
    traverseElementSubtree(root, function (el) {
      if (found) return;
      if (el.id === needle) found = el;
    });
    return found;
  };

  DocumentFragment.prototype.getElementById = function (elementId) {
    nodeIdFromThis(this);
    var needle = String(elementId);
    var nodes = this.childNodes || [];
    for (var i = 0; i < nodes.length; i++) {
      var node = nodes[i];
      if (!(node instanceof Element)) continue;
      if (node.id === needle) return node;

      var found = null;
      traverseElementSubtree(node, function (el) {
        if (found) return;
        if (el.id === needle) found = el;
      });
      if (found) return found;
    }
    return null;
  };

  Object.defineProperty(Element.prototype, "innerHTML", {
    get: function () {
      return g.__fastrender_dom_get_inner_html(nodeIdFromThis(this));
    },
    set: function (html) {
      g.__fastrender_dom_set_inner_html(nodeIdFromThis(this), String(html));
      syncTree(this);
    },
    configurable: true,
  });

  Element.prototype.getAttribute = function (name) {
    var v = g.__fastrender_dom_get_attribute(nodeIdFromThis(this), String(name));
    return v === undefined ? null : v;
  };

  Element.prototype.setAttribute = function (name, value) {
    g.__fastrender_dom_set_attribute(nodeIdFromThis(this), String(name), String(value));
  };

  Element.prototype.removeAttribute = function (name) {
    g.__fastrender_dom_remove_attribute(nodeIdFromThis(this), String(name));
  };

  function cssStyleFromThis(self) {
    if (typeof self !== "object" || self === null) {
      throw new TypeError("Illegal invocation");
    }
    var el = self[STYLE_OWNER];
    if (!(el instanceof HTMLElement)) {
      throw new TypeError("Illegal invocation");
    }
    return el;
  }

  function parseStyleDecls(cssText) {
    var raw = String(cssText || "");
    var out = [];
    var parts = raw.split(";");
    for (var i = 0; i < parts.length; i++) {
      var part = parts[i];
      if (!part) continue;
      var idx = part.indexOf(":");
      if (idx < 0) continue;
      var name = part.slice(0, idx).trim();
      if (!name) continue;
      var value = part.slice(idx + 1).trim();
      // Ignore priority (naively strip a trailing "!important").
      var lower = value.toLowerCase();
      var bang = lower.lastIndexOf("!important");
      if (bang >= 0 && lower.slice(bang).trim() === "!important") {
        value = value.slice(0, bang).trim();
      }
      out.push({ name: name.toLowerCase(), value: value });
    }
    return out;
  }

  function serializeStyleDecls(decls) {
    var out = [];
    for (var i = 0; i < decls.length; i++) {
      var d = decls[i];
      if (!d || !d.name) continue;
      var value = d.value;
      if (value === null || value === undefined) value = "";
      out.push(String(d.name) + ": " + String(value).trim());
    }
    return out.join("; ");
  }

  Object.defineProperty(CSSStyleDeclaration.prototype, "cssText", {
    get: function () {
      var el = cssStyleFromThis(this);
      var v = el.getAttribute("style");
      return v === null ? "" : v;
    },
    set: function (value) {
      var el = cssStyleFromThis(this);
      el.setAttribute("style", String(value));
    },
    configurable: true,
   });

  CSSStyleDeclaration.prototype.getPropertyValue = function (name) {
    var el = cssStyleFromThis(this);
    var needle = String(name).trim().toLowerCase();
    if (!needle) return "";
    var cssText = el.getAttribute("style") || "";
    var decls = parseStyleDecls(cssText);
    for (var i = 0; i < decls.length; i++) {
      if (decls[i].name === needle) {
        return decls[i].value || "";
      }
    }
    return "";
  };

  CSSStyleDeclaration.prototype.setProperty = function (name, value) {
    var el = cssStyleFromThis(this);
    var needle = String(name).trim().toLowerCase();
    if (!needle) return;
    var v = String(value).trim();
    var cssText = el.getAttribute("style") || "";
    var decls = parseStyleDecls(cssText);

    var idx = -1;
    for (var i = 0; i < decls.length; i++) {
      if (decls[i].name === needle) {
        idx = i;
        break;
      }
    }

    if (!v) {
      if (idx >= 0) decls.splice(idx, 1);
    } else {
      if (idx >= 0) {
        decls[idx].value = v;
      } else {
        decls.push({ name: needle, value: v });
      }
    }
    el.setAttribute("style", serializeStyleDecls(decls));
  };

  Object.defineProperty(HTMLElement.prototype, "hidden", {
    get: function () {
      nodeIdFromThis(this);
      if (!(this instanceof HTMLElement)) throw new TypeError("Illegal invocation");
      return this.getAttribute("hidden") !== null;
    },
    set: function (value) {
      nodeIdFromThis(this);
      if (!(this instanceof HTMLElement)) throw new TypeError("Illegal invocation");
      if (value) {
        this.setAttribute("hidden", "");
      } else {
        this.removeAttribute("hidden");
      }
    },
    configurable: true,
  });

  Object.defineProperty(HTMLElement.prototype, "title", {
    get: function () {
      nodeIdFromThis(this);
      if (!(this instanceof HTMLElement)) throw new TypeError("Illegal invocation");
      var v = this.getAttribute("title");
      return v === null ? "" : v;
    },
    set: function (value) {
      nodeIdFromThis(this);
      if (!(this instanceof HTMLElement)) throw new TypeError("Illegal invocation");
      this.setAttribute("title", String(value));
    },
    configurable: true,
  });

  Object.defineProperty(HTMLElement.prototype, "lang", {
    get: function () {
      nodeIdFromThis(this);
      if (!(this instanceof HTMLElement)) throw new TypeError("Illegal invocation");
      var v = this.getAttribute("lang");
      return v === null ? "" : v;
    },
    set: function (value) {
      nodeIdFromThis(this);
      if (!(this instanceof HTMLElement)) throw new TypeError("Illegal invocation");
      this.setAttribute("lang", String(value));
    },
    configurable: true,
  });

  Object.defineProperty(HTMLElement.prototype, "dir", {
    get: function () {
      nodeIdFromThis(this);
      if (!(this instanceof HTMLElement)) throw new TypeError("Illegal invocation");
      var v = this.getAttribute("dir");
      return v === null ? "" : v;
    },
    set: function (value) {
      nodeIdFromThis(this);
      if (!(this instanceof HTMLElement)) throw new TypeError("Illegal invocation");
      this.setAttribute("dir", String(value));
    },
    configurable: true,
  });

  Object.defineProperty(HTMLElement.prototype, "style", {
    get: function () {
      nodeIdFromThis(this);
      if (!(this instanceof HTMLElement)) throw new TypeError("Illegal invocation");
      var cached = STYLE_CACHE.get(this);
      if (cached) return cached;
      var style = Object.create(CSSStyleDeclaration.prototype);
      Object.defineProperty(style, STYLE_OWNER, { value: this });
      STYLE_CACHE.set(this, style);
      return style;
    },
    configurable: true,
  });

  Object.defineProperty(Text.prototype, "data", {
    get: function () {
      return g.__fastrender_dom_get_text_data(nodeIdFromThis(this));
    },
    set: function (value) {
      g.__fastrender_dom_set_text_data(nodeIdFromThis(this), String(value));
    },
    configurable: true,
  });

  Object.defineProperty(Element.prototype, "id", {
    get: function () {
      var v = this.getAttribute("id");
      return v === null ? "" : v;
    },
    set: function (value) {
      this.setAttribute("id", value);
    },
    configurable: true,
  });

  Object.defineProperty(Element.prototype, "className", {
    get: function () {
      var v = this.getAttribute("class");
      return v === null ? "" : v;
    },
    set: function (value) {
      this.setAttribute("class", value);
    },
    configurable: true,
  });

  // --- Minimal HTML form control APIs (attribute/textContent based) ---
  Object.defineProperty(HTMLInputElement.prototype, "value", {
    get: function () {
      nodeIdFromThis(this);
      var v = this.getAttribute("value");
      return v === null ? "" : v;
    },
    set: function (v) {
      nodeIdFromThis(this);
      this.setAttribute("value", String(v));
    },
    configurable: true,
  });
  Object.defineProperty(HTMLInputElement.prototype, "checked", {
    get: function () {
      nodeIdFromThis(this);
      return this.getAttribute("checked") !== null;
    },
    set: function (v) {
      nodeIdFromThis(this);
      if (v) {
        this.setAttribute("checked", "");
      } else {
        this.removeAttribute("checked");
      }
    },
    configurable: true,
  });
  Object.defineProperty(HTMLInputElement.prototype, "disabled", {
    get: function () {
      nodeIdFromThis(this);
      return this.getAttribute("disabled") !== null;
    },
    set: function (v) {
      nodeIdFromThis(this);
      if (v) {
        this.setAttribute("disabled", "");
      } else {
        this.removeAttribute("disabled");
      }
    },
    configurable: true,
  });

  Object.defineProperty(HTMLTextAreaElement.prototype, "value", {
    get: function () {
      nodeIdFromThis(this);
      var v = this.textContent;
      return v === null || v === undefined ? "" : String(v);
    },
    set: function (v) {
      nodeIdFromThis(this);
      this.textContent = String(v);
    },
    configurable: true,
  });

  Object.defineProperty(HTMLOptionElement.prototype, "value", {
    get: function () {
      nodeIdFromThis(this);
      var v = this.getAttribute("value");
      if (v !== null) return v;
      var t = this.textContent;
      return t === null || t === undefined ? "" : String(t);
    },
    set: function (v) {
      nodeIdFromThis(this);
      this.setAttribute("value", String(v));
    },
    configurable: true,
  });
  Object.defineProperty(HTMLOptionElement.prototype, "selected", {
    get: function () {
      nodeIdFromThis(this);
      return this.getAttribute("selected") !== null;
    },
    set: function (v) {
      nodeIdFromThis(this);
      if (v) {
        this.setAttribute("selected", "");
      } else {
        this.removeAttribute("selected");
      }
    },
    configurable: true,
  });

  var SELECT_OPTIONS_CACHE = new WeakMap();
  function optionIdsForSelect(select) {
    var ids = [];
    traverseElementSubtree(select, function (el) {
      if (String(el.tagName).toUpperCase() === "OPTION") {
        ids.push(nodeIdFromThis(el));
      }
    });
    return ids;
  }

  Object.defineProperty(HTMLSelectElement.prototype, "options", {
    get: function () {
      nodeIdFromThis(this);
      var cached = SELECT_OPTIONS_CACHE.get(this);
      if (cached) return cached;
      var self = this;
      var collection = makeLiveElementCollection(function () {
        return optionIdsForSelect(self);
      }, HTMLOptionsCollection.prototype);
      SELECT_OPTIONS_CACHE.set(this, collection);
      return collection;
    },
    configurable: true,
  });

  Object.defineProperty(HTMLSelectElement.prototype, "selectedIndex", {
    get: function () {
      nodeIdFromThis(this);
      var opts = this.options;
      var len = opts.length;
      for (var i = 0; i < len; i++) {
        var opt = opts[i];
        if (opt && opt.getAttribute("selected") !== null) return i;
      }
      return len ? 0 : -1;
    },
    set: function (value) {
      nodeIdFromThis(this);
      var n = Number(value);
      if (!isFinite(n) || isNaN(n)) n = 0;
      n = Math.trunc(n);

      var opts = this.options;
      var len = opts.length;
      if (n < 0 || n >= len) {
        for (var i = 0; i < len; i++) {
          var opt = opts[i];
          if (opt) opt.removeAttribute("selected");
        }
        return;
      }
      for (var i = 0; i < len; i++) {
        var opt = opts[i];
        if (!opt) continue;
        if (i === n) {
          opt.setAttribute("selected", "");
        } else {
          opt.removeAttribute("selected");
        }
      }
    },
    configurable: true,
  });

  Object.defineProperty(HTMLSelectElement.prototype, "value", {
    get: function () {
      nodeIdFromThis(this);
      var idx = this.selectedIndex;
      if (idx < 0) return "";
      var opt = this.options[idx];
      if (!opt) return "";
      return String(opt.value);
    },
    set: function (v) {
      nodeIdFromThis(this);
      var needle = String(v);
      var opts = this.options;
      var len = opts.length;
      var found = -1;
      for (var i = 0; i < len; i++) {
        var opt = opts[i];
        if (opt && String(opt.value) === needle) {
          found = i;
          break;
        }
      }
      if (found < 0) {
        this.selectedIndex = -1;
        return;
      }
      this.selectedIndex = found;
    },
    configurable: true,
  });

  var FORM_ELEMENTS_CACHE = new WeakMap();
  function formControlIdsForForm(form) {
    var ids = [];
    traverseElementSubtree(form, function (el) {
      var tag = String(el.tagName).toUpperCase();
      if (tag === "INPUT" || tag === "SELECT" || tag === "TEXTAREA" || tag === "BUTTON") {
        ids.push(nodeIdFromThis(el));
      }
    });
    return ids;
  }

  Object.defineProperty(HTMLFormElement.prototype, "elements", {
    get: function () {
      nodeIdFromThis(this);
      var cached = FORM_ELEMENTS_CACHE.get(this);
      if (cached) return cached;
      var self = this;
      var collection = makeLiveElementCollection(function () {
        return formControlIdsForForm(self);
      }, HTMLFormControlsCollection.prototype);
      FORM_ELEMENTS_CACHE.set(this, collection);
      return collection;
    },
    configurable: true,
  });

  HTMLFormElement.prototype.submit = function () {
    nodeIdFromThis(this);
    // No-op: the WPT DOM runner does not currently model navigation/form submission.
  };
  HTMLFormElement.prototype.reset = function () {
    nodeIdFromThis(this);
    // No-op: in this shim form control state is reflected to attributes/textContent directly.
  };

  Object.defineProperty(Element.prototype, "outerHTML", {
    get: function () {
      return g.__fastrender_dom_get_outer_html(nodeIdFromThis(this));
    },
    set: function (html) {
      var id = nodeIdFromThis(this);
      var parentId = g.__fastrender_dom_get_parent_node(id);
      g.__fastrender_dom_set_outer_html(id, String(html));
      if (typeof parentId === "number") {
        syncTree(nodeFromId(parentId));
      }
      this.parentNode = null;
    },
    configurable: true,
  });

  Element.prototype.matches = function (selectors) {
    nodeIdFromThis(this);
    if (!(this instanceof Element)) throw new TypeError("Illegal invocation");
    var parsed = parseSelectorList(selectors);
    for (var i = 0; i < parsed.length; i++) {
      if (matchesSelectorChain(this, parsed[i], this, null)) return true;
    }
    return false;
  };

  Element.prototype.closest = function (selectors) {
    nodeIdFromThis(this);
    if (!(this instanceof Element)) throw new TypeError("Illegal invocation");
    var parsed = parseSelectorList(selectors);
    var cur = this;
    while (cur && cur !== g.document) {
      if (cur instanceof Element) {
        for (var i = 0; i < parsed.length; i++) {
          if (matchesSelectorChain(cur, parsed[i], this, null)) return cur;
        }
      }
      cur = cur.parentNode;
    }
    return null;
  };

  Element.prototype.querySelector = function (selectors) {
    nodeIdFromThis(this);
    if (!(this instanceof Element)) throw new TypeError("Illegal invocation");
    var parsed = parseSelectorList(selectors);

    for (var i = 0; i < parsed.length; i++) {
      if (parsed[i].hasScope && matchesSelectorChain(this, parsed[i], this, this)) return this;
    }

    var found = null;
    traverseElementSubtree(this, function (el) {
      if (found) return;
      for (var i = 0; i < parsed.length; i++) {
        if (matchesSelectorChain(el, parsed[i], this, this)) {
          found = el;
          return;
        }
      }
    }.bind(this));
    return found;
  };

  Element.prototype.querySelectorAll = function (selectors) {
    nodeIdFromThis(this);
    if (!(this instanceof Element)) throw new TypeError("Illegal invocation");
    var parsed = parseSelectorList(selectors);
    var out = [];
    var seen = new Set();

    for (var i = 0; i < parsed.length; i++) {
      if (parsed[i].hasScope && matchesSelectorChain(this, parsed[i], this, this)) {
        var id = nodeIdFromThis(this);
        if (!seen.has(id)) {
          seen.add(id);
          out.push(this);
        }
      }
    }

    traverseElementSubtree(this, function (el) {
      var id = nodeIdFromThis(el);
      if (seen.has(id)) return;
      for (var i = 0; i < parsed.length; i++) {
        if (matchesSelectorChain(el, parsed[i], this, this)) {
          seen.add(id);
          out.push(el);
          return;
        }
      }
    }.bind(this));

    return out;
  };

  Document.prototype.querySelector = function (selectors) {
    return g.document.documentElement.querySelector(selectors);
  };
  Document.prototype.querySelectorAll = function (selectors) {
    return g.document.documentElement.querySelectorAll(selectors);
  };

  Node.prototype.appendChild = function (child) {
    var parentId = nodeIdFromThis(this);
    if (typeof child !== "object" || child === null) {
      throw new TypeError("Failed to execute 'appendChild' on 'Node': parameter 1 is not of type 'Node'.");
    }
    var childId = child[NODE_ID];
    if (typeof childId !== "number") {
      throw new TypeError("Failed to execute 'appendChild' on 'Node': parameter 1 is not of type 'Node'.");
    }

    // Keep JS-level pointers/arrays in sync for the tiny smoke corpus. We do not attempt to fully
    // mirror the Rust DOM.
    if (child instanceof DocumentFragment) {
      g.__fastrender_dom_append_child(parentId, childId);

      var parentNodes = ensureArray(this, "childNodes");
      var fragNodes = ensureArray(child, "childNodes");
      var moved = fragNodes.slice();
      for (var i = 0; i < moved.length; i++) {
        var n = moved[i];
        detachFromParent(n);
        parentNodes.push(n);
        n.parentNode = this;
      }
      fragNodes.length = 0;
      return child;
    }

    g.__fastrender_dom_append_child(parentId, childId);

    detachFromParent(child);
    var nodes = ensureArray(this, "childNodes");
    nodes.push(child);
    child.parentNode = this;
    return child;
  };

  Node.prototype.insertBefore = function (child, reference) {
    var parentId = nodeIdFromThis(this);
    if (typeof child !== "object" || child === null) {
      throw new TypeError("Failed to execute 'insertBefore' on 'Node': parameter 1 is not of type 'Node'.");
    }
    var childId = child[NODE_ID];
    if (typeof childId !== "number") {
      throw new TypeError("Failed to execute 'insertBefore' on 'Node': parameter 1 is not of type 'Node'.");
    }

    var referenceNode = null;
    var referenceId = -1;
    if (reference !== null && reference !== undefined) {
      if (typeof reference !== "object" || reference === null) {
        throw new TypeError("Failed to execute 'insertBefore' on 'Node': parameter 2 is not of type 'Node'.");
      }
      referenceId = reference[NODE_ID];
      if (typeof referenceId !== "number") {
        throw new TypeError("Failed to execute 'insertBefore' on 'Node': parameter 2 is not of type 'Node'.");
      }
      referenceNode = reference;
    }

    // Inserting a node before itself is a no-op.
    if (referenceNode === child) {
      var siblings = ensureArray(this, "childNodes");
      var idx = siblings.indexOf(child);
      if (idx >= 0) {
        if (idx + 1 < siblings.length) {
          referenceNode = siblings[idx + 1];
          referenceId = referenceNode[NODE_ID];
        } else {
          referenceNode = null;
          referenceId = -1;
        }
      }
    }

    g.__fastrender_dom_insert_before(parentId, childId, referenceId);

    var parentNodes = ensureArray(this, "childNodes");
    var insertIdx = referenceNode ? parentNodes.indexOf(referenceNode) : parentNodes.length;
    if (insertIdx < 0) insertIdx = parentNodes.length;

    if (child instanceof DocumentFragment) {
      var fragNodes = ensureArray(child, "childNodes");
      var moved = fragNodes.slice();
      for (var i = 0; i < moved.length; i++) {
        var n = moved[i];
        detachFromParent(n);
        parentNodes.splice(insertIdx + i, 0, n);
        n.parentNode = this;
      }
      fragNodes.length = 0;
      return child;
    }

    var oldParent = child.parentNode;
    var oldIdx = oldParent === this ? parentNodes.indexOf(child) : -1;
    detachFromParent(child);
    if (oldParent === this && oldIdx >= 0 && oldIdx < insertIdx) {
      insertIdx -= 1;
    }
    parentNodes.splice(insertIdx, 0, child);
    child.parentNode = this;
    return child;
  };

  Node.prototype.replaceChild = function (child, oldChild) {
    var parentId = nodeIdFromThis(this);
    if (typeof child !== "object" || child === null) {
      throw new TypeError("Failed to execute 'replaceChild' on 'Node': parameter 1 is not of type 'Node'.");
    }
    var childId = child[NODE_ID];
    if (typeof childId !== "number") {
      throw new TypeError("Failed to execute 'replaceChild' on 'Node': parameter 1 is not of type 'Node'.");
    }
    if (typeof oldChild !== "object" || oldChild === null) {
      throw new TypeError("Failed to execute 'replaceChild' on 'Node': parameter 2 is not of type 'Node'.");
    }
    var oldId = oldChild[NODE_ID];
    if (typeof oldId !== "number") {
      throw new TypeError("Failed to execute 'replaceChild' on 'Node': parameter 2 is not of type 'Node'.");
    }

    if (child === oldChild) return oldChild;

    g.__fastrender_dom_replace_child(parentId, childId, oldId);

    var parentNodes = ensureArray(this, "childNodes");
    var idx = parentNodes.indexOf(oldChild);
    if (idx < 0) idx = 0;

    if (child instanceof DocumentFragment) {
      var fragNodes = ensureArray(child, "childNodes");
      var moved = fragNodes.slice();
      for (var i = 0; i < moved.length; i++) {
        var n = moved[i];
        detachFromParent(n);
        parentNodes.splice(idx + i, 0, n);
        n.parentNode = this;
      }
      fragNodes.length = 0;
      detachFromParent(oldChild);
      return oldChild;
    }

    detachFromParent(child);
    idx = parentNodes.indexOf(oldChild);
    if (idx < 0) idx = 0;
    parentNodes.splice(idx, 1, child);
    oldChild.parentNode = null;
    child.parentNode = this;
    return oldChild;
  };

  Node.prototype.removeChild = function (child) {
    var parentId = nodeIdFromThis(this);
    if (typeof child !== "object" || child === null) {
      throw new TypeError("Failed to execute 'removeChild' on 'Node': parameter 1 is not of type 'Node'.");
    }
    var childId = child[NODE_ID];
    if (typeof childId !== "number") {
      throw new TypeError("Failed to execute 'removeChild' on 'Node': parameter 1 is not of type 'Node'.");
    }
    g.__fastrender_dom_remove_child(parentId, childId);
    detachFromParent(child);
    return child;
  };

  Node.prototype.remove = function () {
    nodeIdFromThis(this);
    if (!this.parentNode) return;
    this.parentNode.removeChild(this);
  };

  Node.prototype.hasChildNodes = function () {
    return g.__fastrender_dom_has_child_nodes(nodeIdFromThis(this));
  };

  Node.prototype.contains = function (other) {
    nodeIdFromThis(this);
    if (other === null || other === undefined) return false;
    if (typeof other !== "object" || other === null) {
      throw new TypeError("Failed to execute 'contains' on 'Node': parameter 1 is not of type 'Node'.");
    }
    var otherId = other[NODE_ID];
    if (typeof otherId !== "number") {
      throw new TypeError("Failed to execute 'contains' on 'Node': parameter 1 is not of type 'Node'.");
    }
    var cur = other;
    while (cur) {
      if (cur === this) return true;
      cur = cur.parentNode;
    }
    return false;
  };

  Object.defineProperty(Node.prototype, "nodeType", {
    get: function () {
      nodeIdFromThis(this);
      if (this === g.document) return Node.DOCUMENT_NODE;
      if (this instanceof DocumentFragment) return Node.DOCUMENT_FRAGMENT_NODE;
      if (this instanceof Element) return Node.ELEMENT_NODE;
      if (this instanceof Text) return Node.TEXT_NODE;
      return 0;
    },
    configurable: true,
  });

  Object.defineProperty(Node.prototype, "nodeName", {
    get: function () {
      var t = this.nodeType;
      if (t === Node.ELEMENT_NODE) return this.tagName;
      if (t === Node.TEXT_NODE) return "#text";
      if (t === Node.DOCUMENT_NODE) return "#document";
      if (t === Node.DOCUMENT_FRAGMENT_NODE) return "#document-fragment";
      return "";
    },
    configurable: true,
  });

  Object.defineProperty(Node.prototype, "isConnected", {
    get: function () {
      nodeIdFromThis(this);
      return g.document.contains(this);
    },
    configurable: true,
  });

  Object.defineProperty(Node.prototype, "ownerDocument", {
    get: function () {
      nodeIdFromThis(this);
      return this === g.document ? null : g.document;
    },
    configurable: true,
  });

  Object.defineProperty(Node.prototype, "firstChild", {
    get: function () {
      nodeIdFromThis(this);
      var nodes = this.childNodes || [];
      return nodes.length ? nodes[0] : null;
    },
    configurable: true,
  });
 
  Object.defineProperty(Node.prototype, "lastChild", {
    get: function () {
      nodeIdFromThis(this);
      var nodes = this.childNodes || [];
      return nodes.length ? nodes[nodes.length - 1] : null;
    },
    configurable: true,
  });
 
  Object.defineProperty(Node.prototype, "previousSibling", {
    get: function () {
      nodeIdFromThis(this);
      var parent = this.parentNode;
      if (!parent) return null;
      var siblings = parent.childNodes || [];
      var idx = siblings.indexOf(this);
      if (idx <= 0) return null;
      return siblings[idx - 1] || null;
    },
    configurable: true,
  });
 
  Object.defineProperty(Node.prototype, "nextSibling", {
    get: function () {
      nodeIdFromThis(this);
      var parent = this.parentNode;
      if (!parent) return null;
      var siblings = parent.childNodes || [];
      var idx = siblings.indexOf(this);
      if (idx < 0 || idx >= siblings.length - 1) return null;
      return siblings[idx + 1] || null;
    },
    configurable: true,
  });
 
  Object.defineProperty(Node.prototype, "textContent", {
    get: function () {
      var v = g.__fastrender_dom_get_text_content(nodeIdFromThis(this));
      // Align with DOM: Document.textContent is null. Also treat `undefined` as null in case the
      // host hook returns it.
      if (v === undefined) return null;
      return v;
    },
    set: function (value) {
      nodeIdFromThis(this);
      if (this === g.document) return;
      var created = g.__fastrender_dom_set_text_content(nodeIdFromThis(this), String(value));
      if (!this.childNodes) this.childNodes = [];
      for (var i = 0; i < this.childNodes.length; i++) {
        var n = this.childNodes[i];
        if (n && typeof n === "object") n.parentNode = null;
      }
      this.childNodes.length = 0;
      if (typeof created === "number") {
        var text = makeNode(Text.prototype, created);
        text.parentNode = this;
        this.childNodes.push(text);
      }
    },
    configurable: true,
  });

  // Provide `document.head`/`document.body` for smoke tests.
  if (typeof g.__fastrender_dom_head_id === "number") {
    g.document.head = makeNode(elementPrototypeForTag("head"), g.__fastrender_dom_head_id, "HEAD");
  }
  if (typeof g.__fastrender_dom_body_id === "number") {
    g.document.body = makeNode(elementPrototypeForTag("body"), g.__fastrender_dom_body_id, "BODY");
  }
  if (typeof g.__fastrender_dom_document_element_id === "number") {
    g.document.documentElement = makeNode(
      elementPrototypeForTag("html"),
      g.__fastrender_dom_document_element_id,
      "HTML"
    );
    g.document.documentElement.parentNode = g.document;
    g.document.childNodes.push(g.document.documentElement);
    if (g.document.head) {
      g.document.head.parentNode = g.document.documentElement;
      g.document.documentElement.childNodes.push(g.document.head);
    }
    if (g.document.body) {
      g.document.body.parentNode = g.document.documentElement;
      g.document.documentElement.childNodes.push(g.document.body);
    }
  } else {
    if (g.document.head) g.document.head.parentNode = g.document;
    if (g.document.body) g.document.body.parentNode = g.document;
  }

  Document.prototype.getElementsByTagName = function (qualifiedName) {
    var rootId = nodeIdFromThis(this);
    var q = String(qualifiedName);
    return makeLiveElementCollection(function () {
      return g.__fastrender_dom_get_elements_by_tag_name(rootId, q);
    });
  };
  Element.prototype.getElementsByTagName = Document.prototype.getElementsByTagName;

  Document.prototype.getElementsByTagNameNS = function (namespace, localName) {
    var rootId = nodeIdFromThis(this);
    var ns = (namespace === null || namespace === undefined) ? null : String(namespace);
    var ln = String(localName);
    return makeLiveElementCollection(function () {
      return g.__fastrender_dom_get_elements_by_tag_name_ns(rootId, ns, ln);
    });
  };
  Element.prototype.getElementsByTagNameNS = Document.prototype.getElementsByTagNameNS;

  Document.prototype.getElementsByClassName = function (classNames) {
    var rootId = nodeIdFromThis(this);
    var cls = String(classNames);
    return makeLiveElementCollection(function () {
      return g.__fastrender_dom_get_elements_by_class_name(rootId, cls);
    });
  };
  Element.prototype.getElementsByClassName = Document.prototype.getElementsByClassName;

  Document.prototype.getElementsByName = function (name) {
    if (this !== g.document) {
      throw new TypeError("Illegal invocation");
    }
    var n = String(name);
    return makeLiveElementCollection(function () {
      return g.__fastrender_dom_get_elements_by_name(n);
    });
  };

  Object.defineProperty(g, "Node", { value: Node, configurable: true, writable: true });
  Object.defineProperty(g, "Document", { value: Document, configurable: true, writable: true });
  Object.defineProperty(g, "DocumentFragment", { value: DocumentFragment, configurable: true, writable: true });
  Object.defineProperty(g, "Element", { value: Element, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLElement", { value: HTMLElement, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLInputElement", { value: HTMLInputElement, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLTextAreaElement", { value: HTMLTextAreaElement, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLSelectElement", { value: HTMLSelectElement, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLFormElement", { value: HTMLFormElement, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLOptionElement", { value: HTMLOptionElement, configurable: true, writable: true });
  Object.defineProperty(g, "Text", { value: Text, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLCollection", { value: HTMLCollection, configurable: true, writable: true });
  Object.defineProperty(g, "CSSStyleDeclaration", { value: CSSStyleDeclaration, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLOptionsCollection", { value: HTMLOptionsCollection, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLFormControlsCollection", { value: HTMLFormControlsCollection, configurable: true, writable: true });
  Object.defineProperty(g, "EventTarget", { value: EventTarget, configurable: true, writable: true });
  Object.defineProperty(g, "Event", { value: Event, configurable: true, writable: true });

  // Allow using window as an EventTarget for DOM-style event paths.
  Object.defineProperty(g, "addEventListener", { value: EventTarget.prototype.addEventListener, configurable: true, writable: true });
  Object.defineProperty(g, "removeEventListener", { value: EventTarget.prototype.removeEventListener, configurable: true, writable: true });
  Object.defineProperty(g, "dispatchEvent", { value: EventTarget.prototype.dispatchEvent, configurable: true, writable: true });
})();
"##;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DomShimError {
  HierarchyRequestError,
  NotFoundError,
  InvalidNodeType,
}

impl DomShimError {
  fn code(self) -> &'static str {
    match self {
      DomShimError::HierarchyRequestError => "HierarchyRequestError",
      DomShimError::NotFoundError => "NotFoundError",
      DomShimError::InvalidNodeType => "InvalidNodeType",
    }
  }
}

fn dom_error_to_js_error(err: DomShimError) -> rquickjs::Error {
  rquickjs::Error::new_from_js_message("DOMException", "DOMException", err.code())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NodeId(usize);

#[derive(Debug, Clone, PartialEq, Eq)]
enum NodeKind {
  Document,
  DocumentFragment,
  Element {
    tag_name: String,
    attributes: Vec<(String, String)>,
  },
  Text {
    content: String,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
  kind: NodeKind,
  parent: Option<NodeId>,
  children: Vec<NodeId>,
}

#[derive(Debug, Clone)]
struct Dom {
  nodes: Vec<Node>,
  html: NodeId,
  head: NodeId,
  body: NodeId,
}

impl Dom {
  fn normalize_attr_name(name: &str) -> String {
    name.to_ascii_lowercase()
  }

  fn new() -> Self {
    let mut dom = Self {
      nodes: Vec::new(),
      html: NodeId(0),
      head: NodeId(0),
      body: NodeId(0),
    };
    let document = dom.push_node(NodeKind::Document, None);
    debug_assert_eq!(document, NodeId(0));

    let html = dom.create_element("html");
    let head = dom.create_element("head");
    let body = dom.create_element("body");

    dom
      .append_child(document, html)
      .expect("document should accept <html>");
    dom
      .append_child(html, head)
      .expect("<html> should accept <head>");
    dom
      .append_child(html, body)
      .expect("<html> should accept <body>");

    dom.html = html;
    dom.head = head;
    dom.body = body;

    dom
  }

  fn document_element(&self) -> NodeId {
    self.html
  }

  fn head(&self) -> NodeId {
    self.head
  }

  fn body(&self) -> NodeId {
    self.body
  }

  fn node_checked(&self, id: NodeId) -> Result<&Node, DomShimError> {
    self.nodes.get(id.0).ok_or(DomShimError::NotFoundError)
  }

  fn node_checked_mut(&mut self, id: NodeId) -> Result<&mut Node, DomShimError> {
    self.nodes.get_mut(id.0).ok_or(DomShimError::NotFoundError)
  }

  fn push_node(&mut self, kind: NodeKind, parent: Option<NodeId>) -> NodeId {
    let id = NodeId(self.nodes.len());
    self.nodes.push(Node {
      kind,
      parent,
      children: Vec::new(),
    });
    if let Some(parent) = parent {
      if parent.0 < self.nodes.len() {
        self.nodes[parent.0].children.push(id);
      }
    }
    id
  }

  fn create_element(&mut self, tag_name: &str) -> NodeId {
    self.push_node(
      NodeKind::Element {
        tag_name: tag_name.to_ascii_lowercase(),
        attributes: Vec::new(),
      },
      None,
    )
  }

  fn create_text(&mut self, content: &str, parent: Option<NodeId>) -> NodeId {
    self.push_node(
      NodeKind::Text {
        content: content.to_string(),
      },
      parent,
    )
  }

  fn create_text_node(&mut self, data: &str) -> NodeId {
    self.create_text(data, None)
  }

  fn create_document_fragment(&mut self) -> NodeId {
    self.push_node(NodeKind::DocumentFragment, None)
  }

  fn detach_from_parent(&mut self, child: NodeId) -> Result<(), DomShimError> {
    let old_parent = self.node_checked(child)?.parent;
    let Some(old_parent) = old_parent else {
      return Ok(());
    };
    let parent_children = &mut self.node_checked_mut(old_parent)?.children;
    let idx = parent_children
      .iter()
      .position(|&id| id == child)
      .ok_or(DomShimError::NotFoundError)?;
    parent_children.remove(idx);
    self.node_checked_mut(child)?.parent = None;
    Ok(())
  }

  fn validate_parent_can_have_children(&self, parent: NodeId) -> Result<(), DomShimError> {
    match &self.node_checked(parent)?.kind {
      NodeKind::Text { .. } => Err(DomShimError::HierarchyRequestError),
      _ => Ok(()),
    }
  }

  fn validate_no_cycles(&self, parent: NodeId, child: NodeId) -> Result<(), DomShimError> {
    if parent == child {
      return Err(DomShimError::HierarchyRequestError);
    }
    // Walk up from `parent` and ensure we never reach `child` (which would make `parent` a
    // descendant of `child`).
    let mut current = Some(parent);
    for _ in 0..=self.nodes.len() {
      let Some(id) = current else {
        return Ok(());
      };
      if id == child {
        return Err(DomShimError::HierarchyRequestError);
      }
      current = self.nodes.get(id.0).and_then(|node| node.parent);
    }
    // If we exceed `nodes.len()` steps we likely have a pre-existing cycle; reject insertion.
    Err(DomShimError::HierarchyRequestError)
  }

  fn has_child_nodes(&self, node: NodeId) -> Result<bool, DomShimError> {
    Ok(!self.node_checked(node)?.children.is_empty())
  }

  fn get_node_type(&self, node: NodeId) -> Result<i32, DomShimError> {
    self.node_checked(node)?;
    let value = match &self.nodes[node.0].kind {
      NodeKind::Document => 9,
      NodeKind::DocumentFragment => 11,
      NodeKind::Element { .. } => 1,
      NodeKind::Text { .. } => 3,
    };
    Ok(value)
  }

  fn get_parent_node(&self, node: NodeId) -> Result<Option<NodeId>, DomShimError> {
    Ok(self.node_checked(node)?.parent)
  }

  fn get_child_nodes(&self, node: NodeId) -> Result<Vec<NodeId>, DomShimError> {
    Ok(self.node_checked(node)?.children.clone())
  }

  fn append_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), DomShimError> {
    self.node_checked(parent)?;
    self.node_checked(child)?;
    self.validate_parent_can_have_children(parent)?;

    if matches!(self.node_checked(child)?.kind, NodeKind::Document) {
      return Err(DomShimError::InvalidNodeType);
    }

    if matches!(self.node_checked(child)?.kind, NodeKind::DocumentFragment) {
      // DocumentFragment insertion semantics: move its children into `parent` and empty the
      // fragment.
      let fragment_children = self.node_checked(child)?.children.clone();
      for &moved in &fragment_children {
        if matches!(self.node_checked(moved)?.kind, NodeKind::Document) {
          return Err(DomShimError::InvalidNodeType);
        }
        self.validate_no_cycles(parent, moved)?;
      }

      let fragment_children = std::mem::take(&mut self.node_checked_mut(child)?.children);
      for moved in fragment_children {
        self.node_checked_mut(moved)?.parent = Some(parent);
        self.node_checked_mut(parent)?.children.push(moved);
      }
      // Fragments are never inserted into the tree.
      self.node_checked_mut(child)?.parent = None;
      return Ok(());
    }

    self.validate_no_cycles(parent, child)?;

    if self.node_checked(child)?.parent.is_some() {
      self.detach_from_parent(child)?;
    }

    self.node_checked_mut(child)?.parent = Some(parent);
    self.node_checked_mut(parent)?.children.push(child);
    Ok(())
  }

  fn insert_before(
    &mut self,
    parent: NodeId,
    child: NodeId,
    reference: Option<NodeId>,
  ) -> Result<(), DomShimError> {
    self.node_checked(parent)?;
    self.node_checked(child)?;
    self.validate_parent_can_have_children(parent)?;

    if matches!(self.node_checked(child)?.kind, NodeKind::Document) {
      return Err(DomShimError::InvalidNodeType);
    }

    let mut reference = reference;
    if let Some(reference_id) = reference {
      self.node_checked(reference_id)?;
      if self.node_checked(reference_id)?.parent != Some(parent) {
        return Err(DomShimError::NotFoundError);
      }

      // Inserting a node before itself is a no-op.
      if reference_id == child {
        if self.node_checked(child)?.parent != Some(parent) {
          return Err(DomShimError::NotFoundError);
        }
        let siblings = self.node_checked(parent)?.children.clone();
        if let Some(idx) = siblings.iter().position(|&id| id == child) {
          reference = siblings.get(idx + 1).copied();
        } else {
          reference = None;
        }
      }
    }

    let mut insert_idx = match reference {
      None => self.node_checked(parent)?.children.len(),
      Some(reference) => self
        .node_checked(parent)?
        .children
        .iter()
        .position(|&id| id == reference)
        .ok_or(DomShimError::NotFoundError)?,
    };

    if matches!(self.node_checked(child)?.kind, NodeKind::DocumentFragment) {
      // DocumentFragment insertion semantics: move its children into `parent` and empty the
      // fragment.
      let fragment_children = self.node_checked(child)?.children.clone();
      for &moved in &fragment_children {
        if matches!(self.node_checked(moved)?.kind, NodeKind::Document) {
          return Err(DomShimError::InvalidNodeType);
        }
        self.validate_no_cycles(parent, moved)?;
      }

      let fragment_children = std::mem::take(&mut self.node_checked_mut(child)?.children);
      for &moved in &fragment_children {
        self.node_checked_mut(moved)?.parent = Some(parent);
      }

      let parent_children = &mut self.node_checked_mut(parent)?.children;
      parent_children.splice(insert_idx..insert_idx, fragment_children.iter().copied());
      // Fragments are never inserted into the tree.
      self.node_checked_mut(child)?.parent = None;
      return Ok(());
    }

    self.validate_no_cycles(parent, child)?;

    let old_parent = self.node_checked(child)?.parent;
    if old_parent.is_some() {
      if old_parent == Some(parent) {
        let siblings = self.node_checked(parent)?.children.clone();
        if let Some(idx) = siblings.iter().position(|&id| id == child) {
          if idx < insert_idx {
            insert_idx = insert_idx.saturating_sub(1);
          }
        }
      }
      self.detach_from_parent(child)?;
    }

    self.node_checked_mut(child)?.parent = Some(parent);
    self
      .node_checked_mut(parent)?
      .children
      .insert(insert_idx, child);
    Ok(())
  }

  fn replace_child(
    &mut self,
    parent: NodeId,
    new_child: NodeId,
    old_child: NodeId,
  ) -> Result<(), DomShimError> {
    self.node_checked(parent)?;
    self.node_checked(new_child)?;
    self.node_checked(old_child)?;
    self.validate_parent_can_have_children(parent)?;

    if self.node_checked(old_child)?.parent != Some(parent) {
      return Err(DomShimError::NotFoundError);
    }
    if new_child == old_child {
      return Ok(());
    }

    if matches!(self.node_checked(new_child)?.kind, NodeKind::Document) {
      return Err(DomShimError::InvalidNodeType);
    }

    let mut replace_idx = self
      .node_checked(parent)?
      .children
      .iter()
      .position(|&id| id == old_child)
      .ok_or(DomShimError::NotFoundError)?;

    if matches!(
      self.node_checked(new_child)?.kind,
      NodeKind::DocumentFragment
    ) {
      let fragment_children = self.node_checked(new_child)?.children.clone();
      for &moved in &fragment_children {
        if matches!(self.node_checked(moved)?.kind, NodeKind::Document) {
          return Err(DomShimError::InvalidNodeType);
        }
        self.validate_no_cycles(parent, moved)?;
      }

      let fragment_children = std::mem::take(&mut self.node_checked_mut(new_child)?.children);
      for &moved in &fragment_children {
        self.node_checked_mut(moved)?.parent = Some(parent);
      }

      // Splice the fragment children into the parent's list, removing the replaced node.
      self.node_checked_mut(parent)?.children.splice(
        replace_idx..replace_idx + 1,
        fragment_children.iter().copied(),
      );
      self.node_checked_mut(old_child)?.parent = None;
      self.node_checked_mut(new_child)?.parent = None;
      return Ok(());
    }

    self.validate_no_cycles(parent, new_child)?;

    let old_parent = self.node_checked(new_child)?.parent;
    if old_parent.is_some() {
      if old_parent == Some(parent) {
        let siblings = self.node_checked(parent)?.children.clone();
        if let Some(idx) = siblings.iter().position(|&id| id == new_child) {
          if idx < replace_idx {
            replace_idx = replace_idx.saturating_sub(1);
          }
        }
      }
      self.detach_from_parent(new_child)?;
    }

    self.node_checked_mut(new_child)?.parent = Some(parent);
    self.node_checked_mut(old_child)?.parent = None;
    self.node_checked_mut(parent)?.children[replace_idx] = new_child;
    Ok(())
  }

  fn remove_child(&mut self, parent: NodeId, child: NodeId) -> Result<(), DomShimError> {
    self.node_checked(parent)?;
    self.node_checked(child)?;

    if self.node_checked(child)?.parent != Some(parent) {
      return Err(DomShimError::NotFoundError);
    }
    let parent_children = &mut self.node_checked_mut(parent)?.children;
    let idx = parent_children
      .iter()
      .position(|&id| id == child)
      .ok_or(DomShimError::NotFoundError)?;
    parent_children.remove(idx);
    self.node_checked_mut(child)?.parent = None;
    Ok(())
  }

  fn get_inner_html(&self, element: NodeId) -> Result<String, DomShimError> {
    match &self.node_checked(element)?.kind {
      NodeKind::Element { .. } => {}
      _ => return Err(DomShimError::InvalidNodeType),
    }
    let node = self.node_checked(element)?;
    let mut out = String::new();
    for &child in &node.children {
      self.serialize_node(child, &mut out)?;
    }
    Ok(out)
  }

  fn set_inner_html(&mut self, element: NodeId, html: &str) -> Result<(), DomShimError> {
    let tag_name = match &self.node_checked(element)?.kind {
      NodeKind::Element { tag_name, .. } => tag_name.clone(),
      _ => return Err(DomShimError::InvalidNodeType),
    };

    let new_children = self.parse_html_fragment(&tag_name, html);

    let old_children = std::mem::take(&mut self.node_checked_mut(element)?.children);
    for child in old_children {
      if child.0 < self.nodes.len() {
        self.node_checked_mut(child)?.parent = None;
      }
    }

    for &child in &new_children {
      self.node_checked_mut(child)?.parent = Some(element);
    }
    self.node_checked_mut(element)?.children = new_children;

    Ok(())
  }

  fn get_outer_html(&self, element: NodeId) -> Result<String, DomShimError> {
    match &self.node_checked(element)?.kind {
      NodeKind::Element { .. } => {}
      _ => return Err(DomShimError::InvalidNodeType),
    }
    let mut out = String::new();
    self.serialize_node(element, &mut out)?;
    Ok(out)
  }

  fn set_outer_html(&mut self, element: NodeId, html: &str) -> Result<(), DomShimError> {
    let Some(parent) = self.node_checked(element)?.parent else {
      // Spec: if the element has no parent, `outerHTML = ...` is a no-op.
      return Ok(());
    };

    // When possible, use the parent element tag name as the HTML fragment parsing context. For
    // non-element parents (Document / DocumentFragment) fall back to a neutral `<div>` context.
    let parent_tag = match &self.node_checked(parent)?.kind {
      NodeKind::Element { tag_name, .. } => tag_name.clone(),
      NodeKind::Document | NodeKind::DocumentFragment => "div".to_string(),
      NodeKind::Text { .. } => return Err(DomShimError::HierarchyRequestError),
    };

    let replacement_idx = self
      .node_checked(parent)?
      .children
      .iter()
      .position(|&id| id == element)
      .ok_or(DomShimError::NotFoundError)?;

    let new_nodes = self.parse_html_fragment(&parent_tag, html);

    // Detach the replaced element.
    self.node_checked_mut(element)?.parent = None;

    // Insert new nodes, then remove the old one.
    let parent_children = &mut self.node_checked_mut(parent)?.children;
    parent_children.splice(
      replacement_idx..replacement_idx + 1,
      new_nodes.iter().copied(),
    );
    for node_id in new_nodes {
      self.node_checked_mut(node_id)?.parent = Some(parent);
    }

    Ok(())
  }

  fn get_attribute(&self, element: NodeId, name: &str) -> Result<Option<String>, DomShimError> {
    let name = Self::normalize_attr_name(name);
    let node = self.node_checked(element)?;
    let NodeKind::Element { attributes, .. } = &node.kind else {
      return Err(DomShimError::InvalidNodeType);
    };
    Ok(
      attributes
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(&name))
        .map(|(_, v)| v.clone()),
    )
  }

  fn set_attribute(
    &mut self,
    element: NodeId,
    name: &str,
    value: &str,
  ) -> Result<(), DomShimError> {
    let name = Self::normalize_attr_name(name);
    let node = self.node_checked_mut(element)?;
    let NodeKind::Element { attributes, .. } = &mut node.kind else {
      return Err(DomShimError::InvalidNodeType);
    };

    if let Some((_n, v)) = attributes
      .iter_mut()
      .find(|(n, _)| n.eq_ignore_ascii_case(&name))
    {
      v.clear();
      v.push_str(value);
    } else {
      attributes.push((name, value.to_string()));
    }
    Ok(())
  }

  fn remove_attribute(&mut self, element: NodeId, name: &str) -> Result<(), DomShimError> {
    let name = Self::normalize_attr_name(name);
    let node = self.node_checked_mut(element)?;
    let NodeKind::Element { attributes, .. } = &mut node.kind else {
      return Err(DomShimError::InvalidNodeType);
    };
    if let Some(idx) = attributes
      .iter()
      .position(|(n, _)| n.eq_ignore_ascii_case(&name))
    {
      attributes.remove(idx);
    }
    Ok(())
  }

  fn get_text_data(&self, node: NodeId) -> Result<String, DomShimError> {
    let node = self.node_checked(node)?;
    let NodeKind::Text { content } = &node.kind else {
      return Err(DomShimError::InvalidNodeType);
    };
    Ok(content.clone())
  }

  fn set_text_data(&mut self, node: NodeId, data: &str) -> Result<(), DomShimError> {
    let node = self.node_checked_mut(node)?;
    let NodeKind::Text { content } = &mut node.kind else {
      return Err(DomShimError::InvalidNodeType);
    };
    content.clear();
    content.push_str(data);
    Ok(())
  }

  fn clear_children(&mut self, parent: NodeId) -> Result<(), DomShimError> {
    self.node_checked(parent)?;
    let old_children = std::mem::take(&mut self.node_checked_mut(parent)?.children);
    for child in old_children {
      if child.0 < self.nodes.len() {
        self.node_checked_mut(child)?.parent = None;
      }
    }
    Ok(())
  }

  fn get_text_content(&self, node: NodeId) -> Result<Option<String>, DomShimError> {
    self.node_checked(node)?;
    match &self.nodes[node.0].kind {
      NodeKind::Document => return Ok(None),
      NodeKind::Text { content } => return Ok(Some(content.clone())),
      NodeKind::Element { .. } | NodeKind::DocumentFragment => {}
    }

    let mut out = String::new();
    let mut stack: Vec<NodeId> = self.nodes[node.0].children.iter().copied().rev().collect();
    while let Some(id) = stack.pop() {
      let node = self.node_checked(id)?;
      match &node.kind {
        NodeKind::Text { content } => out.push_str(content),
        NodeKind::Element { .. } | NodeKind::DocumentFragment | NodeKind::Document => {
          for &child in node.children.iter().rev() {
            stack.push(child);
          }
        }
      }
    }

    Ok(Some(out))
  }

  fn set_text_content(&mut self, node: NodeId, data: &str) -> Result<Option<NodeId>, DomShimError> {
    self.node_checked(node)?;
    match &self.nodes[node.0].kind {
      NodeKind::Document => return Ok(None),
      NodeKind::Text { .. } => {
        self.set_text_data(node, data)?;
        return Ok(None);
      }
      NodeKind::Element { .. } | NodeKind::DocumentFragment => {}
    }

    self.clear_children(node)?;
    if data.is_empty() {
      return Ok(None);
    }
    let text = self.create_text(data, Some(node));
    Ok(Some(text))
  }

  fn get_tag_name(&self, node: NodeId) -> Result<String, DomShimError> {
    let node = self.node_checked(node)?;
    let NodeKind::Element { tag_name, .. } = &node.kind else {
      return Err(DomShimError::InvalidNodeType);
    };
    Ok(tag_name.clone())
  }

  fn collect_descendant_elements_matching(
    &self,
    root: NodeId,
    mut matches: impl FnMut(&str, &[(String, String)]) -> bool,
  ) -> Result<Vec<NodeId>, DomShimError> {
    let root_node = self.node_checked(root)?;
    if matches!(root_node.kind, NodeKind::Text { .. }) {
      return Err(DomShimError::InvalidNodeType);
    }

    // Clone so we can traverse without holding a borrow on `root_node`.
    let initial_children = root_node.children.clone();

    let mut remaining = self.nodes.len().saturating_add(1);
    let mut out: Vec<NodeId> = Vec::new();
    let mut stack: Vec<(NodeId, NodeId)> = initial_children
      .into_iter()
      .rev()
      .map(|child| (root, child))
      .collect();

    while let Some((expected_parent, node_id)) = stack.pop() {
      if remaining == 0 {
        break;
      }
      remaining -= 1;

      let Some(node) = self.nodes.get(node_id.0) else {
        continue;
      };
      if node.parent != Some(expected_parent) {
        continue;
      }

      let mut descend = true;
      if let NodeKind::Element {
        tag_name,
        attributes,
      } = &node.kind
      {
        if matches(tag_name, attributes) {
          out.push(node_id);
        }
        // Treat `<template>` contents as inert, mirroring real DOM tree traversal.
        if tag_name.eq_ignore_ascii_case("template") {
          descend = false;
        }
      }

      if descend {
        for &child in node.children.iter().rev() {
          stack.push((node_id, child));
        }
      }
    }

    Ok(out)
  }

  fn get_elements_by_tag_name(
    &self,
    root: NodeId,
    qualified_name: &str,
  ) -> Result<Vec<NodeId>, DomShimError> {
    if qualified_name.is_empty() {
      return Ok(Vec::new());
    }
    if qualified_name == "*" {
      return self.collect_descendant_elements_matching(root, |_, _| true);
    }
    let needle = qualified_name.to_ascii_lowercase();
    self.collect_descendant_elements_matching(root, move |tag, _| tag.eq_ignore_ascii_case(&needle))
  }

  fn get_elements_by_tag_name_ns(
    &self,
    root: NodeId,
    namespace: Option<&str>,
    local_name: &str,
  ) -> Result<Vec<NodeId>, DomShimError> {
    if local_name.is_empty() {
      return Ok(Vec::new());
    }

    let namespace_ok = match namespace {
      None => true,
      Some("*") => true,
      Some("") => true,
      Some(HTML_NAMESPACE) => true,
      Some(_) => false,
    };
    if !namespace_ok {
      return Ok(Vec::new());
    }

    if local_name == "*" {
      return self.collect_descendant_elements_matching(root, |_, _| true);
    }
    let needle = local_name.to_ascii_lowercase();
    self.collect_descendant_elements_matching(root, move |tag, _| tag.eq_ignore_ascii_case(&needle))
  }

  fn get_elements_by_class_name(
    &self,
    root: NodeId,
    class_names: &str,
  ) -> Result<Vec<NodeId>, DomShimError> {
    let required = split_dom_ascii_whitespace(class_names);
    if required.is_empty() {
      return Ok(Vec::new());
    }

    self.collect_descendant_elements_matching(root, |_, attrs| {
      let Some(class_attr) = attrs
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("class"))
        .map(|(_, value)| value.as_str())
      else {
        return false;
      };

      let have = split_dom_ascii_whitespace(class_attr);
      required
        .iter()
        .all(|required| have.iter().any(|token| token == required))
    })
  }

  fn get_elements_by_name(&self, root: NodeId, name: &str) -> Result<Vec<NodeId>, DomShimError> {
    self.collect_descendant_elements_matching(root, |_, attrs| {
      attrs
        .iter()
        .find(|(attr_name, _)| attr_name.eq_ignore_ascii_case("name"))
        .is_some_and(|(_, value)| value == name)
    })
  }
  fn serialize_node(&self, root: NodeId, out: &mut String) -> Result<(), DomShimError> {
    enum Frame {
      Enter(NodeId),
      Exit(NodeId),
    }

    let mut stack = vec![Frame::Enter(root)];
    while let Some(frame) = stack.pop() {
      match frame {
        Frame::Enter(id) => {
          let node = self.node_checked(id)?;
          match &node.kind {
            NodeKind::Document | NodeKind::DocumentFragment => {
              for &child in node.children.iter().rev() {
                stack.push(Frame::Enter(child));
              }
            }
            NodeKind::Text { content } => {
              escape_text(out, content);
            }
            NodeKind::Element {
              tag_name,
              attributes,
            } => {
              out.push('<');
              out.push_str(tag_name);
              for (name, value) in attributes {
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                escape_attr_value(out, value);
                out.push('"');
              }
              out.push('>');
              if is_void_html_element(tag_name) {
                continue;
              }
              stack.push(Frame::Exit(id));
              for &child in node.children.iter().rev() {
                stack.push(Frame::Enter(child));
              }
            }
          }
        }
        Frame::Exit(id) => {
          let node = self.node_checked(id)?;
          if let NodeKind::Element { tag_name, .. } = &node.kind {
            if is_void_html_element(tag_name) {
              continue;
            }
            out.push_str("</");
            out.push_str(tag_name);
            out.push('>');
          }
        }
      }
    }
    Ok(())
  }

  fn parse_html_fragment(&mut self, context_tag: &str, html: &str) -> Vec<NodeId> {
    let context = QualName::new(
      None,
      Namespace::from(HTML_NAMESPACE),
      LocalName::from(context_tag.to_ascii_lowercase()),
    );

    let opts = ParseOpts {
      tree_builder: TreeBuilderOpts {
        scripting_enabled: true,
        ..Default::default()
      },
      ..Default::default()
    };

    // `html5ever::parse_fragment` takes `context_element_allows_scripting` as a separate boolean
    // flag (it only affects the tokenizer initial state when the context element is `<noscript>`).
    // Our DOM shims assume JS-enabled parsing semantics, so keep it enabled.
    let rcdom: RcDom = parse_fragment(RcDom::default(), opts, context, Vec::new(), true).one(html);

    let mut roots: Vec<NodeId> = Vec::new();

    #[derive(Clone)]
    struct WorkItem {
      parent: Option<NodeId>,
      handle: Handle,
    }

    let mut stack: Vec<WorkItem> = fragment_children_from_rcdom(&rcdom)
      .into_iter()
      .rev()
      .map(|handle| WorkItem {
        parent: None,
        handle,
      })
      .collect();

    while let Some(item) = stack.pop() {
      match &item.handle.data {
        NodeData::Document => {
          for child in handle_children(&item.handle).into_iter().rev() {
            stack.push(WorkItem {
              parent: item.parent,
              handle: child,
            });
          }
        }
        NodeData::Text { contents } => {
          let content = contents.borrow().to_string();
          let id = self.create_text(&content, item.parent);
          if item.parent.is_none() {
            roots.push(id);
          }
        }
        NodeData::Element {
          name,
          attrs,
          template_contents,
          ..
        } => {
          let attrs_ref = attrs.borrow();
          let mut attributes = Vec::with_capacity(attrs_ref.len());
          for attr in attrs_ref.iter() {
            // Keep this minimal: treat everything as HTML and ignore namespaces/prefixes.
            attributes.push((attr.name.local.to_string(), attr.value.to_string()));
          }

          let id = self.push_node(
            NodeKind::Element {
              tag_name: name.local.to_string(),
              attributes,
            },
            item.parent,
          );
          if item.parent.is_none() {
            roots.push(id);
          }

          let is_template = name.local.as_ref().eq_ignore_ascii_case("template");
          let children = if is_template {
            template_contents
              .borrow()
              .as_ref()
              .map(handle_children)
              .unwrap_or_else(|| handle_children(&item.handle))
          } else {
            handle_children(&item.handle)
          };

          for child in children.into_iter().rev() {
            stack.push(WorkItem {
              parent: Some(id),
              handle: child,
            });
          }
        }
        _ => {}
      }
    }

    roots
  }
}

fn is_void_html_element(tag: &str) -> bool {
  // https://html.spec.whatwg.org/#void-elements
  matches!(
    tag,
    "area"
      | "base"
      | "br"
      | "col"
      | "embed"
      | "hr"
      | "img"
      | "input"
      | "link"
      | "meta"
      | "param"
      | "source"
      | "track"
      | "wbr"
  )
}

fn escape_text(out: &mut String, text: &str) {
  for ch in text.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      _ => out.push(ch),
    }
  }
}

fn escape_attr_value(out: &mut String, value: &str) {
  for ch in value.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '"' => out.push_str("&quot;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      _ => out.push(ch),
    }
  }
}

#[inline]
fn is_dom_ascii_whitespace(byte: u8) -> bool {
  // DOM "ASCII whitespace" excludes U+000B (vertical tab).
  matches!(byte, b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

fn split_dom_ascii_whitespace(input: &str) -> Vec<&str> {
  let mut out: Vec<&str> = Vec::new();
  let mut start: Option<usize> = None;
  for (idx, byte) in input.bytes().enumerate() {
    if is_dom_ascii_whitespace(byte) {
      if let Some(start) = start.take() {
        out.push(&input[start..idx]);
      }
    } else if start.is_none() {
      start = Some(idx);
    }
  }
  if let Some(start) = start {
    out.push(&input[start..]);
  }
  out
}

fn handle_children(handle: &Handle) -> Vec<Handle> {
  handle.children.borrow().iter().cloned().collect()
}

fn fragment_children_from_rcdom(rcdom: &RcDom) -> Vec<Handle> {
  let children = handle_children(&rcdom.document);
  let significant: Vec<Handle> = children
    .iter()
    .filter(|handle| {
      !matches!(
        handle.data,
        NodeData::Doctype { .. } | NodeData::Comment { .. }
      )
    })
    .cloned()
    .collect();

  // `html5ever`'s RcDom fragment parsing currently returns a synthetic `<html>` element as the sole
  // significant child of the document, with the actual fragment nodes as its children.
  if significant.len() == 1 {
    if let NodeData::Element { name, .. } = &significant[0].data {
      if name.ns.to_string() == HTML_NAMESPACE && name.local.as_ref().eq_ignore_ascii_case("html") {
        return handle_children(&significant[0]);
      }
    }
  }

  significant
}

pub fn install_dom_shims<'js>(ctx: Ctx<'js>, globals: &Object<'js>) -> JsResult<()> {
  let dom = Rc::new(RefCell::new(Dom::new()));

  let (document_element_id, head_id, body_id) = {
    let dom = dom.borrow();
    (
      dom.document_element().0 as i32,
      dom.head().0 as i32,
      dom.body().0 as i32,
    )
  };
  globals.set("__fastrender_dom_document_element_id", document_element_id)?;
  globals.set("__fastrender_dom_head_id", head_id)?;
  globals.set("__fastrender_dom_body_id", body_id)?;

  let create_element = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |tag_name: String| -> JsResult<i32> {
      let id = dom.borrow_mut().create_element(&tag_name);
      Ok(id.0 as i32)
    }
  })?;

  let create_document_fragment = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move || -> JsResult<i32> {
      let id = dom.borrow_mut().create_document_fragment();
      Ok(id.0 as i32)
    }
  })?;

  let create_text_node = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |data: String| -> JsResult<i32> {
      let id = dom.borrow_mut().create_text_node(&data);
      Ok(id.0 as i32)
    }
  })?;

  let get_inner_html = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32| -> JsResult<String> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow()
        .get_inner_html(NodeId(node_id as usize))
        .map_err(dom_error_to_js_error)
    }
  })?;

  let get_text_data = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32| -> JsResult<String> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow()
        .get_text_data(NodeId(node_id as usize))
        .map_err(dom_error_to_js_error)
    }
  })?;

  let set_text_data = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32, data: String| -> JsResult<()> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .set_text_data(NodeId(node_id as usize), &data)
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  let get_text_content = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32| -> JsResult<Option<String>> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow()
        .get_text_content(NodeId(node_id as usize))
        .map_err(dom_error_to_js_error)
    }
  })?;

  let set_text_content = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32, data: String| -> JsResult<Option<i32>> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .set_text_content(NodeId(node_id as usize), &data)
        .map(|id| id.map(|id| id.0 as i32))
        .map_err(dom_error_to_js_error)
    }
  })?;

  let get_attribute = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32, name: String| -> JsResult<Option<String>> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow()
        .get_attribute(NodeId(node_id as usize), &name)
        .map_err(dom_error_to_js_error)
    }
  })?;

  let set_attribute = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32, name: String, value: String| -> JsResult<()> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .set_attribute(NodeId(node_id as usize), &name, &value)
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  let remove_attribute = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32, name: String| -> JsResult<()> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .remove_attribute(NodeId(node_id as usize), &name)
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  let set_inner_html = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32, html: String| -> JsResult<()> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .set_inner_html(NodeId(node_id as usize), &html)
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  let get_outer_html = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32| -> JsResult<String> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow()
        .get_outer_html(NodeId(node_id as usize))
        .map_err(dom_error_to_js_error)
    }
  })?;

  let set_outer_html = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32, html: String| -> JsResult<()> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .set_outer_html(NodeId(node_id as usize), &html)
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  let append_child = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |parent_id: i32, child_id: i32| -> JsResult<()> {
      if parent_id < 0 || child_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .append_child(NodeId(parent_id as usize), NodeId(child_id as usize))
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  let insert_before = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |parent_id: i32, child_id: i32, reference_id: i32| -> JsResult<()> {
      if parent_id < 0 || child_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      let reference = if reference_id < 0 {
        None
      } else {
        Some(NodeId(reference_id as usize))
      };
      dom
        .borrow_mut()
        .insert_before(
          NodeId(parent_id as usize),
          NodeId(child_id as usize),
          reference,
        )
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  let replace_child = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |parent_id: i32, new_child_id: i32, old_child_id: i32| -> JsResult<()> {
      if parent_id < 0 || new_child_id < 0 || old_child_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .replace_child(
          NodeId(parent_id as usize),
          NodeId(new_child_id as usize),
          NodeId(old_child_id as usize),
        )
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  let remove_child = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |parent_id: i32, child_id: i32| -> JsResult<()> {
      if parent_id < 0 || child_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow_mut()
        .remove_child(NodeId(parent_id as usize), NodeId(child_id as usize))
        .map_err(dom_error_to_js_error)?;
      Ok(())
    }
  })?;

  let has_child_nodes = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32| -> JsResult<bool> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow()
        .has_child_nodes(NodeId(node_id as usize))
        .map_err(dom_error_to_js_error)
    }
  })?;

  let get_node_type = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32| -> JsResult<i32> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow()
        .get_node_type(NodeId(node_id as usize))
        .map_err(dom_error_to_js_error)
    }
  })?;

  let get_parent_node = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32| -> JsResult<Option<i32>> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow()
        .get_parent_node(NodeId(node_id as usize))
        .map(|id| id.map(|id| id.0 as i32))
        .map_err(dom_error_to_js_error)
    }
  })?;

  let get_child_nodes = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32| -> JsResult<Vec<i32>> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      let ids = dom
        .borrow()
        .get_child_nodes(NodeId(node_id as usize))
        .map_err(dom_error_to_js_error)?;
      Ok(ids.into_iter().map(|id| id.0 as i32).collect())
    }
  })?;

  let get_tag_name = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32| -> JsResult<String> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow()
        .get_tag_name(NodeId(node_id as usize))
        .map_err(dom_error_to_js_error)
    }
  })?;

  let get_elements_by_tag_name = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |root_id: i32, qualified_name: String| -> JsResult<Vec<i32>> {
      if root_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      let ids = dom
        .borrow()
        .get_elements_by_tag_name(NodeId(root_id as usize), &qualified_name)
        .map_err(dom_error_to_js_error)?;
      Ok(ids.into_iter().map(|id| id.0 as i32).collect())
    }
  })?;

  let get_elements_by_tag_name_ns = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |root_id: i32, namespace: Option<String>, local_name: String| -> JsResult<Vec<i32>> {
      if root_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      let ids = dom
        .borrow()
        .get_elements_by_tag_name_ns(NodeId(root_id as usize), namespace.as_deref(), &local_name)
        .map_err(dom_error_to_js_error)?;
      Ok(ids.into_iter().map(|id| id.0 as i32).collect())
    }
  })?;

  let get_elements_by_class_name = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |root_id: i32, class_names: String| -> JsResult<Vec<i32>> {
      if root_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      let ids = dom
        .borrow()
        .get_elements_by_class_name(NodeId(root_id as usize), &class_names)
        .map_err(dom_error_to_js_error)?;
      Ok(ids.into_iter().map(|id| id.0 as i32).collect())
    }
  })?;

  let get_elements_by_name = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |name: String| -> JsResult<Vec<i32>> {
      let ids = dom
        .borrow()
        .get_elements_by_name(NodeId(0), &name)
        .map_err(dom_error_to_js_error)?;
      Ok(ids.into_iter().map(|id| id.0 as i32).collect())
    }
  })?;

  globals.set("__fastrender_dom_create_element", create_element)?;
  globals.set(
    "__fastrender_dom_create_document_fragment",
    create_document_fragment,
  )?;
  globals.set("__fastrender_dom_create_text_node", create_text_node)?;
  globals.set("__fastrender_dom_get_inner_html", get_inner_html)?;
  globals.set("__fastrender_dom_set_inner_html", set_inner_html)?;
  globals.set("__fastrender_dom_get_text_data", get_text_data)?;
  globals.set("__fastrender_dom_set_text_data", set_text_data)?;
  globals.set("__fastrender_dom_get_text_content", get_text_content)?;
  globals.set("__fastrender_dom_set_text_content", set_text_content)?;
  globals.set("__fastrender_dom_get_attribute", get_attribute)?;
  globals.set("__fastrender_dom_set_attribute", set_attribute)?;
  globals.set("__fastrender_dom_remove_attribute", remove_attribute)?;
  globals.set("__fastrender_dom_get_outer_html", get_outer_html)?;
  globals.set("__fastrender_dom_set_outer_html", set_outer_html)?;
  globals.set("__fastrender_dom_append_child", append_child)?;
  globals.set("__fastrender_dom_insert_before", insert_before)?;
  globals.set("__fastrender_dom_replace_child", replace_child)?;
  globals.set("__fastrender_dom_remove_child", remove_child)?;
  globals.set("__fastrender_dom_has_child_nodes", has_child_nodes)?;
  globals.set("__fastrender_dom_get_node_type", get_node_type)?;
  globals.set("__fastrender_dom_get_parent_node", get_parent_node)?;
  globals.set("__fastrender_dom_get_child_nodes", get_child_nodes)?;
  globals.set("__fastrender_dom_get_tag_name", get_tag_name)?;
  globals.set(
    "__fastrender_dom_get_elements_by_tag_name",
    get_elements_by_tag_name,
  )?;
  globals.set(
    "__fastrender_dom_get_elements_by_tag_name_ns",
    get_elements_by_tag_name_ns,
  )?;
  globals.set(
    "__fastrender_dom_get_elements_by_class_name",
    get_elements_by_class_name,
  )?;
  globals.set(
    "__fastrender_dom_get_elements_by_name",
    get_elements_by_name,
  )?;
  ctx.eval::<(), _>(DOM_SHIM)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::install_dom_shims;
  use rquickjs::{Context, Runtime};

  fn eval_json(ctx: rquickjs::Ctx<'_>, source: &str) -> serde_json::Value {
    let json: String = ctx.eval(source).expect("js eval should succeed");
    serde_json::from_str(&json).expect("js should return valid JSON")
  }

  #[test]
  fn get_elements_by_tag_name_is_live_and_array_like() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();

      let v = eval_json(
        ctx.clone(),
        r#"
        (function () {
          var img1 = document.createElement("img");
          img1.id = "a";
          document.body.appendChild(img1);
          var img2 = document.createElement("IMG");
          img2.id = "b";
          document.body.appendChild(img2);

          var coll = document.getElementsByTagName("img");
          var beforeLen = coll.length;
          var firstId = coll[0].id;
          var secondId = coll.item(1).id;
          var itemNeg = coll.item(-1);
          var idx99 = typeof coll[99];
          var iter = Array.from(coll).map(function (n) { return n.id; }).join(",");

          var img3 = document.createElement("img");
          img3.id = "c";
          document.body.appendChild(img3);
          var afterLen = coll.length;
          var afterIter = Array.from(coll).map(function (n) { return n.id; }).join(",");

          return JSON.stringify({
            beforeLen: beforeLen,
            firstId: firstId,
            secondId: secondId,
            itemNeg: itemNeg,
            idx99: idx99,
            iter: iter,
            afterLen: afterLen,
            afterIter: afterIter,
          });
        })()
        "#,
      );

      assert_eq!(v["beforeLen"], 2);
      assert_eq!(v["firstId"], "a");
      assert_eq!(v["secondId"], "b");
      assert!(v["itemNeg"].is_null(), "item(-1) should return null");
      assert_eq!(v["idx99"], "undefined");
      assert_eq!(v["iter"], "a,b");
      assert_eq!(v["afterLen"], 3);
      assert_eq!(v["afterIter"], "a,b,c");
    });
  }

  #[test]
  fn get_elements_by_class_name_tokenizes_and_matches_all_tokens() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();

      let v = eval_json(
        ctx.clone(),
        r#"
        (function () {
          var d1 = document.createElement("div");
          d1.id = "a";
          d1.className = "foo bar";
          document.body.appendChild(d1);

          var d2 = document.createElement("div");
          d2.id = "b";
          d2.className = "foo";
          document.body.appendChild(d2);

          var d3 = document.createElement("div");
          d3.id = "c";
          d3.className = "bar\tfoo baz";
          document.body.appendChild(d3);

          var coll = document.getElementsByClassName("foo  bar");
          return JSON.stringify({
            len: coll.length,
            ids: Array.from(coll).map(function (n) { return n.id; }).join(",")
          });
        })()
        "#,
      );

      assert_eq!(v["len"], 2);
      assert_eq!(v["ids"], "a,c");
    });
  }

  #[test]
  fn get_elements_by_name_matches_exact_name_attribute() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();

      let v = eval_json(
        ctx.clone(),
        r#"
        (function () {
          var i1 = document.createElement("input");
          i1.id = "a";
          i1.setAttribute("name", "n");
          document.body.appendChild(i1);

          var i2 = document.createElement("div");
          i2.id = "b";
          i2.setAttribute("name", "n");
          document.body.appendChild(i2);

          var coll = document.getElementsByName("n");
          return JSON.stringify({
            len: coll.length,
            ids: Array.from(coll).map(function (n) { return n.id; }).join(",")
          });
        })()
        "#,
      );

      assert_eq!(v["len"], 2);
      assert_eq!(v["ids"], "a,b");
    });
  }

  #[test]
  fn get_elements_by_tag_name_skips_inert_template_contents() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();

      let v = eval_json(
        ctx.clone(),
        r#"
        (function () {
          var tmpl = document.createElement("template");
          tmpl.id = "t";
          document.body.appendChild(tmpl);

          var inside = document.createElement("div");
          inside.id = "inside";
          tmpl.appendChild(inside);

          var outside = document.createElement("div");
          outside.id = "outside";
          document.body.appendChild(outside);

          var divs = document.getElementsByTagName("div");
          var templates = document.getElementsByTagName("template");
          return JSON.stringify({
            divLen: divs.length,
            divIds: Array.from(divs).map(function (n) { return n.id; }).join(","),
            tmplLen: templates.length,
            tmplId: templates[0].id,
          });
        })()
        "#,
      );

      assert_eq!(v["divLen"], 1);
      assert_eq!(v["divIds"], "outside");
      assert_eq!(v["tmplLen"], 1);
      assert_eq!(v["tmplId"], "t");
    });
  }

  #[test]
  fn get_elements_by_tag_name_ns_supports_html_namespace_and_wildcards() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();

      let v = eval_json(
        ctx.clone(),
        r#"
        (function () {
          var d = document.createElement("div");
          d.id = "a";
          document.body.appendChild(d);

          var coll = document.getElementsByTagNameNS("http://www.w3.org/1999/xhtml", "DIV");
          var coll2 = document.getElementsByTagNameNS("*", "div");
          return JSON.stringify({
            len: coll.length,
            first: coll[0].id,
            len2: coll2.length,
            first2: coll2[0].id,
          });
        })()
        "#,
      );

      assert_eq!(v["len"], 1);
      assert_eq!(v["first"], "a");
      assert_eq!(v["len2"], 1);
      assert_eq!(v["first2"], "a");
    });
  }

  #[test]
  fn html_element_constructors_and_tag_name_mapping() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();

      let v = eval_json(
        ctx.clone(),
        r#"
        (function () {
          function throwsIllegalConstructor(ctor) {
            try {
              new ctor();
              return false;
            } catch (e) {
              return e && e.name === "TypeError" && String(e.message) === "Illegal constructor";
            }
          }

          var div = document.createElement("div");
          var input = document.createElement("input");
          var textarea = document.createElement("textarea");
          var select = document.createElement("select");
          var form = document.createElement("form");
          var option = document.createElement("option");

          return JSON.stringify({
            divIsHTMLElement: div instanceof HTMLElement,
            divIsElement: div instanceof Element,
            divIsNode: div instanceof Node,

            inputIsHTMLInputElement: input instanceof HTMLInputElement,
            inputIsHTMLElement: input instanceof HTMLElement,
            inputIsElement: input instanceof Element,
            inputIsNode: input instanceof Node,
            inputProtoIsHTMLInput: Object.getPrototypeOf(input) === HTMLInputElement.prototype,

            textareaIsHTMLTextAreaElement: textarea instanceof HTMLTextAreaElement,
            selectIsHTMLSelectElement: select instanceof HTMLSelectElement,
            formIsHTMLFormElement: form instanceof HTMLFormElement,
            optionIsHTMLOptionElement: option instanceof HTMLOptionElement,

            headIsHTMLElement: document.head instanceof HTMLElement,
            bodyIsHTMLElement: document.body instanceof HTMLElement,
            documentElementIsHTMLElement: document.documentElement instanceof HTMLElement,

            ctorIllegal: {
              HTMLElement: throwsIllegalConstructor(HTMLElement),
              HTMLInputElement: throwsIllegalConstructor(HTMLInputElement),
              HTMLTextAreaElement: throwsIllegalConstructor(HTMLTextAreaElement),
              HTMLSelectElement: throwsIllegalConstructor(HTMLSelectElement),
              HTMLFormElement: throwsIllegalConstructor(HTMLFormElement),
              HTMLOptionElement: throwsIllegalConstructor(HTMLOptionElement),
            }
          });
        })()
        "#,
      );

      assert_eq!(v["divIsHTMLElement"], true);
      assert_eq!(v["divIsElement"], true);
      assert_eq!(v["divIsNode"], true);

      assert_eq!(v["inputIsHTMLInputElement"], true);
      assert_eq!(v["inputIsHTMLElement"], true);
      assert_eq!(v["inputIsElement"], true);
      assert_eq!(v["inputIsNode"], true);
      assert_eq!(v["inputProtoIsHTMLInput"], true);

      assert_eq!(v["textareaIsHTMLTextAreaElement"], true);
      assert_eq!(v["selectIsHTMLSelectElement"], true);
      assert_eq!(v["formIsHTMLFormElement"], true);
      assert_eq!(v["optionIsHTMLOptionElement"], true);

      assert_eq!(v["headIsHTMLElement"], true);
      assert_eq!(v["bodyIsHTMLElement"], true);
      assert_eq!(v["documentElementIsHTMLElement"], true);

      assert_eq!(v["ctorIllegal"]["HTMLElement"], true);
      assert_eq!(v["ctorIllegal"]["HTMLInputElement"], true);
      assert_eq!(v["ctorIllegal"]["HTMLTextAreaElement"], true);
      assert_eq!(v["ctorIllegal"]["HTMLSelectElement"], true);
      assert_eq!(v["ctorIllegal"]["HTMLFormElement"], true);
      assert_eq!(v["ctorIllegal"]["HTMLOptionElement"], true);
    });
  }
}
