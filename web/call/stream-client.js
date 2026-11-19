// SPDX-License-Identifier: EUPL-1.2
// The page's side of the stream worker (stream.worker.js): requests in the message
// interface of crates/stream-wasm, answered in order. The page sees the worker's
// replies only: room ids, proofs, encrypted key messages and key events.

const WORKER = "/stream.worker.js";
// The stream host requires Trusted Types for script URLs; this policy admits the
// stream worker only.
const policy = globalThis.trustedTypes?.createPolicy("nuschl-stream-worker", {
  createScriptURL(url) {
    if (url !== WORKER) throw new TypeError("not the stream worker");
    return url;
  },
});

export class StreamClient {
  #pending = new Map();
  #next = 0;

  /** `onfatal` runs once when the worker ends. */
  constructor(onfatal = () => {}) {
    this.worker = new Worker(policy ? policy.createScriptURL(WORKER) : WORKER, { type: "module" });
    const fail = () => {
      for (const { reject } of this.#pending.values()) reject("worker ended");
      this.#pending.clear();
      this.worker.terminate();
      onfatal();
      onfatal = () => {};
    };
    this.worker.addEventListener("message", ({ data }) => {
      if ("fatal" in data) return fail();
      const pending = this.#pending.get(data.id);
      if (!pending) return;
      this.#pending.delete(data.id);
      if ("error" in data) pending.reject(data.error);
      else pending.resolve(JSON.parse(data.reply));
    });
    this.worker.addEventListener("error", fail);
  }

  /** The reply to `request` (`{kind, …}`); rejects with the worker's error. */
  request(request) {
    const id = ++this.#next;
    this.worker.postMessage({ id, request: JSON.stringify({ v: 1, ...request }) });
    return new Promise((resolve, reject) => this.#pending.set(id, { resolve, reject }));
  }
}
