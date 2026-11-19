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

function outgoingSample(report) {
  for (const entry of report.values()) {
    if (entry.type === "outbound-rtp" && (entry.kind ?? entry.mediaType) === "video" && entry.active !== false) return entry;
  }
}

const number = (value, unit) => Number.isFinite(value) ? `${value.toFixed(1)} ${unit}` : "—";
const dimension = (value) => Number.isInteger(value) && value > 0;
const dimensions = (width, height) => dimension(width) && dimension(height) ? `${width} × ${height}` : "—";

function display(target, texts, status = "") {
  const { values, message } = target;
  for (const [index, text] of texts.entries()) {
    if (values[index].textContent !== text) values[index].textContent = text;
  }
  if (message.textContent !== status) message.textContent = status;
  message.hidden = !status;
}

const tier = (limits) => limits ? `${limits[0]}p${limits[1]}` : "—";

function bitrateText(value) {
  return value >= 1_000_000 ? number(value / 1_000_000, "Mb/s") : number(value / 1000, "kb/s");
}

function displayReceiver(target, rates = {}, status = "") {
  const { video, requested } = target;
  const { videoWidth: width, videoHeight: height } = video;
  const request = requested();
  const belowTarget = request && dimension(width) && dimension(height) && Math.min(width, height) < request[0];
  display(target, [dimensions(width, height), number(rates.decoded, "fps"), number(rates.presented, "fps"),
    bitrateText(rates.bitrate), number(rates.jitterBufferMs, "ms"), tier(request)], status ||
      (belowTarget ? "Received below target. The limiting setting is unavailable here." : ""));
}

const limitations = new Map([
  ["none", "No adaptation reported"],
  ["cpu", "CPU"],
  ["bandwidth", "Bandwidth"],
  ["other", "Other"],
]);

export function publisherMeasurements({ width, height }, requested, cap, scale, sample) {
  const planned = Number.isFinite(scale) && scale >= 1
    ? dimensions(Math.floor(width / scale), Math.floor(height / scale)) : "—";
  const limit = cap ? `${tier(cap)} · ${bitrateText(cap[2])}` : "—";
  return [dimensions(width, height), tier(requested), limit, planned,
    dimensions(sample?.frameWidth, sample?.frameHeight),
    limitations.get(sample?.qualityLimitationReason) ?? "Unavailable"];
}

function displayPublisher(target, sender, sample, status = "") {
  const { source, requested } = target;
  const scale = sender?.getParameters().encodings?.[0]?.scaleResolutionDownBy;
  display(target, publisherMeasurements(source.track.getSettings(), requested(), source.remoteCap, scale, sample), status);
}

/** One stats disclosure and one browser read at a time, even across track changes. */
export class VideoStats {
  constructor(document) {
    this.document = document;
    this.window = document.defaultView;
  }

  open(target) {
    if (this.target?.details === target.details) return;
    this.close();
    this.target = target;
    target.previous = undefined;
    if (target.sender) displayPublisher(target, target.sender(), undefined, "Measuring…");
    else displayReceiver(target, {}, "Measuring…");
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
    if (target.sender) displayPublisher(target, target.sender(), undefined, "Video paused");
    else displayReceiver(target, {}, "Video paused");
    return true;
  }

  async sample() {
    const target = this.target;
    if (!target) return;
    const { details, receiver, video } = target;
    if (!details.open || !details.isConnected) return this.close(details);
    if (this.paused(target) || this.pending) return;
    const sender = target.sender?.();
    this.pending = true;
    let current;
    try {
      const report = await (sender ?? receiver).getStats();
      current = sender ? outgoingSample(report) : videoSample(report, video);
    } catch {
      // A stopped sender or receiver may reject an outstanding read.
    } finally {
      this.pending = false;
    }
    if (this.target !== target || !details.open || !details.isConnected || this.paused(target)) return;
    if (sender) {
      if (target.sender() === sender) {
        const status = dimension(current?.frameWidth) ? "" : "Waiting for encoded video stats";
        displayPublisher(target, sender, current, status);
      }
      return;
    }
    const rates = current ? receiverRates(target.previous, current) : {};
    target.previous = current;
    const status = current ? ("decoded" in rates ? "" : "Measuring…") : "Waiting for video stats";
    displayReceiver(target, rates, status);
  }
}
