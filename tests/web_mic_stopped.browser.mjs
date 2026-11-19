// SPDX-License-Identifier: EUPL-1.2
// The packaged web app in nix/vm-web.nix with Chromium's fake microphone: when
// the browser ends or mutes the capture track, Nuschl mutes, says "Microphone
// stopped" and asks for no new capture. A second profile measures received PCM
// from the deployed app before the capture track is stopped.
// Usage: node tests/web_mic_stopped.browser.mjs <chromium> <playwright-core>
import assert from "node:assert/strict";
import {join} from "node:path";
import {pathToFileURL} from "node:url";
const [chromiumPath,playwright]=process.argv.slice(2);
assert.ok(chromiumPath&&playwright,"usage: <chromium> <playwright-core>");
const {chromium}=await import(pathToFileURL(join(playwright,"index.mjs")).href);
// Plain HTTP in the VM: treat the site as the HTTPS origin it is in production.
const browser=await chromium.launch({headless:true,executablePath:chromiumPath,
  args:["--unsafely-treat-insecure-origin-as-secure=http://web","--use-fake-device-for-media-stream","--disable-audio-output"]});
// Hang guards sized for a loaded host: within them, slowness is timing (#376).
const HANG=180000;
async function openVoice(context,page,user) {
  context.setDefaultTimeout(HANG);context.setDefaultNavigationTimeout(HANG);
  await page.goto(`http://web/app/#s=murmur:64738&u=${user}`);
  assert.equal(await page.evaluate(()=>crossOriginIsolated),true);
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).click();
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected");
  await page.getByRole("button",{name:"Join voice",exact:true}).click();
}
try {
  for(const event of ["ended","mute"]) {
    const context=await browser.newContext({viewport:{width:1280,height:800},permissions:["microphone"]});
    const page=await context.newPage(),errors=[];
    const receiver=event==="ended"?await browser.newContext({viewport:{width:1280,height:800}}):null;
    const listener=await receiver?.newPage();
    if(listener) {
      listener.on("pageerror",error=>errors.push(error.message));
      await listener.addInitScript(()=>{
        const RealWorklet=AudioWorkletNode;
        globalThis.AudioWorkletNode=class extends RealWorklet {
          constructor(context,name,options) {
            super(context,name,options);
            if(name!=="nuschl-playback")return;
            const analyser=context.createAnalyser();
            this.connect(analyser);
            const samples=new Float32Array(analyser.fftSize);
            globalThis.receivedVoice=()=>{
              analyser.getFloatTimeDomainData(samples);
              const power=samples.reduce((sum,value)=>sum+value*value,0);
              return power/samples.length>1e-6;
            };
          }
        };
      });
      await openVoice(receiver,listener,"mic-listener");
    }
    page.on("pageerror",error=>errors.push(error.message));
    // Counts capture requests and keeps the tracks, to end them as the browser would.
    await page.addInitScript(()=>{
      const getUserMedia=navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
      window.capture={calls:0,tracks:[]};
      navigator.mediaDevices.getUserMedia=async constraints=>{
        capture.calls++;const stream=await getUserMedia(constraints);capture.tracks.push(...stream.getAudioTracks());return stream;
      };
    });
    await openVoice(context,page,`mic-${event}`);
    const status=page.locator(".voice-description small").first();
    await page.getByRole("button",{name:"Unmute microphone",exact:true}).click();
    await status.filter({hasText:"Microphone on"}).waitFor();
    if(listener) {
      await listener.waitForFunction(()=>globalThis.receivedVoice?.());
      console.log("synthetic microphone reached the second packaged app's playback worklet");
      await receiver.close();
    }
    await page.evaluate(event=>capture.tracks[0].dispatchEvent(new Event(event)),event);
    await status.filter({hasText:"Microphone stopped"}).waitFor();
    // The server acknowledged the mute.
    await page.getByRole("button",{name:"Unmute microphone",exact:true}).waitFor();
    assert.equal(await page.evaluate(()=>capture.tracks[0].readyState),"ended");
    // The page coming back opens nothing.
    await page.evaluate(()=>document.dispatchEvent(new Event("visibilitychange")));
    await page.waitForTimeout(1000);
    assert.equal(await page.evaluate(()=>capture.calls),1);
    assert.match(await status.textContent(),/Microphone stopped/);
    assert.deepEqual(errors,[]);
    await context.close();
    console.log(`capture track ${event}: muted, "Microphone stopped", one getUserMedia call`);
  }
} finally {
  await browser.close();
}
