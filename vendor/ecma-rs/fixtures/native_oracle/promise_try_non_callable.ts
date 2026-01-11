// EXPECT: TypeError
globalThis.__native_result = "pending";

// Promise.try should throw a TypeError if the callback is not callable.
try {
  Promise.try(123);
  globalThis.__native_result = "no-throw";
} catch (e) {
  globalThis.__native_result = e.name;
}
