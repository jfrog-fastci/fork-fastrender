if (globalThis.__step === "one") {
  document.getElementById("root").className = "step2";
} else {
  // If the defer scripts execute out of order, leave the box red.
  document.getElementById("root").className = "off";
}
