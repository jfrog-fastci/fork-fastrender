// Returns a Promise<string> that settles during a microtask checkpoint.
(async () => {
  return await Promise.resolve("ok");
})()

