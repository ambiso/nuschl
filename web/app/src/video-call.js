// SPDX-License-Identifier: EUPL-1.2
import { Call } from "../../call/session.js";
import { StreamClient } from "../../call/stream-client.js";
import { support } from "../../call/support.js";

export const DEFAULT_CALL_HOST = "stream.nuschl.eu";
const invitation = /^#v=1&k=[\w-]{43}&h=[\w-]{43}$/;

export function configuredCallLink(link, host) {
  if (!host || typeof link !== "string" || link.length > 512) return null;
  try {
    const url = new URL(link);
    return url.origin === `https://${host}` && url.pathname === "/call" &&
      invitation.test(url.hash) && link === `${url.origin}${url.pathname}${url.hash}`
      ? link : null;
  } catch {
    return null;
  }
}

export function configuredCallHost() {
  const host = document.querySelector('meta[name="nuschl-call-host"]')?.content ?? DEFAULT_CALL_HOST;
  try {
    return host.length <= 259 && new URL(`https://${host}`).origin === `https://${host}` ? host : null;
  } catch {
    return null;
  }
}

export function createVideoCall(relay) {
  const found = support();
  let call;
  const worker = found.refusal ? null : new StreamClient(() => call?.end("lost"), () =>
    new Worker(`${import.meta.env.BASE_URL}video/stream.worker.js`, { type: "module" }));
  call = new Call(worker, { codecs: found.codecs ?? [], relay, allowDirect: false });
  if (found.refusal) call.end(found.refusal);
  return call;
}
