// SPDX-License-Identifier: EUPL-1.2
// Static same-origin AudioWorklet module.
export const AUDIO_RATE = 48000;
export const AUDIO_FRAME = 960;
export const PLAYOUT_FRAME = 480;
export const PLAYOUT_BLOCKS = 12;
export const CAPTURE_BLOCKS = 10;
const WRITE = 0, READ = 1, WAKE = 2, TICKS = 3;
const HEADER_BYTES = 4 * Int32Array.BYTES_PER_ELEMENT;

// One producer owns WRITE and one consumer owns READ. Publishing an index
// transfers ownership of the slot and its PCM. Wrap at twice the capacity so
// a full ring and an empty ring have different indices.
export class BlockRing {
  static bytes(blocks, samples, Pcm = Float32Array) {
    return HEADER_BYTES + blocks * 8 + blocks * samples * Pcm.BYTES_PER_ELEMENT;
  }
  constructor(buffer, blocks, samples, Pcm = Float32Array) {
    this.blocks = blocks;
    this.span = 2 * blocks;
    this.samples = samples;
    this.state = new Int32Array(buffer, 0, 4);
    this.tags = new Int32Array(buffer, HEADER_BYTES, blocks * 2);
    this.data = new Pcm(buffer, HEADER_BYTES + blocks * 8, blocks * samples);
  }
  index(field) { return (Atomics.load(this.state, field) >>> 0) % this.span; }
  size() { return (this.index(WRITE) + this.span - this.index(READ)) % this.span; }
  head() { return this.index(READ) % this.blocks; }
  push(epoch, frame, samples) {
    if (this.size() >= this.blocks) return false;
    const write = this.index(WRITE), at = write % this.blocks;
    this.tags[at * 2] = epoch;
    this.tags[at * 2 + 1] = frame;
    this.data.set(samples, at * this.samples);
    Atomics.store(this.state, WRITE, (write + 1) % this.span);
    return true;
  }
  pop() { Atomics.store(this.state, READ, (this.index(READ) + 1) % this.span); }
  ticks() { return Atomics.load(this.state, TICKS) >>> 0; }
  tick(count) { Atomics.add(this.state, TICKS, count); }
  setTicks(count) { Atomics.store(this.state, TICKS, count); }
  wake(port) {
    if (port && Atomics.exchange(this.state, WAKE, 1) === 0) port.postMessage("wake");
  }
  clearWake() { Atomics.store(this.state, WAKE, 0); }
}

// Policy is on a separate page-to-worklet buffer. The codec worker owns
// neither that buffer nor microphone and output device access.
export class CaptureFrames {
  constructor(ring, policy) {
    this.ring = ring;
    this.policy = policy;
    this.buffer = new Float32Array(AUDIO_FRAME);
    this.epoch = 0;
    this.cursor = 0;
    this.frame = 0;
  }
  append(input) {
    const epoch = Atomics.load(this.policy, 0);
    if (epoch !== this.epoch) {
      this.epoch = epoch;
      this.cursor = 0;
      this.frame = 0;
    }
    if (!epoch) return;
    for (let i = 0; i < input.length; i++) {
      const value = input[i];
      this.buffer[this.cursor++] = Number.isFinite(value) ? Math.max(-1, Math.min(1, value)) : 0;
      if (this.cursor === AUDIO_FRAME) {
        this.ring.setTicks(this.frame + 1);
        this.ring.push(epoch, this.frame++, this.buffer);
        this.cursor = 0;
      }
    }
  }
}

