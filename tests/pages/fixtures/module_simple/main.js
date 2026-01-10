import "./dep.js";

if (globalThis.__dep_loaded) {
  document.documentElement.className = "js-enabled";
}

