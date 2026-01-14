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
  var COLLECTION_GET_IDS = Symbol("fastrender_collection_get_ids");
  var NODELIST_ITEMS = Symbol("fastrender_nodelist_items");
  var SHADOW_ROOT_IDS = new Set(); // node id (DocumentFragment) -> is a ShadowRoot wrapper
  var SHADOW_ROOT_BY_HOST = new WeakMap(); // Element -> ShadowRoot
  var SLOT_MANUAL_ASSIGNMENTS = new WeakMap(); // HTMLSlotElement -> Node[]
  var NODE_MANUAL_ASSIGNED_SLOT = new WeakMap(); // Node -> HTMLSlotElement

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

  // Brand the global object (`window`) as an EventTarget.
  // This is required for WPT tests like `window instanceof EventTarget`.
  Object.setPrototypeOf(g, EventTarget.prototype);

  function Node() { illegal(); }
  function Document() { illegal(); }
  function DocumentFragment() { illegal(); }
  function ShadowRoot() { illegal(); }
  function Element() { illegal(); }
  function HTMLElement() { illegal(); }
  function HTMLDivElement() { illegal(); }
  function HTMLInputElement() { illegal(); }
  function HTMLTextAreaElement() { illegal(); }
  function HTMLSelectElement() { illegal(); }
  function HTMLFormElement() { illegal(); }
  function HTMLOptionElement() { illegal(); }
  function HTMLSlotElement() { illegal(); }
  function Text() { illegal(); }
  function Comment() { illegal(); }
  function NodeList() { illegal(); }
  function MutationObserver(callback) {
    if (typeof callback !== "function") {
      throw new TypeError(
        "Failed to construct 'MutationObserver': parameter 1 is not a function."
      );
    }
    OBSERVER_STATE.set(this, {
      callback: callback,
      records: [],
      targets: new Map(),
    });
  }
  function MutationRecord() { illegal(); }
  function HTMLCollection() { illegal(); }
  function CSSStyleDeclaration() { illegal(); }
  function HTMLOptionsCollection() { illegal(); }
  function HTMLFormControlsCollection() { illegal(); }
  function NodeFilter() { illegal(); }
  function Range() {
    if (!(this instanceof Range)) {
      throw new TypeError("Illegal constructor");
    }
    // Spec-aligned initial state: new Range() is collapsed at (document, 0).
    this.startContainer = g.document;
    this.startOffset = 0;
    this.endContainer = g.document;
    this.endOffset = 0;
  }

  Object.defineProperty(Range.prototype, "collapsed", {
    get: function () {
      return this.startContainer === this.endContainer && this.startOffset === this.endOffset;
    },
    configurable: true,
  });
  Object.defineProperty(Range.prototype, "commonAncestorContainer", {
    get: function () {
      // Minimal implementation for the smoke corpus; new Range() is rooted at document.
      return this.startContainer;
    },
    configurable: true,
  });

  Object.setPrototypeOf(Node.prototype, EventTarget.prototype);
  Object.setPrototypeOf(Document.prototype, Node.prototype);
  Object.setPrototypeOf(DocumentFragment.prototype, Node.prototype);
  Object.setPrototypeOf(ShadowRoot.prototype, DocumentFragment.prototype);
  Object.setPrototypeOf(Element.prototype, Node.prototype);
  Object.setPrototypeOf(HTMLElement.prototype, Element.prototype);
  Object.setPrototypeOf(HTMLDivElement.prototype, HTMLElement.prototype);
  Object.setPrototypeOf(HTMLInputElement.prototype, HTMLElement.prototype);
  Object.setPrototypeOf(HTMLTextAreaElement.prototype, HTMLElement.prototype);
  Object.setPrototypeOf(HTMLSelectElement.prototype, HTMLElement.prototype);
  Object.setPrototypeOf(HTMLFormElement.prototype, HTMLElement.prototype);
  Object.setPrototypeOf(HTMLOptionElement.prototype, HTMLElement.prototype);
  Object.setPrototypeOf(HTMLSlotElement.prototype, HTMLElement.prototype);
  Object.setPrototypeOf(Text.prototype, Node.prototype);
  Object.setPrototypeOf(Comment.prototype, Node.prototype);
  // Make NodeList-backed collections usable as arrays in the QuickJS shim (so `childNodes` can be
  // a real JS Array that is also `instanceof NodeList`).
  Object.setPrototypeOf(NodeList.prototype, Array.prototype);
  Object.setPrototypeOf(MutationObserver.prototype, Object.prototype);
  Object.setPrototypeOf(MutationRecord.prototype, Object.prototype);
  Object.setPrototypeOf(HTMLOptionsCollection.prototype, HTMLCollection.prototype);
  Object.setPrototypeOf(HTMLFormControlsCollection.prototype, HTMLCollection.prototype);

  // Node type constants.
  Node.ELEMENT_NODE = 1;
  Node.TEXT_NODE = 3;
  Node.COMMENT_NODE = 8;
  Node.DOCUMENT_NODE = 9;
  Node.DOCUMENT_TYPE_NODE = 10;
  Node.DOCUMENT_FRAGMENT_NODE = 11;

  // NodeFilter constants.
  NodeFilter.FILTER_ACCEPT = 1;
  NodeFilter.FILTER_REJECT = 2;
  NodeFilter.FILTER_SKIP = 3;
  NodeFilter.SHOW_ALL = 0xFFFFFFFF;
  NodeFilter.SHOW_ELEMENT = 0x1;
  NodeFilter.SHOW_ATTRIBUTE = 0x2;
  NodeFilter.SHOW_TEXT = 0x4;
  NodeFilter.SHOW_CDATA_SECTION = 0x8;
  NodeFilter.SHOW_ENTITY_REFERENCE = 0x10;
  NodeFilter.SHOW_ENTITY = 0x20;
  NodeFilter.SHOW_PROCESSING_INSTRUCTION = 0x40;
  NodeFilter.SHOW_COMMENT = 0x80;
  NodeFilter.SHOW_DOCUMENT = 0x100;
  NodeFilter.SHOW_DOCUMENT_TYPE = 0x200;
  NodeFilter.SHOW_DOCUMENT_FRAGMENT = 0x400;
  NodeFilter.SHOW_NOTATION = 0x800;

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
  g.document.childNodes = makeChildNodeList();
  NODE_CACHE.set(0, g.document);

  function makeChildNodeList() {
    var arr = [];
    Object.setPrototypeOf(arr, NodeList.prototype);
    return arr;
  }

  function ensureArray(o, key) {
    if (!o[key]) {
      o[key] = key === "childNodes" ? makeChildNodeList() : [];
    }
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
    o.childNodes = makeChildNodeList();
    if (tagName !== undefined) {
      o.tagName = String(tagName);
    }
    return o;
  }

  function collectionGetIdsFromThis(self) {
    if (typeof self !== "object" || self === null) {
      throw new TypeError("Illegal invocation");
    }
    var getIds = self[COLLECTION_GET_IDS];
    if (typeof getIds !== "function") {
      throw new TypeError("Illegal invocation");
    }
    return getIds;
  }

  function nodelistItemsFromThis(self) {
    if (typeof self !== "object" || self === null) {
      throw new TypeError("Illegal invocation");
    }
    // In the QuickJS shim, some NodeLists (notably `childNodes`) are backed by real JS arrays with
    // their prototype set to `NodeList.prototype`.
    if (Array.isArray(self)) return self;
    var items = self[NODELIST_ITEMS];
    if (!items || typeof items.length !== "number") {
      throw new TypeError("Illegal invocation");
    }
    return items;
  }

  NodeList.prototype.item = function (index) {
    var items = nodelistItemsFromThis(this);
    var i = Number(index);
    if (!isFinite(i) || isNaN(i)) i = 0;
    i = Math.trunc(i);
    if (i < 0 || i >= items.length) return null;
    return items[i] || null;
  };

  Object.defineProperty(NodeList.prototype, "length", {
    get: function () {
      var items = nodelistItemsFromThis(this);
      return items.length;
    },
    configurable: true,
  });

  function makeStaticNodeList(nodes) {
    var target = Object.create(NodeList.prototype);
    target[NODELIST_ITEMS] = Array.isArray(nodes) ? nodes.slice() : [];
    return new Proxy(target, {
      get: function (t, prop, recv) {
        if (isArrayIndex(prop)) {
          var items = t[NODELIST_ITEMS];
          var idx = Number(prop);
          if (idx < items.length) return items[idx];
          return undefined;
        }
        return Reflect.get(t, prop, recv);
      },
    });
  }

  function makeIterator(nextFn) {
    return {
      next: nextFn,
      [Symbol.iterator]: function () {
        return this;
      },
    };
  }

  NodeList.prototype.values = function () {
    var items = nodelistItemsFromThis(this);
    var i = 0;
    return makeIterator(function () {
      if (i >= items.length) return { done: true, value: undefined };
      return { done: false, value: items[i++] };
    });
  };
  NodeList.prototype.keys = function () {
    var items = nodelistItemsFromThis(this);
    var i = 0;
    return makeIterator(function () {
      if (i >= items.length) return { done: true, value: undefined };
      return { done: false, value: i++ };
    });
  };
  NodeList.prototype.entries = function () {
    var items = nodelistItemsFromThis(this);
    var i = 0;
    return makeIterator(function () {
      if (i >= items.length) return { done: true, value: undefined };
      var idx = i++;
      return { done: false, value: [idx, items[idx]] };
    });
  };
  NodeList.prototype.forEach = function (callback, thisArg) {
    var items = nodelistItemsFromThis(this);
    if (callback === null || callback === undefined) return;
    if (typeof callback !== "function") {
      throw new TypeError("callback is not a function");
    }
    for (var i = 0; i < items.length; i++) {
      callback.call(thisArg, items[i], i, this);
    }
  };
  NodeList.prototype[Symbol.iterator] = NodeList.prototype.values;

  HTMLCollection.prototype.values = function () {
    var getIds = collectionGetIdsFromThis(this);
    var ids = getIds();
    var i = 0;
    return makeIterator(function () {
      if (i >= ids.length) return { done: true, value: undefined };
      return { done: false, value: elementFromId(ids[i++]) };
    });
  };
  HTMLCollection.prototype.keys = function () {
    var getIds = collectionGetIdsFromThis(this);
    var ids = getIds();
    var i = 0;
    return makeIterator(function () {
      if (i >= ids.length) return { done: true, value: undefined };
      return { done: false, value: i++ };
    });
  };
  HTMLCollection.prototype.entries = function () {
    var getIds = collectionGetIdsFromThis(this);
    var ids = getIds();
    var i = 0;
    return makeIterator(function () {
      if (i >= ids.length) return { done: true, value: undefined };
      var idx = i++;
      return { done: false, value: [idx, elementFromId(ids[idx])] };
    });
  };
  HTMLCollection.prototype.forEach = function (callback, thisArg) {
    var getIds = collectionGetIdsFromThis(this);
    var ids = getIds();
    if (callback === null || callback === undefined) return;
    if (typeof callback !== "function") {
      throw new TypeError("callback is not a function");
    }
    for (var i = 0; i < ids.length; i++) {
      callback.call(thisArg, elementFromId(ids[i]), i, this);
    }
  };
  HTMLCollection.prototype[Symbol.iterator] = HTMLCollection.prototype.values;

  var TARGET_OBSERVERS = new WeakMap(); // target -> Set(observer)
  var OBSERVER_STATE = new WeakMap(); // observer -> {callback,records,targets}

  MutationObserver.prototype.observe = function (target, options) {
    if (typeof target !== "object" || target === null) {
      throw new TypeError("Failed to execute 'observe' on 'MutationObserver': parameter 1 is not of type 'Node'.");
    }
    var state = OBSERVER_STATE.get(this);
    if (!state) throw new TypeError("Illegal invocation");
    var opts = options && typeof options === "object" ? options : {};
    state.targets.set(target, opts);
    var set = TARGET_OBSERVERS.get(target);
    if (!set) {
      set = new Set();
      TARGET_OBSERVERS.set(target, set);
    }
    set.add(this);
  };

  MutationObserver.prototype.disconnect = function () {
    var state = OBSERVER_STATE.get(this);
    if (!state) throw new TypeError("Illegal invocation");
    state.targets.forEach(function (_opts, target) {
      var set = TARGET_OBSERVERS.get(target);
      if (set) set.delete(this);
    }, this);
    state.targets.clear();
    state.records.length = 0;
  };

  MutationObserver.prototype.takeRecords = function () {
    var state = OBSERVER_STATE.get(this);
    if (!state) throw new TypeError("Illegal invocation");
    var out = state.records.slice();
    state.records.length = 0;
    return out;
  };

  function queueChildListMutation(target, addedNodes, removedNodes) {
    var observers = TARGET_OBSERVERS.get(target);
    if (!observers) return;
    observers.forEach(function (obs) {
      var state = OBSERVER_STATE.get(obs);
      if (!state) return;
      var opts = state.targets.get(target);
      if (!opts || !opts.childList) return;
      var record = Object.create(MutationRecord.prototype);
      record.type = "childList";
      record.target = target;
      record.addedNodes = makeStaticNodeList(addedNodes || []);
      record.removedNodes = makeStaticNodeList(removedNodes || []);
      state.records.push(record);
    });
  }

  function elementPrototypeForTag(tagNameLower) {
    // The shim only needs a small subset of element interfaces for WPT and common scripts.
    // Default to `HTMLElement` for all HTML tags.
    switch (String(tagNameLower).toLowerCase()) {
      case "div":
        return HTMLDivElement.prototype;
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
      case "slot":
        return HTMLSlotElement.prototype;
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
    if (t === Node.COMMENT_NODE) return makeNode(Comment.prototype, id);
    if (t === Node.DOCUMENT_FRAGMENT_NODE) {
      if (SHADOW_ROOT_IDS.has(id)) return makeNode(ShadowRoot.prototype, id);
      return makeNode(DocumentFragment.prototype, id);
    }
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
    if (!(el instanceof Element)) {
      // Allow `:scope` to match non-Element scoping roots (DocumentFragment/ShadowRoot) when the
      // compound contains *only* `:scope`.
      if (
        compound.isScope &&
        el === scopeEl &&
        !compound.tag &&
        !compound.id &&
        (!compound.classes || compound.classes.length === 0)
      ) {
        return true;
      }
      return false;
    }
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
    // Treat inert HTML `<template>` contents as disconnected from traversal APIs.
    //
    // In the real DOM, `template.content` owns the template's descendants, so querying from the
    // `<template>` element itself must not expose those nodes.
    if (g.__fastrender_dom_is_inert_template(nodeIdFromThis(root))) return;
    var stack = [];
    var kids = root.childNodes || [];
    for (var i = kids.length - 1; i >= 0; i--) stack.push(kids[i]);

    while (stack.length) {
      var node = stack.pop();
      if (!(node instanceof Element)) continue;
      visit(node);
      // Treat inert HTML `<template>` contents as inert.
      if (g.__fastrender_dom_is_inert_template(nodeIdFromThis(node))) continue;
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
    target[COLLECTION_GET_IDS] = getIds;

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

  Object.defineProperty(Element.prototype, "children", {
    get: function () {
      nodeIdFromThis(this);
      var self = this;
      return makeLiveElementCollection(function () {
        var out = [];
        var nodes = self.childNodes || [];
        for (var i = 0; i < nodes.length; i++) {
          var n = nodes[i];
          if (n instanceof Element) out.push(nodeIdFromThis(n));
        }
        return out;
      });
    },
    configurable: true,
  });

  Document.prototype.createElement = function (tagName) {
    var raw = String(tagName);
    var id = g.__fastrender_dom_create_element(raw);
    return makeNode(elementPrototypeForTag(raw.toLowerCase()), id, raw.toUpperCase());
  };

  Document.prototype.createDocumentFragment = function () {
    var id = g.__fastrender_dom_create_document_fragment();
    return makeNode(DocumentFragment.prototype, id);
  };

  Element.prototype.attachShadow = function (init) {
    nodeIdFromThis(this);
    if (!(this instanceof Element)) throw new TypeError("Illegal invocation");
    if (typeof init !== "object" || init === null) {
      throw new TypeError(
        "Failed to execute 'attachShadow' on 'Element': parameter 1 is not of type 'ShadowRootInit'."
      );
    }
    var mode = String(init.mode);
    if (mode !== "open" && mode !== "closed") {
      throw new TypeError(
        "Failed to execute 'attachShadow' on 'Element': 'mode' must be either 'open' or 'closed'."
      );
    }
    var existing = SHADOW_ROOT_BY_HOST.get(this);
    if (existing) {
      // DOMException NotSupportedError; WPT does not assert the name yet, so keep it simple.
      throw new Error("NotSupportedError");
    }

    var slotAssignment = init.slotAssignment === undefined ? "named" : String(init.slotAssignment);
    if (slotAssignment !== "manual" && slotAssignment !== "named") slotAssignment = "named";

    // Model a ShadowRoot as a detached DocumentFragment with a ShadowRoot prototype.
    var root = g.document.createDocumentFragment();
    Object.setPrototypeOf(root, ShadowRoot.prototype);
    root.host = this;
    root.mode = mode;
    root.slotAssignment = slotAssignment;
    SHADOW_ROOT_BY_HOST.set(this, root);
    SHADOW_ROOT_IDS.add(nodeIdFromThis(root));
    return root;
  };

  Object.defineProperty(Element.prototype, "shadowRoot", {
    get: function () {
      nodeIdFromThis(this);
      var root = SHADOW_ROOT_BY_HOST.get(this) || null;
      if (!root) return null;
      return root.mode === "open" ? root : null;
    },
    configurable: true,
  });

  Document.prototype.createTextNode = function (data) {
    var id = g.__fastrender_dom_create_text_node(String(data));
    return makeNode(Text.prototype, id);
  };

  Document.prototype.createComment = function (data) {
    var id = g.__fastrender_dom_create_comment(String(data));
    return makeNode(Comment.prototype, id);
  };

  // ----------------------------------------------------------------------------
  // Traversal: NodeIterator / TreeWalker / NodeFilter
  // ----------------------------------------------------------------------------
  //
  // The QuickJS backend relies on this JS DOM shim, so we implement a small but spec-aligned
  // subset of the DOM traversal APIs sufficient for the curated WPT cases in `tests/dom/`.
  var NODE_ITERATORS = new Set();
  var NODE_ITERATOR_STATE = new WeakMap();
  var TREE_WALKER_STATE = new WeakMap();

  function invalidStateError() {
    var e = new Error("InvalidStateError");
    e.name = "InvalidStateError";
    return e;
  }

  function nodeTypeShowBit(node) {
    var t = node.nodeType;
    if (t === Node.ELEMENT_NODE) return NodeFilter.SHOW_ELEMENT;
    if (t === Node.TEXT_NODE) return NodeFilter.SHOW_TEXT;
    if (t === Node.COMMENT_NODE) return NodeFilter.SHOW_COMMENT;
    if (t === Node.DOCUMENT_NODE) return NodeFilter.SHOW_DOCUMENT;
    if (t === Node.DOCUMENT_FRAGMENT_NODE) return NodeFilter.SHOW_DOCUMENT_FRAGMENT;
    return 0;
  }

  function runNodeFilter(filter, node) {
    if (filter === null || filter === undefined) return NodeFilter.FILTER_ACCEPT;
    if (typeof filter === "function") return Number(filter(node));
    if (typeof filter === "object" && typeof filter.acceptNode === "function") {
      return Number(filter.acceptNode.call(filter, node));
    }
    return NodeFilter.FILTER_ACCEPT;
  }

  function acceptNodeWithWhatToShow(whatToShow, filter, node) {
    // Apply whatToShow first; do not invoke the filter for excluded nodes.
    var show = nodeTypeShowBit(node);
    if (((whatToShow >>> 0) & show) === 0) return NodeFilter.FILTER_SKIP;
    var res = runNodeFilter(filter, node);
    if (res === NodeFilter.FILTER_REJECT) return NodeFilter.FILTER_REJECT;
    if (res === NodeFilter.FILTER_SKIP) return NodeFilter.FILTER_SKIP;
    return NodeFilter.FILTER_ACCEPT;
  }

  function isInertTemplate(node) {
    // FastRender represents HTML `<template>` contents as children of the template element, marked
    // `inert_subtree=true`. DOM traversal APIs must treat such templates as leaf nodes (equivalent
    // to how real DOM stores template contents under `template.content`).
    return g.__fastrender_dom_is_inert_template(nodeIdFromThis(node));
  }

  function treeOrderNext(root, node) {
    if (!node) return null;
    if (node.firstChild && !isInertTemplate(node)) return node.firstChild;
    while (node && node !== root) {
      if (node.nextSibling) return node.nextSibling;
      node = node.parentNode;
    }
    return null;
  }

  function treeOrderNextSkippingChildren(root, node) {
    // Like `treeOrderNext`, but treats `node` as having no children (used for pruning).
    while (node && node !== root) {
      if (node.nextSibling) return node.nextSibling;
      node = node.parentNode;
    }
    return null;
  }

  function treeOrderPrevious(root, node) {
    if (!node || node === root) return null;
    var prev = node.previousSibling;
    if (prev) {
      while (prev.lastChild && !isInertTemplate(prev)) prev = prev.lastChild;
      return prev;
    }
    return node.parentNode;
  }

  function lastInclusiveDescendant(node) {
    var n = node;
    while (n && n.lastChild && !isInertTemplate(n)) n = n.lastChild;
    return n;
  }

  function isInclusiveAncestor(ancestor, node) {
    for (var cur = node; cur; cur = cur.parentNode) {
      if (cur === ancestor) return true;
    }
    return false;
  }

  function nodeIteratorStateFromThis(self) {
    if (typeof self !== "object" || self === null) throw new TypeError("Illegal invocation");
    var st = NODE_ITERATOR_STATE.get(self);
    if (!st) throw new TypeError("Illegal invocation");
    return st;
  }

  function runNodeIteratorPreRemoveSteps(toBeRemovedNode) {
    NODE_ITERATORS.forEach(function (it) {
      var st = NODE_ITERATOR_STATE.get(it);
      if (!st) return;
      if (toBeRemovedNode === st.root) return;
      if (!isInclusiveAncestor(toBeRemovedNode, st.referenceNode)) return;

      if (st.pointerBeforeReferenceNode) {
        var next = treeOrderNextSkippingChildren(st.root, toBeRemovedNode);
        if (next) {
          st.referenceNode = next;
          st.pointerBeforeReferenceNode = true;
          return;
        }

        st.pointerBeforeReferenceNode = false;
        var prev = toBeRemovedNode.previousSibling;
        if (prev) {
          st.referenceNode = lastInclusiveDescendant(prev);
        } else {
          st.referenceNode = toBeRemovedNode.parentNode;
        }
        return;
      }

      var prev = toBeRemovedNode.previousSibling;
      if (prev) {
        st.referenceNode = lastInclusiveDescendant(prev);
      } else {
        st.referenceNode = toBeRemovedNode.parentNode;
      }
      st.pointerBeforeReferenceNode = false;
    });
  }

  function NodeIteratorImpl() { illegal(); }
  Object.defineProperty(NodeIteratorImpl.prototype, "root", {
    get: function () { return nodeIteratorStateFromThis(this).root; },
    configurable: true,
  });
  Object.defineProperty(NodeIteratorImpl.prototype, "referenceNode", {
    get: function () { return nodeIteratorStateFromThis(this).referenceNode; },
    configurable: true,
  });
  Object.defineProperty(NodeIteratorImpl.prototype, "pointerBeforeReferenceNode", {
    get: function () { return nodeIteratorStateFromThis(this).pointerBeforeReferenceNode; },
    configurable: true,
  });
  Object.defineProperty(NodeIteratorImpl.prototype, "whatToShow", {
    get: function () { return nodeIteratorStateFromThis(this).whatToShow; },
    configurable: true,
  });
  Object.defineProperty(NodeIteratorImpl.prototype, "filter", {
    get: function () { return nodeIteratorStateFromThis(this).filter; },
    configurable: true,
  });

  Object.defineProperty(NodeIteratorImpl.prototype, "nextNode", {
    value: function () {
      var st = nodeIteratorStateFromThis(this);
      if (st.active) throw invalidStateError();
      st.active = true;
      try {
        var node = st.referenceNode;
        var before = st.pointerBeforeReferenceNode;
        while (true) {
          if (before) {
            before = false;
          } else {
            node = treeOrderNext(st.root, node);
            if (!node) {
              st.pointerBeforeReferenceNode = false;
              return null;
            }
          }

          var res = acceptNodeWithWhatToShow(st.whatToShow, st.filter, node);
          if (res === NodeFilter.FILTER_ACCEPT) {
            st.referenceNode = node;
            st.pointerBeforeReferenceNode = false;
            return node;
          }
          // NodeIterator does not prune; treat non-ACCEPT as skip and continue.
        }
      } finally {
        st.active = false;
      }
    },
    writable: true,
    configurable: true,
  });

  Object.defineProperty(NodeIteratorImpl.prototype, "previousNode", {
    value: function () {
      var st = nodeIteratorStateFromThis(this);
      if (st.active) throw invalidStateError();
      st.active = true;
      try {
        var node;
        if (!st.pointerBeforeReferenceNode) {
          st.pointerBeforeReferenceNode = true;
          node = st.referenceNode;
        } else {
          node = treeOrderPrevious(st.root, st.referenceNode);
        }

        while (node) {
          var res = acceptNodeWithWhatToShow(st.whatToShow, st.filter, node);
          if (res === NodeFilter.FILTER_ACCEPT) {
            st.referenceNode = node;
            st.pointerBeforeReferenceNode = true;
            return node;
          }
          node = treeOrderPrevious(st.root, node);
        }

        return null;
      } finally {
        st.active = false;
      }
    },
    writable: true,
    configurable: true,
  });

  Document.prototype.createNodeIterator = function (root, whatToShow, filter) {
    nodeIdFromThis(this);
    if (typeof root !== "object" || root === null || typeof root[NODE_ID] !== "number") {
      throw new TypeError(
        "Failed to execute 'createNodeIterator' on 'Document': parameter 1 is not of type 'Node'."
      );
    }

    var it = Object.create(NodeIteratorImpl.prototype);
    var w = (whatToShow === undefined ? NodeFilter.SHOW_ALL : Number(whatToShow)) >>> 0;
    var f = (filter === undefined ? null : filter);
    NODE_ITERATOR_STATE.set(it, {
      root: root,
      referenceNode: root,
      pointerBeforeReferenceNode: true,
      whatToShow: w,
      filter: f,
      active: false,
    });
    NODE_ITERATORS.add(it);
    return it;
  };

  function treeWalkerStateFromThis(self) {
    if (typeof self !== "object" || self === null) throw new TypeError("Illegal invocation");
    var st = TREE_WALKER_STATE.get(self);
    if (!st) throw new TypeError("Illegal invocation");
    return st;
  }

  function TreeWalkerImpl() { illegal(); }
  Object.defineProperty(TreeWalkerImpl.prototype, "root", {
    get: function () { return treeWalkerStateFromThis(this).root; },
    configurable: true,
  });
  Object.defineProperty(TreeWalkerImpl.prototype, "currentNode", {
    get: function () { return treeWalkerStateFromThis(this).currentNode; },
    set: function (node) {
      var st = treeWalkerStateFromThis(this);
      if (typeof node !== "object" || node === null || typeof node[NODE_ID] !== "number") {
        throw new TypeError(
          "Failed to set the 'currentNode' property on 'TreeWalker': The provided value is not of type 'Node'."
        );
      }
      // Only allow nodes within the root subtree.
      if (!isInclusiveAncestor(st.root, node)) {
        throw new TypeError("TreeWalker currentNode must be a descendant of root");
      }
      st.currentNode = node;
    },
    configurable: true,
  });
  Object.defineProperty(TreeWalkerImpl.prototype, "whatToShow", {
    get: function () { return treeWalkerStateFromThis(this).whatToShow; },
    configurable: true,
  });
  Object.defineProperty(TreeWalkerImpl.prototype, "filter", {
    get: function () { return treeWalkerStateFromThis(this).filter; },
    configurable: true,
  });

  Object.defineProperty(TreeWalkerImpl.prototype, "nextNode", {
    value: function () {
      var st = treeWalkerStateFromThis(this);
      var node = st.currentNode;
      var skipChildren = false;
      while (true) {
        node = skipChildren ? treeOrderNextSkippingChildren(st.root, node) : treeOrderNext(st.root, node);
        if (!node) return null;

        var res = acceptNodeWithWhatToShow(st.whatToShow, st.filter, node);
        if (res === NodeFilter.FILTER_ACCEPT) {
          st.currentNode = node;
          return node;
        }

        // FILTER_SKIP: traverse into children.
        // FILTER_REJECT: skip this node's subtree.
        skipChildren = (res === NodeFilter.FILTER_REJECT);
      }
    },
    writable: true,
    configurable: true,
  });

  Document.prototype.createTreeWalker = function (root, whatToShow, filter) {
    nodeIdFromThis(this);
    if (typeof root !== "object" || root === null || typeof root[NODE_ID] !== "number") {
      throw new TypeError(
        "Failed to execute 'createTreeWalker' on 'Document': parameter 1 is not of type 'Node'."
      );
    }
    var tw = Object.create(TreeWalkerImpl.prototype);
    var w = (whatToShow === undefined ? NodeFilter.SHOW_ALL : Number(whatToShow)) >>> 0;
    var f = (filter === undefined ? null : filter);
    TREE_WALKER_STATE.set(tw, {
      root: root,
      currentNode: root,
      whatToShow: w,
      filter: f,
    });
    return tw;
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

  var ELEMENT_CHILDREN_CACHE = new WeakMap();
  Object.defineProperty(Element.prototype, "children", {
    get: function () {
      nodeIdFromThis(this);
      var cached = ELEMENT_CHILDREN_CACHE.get(this);
      if (cached) return cached;
      var self = this;
      var collection = makeLiveElementCollection(function () {
        var ids = [];
        var nodes = self.childNodes || [];
        for (var i = 0; i < nodes.length; i++) {
          var n = nodes[i];
          if (n instanceof Element) ids.push(nodeIdFromThis(n));
        }
        return ids;
      });
      ELEMENT_CHILDREN_CACHE.set(this, collection);
      return collection;
    },
    configurable: true,
  });

  var DOCUMENT_FRAGMENT_CHILDREN_CACHE = new WeakMap();
  Object.defineProperty(DocumentFragment.prototype, "children", {
    get: function () {
      nodeIdFromThis(this);
      var cached = DOCUMENT_FRAGMENT_CHILDREN_CACHE.get(this);
      if (cached) return cached;
      var self = this;
      var collection = makeLiveElementCollection(function () {
        var ids = [];
        var nodes = self.childNodes || [];
        for (var i = 0; i < nodes.length; i++) {
          var n = nodes[i];
          if (n instanceof Element) ids.push(nodeIdFromThis(n));
        }
        return ids;
      });
      DOCUMENT_FRAGMENT_CHILDREN_CACHE.set(this, collection);
      return collection;
    },
    configurable: true,
  });

  function childElementCountForParentNode(parent) {
    var nodes = parent.childNodes || [];
    var count = 0;
    for (var i = 0; i < nodes.length; i++) {
      if (nodes[i] instanceof Element) count++;
    }
    return count;
  }

  function firstElementChildForParentNode(parent) {
    var nodes = parent.childNodes || [];
    for (var i = 0; i < nodes.length; i++) {
      var n = nodes[i];
      if (n instanceof Element) return n;
    }
    return null;
  }

  function lastElementChildForParentNode(parent) {
    var nodes = parent.childNodes || [];
    for (var i = nodes.length - 1; i >= 0; i--) {
      var n = nodes[i];
      if (n instanceof Element) return n;
    }
    return null;
  }

  Object.defineProperty(Element.prototype, "childElementCount", {
    get: function () {
      nodeIdFromThis(this);
      return childElementCountForParentNode(this);
    },
    configurable: true,
  });
  Object.defineProperty(DocumentFragment.prototype, "childElementCount", {
    get: function () {
      nodeIdFromThis(this);
      return childElementCountForParentNode(this);
    },
    configurable: true,
  });

  Object.defineProperty(Element.prototype, "firstElementChild", {
    get: function () {
      nodeIdFromThis(this);
      return firstElementChildForParentNode(this);
    },
    configurable: true,
  });
  Object.defineProperty(DocumentFragment.prototype, "firstElementChild", {
    get: function () {
      nodeIdFromThis(this);
      return firstElementChildForParentNode(this);
    },
    configurable: true,
  });

  Object.defineProperty(Element.prototype, "lastElementChild", {
    get: function () {
      nodeIdFromThis(this);
      return lastElementChildForParentNode(this);
    },
    configurable: true,
  });
  Object.defineProperty(DocumentFragment.prototype, "lastElementChild", {
    get: function () {
      nodeIdFromThis(this);
      return lastElementChildForParentNode(this);
    },
    configurable: true,
  });

  function nextElementSiblingForElement(el, dir) {
    var parent = el.parentNode;
    if (!parent) return null;
    var nodes = parent.childNodes || [];
    var idx = nodes.indexOf(el);
    if (idx < 0) return null;
    if (dir > 0) {
      for (var i = idx + 1; i < nodes.length; i++) {
        if (nodes[i] instanceof Element) return nodes[i];
      }
    } else {
      for (var i = idx - 1; i >= 0; i--) {
        if (nodes[i] instanceof Element) return nodes[i];
      }
    }
    return null;
  }

  Object.defineProperty(Element.prototype, "nextElementSibling", {
    get: function () {
      nodeIdFromThis(this);
      return nextElementSiblingForElement(this, 1);
    },
    configurable: true,
  });
  Object.defineProperty(Element.prototype, "previousElementSibling", {
    get: function () {
      nodeIdFromThis(this);
      return nextElementSiblingForElement(this, -1);
    },
    configurable: true,
  });

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

  CSSStyleDeclaration.prototype.removeProperty = function (name) {
    var el = cssStyleFromThis(this);
    var needle = String(name).trim().toLowerCase();
    if (!needle) return "";
    var cssText = el.getAttribute("style") || "";
    var decls = parseStyleDecls(cssText);
    for (var i = 0; i < decls.length; i++) {
      if (decls[i].name === needle) {
        var prev = decls[i].value || "";
        decls.splice(i, 1);
        el.setAttribute("style", serializeStyleDecls(decls));
        return prev;
      }
    }
    return "";
  };

  // Minimal named CSS properties used by common scripts and our WPT corpus.
  function defineStyleProperty(prop) {
    Object.defineProperty(CSSStyleDeclaration.prototype, prop, {
      get: function () {
        return CSSStyleDeclaration.prototype.getPropertyValue.call(this, prop);
      },
      set: function (value) {
        CSSStyleDeclaration.prototype.setProperty.call(this, prop, value);
      },
      enumerable: true,
      configurable: true,
    });
  }
  defineStyleProperty("display");
  defineStyleProperty("cursor");
  defineStyleProperty("width");
  defineStyleProperty("height");

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

  // --- Shadow DOM slotting (minimal; enough for curated WPT corpus) ---
  Object.defineProperty(Element.prototype, "slot", {
    get: function () {
      nodeIdFromThis(this);
      var v = this.getAttribute("slot");
      return v === null ? "" : String(v);
    },
    set: function (value) {
      nodeIdFromThis(this);
      var v = String(value);
      if (v === "") {
        this.removeAttribute("slot");
      } else {
        this.setAttribute("slot", v);
      }
    },
    configurable: true,
  });

  function shadowRootForSlotElement(slotEl) {
    var cur = slotEl;
    while (cur) {
      if (cur instanceof ShadowRoot) return cur;
      cur = cur.parentNode;
    }
    return null;
  }

  function firstSlotInShadowRoot(root, name) {
    var needle = String(name || "");
    var found = null;
    traverseElementSubtree(root, function (el) {
      if (found) return;
      if (!(el instanceof HTMLSlotElement)) return;
      var n = el.getAttribute("name");
      var slotName = n === null ? "" : String(n);
      if (slotName === needle) found = el;
    });
    return found;
  }

  function assignedNodesForSlot(slotEl) {
    var root = shadowRootForSlotElement(slotEl);
    if (!root) return [];

    var slotAssignment = String(root.slotAssignment || "named");
    if (slotAssignment === "manual") {
      var manual = SLOT_MANUAL_ASSIGNMENTS.get(slotEl);
      if (manual !== undefined) return manual.slice();
      return (slotEl.childNodes || []).slice();
    }

    var nameAttr = slotEl.getAttribute("name");
    var name = nameAttr === null ? "" : String(nameAttr);

    // Only the first slot with a given name participates in distribution.
    var first = firstSlotInShadowRoot(root, name);
    if (first && first !== slotEl) return (slotEl.childNodes || []).slice();

    var host = root.host;
    if (!(host instanceof Element)) return (slotEl.childNodes || []).slice();

    var out = [];
    var nodes = host.childNodes || [];
    for (var i = 0; i < nodes.length; i++) {
      var n = nodes[i];
      if (!(n instanceof Element)) continue;
      if (n.slot === name) out.push(n);
    }
    if (out.length) return out;
    return (slotEl.childNodes || []).slice();
  }

  function assignedNodesFlattened(slotEl, visited) {
    var base = assignedNodesForSlot(slotEl);
    if (!visited) visited = new Set();
    var out = [];
    for (var i = 0; i < base.length; i++) {
      var n = base[i];
      if (n instanceof HTMLSlotElement) {
        if (visited.has(n)) continue;
        visited.add(n);
        var inner = assignedNodesFlattened(n, visited);
        for (var j = 0; j < inner.length; j++) out.push(inner[j]);
      } else {
        out.push(n);
      }
    }
    return out;
  }

  function normalizeAssignedNodesOptions(options, methodName) {
    if (options === undefined || options === null) return { flatten: false };
    if (typeof options !== "object") {
      throw new TypeError(
        "Failed to execute '" + methodName + "' on 'HTMLSlotElement': parameter 1 is not of type 'AssignedNodesOptions'."
      );
    }
    return { flatten: !!options.flatten };
  }

  HTMLSlotElement.prototype.assign = function () {
    nodeIdFromThis(this);
    if (!(this instanceof HTMLSlotElement)) throw new TypeError("Illegal invocation");

    var nodes = [];
    for (var i = 0; i < arguments.length; i++) {
      var n = arguments[i];
      if (typeof n !== "object" || n === null || typeof n[NODE_ID] !== "number") {
        throw new TypeError(
          "Failed to execute 'assign' on 'HTMLSlotElement': parameter " +
            (i + 1) +
            " is not of type 'Node'."
        );
      }
      nodes.push(n);
    }

    // Clear prior assignments from this slot.
    var prev = SLOT_MANUAL_ASSIGNMENTS.get(this);
    if (prev !== undefined) {
      for (var i = 0; i < prev.length; i++) {
        if (NODE_MANUAL_ASSIGNED_SLOT.get(prev[i]) === this) {
          NODE_MANUAL_ASSIGNED_SLOT.delete(prev[i]);
        }
      }
    }

    // Move nodes from any previous slot assignment into this one.
    for (var i = 0; i < nodes.length; i++) {
      var n = nodes[i];
      var oldSlot = NODE_MANUAL_ASSIGNED_SLOT.get(n);
      if (oldSlot && oldSlot !== this) {
        var oldList = SLOT_MANUAL_ASSIGNMENTS.get(oldSlot);
        if (oldList !== undefined) {
          var filtered = [];
          for (var j = 0; j < oldList.length; j++) {
            if (oldList[j] !== n) filtered.push(oldList[j]);
          }
          SLOT_MANUAL_ASSIGNMENTS.set(oldSlot, filtered);
        }
      }
      NODE_MANUAL_ASSIGNED_SLOT.set(n, this);
    }

    SLOT_MANUAL_ASSIGNMENTS.set(this, nodes.slice());
  };

  HTMLSlotElement.prototype.assignedNodes = function (options) {
    nodeIdFromThis(this);
    if (!(this instanceof HTMLSlotElement)) throw new TypeError("Illegal invocation");
    var opts = normalizeAssignedNodesOptions(options, "assignedNodes");
    if (opts.flatten) return assignedNodesFlattened(this);
    return assignedNodesForSlot(this);
  };

  HTMLSlotElement.prototype.assignedElements = function (options) {
    nodeIdFromThis(this);
    if (!(this instanceof HTMLSlotElement)) throw new TypeError("Illegal invocation");
    var opts = normalizeAssignedNodesOptions(options, "assignedElements");
    var nodes = opts.flatten ? assignedNodesFlattened(this) : assignedNodesForSlot(this);
    var out = [];
    for (var i = 0; i < nodes.length; i++) {
      if (nodes[i] instanceof Element) out.push(nodes[i]);
    }
    return out;
  };

  Object.defineProperty(Element.prototype, "assignedSlot", {
    get: function () {
      nodeIdFromThis(this);
      var parent = this.parentNode;
      if (!(parent instanceof Element)) return null;
      var root = SHADOW_ROOT_BY_HOST.get(parent);
      if (!root) return null;
      // `assignedSlot` uses the "open" find-a-slot variant.
      if (String(root.mode) !== "open") return null;

      var slotAssignment = String(root.slotAssignment || "named");
      if (slotAssignment === "manual") {
        var slot = NODE_MANUAL_ASSIGNED_SLOT.get(this) || null;
        if (slot && shadowRootForSlotElement(slot) === root) return slot;
        return null;
      }

      var name = this.slot;
      var slot = firstSlotInShadowRoot(root, name);
      return slot || null;
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

  Element.prototype.insertAdjacentHTML = function (position, html) {
    nodeIdFromThis(this);
    if (!(this instanceof Element)) throw new TypeError("Illegal invocation");
    var where = String(position).toLowerCase();
    var parent = null;
    var reference = null;
    var contextTag = "div";
    if (where === "beforebegin") {
      parent = this.parentNode;
      if (!parent) return;
      reference = this;
      if (parent instanceof Element) contextTag = String(parent.tagName).toLowerCase();
    } else if (where === "afterbegin") {
      parent = this;
      reference = this.firstChild;
      contextTag = String(this.tagName).toLowerCase();
    } else if (where === "beforeend") {
      parent = this;
      reference = null;
      contextTag = String(this.tagName).toLowerCase();
    } else if (where === "afterend") {
      parent = this.parentNode;
      if (!parent) return;
      reference = this.nextSibling;
      if (parent instanceof Element) contextTag = String(parent.tagName).toLowerCase();
    } else {
      throw new TypeError(
        "Failed to execute 'insertAdjacentHTML' on 'Element': The provided position is not valid."
      );
    }

    var tmp = g.document.createElement(contextTag);
    tmp.innerHTML = String(html);
    var frag = g.document.createDocumentFragment();
    while (tmp.firstChild) {
      frag.appendChild(tmp.firstChild);
    }
    parent.insertBefore(frag, reference);
  };

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

    return makeStaticNodeList(out);
  };

  Document.prototype.querySelector = function (selectors) {
    return g.document.documentElement.querySelector(selectors);
  };
  Document.prototype.querySelectorAll = function (selectors) {
    return g.document.documentElement.querySelectorAll(selectors);
  };

  DocumentFragment.prototype.querySelector = function (selectors) {
    nodeIdFromThis(this);
    var parsed = parseSelectorList(selectors);
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

  DocumentFragment.prototype.querySelectorAll = function (selectors) {
    nodeIdFromThis(this);
    var parsed = parseSelectorList(selectors);
    var out = [];
    var seen = new Set();
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
    return makeStaticNodeList(out);
  };

  // ShadowRoot inherits DocumentFragment, but WPT expects querySelector(All) to be own-properties on
  // ShadowRoot.prototype (not inherited from DocumentFragment.prototype).
  ShadowRoot.prototype.querySelector = DocumentFragment.prototype.querySelector;
  ShadowRoot.prototype.querySelectorAll = DocumentFragment.prototype.querySelectorAll;

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
      queueChildListMutation(this, moved, []);
      return child;
    }

    g.__fastrender_dom_append_child(parentId, childId);

    detachFromParent(child);
    var nodes = ensureArray(this, "childNodes");
    nodes.push(child);
    child.parentNode = this;
    queueChildListMutation(this, [child], []);
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
      queueChildListMutation(this, moved, []);
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
    queueChildListMutation(this, [child], []);
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
    runNodeIteratorPreRemoveSteps(oldChild);

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
      queueChildListMutation(this, moved, [oldChild]);
      return oldChild;
    }

    detachFromParent(child);
    idx = parentNodes.indexOf(oldChild);
    if (idx < 0) idx = 0;
    parentNodes.splice(idx, 1, child);
    oldChild.parentNode = null;
    child.parentNode = this;
    queueChildListMutation(this, [child], [oldChild]);
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
    runNodeIteratorPreRemoveSteps(child);
    detachFromParent(child);
    queueChildListMutation(this, [], [child]);
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
      if (this instanceof Comment) return Node.COMMENT_NODE;
      return 0;
    },
    configurable: true,
  });

  Object.defineProperty(Node.prototype, "nodeName", {
    get: function () {
      var t = this.nodeType;
      if (t === Node.ELEMENT_NODE) return this.tagName;
      if (t === Node.TEXT_NODE) return "#text";
      if (t === Node.COMMENT_NODE) return "#comment";
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
      if (!this.childNodes) this.childNodes = makeChildNodeList();
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
  Object.defineProperty(g, "NodeFilter", { value: NodeFilter, configurable: true, writable: true });
  Object.defineProperty(g, "NodeIterator", { value: NodeIteratorImpl, configurable: true, writable: true });
  Object.defineProperty(g, "TreeWalker", { value: TreeWalkerImpl, configurable: true, writable: true });
  Object.defineProperty(g, "Range", { value: Range, configurable: true, writable: true });
  Object.defineProperty(g, "Document", { value: Document, configurable: true, writable: true });
  Object.defineProperty(g, "DocumentFragment", { value: DocumentFragment, configurable: true, writable: true });
  Object.defineProperty(g, "ShadowRoot", { value: ShadowRoot, configurable: true, writable: true });
  Object.defineProperty(g, "Element", { value: Element, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLElement", { value: HTMLElement, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLSlotElement", { value: HTMLSlotElement, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLDivElement", { value: HTMLDivElement, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLInputElement", { value: HTMLInputElement, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLTextAreaElement", { value: HTMLTextAreaElement, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLSelectElement", { value: HTMLSelectElement, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLFormElement", { value: HTMLFormElement, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLOptionElement", { value: HTMLOptionElement, configurable: true, writable: true });
  Object.defineProperty(g, "Text", { value: Text, configurable: true, writable: true });
  Object.defineProperty(g, "Comment", { value: Comment, configurable: true, writable: true });
  Object.defineProperty(g, "NodeList", { value: NodeList, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLCollection", { value: HTMLCollection, configurable: true, writable: true });
  Object.defineProperty(g, "CSSStyleDeclaration", { value: CSSStyleDeclaration, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLOptionsCollection", { value: HTMLOptionsCollection, configurable: true, writable: true });
  Object.defineProperty(g, "HTMLFormControlsCollection", { value: HTMLFormControlsCollection, configurable: true, writable: true });
  Object.defineProperty(g, "EventTarget", { value: EventTarget, configurable: true, writable: true });
  Object.defineProperty(g, "Event", { value: Event, configurable: true, writable: true });
  Object.defineProperty(g, "MutationObserver", { value: MutationObserver, configurable: true, writable: true });
  Object.defineProperty(g, "MutationRecord", { value: MutationRecord, configurable: true, writable: true });

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
      DomShimError::InvalidNodeType => "InvalidNodeTypeError",
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
  Comment {
    content: String,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
  kind: NodeKind,
  parent: Option<NodeId>,
  children: Vec<NodeId>,
  /// Whether this node represents an inert HTML `<template>` element.
  ///
  /// HTML templates store their real children in `template_contents` during parsing, and the
  /// template's contents are inert for DOM tree traversal APIs like `getElementsByTagName`.
  ///
  /// Non-HTML `<template>` elements (e.g. in the SVG namespace) are *not* inert and should not
  /// suppress descendant traversal.
  is_inert_template: bool,
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
      is_inert_template: false,
    });
    if let Some(parent) = parent {
      if parent.0 < self.nodes.len() {
        self.nodes[parent.0].children.push(id);
      }
    }
    id
  }

  fn create_element(&mut self, tag_name: &str) -> NodeId {
    let id = self.push_node(
      NodeKind::Element {
        tag_name: tag_name.to_ascii_lowercase(),
        attributes: Vec::new(),
      },
      None,
    );
    if tag_name.eq_ignore_ascii_case("template") {
      self.nodes[id.0].is_inert_template = true;
    }
    id
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

  fn create_comment_node(&mut self, data: &str) -> NodeId {
    self.push_node(
      NodeKind::Comment {
        content: data.to_string(),
      },
      None,
    )
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
      NodeKind::Text { .. } | NodeKind::Comment { .. } => Err(DomShimError::HierarchyRequestError),
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

  fn is_inert_template(&self, node: NodeId) -> Result<bool, DomShimError> {
    Ok(self.node_checked(node)?.is_inert_template)
  }

  fn get_node_type(&self, node: NodeId) -> Result<i32, DomShimError> {
    self.node_checked(node)?;
    let value = match &self.nodes[node.0].kind {
      NodeKind::Document => 9,
      NodeKind::DocumentFragment => 11,
      NodeKind::Element { .. } => 1,
      NodeKind::Text { .. } => 3,
      NodeKind::Comment { .. } => 8,
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
      NodeKind::Text { .. } | NodeKind::Comment { .. } => {
        return Err(DomShimError::HierarchyRequestError)
      }
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
      NodeKind::Comment { content } => return Ok(Some(content.clone())),
      NodeKind::Element { .. } | NodeKind::DocumentFragment => {}
    }

    let mut out = String::new();
    let mut stack: Vec<NodeId> = self.nodes[node.0].children.iter().copied().rev().collect();
    while let Some(id) = stack.pop() {
      let node = self.node_checked(id)?;
      match &node.kind {
        NodeKind::Text { content } => out.push_str(content),
        NodeKind::Comment { .. } => {}
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
      NodeKind::Comment { .. } => {
        let node = self.node_checked_mut(node)?;
        let NodeKind::Comment { content } = &mut node.kind else {
          return Err(DomShimError::InvalidNodeType);
        };
        content.clear();
        content.push_str(data);
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
    if matches!(root_node.kind, NodeKind::Text { .. } | NodeKind::Comment { .. }) {
      return Err(DomShimError::InvalidNodeType);
    }
    if root_node.is_inert_template {
      return Ok(Vec::new());
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
        // Treat inert HTML `<template>` contents as inert, mirroring real DOM tree traversal.
        if node.is_inert_template {
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

    // This shim models all elements as being in the HTML namespace (we do not currently track
    // namespaces per-node). Match the DOM Standard's behavior for the cases exercised by the
    // curated WPT corpus:
    //
    // - `namespace = "*" or HTML_NAMESPACE` matches.
    // - `namespace = null or ""` (treated as null) matches only nodes in the null namespace,
    //   which this shim never creates.
    // - Any other namespace yields no matches.
    match namespace {
      None | Some("") => return Ok(Vec::new()),
      Some("*") | Some(HTML_NAMESPACE) => {}
      Some(_) => return Ok(Vec::new()),
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
            NodeKind::Comment { content } => {
              out.push_str("<!--");
              out.push_str(content);
              out.push_str("-->");
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
        NodeData::Comment { contents } => {
          let content = contents.to_string();
          let id = self.push_node(NodeKind::Comment { content }, item.parent);
          if item.parent.is_none() {
            roots.push(id);
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
          let (inert_template, children) = if is_template {
            let borrowed = template_contents.borrow();
            let inert = borrowed.is_some();
            (
              inert,
              borrowed
                .as_ref()
                .map(handle_children)
                .unwrap_or_else(|| handle_children(&item.handle)),
            )
          } else {
            (false, handle_children(&item.handle))
          };
          if inert_template {
            self.nodes[id.0].is_inert_template = true;
          }

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
  let nodes: Vec<Handle> = children
    .into_iter()
    .filter(|handle| !matches!(handle.data, NodeData::Doctype { .. }))
    .collect();

  // `html5ever`'s RcDom fragment parsing currently returns a synthetic `<html>` element as a
  // document child, with the actual fragment nodes as its children. Some inputs (notably comments)
  // can appear as siblings of that synthetic node, so unwrap the `<html>` element in-place when it
  // is the only element child of the document.
  let element_children: Vec<(usize, &Handle)> = nodes
    .iter()
    .enumerate()
    .filter(|(_idx, handle)| matches!(handle.data, NodeData::Element { .. }))
    .collect();
  if element_children.len() == 1 {
    let (html_idx, html_handle) = element_children[0];
    if let NodeData::Element { name, .. } = &html_handle.data {
      if name.ns.to_string() == HTML_NAMESPACE && name.local.as_ref().eq_ignore_ascii_case("html") {
        let mut out: Vec<Handle> = Vec::new();
        for (idx, handle) in nodes.into_iter().enumerate() {
          if idx == html_idx {
            out.extend(handle_children(&handle));
          } else {
            out.push(handle);
          }
        }
        return out;
      }
    }
  }

  nodes
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

  let create_comment_node = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |data: String| -> JsResult<i32> {
      let id = dom.borrow_mut().create_comment_node(&data);
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

  let is_inert_template = Function::new(ctx.clone(), {
    let dom = Rc::clone(&dom);
    move |node_id: i32| -> JsResult<bool> {
      if node_id < 0 {
        return Err(dom_error_to_js_error(DomShimError::NotFoundError));
      }
      dom
        .borrow()
        .is_inert_template(NodeId(node_id as usize))
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
  globals.set("__fastrender_dom_create_comment", create_comment_node)?;
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
  globals.set("__fastrender_dom_is_inert_template", is_inert_template)?;
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
  fn get_elements_by_tag_name_traverses_svg_template_contents() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();
      let v = eval_json(
        ctx.clone(),
        r#"
        (function () {
          // `<template>` in SVG namespace is not an inert HTML template; traversal APIs should still
          // walk its descendants.
          document.body.innerHTML = "<svg><template><g id='inside'></g></template></svg><g id='outside'></g>";
          var divs = document.getElementsByTagName("g");
          return JSON.stringify({
            len: divs.length,
            ids: Array.from(divs).map(function (n) { return n.id; }).join(",")
          });
        })()
        "#,
      );

      assert_eq!(v["len"], 2);
      assert_eq!(v["ids"], "inside,outside");
    });
  }

  #[test]
  fn query_selector_skips_inert_template_contents() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();
      let v = eval_json(
        ctx.clone(),
        r##"
        (function () {
          document.body.innerHTML = "<template><div id='inside'></div></template><div id='outside'></div>";
          var inside = document.querySelector("#inside");
          var outside = document.querySelector("#outside");
          return JSON.stringify({
            insideIsNull: inside === null,
            outsideId: outside ? outside.id : null,
          });
        })()
        "##,
      );

      assert_eq!(v["insideIsNull"], true);
      assert_eq!(v["outsideId"], "outside");
    });
  }

  #[test]
  fn query_selector_traverses_svg_template_contents() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();
      let v = eval_json(
        ctx.clone(),
        r##"
        (function () {
          // `<template>` in SVG namespace is not an inert HTML template; selector traversal should
          // still walk its descendants.
          document.body.innerHTML = "<svg><template><g id='inside'></g></template></svg><g id='outside'></g>";
          var inside = document.querySelector("#inside");
          var outside = document.querySelector("#outside");
          return JSON.stringify({
            insideId: inside ? inside.id : null,
            outsideId: outside ? outside.id : null,
          });
        })()
        "##,
      );

      assert_eq!(v["insideId"], "inside");
      assert_eq!(v["outsideId"], "outside");
    });
  }

  #[test]
  fn element_query_selector_does_not_traverse_inert_template_contents() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();
      let v = eval_json(
        ctx.clone(),
        r##"
        (function () {
          var tmpl = document.createElement("template");
          tmpl.id = "t";
          document.body.appendChild(tmpl);

          var inside = document.createElement("div");
          inside.id = "inside";
          tmpl.appendChild(inside);

          var inTemplate = tmpl.querySelector("#inside");
          var inDoc = document.querySelector("#inside");
          return JSON.stringify({
            inTemplateIsNull: inTemplate === null,
            inDocIsNull: inDoc === null,
          });
        })()
        "##,
      );

      assert_eq!(v["inTemplateIsNull"], true);
      assert_eq!(v["inDocIsNull"], true);
    });
  }

  #[test]
  fn element_query_selector_traverses_svg_template_contents() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();
      let v = eval_json(
        ctx.clone(),
        r##"
        (function () {
          document.body.innerHTML = "<svg><template><g id='inside'></g></template></svg><g id='outside'></g>";
          var tmpl = document.querySelector("svg template");
          var inTemplate = tmpl.querySelector("#inside");
          return JSON.stringify({
            inTemplateId: inTemplate ? inTemplate.id : null,
          });
        })()
        "##,
      );

      assert_eq!(v["inTemplateId"], "inside");
    });
  }

  #[test]
  fn element_get_elements_by_tag_name_does_not_traverse_inert_template_contents() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();
      let v = eval_json(
        ctx.clone(),
        r##"
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

          var divsInTemplate = tmpl.getElementsByTagName("div");
          var divsInDoc = document.getElementsByTagName("div");
          return JSON.stringify({
            templateLen: divsInTemplate.length,
            docLen: divsInDoc.length,
            docIds: Array.from(divsInDoc).map(function (n) { return n.id; }).join(","),
          });
        })()
        "##,
      );

      assert_eq!(v["templateLen"], 0);
      assert_eq!(v["docLen"], 1);
      assert_eq!(v["docIds"], "outside");
    });
  }

  #[test]
  fn element_get_elements_by_tag_name_traverses_svg_template_contents() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();
      let v = eval_json(
        ctx.clone(),
        r##"
        (function () {
          document.body.innerHTML = "<svg><template><g id='inside'></g></template></svg><g id='outside'></g>";
          var tmpl = document.querySelector("svg template");
          var divs = tmpl.getElementsByTagName("g");
          return JSON.stringify({
            len: divs.length,
            ids: Array.from(divs).map(function (n) { return n.id; }).join(","),
          });
        })()
        "##,
      );

      assert_eq!(v["len"], 1);
      assert_eq!(v["ids"], "inside");
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
  fn htmlelement_and_specialized_prototypes() {
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
            divIsHTMLDivElement: div instanceof HTMLDivElement,
            divIsElement: div instanceof Element,
            divIsNode: div instanceof Node,
            divProtoIsHTMLDiv: Object.getPrototypeOf(div) === HTMLDivElement.prototype,

            inputIsHTMLElement: input instanceof HTMLElement,
            inputIsHTMLInputElement: input instanceof HTMLInputElement,
            inputIsElement: input instanceof Element,
            inputIsNode: input instanceof Node,
            inputProtoIsHTMLInput: Object.getPrototypeOf(input) === HTMLInputElement.prototype,

            textareaIsHTMLElement: textarea instanceof HTMLElement,
            textareaIsHTMLTextAreaElement: textarea instanceof HTMLTextAreaElement,
            textareaIsElement: textarea instanceof Element,
            textareaIsNode: textarea instanceof Node,
            textareaProtoIsHTMLTextArea: Object.getPrototypeOf(textarea) === HTMLTextAreaElement.prototype,

            selectIsHTMLElement: select instanceof HTMLElement,
            selectIsHTMLSelectElement: select instanceof HTMLSelectElement,
            selectIsElement: select instanceof Element,
            selectIsNode: select instanceof Node,
            selectProtoIsHTMLSelect: Object.getPrototypeOf(select) === HTMLSelectElement.prototype,

            formIsHTMLElement: form instanceof HTMLElement,
            formIsHTMLFormElement: form instanceof HTMLFormElement,
            formIsElement: form instanceof Element,
            formIsNode: form instanceof Node,
            formProtoIsHTMLForm: Object.getPrototypeOf(form) === HTMLFormElement.prototype,

            optionIsHTMLElement: option instanceof HTMLElement,
            optionIsHTMLOptionElement: option instanceof HTMLOptionElement,
            optionIsElement: option instanceof Element,
            optionIsNode: option instanceof Node,
            optionProtoIsHTMLOption: Object.getPrototypeOf(option) === HTMLOptionElement.prototype,

            headIsHTMLElement: document.head instanceof HTMLElement,
            bodyIsHTMLElement: document.body instanceof HTMLElement,
            documentElementIsHTMLElement: document.documentElement instanceof HTMLElement,

            ctorIllegal: {
              HTMLElement: throwsIllegalConstructor(HTMLElement),
              HTMLDivElement: throwsIllegalConstructor(HTMLDivElement),
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
      assert_eq!(v["divIsHTMLDivElement"], true);
      assert_eq!(v["divIsElement"], true);
      assert_eq!(v["divIsNode"], true);
      assert_eq!(v["divProtoIsHTMLDiv"], true);

      assert_eq!(v["inputIsHTMLInputElement"], true);
      assert_eq!(v["inputIsHTMLElement"], true);
      assert_eq!(v["inputIsElement"], true);
      assert_eq!(v["inputIsNode"], true);
      assert_eq!(v["inputProtoIsHTMLInput"], true);

      assert_eq!(v["textareaIsHTMLElement"], true);
      assert_eq!(v["textareaIsHTMLTextAreaElement"], true);
      assert_eq!(v["textareaIsElement"], true);
      assert_eq!(v["textareaIsNode"], true);
      assert_eq!(v["textareaProtoIsHTMLTextArea"], true);

      assert_eq!(v["selectIsHTMLElement"], true);
      assert_eq!(v["selectIsHTMLSelectElement"], true);
      assert_eq!(v["selectIsElement"], true);
      assert_eq!(v["selectIsNode"], true);
      assert_eq!(v["selectProtoIsHTMLSelect"], true);

      assert_eq!(v["formIsHTMLElement"], true);
      assert_eq!(v["formIsHTMLFormElement"], true);
      assert_eq!(v["formIsElement"], true);
      assert_eq!(v["formIsNode"], true);
      assert_eq!(v["formProtoIsHTMLForm"], true);

      assert_eq!(v["optionIsHTMLElement"], true);
      assert_eq!(v["optionIsHTMLOptionElement"], true);
      assert_eq!(v["optionIsElement"], true);
      assert_eq!(v["optionIsNode"], true);
      assert_eq!(v["optionProtoIsHTMLOption"], true);

      assert_eq!(v["headIsHTMLElement"], true);
      assert_eq!(v["bodyIsHTMLElement"], true);
      assert_eq!(v["documentElementIsHTMLElement"], true);

      assert_eq!(v["ctorIllegal"]["HTMLElement"], true);
      assert_eq!(v["ctorIllegal"]["HTMLDivElement"], true);
      assert_eq!(v["ctorIllegal"]["HTMLInputElement"], true);
      assert_eq!(v["ctorIllegal"]["HTMLTextAreaElement"], true);
      assert_eq!(v["ctorIllegal"]["HTMLSelectElement"], true);
      assert_eq!(v["ctorIllegal"]["HTMLFormElement"], true);
      assert_eq!(v["ctorIllegal"]["HTMLOptionElement"], true);
    });
  }

  #[test]
  fn form_control_properties_roundtrip() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();

      let v = eval_json(
        ctx.clone(),
        r#"
        (function () {
          var input = document.createElement("input");
          input.value = "hello";
          input.checked = true;
          input.disabled = true;
          var inputCheckedAfterTrue = input.checked;
          var inputDisabledAfterTrue = input.disabled;
          input.checked = false;
          input.disabled = false;
          var inputCheckedAfterFalse = input.checked;
          var inputDisabledAfterFalse = input.disabled;

          var textarea = document.createElement("textarea");
          textarea.value = "world";

          var select = document.createElement("select");
          var options = select.options;
          var optionsSame = options === select.options;
          var optionsIsHTMLOptionsCollection = options instanceof HTMLOptionsCollection;
          var optionsIsHTMLCollection = options instanceof HTMLCollection;
          var optionsLen0 = options.length;
          var optA = document.createElement("option");
          optA.value = "a";
          optA.textContent = "A";
          select.appendChild(optA);
          var optionsLen1 = options.length;
          var optB = document.createElement("option");
          optB.value = "b";
          optB.textContent = "B";
          select.appendChild(optB);
          var optionsLen2 = options.length;
          var options0IsOptA = options[0] === optA;
          var options1IsOptB = options[1] === optB;

          select.selectedIndex = 1;
          var selectSelectedIndexAfterSelectedIndex = select.selectedIndex;
          var selectValueAfterSelectedIndex = select.value;
          var optASelectedAfterSelectedIndex = optA.selected;
          var optBSelectedAfterSelectedIndex = optB.selected;
          var optASelectedAttrAfterSelectedIndex = optA.getAttribute("selected") !== null;
          var optBSelectedAttrAfterSelectedIndex = optB.getAttribute("selected") !== null;

          select.value = "a";
          var selectSelectedIndexAfterValue = select.selectedIndex;
          var selectValueAfterValue = select.value;
          var optASelectedAfterValue = optA.selected;
          var optBSelectedAfterValue = optB.selected;
          var optASelectedAttrAfterValue = optA.getAttribute("selected") !== null;
          var optBSelectedAttrAfterValue = optB.getAttribute("selected") !== null;

          // Ensure options collection is live when nodes are removed (remove the non-selected option).
          select.removeChild(optB);
          var optionsLenAfterRemove = options.length;
          var options0IsOptAAfterRemove = options[0] === optA;
          var optionsItem0IsOptAAfterRemove = options.item(0) === optA;
          var optionsItemNeg = options.item(-1);
          var optionsItem99 = options.item(99);
          var selectValueAfterRemove = select.value;

          var form = document.createElement("form");
          var elements = form.elements;
          var elementsSame = elements === form.elements;
          var elementsIsHTMLFormControlsCollection = elements instanceof HTMLFormControlsCollection;
          var elementsIsHTMLCollection = elements instanceof HTMLCollection;
          var formLen0 = elements.length;
          form.appendChild(input);
          var formLen1 = elements.length;
          form.appendChild(textarea);
          form.appendChild(select);
          var formLen3 = elements.length;
          var elements0IsInput = elements[0] === input;
          var elements1IsTextarea = elements[1] === textarea;
          var elements2IsSelect = elements[2] === select;
          var elementsOrder = Array.from(elements).map(function (n) { return n.tagName; }).join(",");

          form.removeChild(textarea);
          var formLen2AfterRemove = elements.length;
          var elements0IsInputAfterRemove = elements[0] === input;
          var elements1IsSelectAfterRemove = elements[1] === select;
          var elementsOrderAfterRemove = Array.from(elements).map(function (n) { return n.tagName; }).join(",");
          var elementsItem0IsInput = elements.item(0) === input;
          var elementsItemNeg = elements.item(-1);
          var elementsItem99 = elements.item(99);

          var submitIsFunction = typeof form.submit === "function";
          var resetIsFunction = typeof form.reset === "function";
          var submitOk;
          try {
            form.submit();
            submitOk = true;
          } catch (e) {
            submitOk = false;
          }
          var resetOk;
          try {
            form.reset();
            resetOk = true;
          } catch (e) {
            resetOk = false;
          }

          return JSON.stringify({
            inputValue: input.value,
            inputCheckedAfterTrue: inputCheckedAfterTrue,
            inputDisabledAfterTrue: inputDisabledAfterTrue,
            inputCheckedAfterFalse: inputCheckedAfterFalse,
            inputDisabledAfterFalse: inputDisabledAfterFalse,

            textareaValue: textarea.value,
            textareaTextContent: textarea.textContent,

            optionsSame: optionsSame,
            optionsIsHTMLOptionsCollection: optionsIsHTMLOptionsCollection,
            optionsIsHTMLCollection: optionsIsHTMLCollection,
            optionsLen0: optionsLen0,
            optionsLen1: optionsLen1,
            optionsLen2: optionsLen2,
            options0IsOptA: options0IsOptA,
            options1IsOptB: options1IsOptB,
            optionsLenAfterRemove: optionsLenAfterRemove,
            options0IsOptAAfterRemove: options0IsOptAAfterRemove,
            optionsItem0IsOptAAfterRemove: optionsItem0IsOptAAfterRemove,
            optionsItemNeg: optionsItemNeg,
            optionsItem99: optionsItem99,
            selectValueAfterRemove: selectValueAfterRemove,

            selectSelectedIndexAfterSelectedIndex: selectSelectedIndexAfterSelectedIndex,
            selectValueAfterSelectedIndex: selectValueAfterSelectedIndex,
            optASelectedAfterSelectedIndex: optASelectedAfterSelectedIndex,
            optBSelectedAfterSelectedIndex: optBSelectedAfterSelectedIndex,
            optASelectedAttrAfterSelectedIndex: optASelectedAttrAfterSelectedIndex,
            optBSelectedAttrAfterSelectedIndex: optBSelectedAttrAfterSelectedIndex,
            selectSelectedIndexAfterValue: selectSelectedIndexAfterValue,
            selectValueAfterValue: selectValueAfterValue,
            optASelectedAfterValue: optASelectedAfterValue,
            optBSelectedAfterValue: optBSelectedAfterValue,
            optASelectedAttrAfterValue: optASelectedAttrAfterValue,
            optBSelectedAttrAfterValue: optBSelectedAttrAfterValue,

            elementsSame: elementsSame,
            elementsIsHTMLFormControlsCollection: elementsIsHTMLFormControlsCollection,
            elementsIsHTMLCollection: elementsIsHTMLCollection,
            formLen0: formLen0,
            formLen1: formLen1,
            formLen3: formLen3,
            elements0IsInput: elements0IsInput,
            elements1IsTextarea: elements1IsTextarea,
            elements2IsSelect: elements2IsSelect,
            elementsOrder: elementsOrder,
            formLen2AfterRemove: formLen2AfterRemove,
            elements0IsInputAfterRemove: elements0IsInputAfterRemove,
            elements1IsSelectAfterRemove: elements1IsSelectAfterRemove,
            elementsOrderAfterRemove: elementsOrderAfterRemove,
            elementsItem0IsInput: elementsItem0IsInput,
            elementsItemNeg: elementsItemNeg,
            elementsItem99: elementsItem99,

            submitIsFunction: submitIsFunction,
            resetIsFunction: resetIsFunction,
            submitOk: submitOk,
            resetOk: resetOk,
          });
        })()
        "#,
      );

      assert_eq!(v["inputValue"], "hello");
      assert_eq!(v["inputCheckedAfterTrue"], true);
      assert_eq!(v["inputDisabledAfterTrue"], true);
      assert_eq!(v["inputCheckedAfterFalse"], false);
      assert_eq!(v["inputDisabledAfterFalse"], false);

      assert_eq!(v["textareaValue"], "world");
      assert_eq!(v["textareaTextContent"], "world");

      assert_eq!(v["optionsSame"], true);
      assert_eq!(v["optionsIsHTMLOptionsCollection"], true);
      assert_eq!(v["optionsIsHTMLCollection"], true);
      assert_eq!(v["optionsLen0"], 0);
      assert_eq!(v["optionsLen1"], 1);
      assert_eq!(v["optionsLen2"], 2);
      assert_eq!(v["options0IsOptA"], true);
      assert_eq!(v["options1IsOptB"], true);
      assert_eq!(v["optionsLenAfterRemove"], 1);
      assert_eq!(v["options0IsOptAAfterRemove"], true);
      assert_eq!(v["optionsItem0IsOptAAfterRemove"], true);
      assert!(v["optionsItemNeg"].is_null(), "options.item(-1) should return null");
      assert!(v["optionsItem99"].is_null(), "options.item(99) should return null");
      assert_eq!(v["selectValueAfterRemove"], "a");

      assert_eq!(v["selectSelectedIndexAfterSelectedIndex"], 1);
      assert_eq!(v["selectValueAfterSelectedIndex"], "b");
      assert_eq!(v["optASelectedAfterSelectedIndex"], false);
      assert_eq!(v["optBSelectedAfterSelectedIndex"], true);
      assert_eq!(v["optASelectedAttrAfterSelectedIndex"], false);
      assert_eq!(v["optBSelectedAttrAfterSelectedIndex"], true);
      assert_eq!(v["selectSelectedIndexAfterValue"], 0);
      assert_eq!(v["selectValueAfterValue"], "a");
      assert_eq!(v["optASelectedAfterValue"], true);
      assert_eq!(v["optBSelectedAfterValue"], false);
      assert_eq!(v["optASelectedAttrAfterValue"], true);
      assert_eq!(v["optBSelectedAttrAfterValue"], false);

      assert_eq!(v["elementsSame"], true);
      assert_eq!(v["elementsIsHTMLFormControlsCollection"], true);
      assert_eq!(v["elementsIsHTMLCollection"], true);
      assert_eq!(v["formLen0"], 0);
      assert_eq!(v["formLen1"], 1);
      assert_eq!(v["formLen3"], 3);
      assert_eq!(v["elements0IsInput"], true);
      assert_eq!(v["elements1IsTextarea"], true);
      assert_eq!(v["elements2IsSelect"], true);
      assert_eq!(v["elementsOrder"], "INPUT,TEXTAREA,SELECT");
      assert_eq!(v["formLen2AfterRemove"], 2);
      assert_eq!(v["elements0IsInputAfterRemove"], true);
      assert_eq!(v["elements1IsSelectAfterRemove"], true);
      assert_eq!(v["elementsOrderAfterRemove"], "INPUT,SELECT");
      assert_eq!(v["elementsItem0IsInput"], true);
      assert!(v["elementsItemNeg"].is_null(), "elements.item(-1) should return null");
      assert!(v["elementsItem99"].is_null(), "elements.item(99) should return null");

      assert_eq!(v["submitIsFunction"], true);
      assert_eq!(v["resetIsFunction"], true);
      assert_eq!(v["submitOk"], true);
      assert_eq!(v["resetOk"], true);
    });
  }

  #[test]
  fn form_elements_skips_template_contents_and_includes_nested_controls() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();

      let v = eval_json(
        ctx.clone(),
        r#"
        (function () {
          var form = document.createElement("form");
          var elements = form.elements;

          var outer = document.createElement("input");
          form.appendChild(outer);
          var len1 = elements.length;

          var container = document.createElement("div");
          form.appendChild(container);
          var nested = document.createElement("input");
          container.appendChild(nested);
          var len2 = elements.length;

          var tmpl = document.createElement("template");
          form.appendChild(tmpl);
          var insideTemplate = document.createElement("input");
          tmpl.appendChild(insideTemplate);
          // Template contents should be inert for form.elements.
          var len2Still = elements.length;

          // Moving the control out of the template should make it appear.
          tmpl.removeChild(insideTemplate);
          form.appendChild(insideTemplate);
          var len3 = elements.length;
          var orderAfterLen3 = Array.from(elements).map(function (n) { return n === outer ? "outer" : (n === insideTemplate ? "inside" : "other"); }).join(",");

          // Removing a nested control should remove it from the collection.
          container.removeChild(nested);
          var len2Again = elements.length;

          return JSON.stringify({
            len1: len1,
            len2: len2,
            len2Still: len2Still,
            len3: len3,
            len2Again: len2Again,
            orderAfterLen3: orderAfterLen3,
          });
        })()
        "#,
      );

      assert_eq!(v["len1"], 1);
      assert_eq!(v["len2"], 2);
      assert_eq!(v["len2Still"], 2);
      assert_eq!(v["len3"], 3);
      assert_eq!(v["orderAfterLen3"], "outer,other,inside");
      assert_eq!(v["len2Again"], 2);
    });
  }

  #[test]
  fn option_selected_property_affects_select_selection() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();

      let v = eval_json(
        ctx.clone(),
        r#"
        (function () {
          var select = document.createElement("select");
          var optA = document.createElement("option");
          optA.textContent = "A";
          select.appendChild(optA);

          var optB = document.createElement("option");
          optB.value = "b";
          optB.textContent = "B";
          select.appendChild(optB);

          var optAValue = optA.value;
          var optBValue = optB.value;
          var defaultSelectedIndex = select.selectedIndex;
          var defaultSelectValue = select.value;

          optB.selected = true;
          var afterSetSelectedIndex = select.selectedIndex;
          var afterSetSelectValue = select.value;
          var optBSelectedAfterTrue = optB.selected;
          var optBSelectedAttrAfterTrue = optB.getAttribute("selected") !== null;

          optB.selected = false;
          var afterClearSelectedIndex = select.selectedIndex;
          var afterClearSelectValue = select.value;
          var optBSelectedAfterFalse = optB.selected;
          var optBSelectedAttrAfterFalse = optB.getAttribute("selected") !== null;

          return JSON.stringify({
            optAValue: optAValue,
            optBValue: optBValue,
            defaultSelectedIndex: defaultSelectedIndex,
            defaultSelectValue: defaultSelectValue,
            afterSetSelectedIndex: afterSetSelectedIndex,
            afterSetSelectValue: afterSetSelectValue,
            optBSelectedAfterTrue: optBSelectedAfterTrue,
            optBSelectedAttrAfterTrue: optBSelectedAttrAfterTrue,
            afterClearSelectedIndex: afterClearSelectedIndex,
            afterClearSelectValue: afterClearSelectValue,
            optBSelectedAfterFalse: optBSelectedAfterFalse,
            optBSelectedAttrAfterFalse: optBSelectedAttrAfterFalse,
          });
        })()
        "#,
      );

      assert_eq!(v["optAValue"], "A");
      assert_eq!(v["optBValue"], "b");
      assert_eq!(v["defaultSelectedIndex"], 0);
      assert_eq!(v["defaultSelectValue"], "A");

      assert_eq!(v["afterSetSelectedIndex"], 1);
      assert_eq!(v["afterSetSelectValue"], "b");
      assert_eq!(v["optBSelectedAfterTrue"], true);
      assert_eq!(v["optBSelectedAttrAfterTrue"], true);

      assert_eq!(v["afterClearSelectedIndex"], 0);
      assert_eq!(v["afterClearSelectValue"], "A");
      assert_eq!(v["optBSelectedAfterFalse"], false);
      assert_eq!(v["optBSelectedAttrAfterFalse"], false);
    });
  }

  #[test]
  fn input_attributes_reflect_to_properties() {
    let rt = Runtime::new().unwrap();
    let context = Context::full(&rt).unwrap();
    context.with(|ctx| {
      install_dom_shims(ctx.clone(), &ctx.globals()).unwrap();

      let v = eval_json(
        ctx.clone(),
        r#"
        (function () {
          var input = document.createElement("input");

          // value: reflect attribute <-> property, defaulting to "" when absent.
          var valueDefault = input.value;
          input.setAttribute("value", "x");
          var valueAfterSetAttr = input.value;
          input.removeAttribute("value");
          var valueAfterRemoveAttr = input.value;
          input.value = "y";
          var valueAttrAfterSetProp = input.getAttribute("value");

          // checked: presence/absence of attribute.
          var checkedDefault = input.checked;
          input.setAttribute("checked", "");
          var checkedAfterSetAttr = input.checked;
          input.removeAttribute("checked");
          var checkedAfterRemoveAttr = input.checked;
          input.checked = true;
          var checkedAttrAfterSetProp = input.getAttribute("checked") !== null;
          input.checked = false;
          var checkedAttrAfterClearProp = input.getAttribute("checked") !== null;

          // disabled: presence/absence of attribute.
          var disabledDefault = input.disabled;
          input.setAttribute("disabled", "");
          var disabledAfterSetAttr = input.disabled;
          input.removeAttribute("disabled");
          var disabledAfterRemoveAttr = input.disabled;
          input.disabled = true;
          var disabledAttrAfterSetProp = input.getAttribute("disabled") !== null;
          input.disabled = false;
          var disabledAttrAfterClearProp = input.getAttribute("disabled") !== null;

          return JSON.stringify({
            valueDefault: valueDefault,
            valueAfterSetAttr: valueAfterSetAttr,
            valueAfterRemoveAttr: valueAfterRemoveAttr,
            valueAttrAfterSetProp: valueAttrAfterSetProp,
            checkedDefault: checkedDefault,
            checkedAfterSetAttr: checkedAfterSetAttr,
            checkedAfterRemoveAttr: checkedAfterRemoveAttr,
            checkedAttrAfterSetProp: checkedAttrAfterSetProp,
            checkedAttrAfterClearProp: checkedAttrAfterClearProp,
            disabledDefault: disabledDefault,
            disabledAfterSetAttr: disabledAfterSetAttr,
            disabledAfterRemoveAttr: disabledAfterRemoveAttr,
            disabledAttrAfterSetProp: disabledAttrAfterSetProp,
            disabledAttrAfterClearProp: disabledAttrAfterClearProp,
          });
        })()
        "#,
      );

      assert_eq!(v["valueDefault"], "");
      assert_eq!(v["valueAfterSetAttr"], "x");
      assert_eq!(v["valueAfterRemoveAttr"], "");
      assert_eq!(v["valueAttrAfterSetProp"], "y");

      assert_eq!(v["checkedDefault"], false);
      assert_eq!(v["checkedAfterSetAttr"], true);
      assert_eq!(v["checkedAfterRemoveAttr"], false);
      assert_eq!(v["checkedAttrAfterSetProp"], true);
      assert_eq!(v["checkedAttrAfterClearProp"], false);

      assert_eq!(v["disabledDefault"], false);
      assert_eq!(v["disabledAfterSetAttr"], true);
      assert_eq!(v["disabledAfterRemoveAttr"], false);
      assert_eq!(v["disabledAttrAfterSetProp"], true);
      assert_eq!(v["disabledAttrAfterClearProp"], false);
    });
  }
}
