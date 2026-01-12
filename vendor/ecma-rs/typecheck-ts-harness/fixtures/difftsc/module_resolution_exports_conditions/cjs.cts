// @moduleResolution: nodenext
// @module: nodenext

import { mode, value } from "pkg-conditional";
import { branding } from "pkg-typings";

mode;
// ^?
value;
// ^?

branding;
// ^?

const brand = branding;
brand;
// ^?
