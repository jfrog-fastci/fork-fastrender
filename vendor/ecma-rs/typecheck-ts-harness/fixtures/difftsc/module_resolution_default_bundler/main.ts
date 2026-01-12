// @lib: es5
// @module: esnext
// Intentionally omit `@moduleResolution` to assert the computed default (TS 5.9: bundler for esnext).
import { mode } from "pkg";

export const resolved = mode;
resolved;
// ^?
