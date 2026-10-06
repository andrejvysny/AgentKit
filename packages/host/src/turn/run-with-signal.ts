/** Cancellation must settle the turn even when a host hook ignores its signal. */
export async function runWithSignal<T>(
  signal: AbortSignal,
  run: () => Promise<T>,
): Promise<T> {
  signal.throwIfAborted();
  let onAbort = (): void => {};
  const cancelled = new Promise<never>((_resolve, reject) => {
    onAbort = () => reject(signal.reason);
    signal.addEventListener("abort", onAbort, { once: true });
  });
  try {
    return await Promise.race([
      Promise.resolve().then(() => {
        signal.throwIfAborted();
        return run();
      }),
      cancelled,
    ]);
  } finally {
    signal.removeEventListener("abort", onAbort);
  }
}
