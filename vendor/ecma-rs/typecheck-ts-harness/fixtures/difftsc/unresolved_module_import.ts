import { foo } from "./missing";

// TypeScript reports the resolution error at the import site, but then treats the
// imported binding as an error type that behaves like `any`, avoiding cascaded
// type errors on subsequent uses.
const n: number = foo;

