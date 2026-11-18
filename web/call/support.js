// SPDX-License-Identifier: EUPL-1.2
// What this browser brings to a call: encoded transforms for the stream worker's
// SFrame, and the royalty-free codecs it decodes in WebRTC (docs/video-plan.md §6).

/** `{codecs}` with the call codecs this browser decodes, AV1 first, or `{refusal}`. */
export function support(scope = globalThis) {
  if (typeof scope.RTCRtpScriptTransform !== "function") return { refusal: "transforms" };
  const decoded = scope.RTCRtpReceiver?.getCapabilities?.("video")?.codecs ?? [];
  const types = new Set(decoded.map((codec) => codec.mimeType.toLowerCase()));
  const codecs = ["av1", "vp9"].filter((name) => types.has(`video/${name}`));
  return codecs.length ? { codecs } : { refusal: "codecs" };
}

const kept = {
  video: ["video/av1", "video/vp9", "video/rtx", "video/red", "video/ulpfec", "video/flexfec-03"],
  audio: ["audio/opus", "audio/red"],
};

/** A transceiver's codecs for `kind`: AV1, VP9 and Opus with their repair formats. */
export function preferences(scope, kind) {
  const codecs = scope.RTCRtpReceiver.getCapabilities(kind)?.codecs ?? [];
  const rank = (codec) => kept[kind].indexOf(codec.mimeType.toLowerCase());
  return codecs.filter((codec) => rank(codec) >= 0).sort((a, b) => rank(a) - rank(b));
}
