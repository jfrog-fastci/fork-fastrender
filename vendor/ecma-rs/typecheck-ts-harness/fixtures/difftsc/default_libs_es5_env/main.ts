// @target: ES5

// `lib.d.ts` (the ES5 default lib entrypoint) should pull in the DOM,
// `webworker.importscripts`, and `scripthost` environment libs.
document.createElement("div");
importScripts("a.js");
WScript;
