// SPDX-License-Identifier: EUPL-1.2
import { getJSON } from "./api.js";

class Resource {
  data = $state.raw(null);
  error = $state(null);
  loading = $state(true);
  revision = $state(0);

  reload() {
    this.revision++;
  }
}

/**
 * Loads JSON from the URL returned by `url()` and reloads whenever it changes;
 * a null URL loads nothing. An optional namespace signal aborts and clears old data
 * synchronously on invalidation, before a reconnect can expose another library.
 * With `keep`, a reload of the same URL (another dependency of `url()` changed)
 * shows the previous data until the new answer arrives.
 */
export function resource(url, signal = () => null, load = getJSON, { keep = false } = {}) {
  const r = new Resource();
  let shown = null;
  $effect(() => {
    r.revision;
    const path = url();
    const namespaceSignal = signal();
    if (path === null || namespaceSignal?.aborted) {
      shown = null;
      r.data = null;
      r.error = null;
      r.loading = false;
      return;
    }
    const controller = new AbortController();
    const abort = () => {
      controller.abort();
      r.data = null;
      r.error = null;
      r.loading = false;
    };
    namespaceSignal?.addEventListener("abort", abort, { once: true });
    r.loading = true;
    r.error = null;
    if (!keep || shown !== path) r.data = null;
    shown = path;
    load(path, controller.signal).then(
      (data) => {
        if (controller.signal.aborted) return;
        r.data = data;
        r.loading = false;
      },
      (error) => {
        if (controller.signal.aborted) return;
        r.error = error instanceof Error ? error.message : String(error);
        r.loading = false;
      },
    );
    return () => {
      namespaceSignal?.removeEventListener("abort", abort);
      controller.abort();
    };
  });
  return r;
}
