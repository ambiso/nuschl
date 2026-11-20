// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import { configuredCallLink } from "./video-call.js";

test("only a canonical guest invitation to the configured call host opens the panel", () => {
  const host = "stream.example:8443";
  const id = "A".repeat(43);
  const invitation = `https://${host}/call#v=1&k=${id}&h=${id}`;
  assert.equal(configuredCallLink(invitation, host), invitation);
  for (const wrong of [
    invitation.replace("https:", "http:"),
    invitation.replace(host, "stream.example"),
    invitation.replace(host, "other.example"),
    invitation.replace(host, "creds@" + host),
    invitation.replace("/call#", "/unexpected#"),
    invitation.replace("/call#", "/call?redirect=x#"),
    invitation + "&p=" + id,
    " " + invitation,
  ]) assert.equal(configuredCallLink(wrong, host), null, wrong);
});
