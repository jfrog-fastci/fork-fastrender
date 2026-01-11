// The harness should surface Promise rejections with the rejection reason stringified.
//
// This rejects on a microtask (similar to `await Promise.reject("nope")` inside an async function).
Promise.resolve().then(function () {
  throw "nope";
})
