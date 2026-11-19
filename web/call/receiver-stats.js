// SPDX-License-Identifier: EUPL-1.2

const INTERVAL_MS = 1000;
const MAX_SAMPLE_SPAN_MS = 5 * INTERVAL_MS;

const delta = (before, after) =>
  Number.isFinite(before) && Number.isFinite(after) && before >= 0 && after >= before
    ? after - before : undefined;

export function receiverRates(previous, current) {
  const span = current.timestamp - previous?.timestamp;
  if (current.id !== previous?.id || !(span > 0 && span <= MAX_SAMPLE_SPAN_MS)) return {};
  const rate = (field, scale = 1000 / span) => {
    const count = delta(previous[field], current[field]);
    return count === undefined ? undefined : count * scale;
  };
  const emitted = delta(previous.jitterBufferEmittedCount, current.jitterBufferEmittedCount);
  return {
    decoded: rate("framesDecoded"),
    presented: rate("presentedFrames"),
    bitrate: rate("bytesReceived", 8000 / span),
    jitterBufferMs: emitted > 0 ? rate("jitterBufferDelay", 1000 / emitted) : undefined,
  };
}

function videoSample(report, video) {
  for (const stats of report.values()) {
    if (stats.type !== "inbound-rtp" || (stats.kind ?? stats.mediaType) !== "video") continue;
    const { id, timestamp, framesDecoded, bytesReceived, jitterBufferDelay, jitterBufferEmittedCount } = stats;
    const quality = video.getVideoPlaybackQuality?.();
    return {
      id, timestamp, framesDecoded, bytesReceived, jitterBufferDelay, jitterBufferEmittedCount,
      presentedFrames: delta(quality?.droppedVideoFrames, quality?.totalVideoFrames),
    };
  }
}

const number = (value, unit) => Number.isFinite(value) ? `${value.toFixed(1)} ${unit}` : "—";

function display(target, rates = {}, status = "") {
  const { video, values, message } = target;
  const resolution = video.videoWidth && video.videoHeight ? `${video.videoWidth} × ${video.videoHeight}` : "—";
  const bitrate = rates.bitrate >= 1_000_000
    ? number(rates.bitrate / 1_000_000, "Mb/s")
    : number(rates.bitrate / 1000, "kb/s");
  const texts = [resolution, number(rates.decoded, "fps"), number(rates.presented, "fps"),
    bitrate, number(rates.jitterBufferMs, "ms")];
  for (const [index, text] of texts.entries()) {
    if (values[index].textContent !== text) values[index].textContent = text;
  }
  if (message.textContent !== status) message.textContent = status;
  message.hidden = !status;
}

/** One stats disclosure and one browser read at a time, even across track changes. */
export class ReceiverStats {
  constructor(document) {
    this.document = document;
    this.window = document.defaultView;
  }

  open(target) {
    if (this.target?.details === target.details) return;
    this.close();
    this.target = target;
    target.previous = undefined;
    display(target, {}, "Measuring…");
    this.timer = this.window.setInterval(() => void this.sample(), INTERVAL_MS);
    void this.sample();
  }

  close(details) {
    if (!this.target || (details && details !== this.target.details)) return;
    this.target.details.open = false;
    this.target = undefined;
    this.window.clearInterval(this.timer);
  }

  paused(target) {
    if (!(this.document.hidden || target.video.paused || target.video.ended)) return false;
    target.previous = undefined;
    display(target, {}, "Video paused");
    return true;
  }

  async sample() {
    const target = this.target;
    if (!target) return;
    const { details, receiver, video } = target;
    if (!details.open || !details.isConnected) return this.close(details);
    if (this.paused(target) || this.pending) return;
    this.pending = true;
    let current;
    try {
      current = videoSample(await receiver.getStats(), video);
    } catch {
      // A stopped receiver may reject an outstanding read.
    } finally {
      this.pending = false;
    }
    if (this.target !== target || !details.open || !details.isConnected || this.paused(target)) return;
    const rates = current ? receiverRates(target.previous, current) : {};
    target.previous = current;
    const status = current ? ("decoded" in rates ? "" : "Measuring…") : "Waiting for video stats";
    display(target, rates, status);
  }
}
