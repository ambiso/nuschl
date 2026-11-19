// SPDX-License-Identifier: EUPL-1.2
// web/app/src/call-session.js in Chromium with the production CSP and the
// browser's own Wake Lock, Media Session and media element: join, hide, show,
// leave; then a freeze, which ends the call, closes a terminated worker's WebSocket and
// plays the disconnect sound (rendered offline).
// Usage: node tests/web_call_session.browser.mjs <repository> <chromium> <playwright-core/index.mjs>
import assert from "node:assert/strict";
import {createHash} from "node:crypto";
import {createServer} from "node:http";
import {readFile} from "node:fs/promises";
import {join} from "node:path";
import {pathToFileURL} from "node:url";
const [source,chromiumPath,playwright]=process.argv.slice(2);
assert.ok(source&&chromiumPath&&playwright,"usage: <repository> <chromium> <playwright-core/index.mjs>");
const csp="default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; media-src blob:; connect-src 'self'; worker-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
// Headless Chromium keeps every page visible (minimising and switching tabs
// leave visibilityState "visible"), so the page sets visibilityState itself
// and releases the browser's lock on hide, as Chromium does for a hidden page.
const fixture=`import {CallSession} from "/call-session.js";
import {BrowserAudio} from "/audio.js";
const worker=new Worker("/socket-worker.js");
const sound=deafened=>{const audio=new BrowserAudio();audio.ctx=new OfflineAudioContext(1,48000,48000);audio.voice={deafened};return audio;};
const audio=sound(false),deaf=sound(true);
let visibility="visible";
Object.defineProperty(document,"visibilityState",{get:()=>visibility});
const request=navigator.wakeLock.request.bind(navigator.wakeLock),locks=[];
navigator.wakeLock.request=async type=>{const lock=await request(type);locks.push(lock);return lock;};
const session=new CallSession({onMute:()=>fixture.actions.push("mute"),onHangUp:()=>fixture.actions.push("hang up"),
  onEnd:()=>{fixture.actions.push("end");audio.playDisconnected();deaf.playDisconnected();worker.terminate();}});session.attach();
document.querySelector("button").onclick=()=>session.prime();
window.fixture={actions:[],
  media:()=>({playing:Boolean(session.media&&!session.media.paused&&session.media.src),title:navigator.mediaSession.metadata?.title??null,state:navigator.mediaSession.playbackState}),
  update:call=>session.update(call),
  visibility:state=>{visibility=state;if(state==="hidden")for(const lock of locks)lock.release();document.dispatchEvent(new Event("visibilitychange"));},
  locks:()=>locks.map(lock=>lock.released),
  freeze:()=>document.dispatchEvent(new Event("freeze")),
  // Peak of the first 0.75 s and of the rest, for the sound and while deafened.
  sound:()=>Promise.all([audio,deaf].map(async({ctx})=>{const pcm=(await ctx.startRendering()).getChannelData(0),peak=part=>part.reduce((max,value)=>Math.max(max,Math.abs(value)),0);return [peak(pcm.subarray(0,36000)),peak(pcm.subarray(36000))];})),
};`;
const routes={"/":["text/html",'<!doctype html><meta charset="utf-8"><title>Call session</title><button>Join</button><script type="module" src="/fixture.js"></script>'],
  "/fixture.js":["text/javascript",fixture],
  "/socket-worker.js":["text/javascript",'new WebSocket(location.origin.replace("http","ws")+"/socket");'],
  "/call-session.js":["text/javascript",await readFile(join(source,"web/app/src/call-session.js"))],
  "/audio.js":["text/javascript",await readFile(join(source,"web/app/src/audio.js"))],
  "/audio-protocol.js":["text/javascript",await readFile(join(source,"web/app/src/audio-protocol.js"))]};
