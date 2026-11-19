// SPDX-License-Identifier: EUPL-1.2
// The call view: a grid of camera tiles, a large view for a shared screen or a
// pinned camera, the roster and the host's actions. DOM nodes only, no markup
// strings (the stream host requires Trusted Types). This page's own sources show as
// tiles too; `controls.js` starts and stops them.

import { qualityOptions } from "./controls.js";
import { fitTier, qualityRequest } from "./signal.js";
import { ReceiverStats } from "./receiver-stats.js";

const endings = {
  transforms: ["Browser not supported", "Use a current Firefox or Chromium."],
  codecs: ["Browser not supported", "No AV1 or VP9 video. Use a current Firefox or Chromium."],
  codec: ["No shared video format", "Use a current Firefox or Chromium."],
  link: ["Link not valid", "Ask for the call link again."],
  proof: ["Link not valid", "Ask for the call link again."],
  no_room: ["Call not found", "It may have ended."],
  ended: ["Call ended", ""],
  removed: ["Removed from the call", ""],
  full: ["Call full", ""],
  busy: ["Call host busy", "Try again later."],
  rate: ["Call host busy", "Try again later."],
  bandwidth: ["Call bandwidth is full", "Try rejoining when a video stops or its quality drops."],
  relay: ["Relay unavailable", "This call host offers no relay."],
};
const rejoin = new Set(["lost", "media", "slow", "busy", "rate", "bandwidth"]);

const element = (document, name, attributes = {}, ...children) => {
  const node = document.createElement(name);
  for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, value);
  node.append(...children);
  return node;
};

const pin = (track) => `${track.peer}/${track.screen ? "screen" : "camera"}`;

export class View {
  /** The pinned source, "peer/camera" or "peer/screen": it keeps its place when its
   * publisher restarts the encoder on a new media section. */
  pinned = null;
  /** A tile per video track mid, kept while the track lasts. */
  tiles = new Map();
  /** Explicit viewer choices survive a publisher's encoder restart. */
  tiers = new Map();
  sizes = new Map();

  constructor(document, call) {
    this.document = document;
    this.call = call;
    this.stats = new ReceiverStats(document);
    this.observer = new document.defaultView.ResizeObserver((entries) => {
      for (const { target, contentRect, devicePixelContentBoxSize } of entries) {
        const device = devicePixelContentBoxSize?.[0];
        const pixels = device ? Math.min(device.inlineSize, device.blockSize) :
          Math.min(contentRect.width, contentRect.height) * document.defaultView.devicePixelRatio;
        this.sizes.get(target)?.resize(pixels);
      }
    });
    const $ = (id) => document.getElementById(id);
    this.nodes = {
      status: $("status"), stage: $("stage"), large: $("large"), grid: $("grid"), empty: $("empty"),
      roster: $("roster"), count: $("count"), controls: $("controls"), host: $("host-actions"),
      ending: $("ending"), main: $("call"),
    };
    call.addEventListener("change", () => this.render());
    this.render();
  }

  name(peer) {
    return peer === this.call.self ? "You" : `Participant ${peer}`;
  }

  /** Shows why the call ended or cannot start. */
  showEnding(reason) {
    const [title, detail] = endings[reason] ?? ["Disconnected", ""];
    const { ending, main } = this.nodes;
    this.stats.close();
    this.observer.disconnect();
    for (const size of this.sizes.values()) size.cancel();
    this.sizes.clear();
    main.hidden = true;
    const actions = [];
    if (rejoin.has(reason) || !(reason in endings)) {
      const button = element(this.document, "button", { type: "button" }, "Rejoin");
      button.addEventListener("click", () => this.document.defaultView.location.reload());
      actions.push(button);
    }
    ending.replaceChildren(element(this.document, "h1", {}, title), ...(detail ? [element(this.document, "p", {}, detail)] : []), ...actions);
    ending.hidden = false;
  }

  render() {
    const { call } = this;
    if (call.ended) return this.showEnding(call.ended);
    const { status, large, grid, empty, roster, count, host } = this.nodes;
    const values = call.self === null ? ["Connecting"] : [call.codec.toUpperCase()];
    if (call.relay) values.push("Relay only");
    status.textContent = values.join(" · ");

    // Tiles: one per video track and per own source; the large view holds the pinned
    // tile or someone else's screen.
    const own = [...call.sources].map(([kind, source]) => [
      `own:${source.track.id}`,
      { peer: call.self, screen: kind === "screen", stream: source.stream, own: true },
    ]);
    const videos = [...call.tracks].filter(([, track]) => track.kind === "video").concat(own);
    const shown = new Map(videos);
    for (const [key, tile] of this.tiles) {
      const video = tile.querySelector("video");
      if (shown.get(key)?.stream === video.srcObject) continue;
      this.observer.unobserve(video);
      this.sizes.get(video)?.cancel();
      this.sizes.delete(video);
      const stats = tile.querySelector(".video-stats");
      if (stats) this.stats.close(stats);
      tile.remove();
      this.tiles.delete(key);
    }
    for (const peer of this.tiers.keys()) if (!call.peers.has(peer)) this.tiers.delete(peer);
    // A pin stays while its source is away, as during an encoder restart.
    const pinned = videos.find(([, track]) => pin(track) === this.pinned)?.[0];
    const featured = pinned ?? videos.find(([, track]) => track.screen && !track.own)?.[0] ?? null;
    for (const [mid, track] of videos) {
      const tile = this.tiles.get(mid) ?? this.tile(mid, track);
      const parent = mid === featured ? large : grid;
      if (tile.parentNode !== parent) {
        parent.append(tile);
        tile.querySelector("video").play().catch(() => {});
      }
      tile.querySelector("button").setAttribute("aria-pressed", String(mid === pinned));
    }
    large.hidden = featured === null;
    empty.hidden = videos.length > 0;

    // The roster: this page first, then everyone else in id order.
    const people = [[call.self, call.role], ...[...call.peers].sort(([a], [b]) => a - b)].filter(([id]) => id !== null);
    count.textContent = String(people.length);
    roster.replaceChildren(...people.map(([id, role]) => this.person(id, role)));
    host.hidden = call.role !== "host";
  }

