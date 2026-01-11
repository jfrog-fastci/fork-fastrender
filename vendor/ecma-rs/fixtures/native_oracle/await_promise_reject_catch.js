// Returns a Promise<string> and ensures awaited Promise rejections re-enter the async function as a
// throw completion at the await site (so `try/catch` can recover).
(async () => {
  try {
    await Promise.reject("boom");
    return "bad";
  } catch (e) {
    return "caught:" + e;
  }
})()
