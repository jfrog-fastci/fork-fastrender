// Minimal EventTarget/Event polyfill for FastRender's offline WPT DOM corpus.
//
// This is *not* a full WHATWG DOM implementation; it exists to keep the curated WPT corpus runnable
// under the QuickJS backend while the real DOM bindings are still being wired up.
//
// The polyfill is idempotent and will not override native `EventTarget`/`Event` if the backend
// provides them.
(function () {
  var g = typeof globalThis !== "undefined" ? globalThis : this;
  if (typeof g.EventTarget === "function" && typeof g.Event === "function") return;

  function isCallable(x) {
    return typeof x === "function";
  }

  function toListener(callback) {
    if (callback === null || callback === undefined) return null;
    if (isCallable(callback)) return callback;
    if (
      typeof callback === "object" &&
      callback !== null &&
      isCallable(callback.handleEvent)
    ) {
      return function (event) {
        return callback.handleEvent(event);
      };
    }
    return null;
  }

  function normalizeOptions(options) {
    if (options === true) {
      return { capture: true, once: false, passive: false };
    }
    if (options === false || options === undefined || options === null) {
      return { capture: false, once: false, passive: false };
    }
    var capture = !!options.capture;
    var once = !!options.once;
    var passive = !!options.passive;
    return { capture: capture, once: once, passive: passive };
  }

  function Event(type, init) {
    if (typeof type !== "string") type = String(type);
    init = init || {};
    this.type = type;
    this.bubbles = !!init.bubbles;
    this.cancelable = !!init.cancelable;
    this.defaultPrevented = false;
    this.target = null;
    this.currentTarget = null;
    // Internal flag set by the dispatcher.
    this.__inPassiveListener = false;
  }

  Event.prototype.preventDefault = function () {
    if (!this.cancelable) return;
    if (this.__inPassiveListener) return;
    this.defaultPrevented = true;
  };

  function EventTarget() {
    // type -> [{ listener, original, capture, once, passive }]
    this.__listeners = Object.create(null);
  }

  EventTarget.prototype.addEventListener = function (type, callback, options) {
    if (typeof type !== "string") type = String(type);
    var listener = toListener(callback);
    if (listener === null) return;

    var opts = normalizeOptions(options);
    var list = this.__listeners[type];
    if (!list) {
      list = [];
      this.__listeners[type] = list;
    }

    // Deduplicate on (callback, capture) per DOM.
    for (var i = 0; i < list.length; i++) {
      var rec = list[i];
      if (rec.original === callback && rec.capture === opts.capture) {
        return;
      }
    }
    list.push({
      listener: listener,
      original: callback,
      capture: opts.capture,
      once: opts.once,
      passive: opts.passive
    });
  };

  EventTarget.prototype.removeEventListener = function (type, callback, options) {
    if (typeof type !== "string") type = String(type);
    var opts = normalizeOptions(options);
    var list = this.__listeners[type];
    if (!list) return;
    for (var i = list.length - 1; i >= 0; i--) {
      var rec = list[i];
      if (rec.original === callback && rec.capture === opts.capture) {
        list.splice(i, 1);
      }
    }
    if (list.length === 0) {
      delete this.__listeners[type];
    }
  };

  EventTarget.prototype.dispatchEvent = function (event) {
    if (!event || typeof event.type !== "string") {
      throw new TypeError("dispatchEvent: event is not a valid Event");
    }

    event.target = this;
    event.currentTarget = this;

    var list = this.__listeners[event.type];
    if (!list || list.length === 0) {
      return !event.defaultPrevented;
    }

    // Snapshot listeners to avoid mutation during dispatch affecting the current dispatch.
    var snapshot = list.slice();

    // At-target capture listeners, then at-target bubble listeners.
    for (var phase = 0; phase < 2; phase++) {
      var capturePhase = phase === 0;
      for (var i = 0; i < snapshot.length; i++) {
        var rec = snapshot[i];
        if (!!rec.capture !== capturePhase) continue;

        var prevPassive = event.__inPassiveListener;
        event.__inPassiveListener = !!rec.passive;
        try {
          rec.listener.call(this, event);
        } finally {
          event.__inPassiveListener = prevPassive;
        }

        if (rec.once) {
          this.removeEventListener(event.type, rec.original, {
            capture: rec.capture
          });
        }
      }
    }

    return !event.defaultPrevented;
  };

  if (typeof g.Event !== "function") g.Event = Event;
  if (typeof g.EventTarget !== "function") g.EventTarget = EventTarget;
})();

