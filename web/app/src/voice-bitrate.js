// SPDX-License-Identifier: EUPL-1.2
// The requested voice bitrate (nuschl_core::bitrate). The SDK worker resolves
// the request against the authenticated server's budget and words the result
// in the voice view's `bitrate_line` and `bitrate_why`; this stores the request.
export const VOICE_BITRATES = [64000, 96000, 128000, 192000, 256000];
export const DEFAULT_VOICE_BITRATE = 128000;
export const VOICE_BITRATE_KEY = "nuschl.voice.bitrate.v1";

export function kbps(bits) {
  return `${bits % 1000 === 0 ? bits / 1000 : (bits / 1000).toFixed(1)} kb/s`;
}

/** A stored request, or the default for anything else. */
export function storedVoiceBitrate(storage) {
  try {
    const value = Number(storage?.getItem(VOICE_BITRATE_KEY));
    return VOICE_BITRATES.includes(value) ? value : DEFAULT_VOICE_BITRATE;
  } catch {
    return DEFAULT_VOICE_BITRATE;
  }
}

/** The stored request in this browser, the default without storage. */
export function initialVoiceBitrate() {
  try { return storedVoiceBitrate(globalThis.localStorage); } catch { return DEFAULT_VOICE_BITRATE; }
}

/** False when the browser refuses to store it (private browsing). */
export function storeVoiceBitrate(storage, value) {
  if (!VOICE_BITRATES.includes(value)) return false;
  try { storage.setItem(VOICE_BITRATE_KEY, String(value)); return true; } catch { return false; }
}
