// SPDX-License-Identifier: EUPL-1.2
// Two Chromium clients in the web VM. The call page's media transport runs
// against the private SFU, and the Mumble tunnel carries real chat and cards.
import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";

const [app, host, port] = process.argv.slice(2);
assert.ok(app && host && port, "owned app URL, Murmur host, and port required");
const { chromium } = await import(pathToFileURL(process.env.NUSCHL_PLAYWRIGHT).href);
const appOrigin = new URL(app).origin;
const browser = await chromium.launch({
  headless: true, executablePath: process.env.NUSCHL_CHROMIUM,
  args: [
    `--unsafely-treat-insecure-origin-as-secure=${appOrigin}`,
    "--use-fake-device-for-media-stream", "--use-fake-ui-for-media-stream",
  ],
});
const errors = [];

async function joinApp(name) {
  const context = await browser.newContext({ ignoreHTTPSErrors: true });
  context.setDefaultTimeout(120000);
  await context.addInitScript(() => {
    const media = navigator.mediaDevices;
    const camera = media.getUserMedia.bind(media);
    const video = { width: 320, height: 180, frameRate: 15 };
    window.videoCaptureCalls = [];
    media.getUserMedia = (options) => {
      videoCaptureCalls.push(options.audio ? "microphone" : "camera");
      return camera({ video, audio: options.audio });
    };
    // Simulate the result of the screen picker with a second synthetic track.
    media.getDisplayMedia = (options) => {
      videoCaptureCalls.push(options.audio ? "screen audio" : "screen");
      return camera({ video, audio: options.audio });
    };
  });
  const page = await context.newPage();
  page.on("pageerror", error => errors.push(error.message));
  await page.goto(`${app}#s=${host}:${port}&u=${name}`);
  await page.getByRole("button", { name: "Trust this certificate", exact: true }).click();
  await page.waitForFunction(() => document.querySelector(".server-connection .status")?.textContent === "Connected");
  return page;
}

try {
  const hostPage = await joinApp("video-host");
  const guest = await joinApp("video-guest");
  const nextLease = hostPage.waitForRequest(request => new URL(request.url()).pathname === "/sfu/lease");
  await hostPage.getByRole("button", { name: "Start call", exact: true }).click();
  await guest.getByRole("button", { name: "Join call", exact: true }).first().click();
  await guest.locator(".video-session #status").getByText(/AV1|VP9/).waitFor();
  assert.deepEqual(await hostPage.evaluate(() => videoCaptureCalls), []);
  assert.deepEqual(await guest.evaluate(() => videoCaptureCalls), []);

  await hostPage.getByLabel("Camera quality", { exact: true }).selectOption("0");
  await hostPage.getByLabel("Screen quality", { exact: true }).selectOption("0");
  await hostPage.getByRole("button", { name: "Start camera", exact: true }).click();
  await hostPage.getByRole("button", { name: "Share screen", exact: true }).click();
  await guest.waitForFunction(() => {
    const videos = [...document.querySelectorAll("#stage .tile video")];
    return videos.length === 2 && videos.every(video => video.readyState >= 2 && video.videoWidth > 0);
  });
  assert.deepEqual(await hostPage.evaluate(() => videoCaptureCalls), ["camera", "screen"]);
  await hostPage.getByRole("button", { name: "Minimize", exact: true }).click();
  await hostPage.getByRole("button", { name: "Lobby", exact: true }).click();
  await hostPage.getByRole("button", { name: "Show video", exact: true }).click();
  await hostPage.getByRole("button", { name: "Stop sharing", exact: true }).waitFor();
  assert.equal((await nextLease).method(), "POST");
  await hostPage.getByRole("button", { name: "End call", exact: true }).click();
  await guest.getByRole("alert").getByText("Call ended", { exact: true }).waitFor();
  await guest.locator("article").getByText("Call ended", { exact: true }).waitFor();
  assert.deepEqual(errors, []);
  console.log("web app: camera and screen decoded, chat card and call lifecycle passed");
} finally {
  await browser.close();
}
