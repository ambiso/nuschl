// SPDX-License-Identifier: EUPL-1.2
import { test } from "node:test";
import assert from "node:assert/strict";
import { VOICE_BITRATES, VOICE_BITRATE_KEY, initialVoiceBitrate, storeVoiceBitrate, storedVoiceBitrate } from "./voice-bitrate.js";

function storage(initial = {}) {
  const values = new Map(Object.entries(initial));
  return {getItem: key => values.has(key) ? values.get(key) : null, setItem: (key, value) => values.set(key, value), values};
}

test("only the supported choices are stored and restored", () => {
  assert.deepEqual(VOICE_BITRATES, [64000, 96000, 128000, 192000, 256000]);
  assert.equal(storedVoiceBitrate(storage()), 128000);
  for (const bad of ["384000", "510000", "6000", "abc", "", "1e9"]) assert.equal(storedVoiceBitrate(storage({[VOICE_BITRATE_KEY]: bad})), 128000);
  const store = storage();
  assert.equal(storeVoiceBitrate(store, 384000), false);
  assert.equal(store.values.size, 0);
  assert.equal(storeVoiceBitrate(store, 256000), true);
  assert.equal(storedVoiceBitrate(store), 256000);
  const refusing = {getItem: () => { throw new Error("denied"); }, setItem: () => { throw new Error("denied"); }};
  assert.equal(storedVoiceBitrate(refusing), 128000);
  assert.equal(storeVoiceBitrate(refusing, 96000), false);
  // Without local storage at all (Node here, private modes in browsers).
  assert.equal(initialVoiceBitrate(), 128000);
});
