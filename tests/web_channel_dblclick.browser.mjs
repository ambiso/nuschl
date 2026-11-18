// SPDX-License-Identifier: EUPL-1.2
// The packaged web app on a live Mumble server with the channels Lobby, Music
// and AFK: double-clicking a channel with a mouse or pen joins its voice with
// the microphone closed; a touch double-tap opens the channel's chat. Run by
// nix/vm-web.nix.
//   node tests/web_channel_dblclick.browser.mjs APP_URL SERVER_HOST SERVER_PORT
import assert from "node:assert/strict";
import {pathToFileURL} from "node:url";
const [app,host,port]=process.argv.slice(2);
assert.ok(app&&host&&port,"app URL, server host and port required");
const {chromium}=await import(pathToFileURL(process.env.NUSCHL_PLAYWRIGHT).href);
const browser=await chromium.launch({headless:true,executablePath:process.env.NUSCHL_CHROMIUM,
  args:[`--unsafely-treat-insecure-origin-as-secure=${new URL(app).origin}`]});
try {
  const context=await browser.newContext({viewport:{width:1200,height:800},hasTouch:true});
  const page=await context.newPage(),errors=[];
  page.on("pageerror",error=>errors.push(error.message));
  await page.addInitScript(()=>{
    const proof=window.__proof={servers:[],voice:null,joins:[],microphoneCalls:0};
    // Playback goes to no device; a microphone request is counted and refused.
    const Context=window.AudioContext;
    window.AudioContext=class extends Context{constructor(options){super({...options,sinkId:{type:"none"}});}};
    navigator.mediaDevices.getUserMedia=()=>{proof.microphoneCalls++;return Promise.reject(new DOMException("refused","NotAllowedError"));};
    const Worker=window.Worker;
    window.Worker=class extends Worker{
      constructor(url,options){super(url,options);this.addEventListener("message",({data})=>{if(data.kind==="state")Object.assign(proof,{servers:data.servers,voice:data.voice});});}
      postMessage(message,...rest){if(message?.method==="join")proof.joins.push(message.args[1]);return super.postMessage(message,...rest);}
    };
  });
  await page.goto(`${app}#s=${host}:${port}&u=dblclick`);
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).click({timeout:30000});
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null,{timeout:30000});
  const centre=async name=>{const box=await page.getByRole("button",{name,exact:true}).boundingBox();return [box.x+box.width/2,box.y+box.height/2];};
  const joined=async name=>{
    await page.waitForFunction(name=>__proof.voice?.joined&&__proof.voice.channel_id===__proof.servers[0].channels.find(channel=>channel.name===name)?.id,name,{timeout:10000})
      .catch(async error=>{throw new Error(`${name}: ${error.message} ${JSON.stringify(await page.evaluate(()=>({joins:__proof.joins,voice:__proof.voice})))}`);});
    assert.equal(await page.evaluate(()=>__proof.voice.muted),true,`joining ${name} unmuted`);
  };
  // Touch: both taps open the chat; the chat header has the Join voice control.
  const [tapX,tapY]=await centre("Music");
  for(let tap=0;tap<2;tap++)await page.touchscreen.tap(tapX,tapY);
  await page.waitForFunction(()=>document.querySelector(".chat-header h2")?.textContent==="Music");
  await page.waitForTimeout(500);
  assert.deepEqual(await page.evaluate(()=>__proof.joins),[],"a touch double-tap joined voice");
  // Mouse, with a person's pace between the clicks.
  await page.mouse.move(...await centre("Lobby"));
  for(const clickCount of [1,2]){await page.mouse.down({clickCount});await page.waitForTimeout(60);await page.mouse.up({clickCount});await page.waitForTimeout(150);}
  await joined("Lobby");
  // Pen, through the browser's input pipeline.
  const [penX,penY]=await centre("AFK"),cdp=await context.newCDPSession(page);
  for(const clickCount of [1,2])for(const type of ["mousePressed","mouseReleased"])
    await cdp.send("Input.dispatchMouseEvent",{type,x:penX,y:penY,button:"left",clickCount,pointerType:"pen"});
  await joined("AFK");
  assert.equal(await page.evaluate(()=>__proof.microphoneCalls),0,"joining voice requested the microphone");
  assert.deepEqual(errors,[]);
  console.log("channel double-click: touch opens the chat; mouse and pen join voice muted");
} finally { await browser.close(); }
