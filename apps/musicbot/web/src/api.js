// SPDX-License-Identifier: EUPL-1.2
// HTTP helpers. Every URL is relative to the token-free page root, which the
// bot authenticates with its cookie, so the app also works when the bot sits
// behind a proxy under a path.
import { isImportId } from "./logic.js";

export class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function failure(response) {
  let message = `${response.status} ${response.statusText}`.trim();
  try {
    const body = await response.json();
    if (typeof body?.error === "string" && body.error) message = body.error;
  } catch {
    // The body was not JSON; keep the status text.
  }
  return new HttpError(response.status, message);
}

/** GETs JSON from `path`; throws HttpError with the server's `error` text on failure. */
export async function getJSON(path, signal) {
  const response = await fetch(path, { signal, headers: { Accept: "application/json" } });
  if (!response.ok) throw await failure(response);
  return response.json();
}

async function post(path, body, signal) {
  const response = await fetch(path, {
    method: "POST",
    signal,
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw await failure(response);
  return response;
}

/** POSTs JSON to `path`; throws HttpError with the server's `error` text on failure. */
export async function postJSON(path, body, signal) {
  await post(path, body, signal);
}

/** POSTs JSON to `path` and returns the JSON reply; throws HttpError with the server's `error` text on failure. */
export async function postForJSON(path, body, signal) {
  const response = await post(path, body, signal);
  return response.json();
}

/** Query string for library calls. */
export function query(path, params) {
  return `${path}?${new URLSearchParams(Object.entries(params).map(([key, value]) => [key, String(value)]))}`;
}

/**
 * URL of a cover image `size` pixels wide. `key` is the library's `library_keys`
 * entry; with it the browser may keep the answer, since the URL changes when
 * the library does.
 */
export function artUrl(library, coverArt, size, key) {
  const url = `art/${library}/${encodeURIComponent(coverArt)}?size=${size}`;
  return typeof key === "string" && key ? `${url}&v=${encodeURIComponent(key)}` : url;
}

/** URL of an import service cover `size` pixels wide, or null for an invalid id. */
export function importArtUrl(id, size) {
  return isImportId(id) ? `import-art/${id}?size=${size}` : null;
}