const server=createServer((request,response)=>{
  const route=routes[new URL(request.url,"http://127.0.0.1").pathname];
  if(!route){response.writeHead(404);response.end();return;}
  response.writeHead(200,{"Content-Type":route[0],"Content-Security-Policy":csp,"Permissions-Policy":"camera=(), geolocation=(), microphone=(self)"});
  response.end(route[1]);
});
// A WebSocket handshake, then only the client's close frame (opcode 8) matters:
// the relay ends the Mumble connection on it (apps/web-relay/src/relay.rs).
const sockets={open:0,closed:0};
server.on("upgrade",(request,socket)=>{
  const accept=createHash("sha1").update(request.headers["sec-websocket-key"]+"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest("base64");
  socket.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
  sockets.open++;socket.on("error",()=>{});
  socket.on("data",frame=>{if((frame[0]&0x0f)===8){sockets.closed++;socket.end(Buffer.from([0x88,0]));}});
});
await new Promise(done=>server.listen(0,"127.0.0.1",done));
const origin=`http://127.0.0.1:${server.address().port}`;
const {chromium}=await import(pathToFileURL(playwright).href);
const browser=await chromium.launch({headless:true,executablePath:chromiumPath,args:["--disable-audio-output"]});
try {
  const context=await browser.newContext();
  await context.route("**/*",route=>route.request().url().startsWith(origin+"/")?route.continue():route.abort());
  const page=await context.newPage(),errors=[];
  page.on("pageerror",error=>errors.push(error.message));
  page.on("console",message=>{if(message.type()==="error")errors.push(message.text());});
  await page.addInitScript(()=>document.addEventListener("securitypolicyviolation",event=>(window.violations??=[]).push(event.violatedDirective)));
  await page.goto(origin+"/");
  await page.waitForFunction(()=>window.fixture);
  const locks=()=>page.evaluate(()=>fixture.locks());
  const until=expected=>page.waitForFunction(expected=>JSON.stringify(fixture.locks())===expected,JSON.stringify(expected),{timeout:5000});
  assert.equal(await page.evaluate(()=>typeof navigator.wakeLock?.request),"function","this Chromium has no Wake Lock API");
  await page.getByRole("button",{name:"Join"}).click();
  await page.evaluate(()=>fixture.update({channel:"Lobby",server:"Home",muted:true}));
  await until([false]);
  await page.waitForFunction(()=>fixture.media().playing,null,{timeout:5000});
  assert.deepEqual(await page.evaluate(()=>fixture.media()),{playing:true,title:"Lobby",state:"playing"});
  await page.evaluate(()=>fixture.visibility("hidden"));
  await until([true]);
  await page.evaluate(()=>fixture.visibility("visible"));
  await until([true,false]);
  await page.evaluate(()=>fixture.update(null));
  await until([true,true]);
  assert.deepEqual(await page.evaluate(()=>fixture.media()),{playing:false,title:null,state:"none"});
  assert.deepEqual(await page.evaluate(()=>fixture.actions),[]);
  await page.evaluate(()=>{fixture.visibility("hidden");fixture.visibility("visible");});
  await page.waitForTimeout(200);
  assert.deepEqual(await locks(),[true,true]);
  // A freeze in a call ends it once: lock and media stop, the worker's WebSocket closes.
  await page.evaluate(()=>fixture.update({channel:"Lobby",server:"Home",muted:true}));
  await until([true,true,false]);
  for(let tries=0;sockets.open!==1&&tries<50;tries++)await page.waitForTimeout(100);
  assert.deepEqual(sockets,{open:1,closed:0});
  const frozen=Date.now();
  await page.evaluate(()=>{fixture.freeze();fixture.freeze();});
  for(let tries=0;!sockets.closed&&tries<50;tries++)await page.waitForTimeout(20);
  assert.deepEqual(sockets,{open:1,closed:1});
  const closedAfter=Date.now()-frozen;assert.ok(closedAfter<1000,`socket closed after ${closedAfter} ms`);
  await until([true,true,true]);
  assert.deepEqual(await page.evaluate(()=>fixture.media()),{playing:false,title:null,state:"none"});
  assert.deepEqual(await page.evaluate(()=>fixture.actions),["end"]);
  const [[sound,after],[deafSound,deafAfter]]=await page.evaluate(()=>fixture.sound());
  assert.ok(sound>0.05&&sound<1,`disconnect sound peak ${sound}`);
  assert.equal(after,0);assert.equal(deafSound,0);assert.equal(deafAfter,0);
  assert.deepEqual(await page.evaluate(()=>window.violations??[]),[]);
  assert.deepEqual(errors,[]);
  console.log(`call session: lock and playing media session on join, lock again after hide and show, both end on leave; a freeze ends the call, closes the socket after ${closedAfter} ms and plays the disconnect sound (peak ${sound.toFixed(3)})`);
} finally {
  await browser.close();server.close();
}
