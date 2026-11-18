// SPDX-License-Identifier: EUPL-1.2
// Encrypted frames as the receive transform gets them. A sender's frame is one
// envelope byte and the SFrame ciphertext (crates/sframe); an AV1 packetizer reads
// the envelope as an OBU header without a size field. libwebrtc's AV1 depacketizer
// (Chromium, Firefox) adds that size field: the header gains bit 0x02 and a LEB128
// size follows. `unsized` restores the frame as sent.

const SIZE_FIELD = 0x02;
const SIZED = new Set([0x08 | SIZE_FIELD, 0x28 | SIZE_FIELD]);

/** The frame as sent, in place; any other frame unchanged. */
export function unsized(frame) {
  if (!SIZED.has(frame[0])) return frame;
  let size = 0;
  for (let index = 1, shift = 0; index < frame.length && shift < 56; index++, shift += 7) {
    const byte = frame[index];
    size += (byte & 0x7f) * 2 ** shift;
    if (byte & 0x80) continue;
    if (size !== frame.length - index - 1) return frame;
    frame[index] = frame[0] & ~SIZE_FIELD;
    return frame.subarray(index);
  }
  return frame;
}
