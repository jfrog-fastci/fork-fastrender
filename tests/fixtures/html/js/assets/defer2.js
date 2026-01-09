if (globalThis.__step === "one") {
  setClass("root", "step2");
} else {
  // If the defer scripts execute out of order, leave the box red.
  setClass("root", "off");
}
