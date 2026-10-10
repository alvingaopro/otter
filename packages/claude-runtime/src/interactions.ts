// Questions the worker is waiting on otterd for — a policy check before a
// tool runs, a permission or a question form — each by its exact id. A
// reply resolves exactly one; a cancelled turn or run rejects them all, and
// a rejection is never turned into an "allow".

export class Pending<T> {
  private waiting = new Map<string, { resolve: (v: T) => void; reject: (e: Error) => void }>();

  /** Wait for the reply to `id`; aborted (or cancelled) → rejected. */
  wait(id: string, signal?: AbortSignal): Promise<T> {
    if (this.waiting.has(id)) return Promise.reject(new Error(`already waiting for ${id}`));
    return new Promise<T>((resolve, reject) => {
      const done = () => this.waiting.delete(id);
      const onAbort = () => {
        done();
        reject(new Error("aborted"));
      };
      if (signal?.aborted) return onAbort();
      signal?.addEventListener("abort", onAbort, { once: true });
      this.waiting.set(id, {
        resolve: (v) => {
          signal?.removeEventListener("abort", onAbort);
          done();
          resolve(v);
        },
        reject: (e) => {
          signal?.removeEventListener("abort", onAbort);
          done();
          reject(e);
        },
      });
    });
  }

  /** Reply to `id`. False when nothing waits for it (late, or unknown). */
  resolve(id: string, value: T): boolean {
    const w = this.waiting.get(id);
    if (!w) return false;
    w.resolve(value);
    return true;
  }

  /** Reject everything still waiting. */
  cancelAll(why: string) {
    for (const w of [...this.waiting.values()]) w.reject(new Error(why));
  }

  get size() {
    return this.waiting.size;
  }
}