  tile(mid, track) {
    const video = element(this.document, "video", { autoplay: "", playsinline: "" });
    video.muted = true;
    video.srcObject = track.stream;
    const label = track.screen ? `${this.name(track.peer)} · Screen` : this.name(track.peer);
    const toggle = element(this.document, "button", { type: "button", class: "pin", "aria-pressed": "false" }, "Pin");
    toggle.setAttribute("aria-label", `Pin ${label}`);
    toggle.addEventListener("click", () => {
      this.pinned = this.pinned === pin(track) ? null : pin(track);
      this.render();
    });
    const tile = element(this.document, "figure", { class: "tile", "data-state": "waiting", "data-peer": String(track.peer) },
      video, element(this.document, "figcaption", {}, label), toggle);
    if (track.screen) tile.dataset.screen = "";
    if (track.own) tile.dataset.own = "";
    else if (track.receiver) tile.append(this.videoStats(mid, video, track, label));
    video.addEventListener("loadeddata", () => { tile.dataset.state = "playing"; });
    this.tiles.set(mid, tile);
    return tile;
  }

  videoStats(mid, video, track, label) {
    const doc = this.document;
    const details = element(doc, "details", { class: "video-stats" });
    const list = element(doc, "dl");
    const jitter = { title: "Average time a frame spent in the receiver’s jitter buffer" };
    const values = ["Resolution", "Decoded frame rate", "Presented frame rate", "Receive bitrate", "Jitter buffer delay"]
      .map(name => {
        const value = element(doc, "dd", {}, "—");
        list.append(element(doc, "dt", name === "Jitter buffer delay" ? jitter : {}, name), value);
        return value;
      });
    const message = element(doc, "p", { hidden: "" });
    details.append(element(doc, "summary", { "aria-label": `Video stats for ${label}` }, "Video stats"),
      this.viewerQuality(mid, video, track, label), message, list);
    details.addEventListener("toggle", () => {
      if (details.open && details.isConnected) this.stats.open({ details, receiver: track.receiver, video, message, values });
      else this.stats.close(details);
    });
    return details;
  }

  viewerQuality(mid, video, track, label) {
    const kind = track.screen ? "screen" : "camera";
    const select = element(this.document, "select", { "aria-label": `Quality for ${label}` });
    qualityOptions(select, kind);
    const tiers = this.tiers.get(track.peer) ?? {};
    this.tiers.set(track.peer, tiers);
    select.value = tiers[kind] ?? "";

    let size = 0, lastTier, sentAt = -Infinity, timer;
    const send = () => {
      clearTimeout(timer);
      if (this.call.tracks.get(mid) !== track || this.call.ended) return;
      const tier = select.value === "" ? fitTier(kind, size) : Number(select.value);
      if (tier === lastTier || !size) return;
      // The SFU accepts one request per second. Keep the latest choice until then.
      const wait = sentAt + 1100 - performance.now();
      if (wait > 0) {
        timer = setTimeout(send, wait);
        return;
      }
      this.call.send(qualityRequest(track.peer, kind, tier));
      lastTier = tier;
      sentAt = performance.now();
    };
    select.addEventListener("change", () => {
      tiers[kind] = select.value;
      send();
    });
    this.sizes.set(video, { resize: (pixels) => { size = pixels; send(); }, cancel: () => clearTimeout(timer) });
    this.observer.observe(video, { box: "device-pixel-content-box" });
    return element(this.document, "label", { class: "viewer-quality" }, "Receive quality", select);
  }

  person(id, role) {
    const own = id === this.call.self ? [...this.call.sources.keys()].map((kind) => ({ screen: kind === "screen" })) : [];
    const sources = [...this.call.tracks.values()]
      .filter((track) => track.peer === id && track.kind === "video")
      .concat(own)
      .map((track) => (track.screen ? "Screen" : "Camera"));
    const item = element(this.document, "li", {}, element(this.document, "span", { class: "name" }, this.name(id)));
    if (role === "host") item.append(element(this.document, "span", { class: "tag" }, "Host"));
    if (sources.length) item.append(element(this.document, "span", { class: "sources" }, sources.join(", ")));
    if (this.call.role === "host" && id !== this.call.self) {
      const remove = element(this.document, "button", { type: "button", "aria-label": `Remove ${this.name(id)}` }, "Remove");
      remove.addEventListener("click", () => this.call.remove(id));
      item.append(remove);
    }
    return item;
  }
}
