if (globalThis.__step === "one") {
  setClass("box", "step2");
} else {
  // If the defer scripts execute out of order, leave the box red.
  setClass("box", "off");
}

