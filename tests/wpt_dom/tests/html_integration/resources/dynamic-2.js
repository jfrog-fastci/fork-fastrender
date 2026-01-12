if (globalThis.__dynamic1_ran !== true) {
  if (globalThis.order_error === "") {
    globalThis.order_error = "dynamic-2 executed before dynamic-1";
  }
}
globalThis.log.push("dynamic-2");
