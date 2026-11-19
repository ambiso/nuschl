// SPDX-License-Identifier: EUPL-1.2
// Start camera and Share screen (docs/video-plan.md §7): the camera opens only on the
// user's own Start camera, every share asks the browser's picker again, and this
// page never opens the microphone. Each source's quality is Auto; Q1 (#414) adds its
// tiers to QUALITIES.

/** Each source's quality choices: [value, label]. */
export const QUALITIES = [["auto", "Auto"]];

const capture = {
  camera: (media) =>
    media.getUserMedia({
      video: { width: { ideal: 1280 }, height: { ideal: 720 }, frameRate: { ideal: 30 } },
      audio: false,
    }),
  // Window sharing first in the picker; a whole monitor stays possible and shows a warning.
  screen: (media) =>
    media.getDisplayMedia({
      video: { displaySurface: "window", frameRate: { ideal: 30 } },
      audio: false,
      selfBrowserSurface: "exclude",
      surfaceSwitching: "exclude",
      monitorTypeSurfaces: "include",
    }),
};
const labels = { camera: ["Start camera", "Stop camera"], screen: ["Share screen", "Stop sharing"] };
const failures = {
  camera: { NotAllowedError: "Camera blocked", NotFoundError: "No camera found" },
  screen: {},
};

export class Controls {
  /** Sources between the button and their first offer. */
  busy = new Set();
  notice = "";

  constructor(document, call) {
    this.call = call;
    this.media = document.defaultView.navigator.mediaDevices;
    const $ = (id) => document.getElementById(id);
    this.status = $("notice");
    this.sources = {};
    for (const kind of ["camera", "screen"]) {
      const button = $(kind);
      const quality = $(`${kind}-quality`);
      quality.replaceChildren(...QUALITIES.map(([value, label]) => new Option(label, value)));
      quality.addEventListener("change", () => {
        const source = call.sources.get(kind);
        if (source) source.quality = quality.value;
      });
      button.addEventListener("click", () => this.toggle(kind));
      this.sources[kind] = { button, quality };
    }
    call.addEventListener("change", () => this.render());
    this.render();
  }

  async toggle(kind) {
    if (this.call.sources.has(kind)) return this.call.unpublish(kind);
    this.busy.add(kind);
    this.notice = "";
    this.render();
    try {
      const [track] = (await capture[kind](this.media)).getVideoTracks();
      if (kind === "screen") track.contentHint = "detail";
      await this.call.publish(kind, track);
    } catch (error) {
      // A cancelled screen picker is the user's choice and needs no notice.
      if (kind === "camera" || error.name !== "NotAllowedError") {
        this.notice = failures[kind][error.name] ?? (kind === "camera" ? "Camera unavailable" : "Screen sharing unavailable");
      }
    } finally {
      this.busy.delete(kind);
      this.render();
    }
  }

  render() {
    const { call } = this;
    for (const [kind, { button, quality }] of Object.entries(this.sources)) {
      const on = call.sources.has(kind);
      button.textContent = labels[kind][Number(on)];
      button.setAttribute("aria-pressed", String(on));
      button.disabled = Boolean(call.ended) || this.busy.has(kind) || (!on && !call.canPublish);
      quality.value = call.sources.get(kind)?.quality ?? quality.value;
    }
    const monitor = call.sources.get("screen")?.track.getSettings().displaySurface === "monitor";
    const unsupported = call.codec && !call.canPublish ? `Can't send ${call.codec.toUpperCase()}` : "";
    this.status.textContent = monitor ? "Sharing your whole screen" : this.notice || unsupported;
    this.status.hidden = !this.status.textContent;
    this.status.classList.toggle("warning", monitor);
  }
}
