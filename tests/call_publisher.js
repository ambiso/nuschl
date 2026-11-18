// SPDX-License-Identifier: EUPL-1.2
// A synthetic publisher for the call page's VM test (nix/call-page-vm.nix), served
// next to the page on the stream host. It joins with the page's own modules and
// publishes canvas sources through `Call.publish`, as Start camera and Share screen
// do. `publish([{color, label, screen}])` starts them; `peer()` is its SFU id.
import { Call } from "/session.js";
import { StreamClient } from "/stream-client.js";
import { support } from "/support.js";

const stream = new StreamClient();
const call = new Call(stream, { codecs: support().codecs, relay: false });
// The call link names the call page; this page opens the same link.
call.start({ link: `${location.origin}/call${location.hash}`, host: location.host });
window.call = call;
window.peer = () => call.self;
window.ended = () => call.ended;

function source({ color, label }) {
  const canvas = Object.assign(document.createElement("canvas"), { width: 640, height: 360 });
  document.body.append(canvas);
  const context = canvas.getContext("2d");
  let x = 0;
  setInterval(() => {
    context.fillStyle = color;
    context.fillRect(0, 0, 640, 360);
    context.fillStyle = "#fff";
    context.fillRect(x, 0, 80, 40);
    context.font = "bold 40px sans-serif";
    context.fillText(label, 24, 110);
    x = (x + 16) % 560;
  }, 66);
  return canvas.captureStream(15).getVideoTracks()[0];
}

window.publish = async (sources) => {
  for (const options of sources) await call.publish(options.screen ? "screen" : "camera", source(options));
};
