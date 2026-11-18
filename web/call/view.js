// SPDX-License-Identifier: EUPL-1.2
// The call view: a grid of camera tiles, a large view for a shared screen or a
// pinned camera, the roster and the host's actions. DOM nodes only, no markup
// strings (the stream host requires Trusted Types). B1 adds Start camera and Share
// screen to `#controls`.

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
  relay: ["Relay unavailable", "This call host offers no relay."],
};
const rejoin = new Set(["lost", "media", "slow", "busy", "rate"]);

const element = (document, name, attributes = {}, ...children) => {
  const node = document.createElement(name);
  for (const [key, value] of Object.entries(attributes)) node.setAttribute(key, value);
  node.append(...children);
  return node;
};

export class View {
  /** The pinned camera's mid. */
  pinned = null;
  /** A tile per video track mid, kept while the track lasts. */
  tiles = new Map();

  constructor(document, call) {
    this.document = document;
    this.call = call;
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

    // Tiles: one per video track; the large view holds the pinned camera or a screen.
    const videos = [...call.tracks].filter(([, track]) => track.kind === "video");
    for (const [mid, tile] of this.tiles) if (!call.tracks.has(mid)) { tile.remove(); this.tiles.delete(mid); }
    if (!call.tracks.has(this.pinned)) this.pinned = null;
    const featured = this.pinned ?? videos.find(([, track]) => track.screen)?.[0] ?? null;
    for (const [mid, track] of videos) {
      const tile = this.tiles.get(mid) ?? this.tile(mid, track);
      const parent = mid === featured ? large : grid;
      if (tile.parentNode !== parent) {
        parent.append(tile);
        tile.querySelector("video").play().catch(() => {});
      }
      tile.querySelector("button").setAttribute("aria-pressed", String(mid === this.pinned));
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
    const pin = element(this.document, "button", { type: "button", class: "pin", "aria-pressed": "false" }, "Pin");
    pin.setAttribute("aria-label", `Pin ${label}`);
    pin.addEventListener("click", () => {
      this.pinned = this.pinned === mid ? null : mid;
      this.render();
    });
    const tile = element(this.document, "figure", { class: "tile", "data-state": "waiting", "data-peer": String(track.peer) },
      video, element(this.document, "figcaption", {}, label), pin);
    if (track.screen) tile.dataset.screen = "";
    video.addEventListener("loadeddata", () => { tile.dataset.state = "playing"; });
    this.tiles.set(mid, tile);
    return tile;
  }

  person(id, role) {
    const sources = [...this.call.tracks.values()]
      .filter((track) => track.peer === id && track.kind === "video")
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