// Read the 95th percentile of one-second peak wake gaps. Sampling peaks
// retains short stalls that regularly occupy only a fraction of a second.
// The current peak lets the lead grow on the first observed stall.
export class Lead {
  constructor() {
    this.counts = new Float64Array(PLAYOUT_BLOCKS + 1);
    this.second = null;
    this.peak = 0;
    this.blocks = 3;
    this.raiseAt = 0;
  }
  observe(step, now) {
    const second = Math.floor(now / 1000);
    const elapsed = this.second === null ? 0 : second - this.second;
    if (elapsed > 0) {
      const decay = 0.98 ** elapsed;
      for (let i = 0; i < this.counts.length; i++) this.counts[i] *= decay;
      this.counts[this.peak] += 1;
      this.peak = 0;
    }
    this.second = second;
    this.peak = Math.min(PLAYOUT_BLOCKS, Math.max(this.peak, step));
    let sum = this.counts.reduce((total, count) => total + count, 0) * 0.95;
    let percentile = 0;
    while (percentile < PLAYOUT_BLOCKS && (sum -= this.counts[percentile]) > 0) percentile++;
    const target = Math.min(PLAYOUT_BLOCKS, Math.max(3, Math.max(percentile, this.peak) + 1));
    if (target <= this.blocks) {
      this.blocks = target;
    } else if (now >= this.raiseAt) {
      // Grow by one block so the refill leaves the network buffer time to adapt.
      this.blocks++;
      this.raiseAt = now + 400;
    }
  }
}

export function mixSpeaker(mix, samples, gain) {
  for (let i = 0; i < mix.length; i++) mix[i] += samples[i] * gain;
}

export class RingPlayout {
  constructor(ring, policy) {
    this.ring = ring;
    this.policy = policy;
    this.port = null;
    this.offset = 0;
    this.frames = 0;
  }
  render(left, right = null) {
    left.fill(0);
    right?.fill(0);
    const ring = this.ring, epoch = Atomics.load(this.policy, 0);
    for (let output = 0, slots = ring.size(); output < left.length && slots > 0 && slots <= ring.blocks;) {
      const at = ring.head(), tag = ring.tags[at * 2];
      if (!epoch || tag !== epoch) {
        // The page can authorize a new epoch during this render quantum.
        if (tag > epoch && Atomics.load(this.policy, 0) === tag) break;
        ring.pop();
        this.offset = 0;
        slots--;
        continue;
      }
      const base = at * PLAYOUT_FRAME * 2;
      for (; output < left.length && this.offset < PLAYOUT_FRAME; output++, this.offset++) {
        const l = ring.data[base + this.offset * 2], r = ring.data[base + this.offset * 2 + 1];
        left[output] = clip(right ? l : (l + r) * 0.5);
        if (right) right[output] = clip(r);
      }
      if (this.offset === PLAYOUT_FRAME) {
        ring.pop();
        this.offset = 0;
        slots--;
      }
    }
    this.frames += left.length;
    if (this.frames >= PLAYOUT_FRAME) {
      ring.tick(Math.floor(this.frames / PLAYOUT_FRAME));
      this.frames %= PLAYOUT_FRAME;
      if (epoch) ring.wake(this.port);
    }
  }
}

function clip(sample) {
  return Number.isFinite(sample) ? Math.max(-1, Math.min(1, sample)) : 0;
}

if (typeof registerProcessor === "function") {
  class CaptureProcessor extends AudioWorkletProcessor {
    constructor({processorOptions: {buffer, policy}}) {
      super();
      this.capture = new CaptureFrames(new BlockRing(buffer, CAPTURE_BLOCKS, AUDIO_FRAME), policy);
    }
    process(inputs, outputs) {
      // Capture stays silent on the output bus.
      for (const bus of outputs) for (const channel of bus) channel.fill(0);
      const mono = inputs[0]?.[0];
      if (mono && sampleRate === AUDIO_RATE) this.capture.append(mono);
      return true;
    }
  }
  class PlaybackProcessor extends AudioWorkletProcessor {
    constructor({processorOptions: {buffer, policy}}) {
      super();
      this.playout = new RingPlayout(new BlockRing(buffer, PLAYOUT_BLOCKS, PLAYOUT_FRAME * 2, Float64Array), policy);
      this.port.onmessage = ({data}) => {
        if (data?.kind === "wake-port") this.playout.port = data.port;
        if (data?.kind === "close") {
          this.playout.port?.close();
          this.playout.port = null;
        }
      };
    }
    process(_inputs, [bus]) {
      if (bus?.[0]) this.playout.render(bus[0], bus[1]);
      return true;
    }
  }
  registerProcessor("nuschl-capture", CaptureProcessor);
  registerProcessor("nuschl-playback", PlaybackProcessor);
}
