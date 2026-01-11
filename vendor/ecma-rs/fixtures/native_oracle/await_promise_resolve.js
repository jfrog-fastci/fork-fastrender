// Returns a Promise<string> that settles during a microtask checkpoint.
//
// This is equivalent to:
//   (async () => await Promise.resolve("ok"))()
// and ensures the oracle VM supports `async`/`await` syntax.
(async () => await Promise.resolve("ok"))()
